use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use futures_util::{SinkExt, StreamExt};
use geo::{Coord as GeoCoord, LineString, Simplify};
use rand::random;
use s2::cellid::CellID;
use s2::latlng::LatLng;
use s2::rect::Rect as S2Rect;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use url::form_urlencoded;

const WORLD_TICK_MS: u64 = 1000;
const MAX_CATCH_UP_TICKS: usize = 60; // Avoid freezing after the server has been offline for a while.
const MAX_HTTP_WORKERS: usize = 64; // Cap in-flight request handler threads.
const REPO_REFRESH_MS: i64 = 10_000;
const ARMY_CAP: i64 = 1_000_000;
const CITY_RADIUS_KM: f64 = 2.0;
const BARRACKS_COST: i64 = 100_000;
const CITY_COST: i64 = 100_000_000;
const MARKET_COST: i64 = 100_000;
const BANK_COST: i64 = 10_000_000;
const HOSPITAL_COST: i64 = 100_000_000;
const BAR_COST: i64 = 100_000;
const SILO_COST: i64 = 10_000_000;
const SAM_COST: i64 = 50_000_000;
const MISSILE_SMALL_COST: i64 = 10_000_000;
const MISSILE_SMALL_RADIUS_KM: f64 = 0.3;
const MISSILE_NUKE_COST: i64 = 100_000_000;
const MISSILE_NUKE_RADIUS_KM: f64 = 1.0;
const MISSILE_HYDROGEN_COST: i64 = 1_000_000_000;
const MISSILE_HYDROGEN_RADIUS_KM: f64 = 3.0;
const SILO_COOLDOWN_MS: i64 = 10 * 60 * 1000;
const SAM_COOLDOWN_MS: i64 = 12 * 60 * 1000;
const SAM_RADIUS_KM: f64 = 5.0;
// Master switch for the buildings/gold-spend/missile feature set. While false,
// /api/build and /api/launch-missile are gated off; gold still accrues so the
// feature can be re-enabled without a state migration.
const BUILDINGS_ENABLED: bool = false;
const PLAYER_COLORS: &[&str] = &[
    "#c81c1c", "#c8391c", "#c8561c", "#c8721c", "#c88f1c", "#c8ac1c", "#c8c81c", "#acc81c",
    "#8fc81c", "#72c81c", "#56c81c", "#39c81c", "#1cc81c", "#1cc839", "#1cc856", "#1cc872",
    "#1cc88f", "#1cc8ac", "#1cc8c8", "#1cacc8", "#1c8fc8", "#1c72c8", "#1c56c8", "#1c39c8",
    "#1c1cc8", "#391cc8", "#561cc8", "#721cc8", "#8f1cc8", "#ac1cc8", "#c81cc8", "#c81cac",
    "#c81c8f", "#c81c72", "#c81c56", "#c81c39",
];

fn random_player_color() -> String {
    let mut bytes = [0u8; 8];
    for b in bytes.iter_mut() {
        *b = random::<u8>();
    }
    PLAYER_COLORS[bytes[0] as usize % PLAYER_COLORS.len()].to_string()
}

fn normalize_player_color(color: Option<&str>) -> String {
    if let Some(c) = color {
        let c = c.trim();
        if c.len() == 7 && c.starts_with('#') && c.chars().skip(1).all(|ch| ch.is_ascii_hexdigit()) {
            return c.to_ascii_lowercase();
        }
    }
    random_player_color()
}

#[derive(Clone, Copy, Debug)]
struct BBox {
    west: f64,
    east: f64,
    south: f64,
    north: f64,
}

impl BBox {
    fn empty() -> Self {
        Self {
            west: f64::INFINITY,
            east: f64::NEG_INFINITY,
            south: f64::INFINITY,
            north: f64::NEG_INFINITY,
        }
    }
}

#[derive(Clone)]
struct BoundaryFeature {
    geometry_type: String,
    coordinates: Value,
    bbox: BBox,
}

#[derive(Default)]
struct BoundaryRepo {
    refreshed_at: i64,
    signature: String,
    features: Vec<BoundaryFeature>,
    // Union of all feature bboxes; used to cheaply reject off-region tiles.
    overall_bbox: Option<BBox>,
}

#[derive(Clone)]
struct RepoNode {
    id: String,
    lat: f64,
    lon: f64,
    degree: i64,
    s2_cell_id: u64,
}

#[derive(Clone, Copy)]
struct Coord {
    lat: f64,
    lon: f64,
}

#[derive(Clone)]
struct Edge {
    to: String,
    weight: f64,
}

// Min-heap entry for pathfinding open lists. BinaryHeap is max-first, so the
// ordering is reversed to pop the lowest score first.
struct OpenNode {
    node_id: String,
    score: f64,
}

impl PartialEq for OpenNode {
    fn eq(&self, other: &Self) -> bool {
        self.score.total_cmp(&other.score).is_eq()
    }
}

impl Eq for OpenNode {}

impl PartialOrd for OpenNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OpenNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.score.total_cmp(&self.score)
    }
}

#[derive(Default)]
struct NodeRepo {
    refreshed_at: i64,
    preview_mtime_ms: i64,
    cache_signature: String,
    boundary_signature: String,
    graph_version: i64,
    nodes_by_id: HashMap<String, RepoNode>,
    adjacency: HashMap<String, Vec<Edge>>,
    coords_by_id: HashMap<String, Coord>,
    connected_components: HashMap<String, i64>,
    immediate_neighbor_cache: HashMap<String, Vec<String>>,
    route_cache: HashMap<String, Vec<[f64; 2]>>,
    tile_node_id_cache: HashMap<String, Vec<String>>,
    s2_sorted_nodes: Vec<(u64, String)>,
    s2_cell_ranges: HashMap<u64, (usize, usize)>,
    s2_index_level: u64,
    display_names: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
enum BuildingType {
    Barracks,
    City,
    Market,
    Bank,
    Hospital,
    Bar,
    Silo,
    Sam,
}

impl BuildingType {
    fn from_str(value: &str) -> Option<Self> {
        match value.to_lowercase().as_str() {
            "barracks" => Some(BuildingType::Barracks),
            "city" => Some(BuildingType::City),
            "market" => Some(BuildingType::Market),
            "bank" => Some(BuildingType::Bank),
            "hospital" => Some(BuildingType::Hospital),
            "bar" => Some(BuildingType::Bar),
            "silo" => Some(BuildingType::Silo),
            "sam" => Some(BuildingType::Sam),
            _ => None,
        }
    }

    fn to_label(&self) -> &'static str {
        match self {
            BuildingType::Barracks => "Barracks",
            BuildingType::City => "City",
            BuildingType::Market => "Market",
            BuildingType::Bank => "Bank",
            BuildingType::Hospital => "Hospital",
            BuildingType::Bar => "Bar",
            BuildingType::Silo => "Silo",
            BuildingType::Sam => "SAM",
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Player {
    id: String,
    username: String,
    password_hash: String,
    created_at: i64,
    start_node_ids: Vec<String>,
    #[serde(default = "random_player_color")]
    color: String,
    #[serde(default)]
    gold: i64,
    #[serde(default)]
    gold_income_per_sec: i64,
    #[serde(default)]
    gold_updated_at: i64,
    #[serde(default)]
    damage_reduction_percent: i64,
    #[serde(default)]
    happiness_percent: i64,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Session {
    token: String,
    player_id: String,
    created_at: i64,
    #[serde(default)]
    expires_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct WorldNode {
    owner_id: Option<String>,
    army: i64,
    #[serde(default)]
    building: Option<BuildingType>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Attack {
    id: String,
    owner_id: String,
    mode: String,
    from_node_id: String,
    to_node_id: String,
    path: Vec<[f64; 2]>,
    #[serde(default = "default_send_per_tick")]
    send_per_tick: i64,
    created_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GameState {
    version: i64,
    saved_at: i64,
    // Game clock baseline, advanced only by tick_world_locked. Separate from
    // saved_at so mutation-triggered saves do not perturb elapsed-tick math.
    #[serde(default)]
    last_tick_at: i64,
    #[serde(default)]
    last_persisted_at: i64,
    #[serde(default)]
    players: HashMap<String, Player>,
    #[serde(default)]
    usernames: HashMap<String, String>,
    #[serde(default)]
    sessions: HashMap<String, Session>,
    #[serde(default)]
    nodes: HashMap<String, WorldNode>,
    #[serde(default)]
    attacks: HashMap<String, Attack>,
    // Per-building cooldowns: node_id -> last-fired ms (Silo/SAM).
    #[serde(default)]
    building_cooldowns: HashMap<String, i64>,
}

impl GameState {
    fn new() -> Self {
        Self {
            version: 1,
            saved_at: now_ms(),
            last_tick_at: now_ms(),
            last_persisted_at: now_ms(),
            players: HashMap::new(),
            usernames: HashMap::new(),
            sessions: HashMap::new(),
            nodes: HashMap::new(),
            attacks: HashMap::new(),
            building_cooldowns: HashMap::new(),
        }
    }
}

struct AppData {
    state: GameState,
    node_repo: NodeRepo,
    boundary_repo: BoundaryRepo,
    synced_graph_version: i64,
    // Cached build_status.json parsed value, keyed by the file's mtime (ms).
    build_status_cache: Option<(i64, Value)>,
    // node_id -> player_id of the city whose radius contains the node.
    // Rebuilt lazily when dirty or when the repo graph version changes.
    city_membership: HashMap<String, String>,
    city_membership_dirty: bool,
    city_membership_graph_version: i64,
    // player_id -> owned node ids (state.nodes iteration order).
    player_nodes: HashMap<String, Vec<String>>,
    player_nodes_dirty: bool,
    // Background-tick counter driving the periodic session purge.
    ticks_since_session_purge: u64,
}

#[derive(Deserialize, Clone)]
struct GenericCollection {
    #[serde(default)]
    features: Vec<GenericFeature>,
}

#[derive(Deserialize, Clone)]
struct GenericFeature {
    #[serde(default)]
    properties: Map<String, Value>,
    geometry: GeometryValue,
}

#[derive(Deserialize, Clone)]
struct GeometryValue {
    #[serde(rename = "type")]
    type_name: String,
    coordinates: Value,
}

#[derive(Deserialize)]
struct OverpassData {
    #[serde(default)]
    elements: Vec<OverpassElement>,
}

#[derive(Deserialize)]
struct OverpassElement {
    #[serde(rename = "type")]
    kind: String,
    id: Option<Value>,
    lat: Option<f64>,
    lon: Option<f64>,
    #[serde(default)]
    nodes: Vec<Value>,
}

struct App {
    root: PathBuf,
    state_path: PathBuf,
    build_status_path: PathBuf,
    preview_path: PathBuf,
    intersections_csv_path: PathBuf,
    cache_dir: PathBuf,
    region_manifest_path: PathBuf,
    state_boundaries_path: PathBuf,
    display_names_path: PathBuf,
    inner: Mutex<AppData>,
    ws_update_sender: crossbeam_channel::Sender<String>,
}

impl App {
    fn new(root: PathBuf, ws_update_sender: crossbeam_channel::Sender<String>) -> Result<Self, String> {
        let data_dir = root.join("game_data");
        fs::create_dir_all(&data_dir).map_err(|err| err.to_string())?;
        let state_path = data_dir.join("state.json");
        let state = read_json_file::<GameState>(&state_path).unwrap_or_else(GameState::new);
        Ok(Self {
            root: root.clone(),
            state_path,
            build_status_path: root
                .join("local_node_store")
                .join("northern_new_england")
                .join("build_status.json"),
            preview_path: root
                .join("local_node_store")
                .join("northern_new_england")
                .join("preview_intersections.geojson"),
            intersections_csv_path: root
                .join("local_node_store")
                .join("northern_new_england")
                .join("intersections.csv"),
            cache_dir: root
                .join("local_node_store")
                .join("northern_new_england")
                .join("_overpass_cache"),
            region_manifest_path: root.join("local_node_store").join("region_manifest.json"),
            state_boundaries_path: root.join("local_node_store").join("us_state_boundaries.geojson"),
            display_names_path: root
                .join("local_node_store")
                .join("northern_new_england")
                .join("node_display_names.json"),
            inner: Mutex::new(AppData {
                state,
                node_repo: NodeRepo::default(),
                boundary_repo: BoundaryRepo::default(),
                synced_graph_version: -1,
                build_status_cache: None,
                city_membership: HashMap::new(),
                city_membership_dirty: true,
                city_membership_graph_version: -1,
                player_nodes: HashMap::new(),
                player_nodes_dirty: true,
                ticks_since_session_purge: 0,
            }),
            ws_update_sender,
        })
    }

    fn save_state_locked(&self, state: &mut GameState) -> Result<(), String> {
        let now = now_ms();
        state.saved_at = now;
        state.last_persisted_at = now;
        write_json_file(&self.state_path, state)
    }

    fn current_build_status(&self, data: &mut AppData) -> Value {
        let mtime_ms = file_mtime_ms(&self.build_status_path).unwrap_or(0);
        if let Some((cached_mtime_ms, cached)) = &data.build_status_cache {
            if *cached_mtime_ms == mtime_ms {
                return cached.clone();
            }
        }
        let value = read_json_value(&self.build_status_path).unwrap_or_else(|| {
            json!({
                "phase": "missing",
                "current": 0,
                "total": 0,
                "node_count": 0,
                "edge_count": 0,
                "message": "Builder has not started yet."
            })
        });
        data.build_status_cache = Some((mtime_ms, value.clone()));
        value
    }

    fn ensure_boundary_repo_fresh(&self, data: &mut AppData, force: bool) -> Result<(), String> {
        let now = now_ms();
        if !force && now - data.boundary_repo.refreshed_at < REPO_REFRESH_MS {
            return Ok(());
        }

        let manifest_mtime_ms = file_mtime_ms(&self.region_manifest_path).unwrap_or(0);
        let boundaries_mtime_ms = file_mtime_ms(&self.state_boundaries_path).unwrap_or(0);
        let manifest = read_json_value(&self.region_manifest_path).unwrap_or_else(|| json!({ "active_region": {} }));
        let region = manifest.get("active_region").cloned().unwrap_or_else(|| json!({}));
        let mut state_names = region
            .get("states")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|value| value.trim().to_lowercase()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        state_names.sort();
        let next_signature = format!(
            "{}|{}|{}",
            manifest_mtime_ms,
            boundaries_mtime_ms,
            state_names.join("|")
        );
        if !force && data.boundary_repo.signature == next_signature {
            data.boundary_repo.refreshed_at = now;
            return Ok(());
        }

        let mut features = if boundaries_mtime_ms > 0 {
            read_json_file::<GenericCollection>(&self.state_boundaries_path)
                .unwrap_or(GenericCollection { features: Vec::new() })
                .features
                .into_iter()
                .filter(|feature| {
                    feature
                        .properties
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|name| state_names.contains(&name.trim().to_lowercase()))
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if features.is_empty() {
            features = fallback_boundary_features(&region);
        }

        // Also include rectangular query bounds from the manifest so that nodes
        // fetched for the active region (which may extend slightly beyond the
        // state boundary polygons) are retained.
        if let Some(bounds) = region.get("query_bounds").and_then(Value::as_array) {
            for bound in bounds {
                if let Some((west, south, east, north, id)) = bound
                    .get("west")
                    .and_then(Value::as_f64)
                    .zip(bound.get("south").and_then(Value::as_f64))
                    .zip(bound.get("east").and_then(Value::as_f64))
                    .zip(bound.get("north").and_then(Value::as_f64))
                    .map(|(((w, s), e), n)| (w, s, e, n))
                    .zip(
                        bound
                            .get("id")
                            .and_then(value_to_string)
                            .or_else(|| bound.get("name").and_then(value_to_string))
                            .or_else(|| Some("query_bound".to_string())),
                    )
                    .map(|((w, s, e, n), id)| (w, s, e, n, id))
                {
                    let coordinates = json!([[
                        [west, south],
                        [east, south],
                        [east, north],
                        [west, north],
                        [west, south]
                    ]]);
                    let mut properties = Map::new();
                    properties.insert("name".to_string(), Value::String(id));
                    features.push(GenericFeature {
                        properties,
                        geometry: GeometryValue {
                            type_name: "Polygon".to_string(),
                            coordinates,
                        },
                    });
                }
            }
        }

        data.boundary_repo.features = features
            .into_iter()
            .map(|feature| {
                let mut bbox = BBox::empty();
                update_bbox_from_coordinates(&feature.geometry.coordinates, &mut bbox);
                BoundaryFeature {
                    geometry_type: feature.geometry.type_name,
                    coordinates: feature.geometry.coordinates,
                    bbox,
                }
            })
            .collect();
        data.boundary_repo.signature = next_signature;
        data.boundary_repo.refreshed_at = now;
        let mut overall = BBox::empty();
        for feature in &data.boundary_repo.features {
            overall.west = overall.west.min(feature.bbox.west);
            overall.east = overall.east.max(feature.bbox.east);
            overall.south = overall.south.min(feature.bbox.south);
            overall.north = overall.north.max(feature.bbox.north);
        }
        data.boundary_repo.overall_bbox =
            (overall.west <= overall.east && overall.south <= overall.north).then_some(overall);
        Ok(())
    }

    fn point_inside_active_region(&self, data: &AppData, lat: f64, lon: f64) -> bool {
        data.boundary_repo.features.iter().any(|feature| {
            !(lon < feature.bbox.west
                || lon > feature.bbox.east
                || lat < feature.bbox.south
                || lat > feature.bbox.north)
                && point_in_geometry(lon, lat, &feature.geometry_type, &feature.coordinates)
        })
    }

    fn load_repo_nodes_from_csv(&self, data: &mut AppData) -> Result<(), String> {
        let file = fs::File::open(&self.intersections_csv_path)
            .map_err(|err| format!("Failed to open intersections CSV: {err}"))?;
        let mut lines = BufReader::new(file).lines();
        let _ = lines.next();
        for line in lines {
            let line = line.map_err(|err| err.to_string())?;
            let mut parts = line.split(',');
            let node_id = parts.next().unwrap_or_default().trim();
            let lat = parts
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok())
                .unwrap_or(f64::NAN);
            let lon = parts
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok())
                .unwrap_or(f64::NAN);
            let degree = parts
                .next()
                .and_then(|value| value.trim().parse::<i64>().ok())
                .unwrap_or(0);
            if node_id.is_empty() || !lon.is_finite() || !lat.is_finite() {
                continue;
            }
            if !self.point_inside_active_region(data, lat, lon) {
                continue;
            }
            let cell_id = CellID::from(LatLng::from_degrees(lat, lon));
            data.node_repo.nodes_by_id.insert(
                node_id.to_string(),
                RepoNode {
                    id: node_id.to_string(),
                    lat,
                    lon,
                    degree,
                    s2_cell_id: cell_id.0,
                },
            );
            data.node_repo
                .coords_by_id
                .insert(node_id.to_string(), Coord { lat, lon });
        }
        Ok(())
    }

    fn build_s2_index(&self, data: &mut AppData) {
        let level = data.node_repo.s2_index_level.max(1).min(30);
        let mut sorted: Vec<(u64, String)> = data
            .node_repo
            .nodes_by_id
            .values()
            .map(|node| (node.s2_cell_id, node.id.clone()))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut ranges: HashMap<u64, (usize, usize)> = HashMap::new();
        for (index, (leaf_cell_raw, _)) in sorted.iter().enumerate() {
            let parent = CellID(*leaf_cell_raw).parent(level);
            let entry = ranges.entry(parent.0).or_insert((index, index));
            entry.1 = index;
        }

        data.node_repo.s2_sorted_nodes = sorted;
        data.node_repo.s2_cell_ranges = ranges;
    }

    fn ensure_node_repo_fresh(&self, data: &mut AppData, force: bool) -> Result<(), String> {
        let now = now_ms();
        if !force && now - data.node_repo.refreshed_at < REPO_REFRESH_MS {
            return Ok(());
        }
        self.ensure_boundary_repo_fresh(data, force)?;

        let preview_mtime_ms = file_mtime_ms(&self.preview_path).unwrap_or(0);
        let intersections_csv_mtime_ms = file_mtime_ms(&self.intersections_csv_path).unwrap_or(0);
        let node_source_mtime_ms = preview_mtime_ms.max(intersections_csv_mtime_ms);
        let cache_signature = cache_signature(&self.cache_dir);
        if !force
            && data.node_repo.preview_mtime_ms == node_source_mtime_ms
            && data.node_repo.cache_signature == cache_signature
            && data.node_repo.boundary_signature == data.boundary_repo.signature
        {
            data.node_repo.refreshed_at = now;
            return Ok(());
        }

        data.node_repo.nodes_by_id.clear();
        data.node_repo.adjacency.clear();
        data.node_repo.coords_by_id.clear();
        data.node_repo.connected_components.clear();
        data.node_repo.immediate_neighbor_cache.clear();
        data.node_repo.route_cache.clear();
        data.node_repo.tile_node_id_cache.clear();
        data.node_repo.s2_sorted_nodes.clear();
        data.node_repo.s2_cell_ranges.clear();
        data.node_repo.s2_index_level = 12;
        data.node_repo.display_names.clear();

        if self.preview_path.exists() {
            let preview = read_json_file::<GenericCollection>(&self.preview_path)
                .unwrap_or(GenericCollection { features: Vec::new() });
            for feature in preview.features {
                let node_id = feature
                    .properties
                    .get("id")
                    .and_then(value_to_string)
                    .unwrap_or_default();
                let (lon, lat) = pair_from_value(&feature.geometry.coordinates).unwrap_or((f64::NAN, f64::NAN));
                if node_id.is_empty() || !lon.is_finite() || !lat.is_finite() {
                    continue;
                }
                if !self.point_inside_active_region(data, lat, lon) {
                    continue;
                }
                let degree = feature
                    .properties
                    .get("degree")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .round() as i64;
                let cell_id = CellID::from(LatLng::from_degrees(lat, lon));
                data.node_repo.nodes_by_id.insert(
                    node_id.clone(),
                    RepoNode {
                        id: node_id.clone(),
                        lat,
                        lon,
                        degree,
                        s2_cell_id: cell_id.0,
                    },
                );
                data.node_repo.coords_by_id.insert(node_id, Coord { lat, lon });
            }
        } else if self.intersections_csv_path.exists() {
            self.load_repo_nodes_from_csv(data)?;
        } else {
            return Err("No prepared node source found.".to_string());
        }

        self.build_s2_index(data);

        let mut cache_files = fs::read_dir(&self.cache_dir)
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(Result::ok))
            .filter_map(|entry| {
                let path = entry.path();
                let is_json = path.extension().and_then(|ext| ext.to_str()) == Some("json");
                is_json.then_some(path)
            })
            .collect::<Vec<_>>();
        cache_files.sort();

        for file_path in cache_files {
            let data_file =
                read_json_file::<OverpassData>(&file_path).unwrap_or(OverpassData { elements: Vec::new() });
            let mut local_coords = HashMap::<String, Coord>::new();
            for element in &data_file.elements {
                if element.kind == "node" {
                    if let (Some(lat), Some(lon), Some(node_id)) =
                        (element.lat, element.lon, element.id.as_ref().and_then(value_to_string))
                    {
                        if !self.point_inside_active_region(data, lat, lon) {
                            continue;
                        }
                        let coord = Coord { lat, lon };
                        local_coords.insert(node_id.clone(), coord);
                        data.node_repo.coords_by_id.insert(node_id, coord);
                    }
                }
            }
            for element in &data_file.elements {
                if element.kind != "way" || element.nodes.len() < 2 {
                    continue;
                }
                for pair in element.nodes.windows(2) {
                    let a = value_to_string(&pair[0]).unwrap_or_default();
                    let b = value_to_string(&pair[1]).unwrap_or_default();
                    if a.is_empty() || b.is_empty() {
                        continue;
                    }
                    let a_coord = data
                        .node_repo
                        .coords_by_id
                        .get(&a)
                        .copied()
                        .or_else(|| local_coords.get(&a).copied());
                    let b_coord = data
                        .node_repo
                        .coords_by_id
                        .get(&b)
                        .copied()
                        .or_else(|| local_coords.get(&b).copied());
                    let (Some(a_coord), Some(b_coord)) = (a_coord, b_coord) else {
                        continue;
                    };
                    let dist = haversine_km(a_coord.lat, a_coord.lon, b_coord.lat, b_coord.lon);
                    data.node_repo
                        .adjacency
                        .entry(a.clone())
                        .or_default()
                        .push(Edge {
                            to: b.clone(),
                            weight: dist,
                        });
                    data.node_repo.adjacency.entry(b).or_default().push(Edge { to: a, weight: dist });
                }
            }
        }
        
        let mut component_id = 0_i64;
        let all_graph_node_ids = data
            .node_repo
            .coords_by_id
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut visited_graph_nodes = HashSet::<String>::new();
        for node_id in all_graph_node_ids {
            if visited_graph_nodes.contains(&node_id) {
                continue;
            }
            component_id += 1;
            let mut stack = vec![node_id.clone()];
            while let Some(current) = stack.pop() {
                if !visited_graph_nodes.insert(current.clone()) {
                    continue;
                }
                if data.node_repo.nodes_by_id.contains_key(&current) {
                    data.node_repo
                        .connected_components
                        .insert(current.clone(), component_id);
                }
                let neighbors = data
                    .node_repo
                    .adjacency
                    .get(&current)
                    .cloned()
                    .unwrap_or_default();
                for edge in neighbors {
                    if !visited_graph_nodes.contains(&edge.to) {
                        stack.push(edge.to.clone());
                    }
                }
            }
        }

        if self.display_names_path.exists() {
            let names = read_json_file::<HashMap<String, String>>(&self.display_names_path)
                .unwrap_or_default();
            data.node_repo.display_names = names;
        }

        data.node_repo.preview_mtime_ms = node_source_mtime_ms;
        data.node_repo.cache_signature = cache_signature;
        data.node_repo.boundary_signature = data.boundary_repo.signature.clone();
        data.node_repo.graph_version += 1;
        data.node_repo.refreshed_at = now;
        Ok(())
    }

    fn sync_world_nodes_locked(&self, data: &mut AppData) -> Result<bool, String> {
        self.ensure_node_repo_fresh(data, false)?;
        if data.synced_graph_version == data.node_repo.graph_version {
            return Ok(false);
        }
        data.synced_graph_version = data.node_repo.graph_version;
        // The world node set changed; cached ownership/city-membership
        // indexes must be rebuilt before their next use.
        data.city_membership_dirty = true;
        data.player_nodes_dirty = true;
        let mut modified = false;
        let valid_node_ids = data.node_repo.nodes_by_id.keys().cloned().collect::<HashSet<_>>();

        for node_id in data.node_repo.nodes_by_id.keys() {
            if !data.state.nodes.contains_key(node_id) {
                data.state.nodes.insert(
                    node_id.clone(),
                    WorldNode {
                        owner_id: None,
                        army: 10,
                        building: None,
                    },
                );
                modified = true;
            }
        }

        let existing_node_ids = data.state.nodes.keys().cloned().collect::<Vec<_>>();
        for node_id in existing_node_ids {
            if !valid_node_ids.contains(&node_id) {
                data.state.nodes.remove(&node_id);
                data.state.building_cooldowns.remove(&node_id);
                modified = true;
            }
        }

        let attack_ids = data.state.attacks.keys().cloned().collect::<Vec<_>>();
        for attack_id in attack_ids {
            let should_remove = data
                .state
                .attacks
                .get(&attack_id)
                .map(|attack| {
                    !valid_node_ids.contains(&attack.from_node_id)
                        || !valid_node_ids.contains(&attack.to_node_id)
                })
                .unwrap_or(false);
            if should_remove {
                data.state.attacks.remove(&attack_id);
                modified = true;
            }
        }

        if modified {
            self.save_state_locked(&mut data.state)?;
        }
        Ok(modified)
    }

    fn warm_node_repo(&self) -> Result<(), String> {
        let mut data = app_lock(&self.inner);
        println!("Warming node repository...");
        self.ensure_node_repo_fresh(&mut data, true)?;
        self.sync_world_nodes_locked(&mut data)?;
        println!(
            "Node repository ready: {} nodes, {} edges.",
            data.node_repo.nodes_by_id.len(),
            data.node_repo.adjacency.values().map(|edges| edges.len()).sum::<usize>()
        );
        Ok(())
    }

    fn sorted_neutral_nodes_locked(&self, data: &mut AppData) -> Result<Vec<RepoNode>, String> {
        self.sync_world_nodes_locked(data)?;
        Ok(data
            .node_repo
            .nodes_by_id
            .values()
            .filter(|node| data.state.nodes.get(&node.id).map(|entry| entry.owner_id.is_none()).unwrap_or(false))
            .cloned()
            .collect())
    }

    fn choose_start_nodes_locked(&self, data: &mut AppData) -> Result<Vec<String>, String> {
        let neutrals = self.sorted_neutral_nodes_locked(data)?;
        if neutrals.len() < 10 {
            return Err("Not enough generated nodes yet. Wait for more node batches to complete.".to_string());
        }
        let seed = neutrals[(random::<u64>() as usize) % neutrals.len()].clone();
        let mut scored = neutrals
            .into_iter()
            .map(|node| {
                let score = haversine_km(seed.lat, seed.lon, node.lat, node.lon);
                (node.id, score)
            })
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().take(10).map(|item| item.0).collect())
    }

    fn choose_start_nodes_around_locked(
        &self,
        data: &mut AppData,
        start_node_id: &str,
    ) -> Result<Vec<String>, String> {
        self.sync_world_nodes_locked(data)?;
        let start_repo = data
            .node_repo
            .nodes_by_id
            .get(start_node_id)
            .cloned()
            .ok_or_else(|| "Selected starting node does not exist.".to_string())?;
        let start_world = data
            .state
            .nodes
            .get(start_node_id)
            .cloned()
            .ok_or_else(|| "Selected starting node is not part of the world yet.".to_string())?;
        if start_world.owner_id.is_some() {
            return Err("That node is already owned. Choose a neutral node.".to_string());
        }
        let mut scored: Vec<(String, f64)> = data
            .node_repo
            .nodes_by_id
            .values()
            .filter_map(|node| {
                let world_node = data.state.nodes.get(&node.id)?;
                if world_node.owner_id.is_some() {
                    return None;
                }
                let dist = haversine_km(start_repo.lat, start_repo.lon, node.lat, node.lon);
                Some((node.id.clone(), dist))
            })
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().take(10).map(|item| item.0).collect())
    }

    fn update_player_gold_locked(&self, data: &mut AppData, player_id: &str) {
        let now = now_ms();
        if let Some(player) = data.state.players.get_mut(player_id) {
            let elapsed_ms = (now - player.gold_updated_at).max(0);
            // Cap catch-up to one hour so an offline gap does not overflow.
            let elapsed_secs = (elapsed_ms / 1000).min(60 * 60);
            if elapsed_secs > 0 {
                player.gold = player
                    .gold
                    .saturating_add(player.gold_income_per_sec.saturating_mul(elapsed_secs));
                player.gold_updated_at = now;
            }
        }
    }

    // Rebuild the node_id -> player_id city-membership map: a node gets an
    // entry when it lies within CITY_RADIUS_KM of a City building. When radii
    // of different players' cities overlap, the first matching city wins.
    // Cost is O(cities x repo nodes), paid only when the dirty flag is set.
    fn rebuild_city_membership_locked(&self, data: &mut AppData) {
        let cities: Vec<(String, Coord)> = data
            .state
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.owner_id.is_some() && node.building == Some(BuildingType::City)
            })
            .filter_map(|(node_id, node)| {
                data.node_repo
                    .coords_by_id
                    .get(node_id)
                    .map(|coord| (node.owner_id.clone().unwrap(), *coord))
            })
            .collect();
        let mut membership = HashMap::new();
        if !cities.is_empty() {
            for node in data.node_repo.nodes_by_id.values() {
                for (owner_id, city_coord) in &cities {
                    if haversine_km(node.lat, node.lon, city_coord.lat, city_coord.lon)
                        <= CITY_RADIUS_KM
                    {
                        membership.insert(node.id.clone(), owner_id.clone());
                        break;
                    }
                }
            }
        }
        data.city_membership = membership;
        data.city_membership_graph_version = data.node_repo.graph_version;
        data.city_membership_dirty = false;
    }

    fn ensure_city_membership_locked(&self, data: &mut AppData) {
        if data.city_membership_dirty
            || data.city_membership_graph_version != data.node_repo.graph_version
        {
            self.rebuild_city_membership_locked(data);
        }
    }

    // Rebuild the player_id -> owned node ids index in one pass over world
    // nodes, preserving state.nodes iteration order so economy math sees the
    // same node sequence as a full scan would.
    fn rebuild_player_nodes_locked(&self, data: &mut AppData) {
        let mut player_nodes: HashMap<String, Vec<String>> = HashMap::new();
        for (node_id, node) in data.state.nodes.iter() {
            if let Some(owner_id) = node.owner_id.as_ref() {
                player_nodes
                    .entry(owner_id.clone())
                    .or_default()
                    .push(node_id.clone());
            }
        }
        data.player_nodes = player_nodes;
        data.player_nodes_dirty = false;
    }

    fn ensure_player_nodes_locked(&self, data: &mut AppData) {
        if data.player_nodes_dirty {
            self.rebuild_player_nodes_locked(data);
        }
    }

    // Drop sessions past their expiry so state.json does not grow forever.
    // Returns true when at least one session was removed.
    fn purge_expired_sessions_locked(&self, data: &mut AppData) -> bool {
        let now = now_ms();
        let before = data.state.sessions.len();
        data.state.sessions.retain(|_, session| session.expires_at > now);
        data.state.sessions.len() != before
    }

    fn player_city_node_ids_locked(&self, data: &AppData, player_id: &str) -> HashSet<String> {
        data.city_membership
            .iter()
            .filter_map(|(node_id, owner_id)| (owner_id == player_id).then(|| node_id.clone()))
            .collect()
    }

    fn is_node_inside_player_city_locked(
        &self,
        data: &AppData,
        node_id: &str,
        player_id: &str,
    ) -> bool {
        data.city_membership
            .get(node_id)
            .map(|owner_id| owner_id == player_id)
            .unwrap_or(false)
    }

    fn nearest_own_city_id_locked(
        &self,
        data: &AppData,
        player_id: &str,
        node_id: &str,
    ) -> Option<String> {
        let coord = data.node_repo.coords_by_id.get(node_id)?;
        let mut best: Option<(String, f64)> = None;
        for city_id in data.player_nodes.get(player_id).into_iter().flatten() {
            let is_city = data
                .state
                .nodes
                .get(city_id)
                .map(|city_node| city_node.building == Some(BuildingType::City))
                .unwrap_or(false);
            if !is_city {
                continue;
            }
            let Some(city_coord) = data.node_repo.coords_by_id.get(city_id) else {
                continue;
            };
            let dist = haversine_km(coord.lat, coord.lon, city_coord.lat, city_coord.lon);
            if dist <= CITY_RADIUS_KM
                && best.as_ref().map(|(_, d)| dist < *d).unwrap_or(true)
            {
                best = Some((city_id.clone(), dist));
            }
        }
        best.map(|(id, _)| id)
    }

    fn recompute_player_economy_locked(&self, data: &mut AppData, player_id: &str) {
        self.update_player_gold_locked(data, player_id);
        self.ensure_city_membership_locked(data);
        self.ensure_player_nodes_locked(data);
        let city_nodes = self.player_city_node_ids_locked(data, player_id);
        // Iterate only this player's owned nodes (indexed in state.nodes
        // iteration order) instead of scanning the whole world per player.
        let owned_ids = data
            .player_nodes
            .get(player_id)
            .cloned()
            .unwrap_or_default();
        let owned_count = owned_ids.len() as i64;
        let mut income = owned_count; // 1 gold per node per second
        let mut hospitals = 0_i64;
        let mut bars = 0_i64;
        let mut markets = 0_i64;
        let mut bank_cities = HashSet::<String>::new();
        for node_id in &owned_ids {
            let Some(node) = data.state.nodes.get(node_id) else { continue };
            let Some(building) = &node.building else { continue };
            let in_city = city_nodes.contains(node_id);
            match building {
                BuildingType::Market => {
                    let base = 20 + 10 * markets;
                    income += if in_city { (base * 15) / 10 } else { base };
                    markets += 1;
                }
                BuildingType::Bank => {
                    if in_city {
                        if let Some(city_id) = self.nearest_own_city_id_locked(data, player_id, node_id) {
                            if bank_cities.insert(city_id) {
                                income += (300 * 15) / 10;
                            }
                        }
                    }
                }
                BuildingType::Hospital => hospitals += 1,
                BuildingType::Bar => bars += 1,
                _ => {}
            }
        }
        if let Some(player) = data.state.players.get_mut(player_id) {
            player.gold_income_per_sec = income;
            // Clamp at the source so the UI never shows >99% reduction.
            player.damage_reduction_percent = hospitals.min(99);
            player.happiness_percent = bars;
        }
    }

    fn building_cost(&self, building: &BuildingType) -> i64 {
        match building {
            BuildingType::Barracks => BARRACKS_COST,
            BuildingType::City => CITY_COST,
            BuildingType::Market => MARKET_COST,
            BuildingType::Bank => BANK_COST,
            BuildingType::Hospital => HOSPITAL_COST,
            BuildingType::Bar => BAR_COST,
            BuildingType::Silo => SILO_COST,
            BuildingType::Sam => SAM_COST,
        }
    }

    fn build_building_locked(
        &self,
        data: &mut AppData,
        player_id: &str,
        node_id: &str,
        building: BuildingType,
    ) -> Result<Value, String> {
        self.sync_world_nodes_locked(data)?;
        self.ensure_city_membership_locked(data);
        self.ensure_player_nodes_locked(data);
        self.update_player_gold_locked(data, player_id);
        let node = data
            .state
            .nodes
            .get(node_id)
            .cloned()
            .ok_or_else(|| "Node does not exist.".to_string())?;
        if node.owner_id.as_deref() != Some(player_id) {
            return Err("You do not own this node.".to_string());
        }
        if node.building.is_some() {
            return Err("This node already has a building.".to_string());
        }
        let cost = self.building_cost(&building);
        let player = data
            .state
            .players
            .get(player_id)
            .ok_or_else(|| "Player not found.".to_string())?;
        if player.gold < cost {
            return Err(format!(
                "Not enough gold. {} costs {} gold.",
                building.to_label(),
                cost
            ));
        }

        // Requirement checks
        match &building {
            BuildingType::Market | BuildingType::Bank | BuildingType::Hospital | BuildingType::Bar => {
                if !self.is_node_inside_player_city_locked(data, node_id, player_id) {
                    return Err(format!(
                        "{} must be built inside one of your cities.",
                        building.to_label()
                    ));
                }
            }
            _ => {}
        }
        if building == BuildingType::Bank {
            if let Some(city_id) = self.nearest_own_city_id_locked(data, player_id, node_id) {
                let has_bank = data.state.nodes.iter().any(|(nid, n)| {
                    n.owner_id.as_deref() == Some(player_id)
                        && n.building == Some(BuildingType::Bank)
                        && self.nearest_own_city_id_locked(data, player_id, nid).as_deref() == Some(city_id.as_str())
                });
                if has_bank {
                    return Err("This city already has a bank.".to_string());
                }
            }
        }

        let player = data.state.players.get_mut(player_id).unwrap();
        player.gold -= cost;
        let building_label = building.to_label().to_string();
        if let Some(node) = data.state.nodes.get_mut(node_id) {
            node.building = Some(building);
            node.army = 0;
        }
        // The set of city buildings (and their radii) may have changed.
        data.city_membership_dirty = true;
        self.recompute_player_economy_locked(data, player_id);
        data.state.version += 1;
        self.save_state_locked(&mut data.state)?;
        Ok(json!({
            "ok": true,
            "nodeId": node_id,
            "building": building_label,
            "gold": data.state.players.get(player_id).map(|p| p.gold).unwrap_or(0),
            "goldIncomePerSec": data.state.players.get(player_id).map(|p| p.gold_income_per_sec).unwrap_or(0)
        }))
    }

    fn launch_missile_locked(
        &self,
        data: &mut AppData,
        player_id: &str,
        missile_type: &str,
        target_lat: f64,
        target_lon: f64,
    ) -> Result<Value, String> {
        self.sync_world_nodes_locked(data)?;
        self.update_player_gold_locked(data, player_id);
        let (cost, radius_km) = match missile_type.to_lowercase().as_str() {
            "small" => (MISSILE_SMALL_COST, MISSILE_SMALL_RADIUS_KM),
            "nuke" => (MISSILE_NUKE_COST, MISSILE_NUKE_RADIUS_KM),
            "hydrogen" => (MISSILE_HYDROGEN_COST, MISSILE_HYDROGEN_RADIUS_KM),
            _ => return Err("Unknown missile type.".to_string()),
        };
        // Reject targets outside the loaded map region.
        if let Some(bbox) = &data.boundary_repo.overall_bbox {
            if target_lon < bbox.west
                || target_lon > bbox.east
                || target_lat < bbox.south
                || target_lat > bbox.north
            {
                return Err("Target is outside the active map region.".to_string());
            }
        }
        let now = now_ms();
        let silo_ids: Vec<String> = data
            .state
            .nodes
            .iter()
            .filter_map(|(node_id, node)| {
                (node.owner_id.as_deref() == Some(player_id)
                    && node.building == Some(BuildingType::Silo))
                .then(|| node_id.clone())
            })
            .collect();
        if silo_ids.is_empty() {
            return Err("You need a Silo to launch missiles.".to_string());
        }
        let player = data.state.players.get(player_id).ok_or_else(|| "Player not found.".to_string())?;
        if player.gold < cost {
            return Err(format!("Not enough gold. This missile costs {} gold.", cost));
        }
        // Cooldowns are per silo: firing requires at least one owned Silo
        // whose own cooldown is ready, and that silo gets stamped.
        let ready_silo_id = silo_ids
            .iter()
            .find(|node_id| {
                now - data.state.building_cooldowns.get(*node_id).copied().unwrap_or(0)
                    >= SILO_COOLDOWN_MS
            })
            .cloned()
            .ok_or_else(|| "Silo is on cooldown.".to_string())?;

        // SAM interception: any enemy SAM within SAM_RADIUS_KM of the target
        // whose own cooldown is ready can intercept.
        let mut intercepted = false;
        let mut interceptor_id: Option<String> = None;
        let mut interceptor_sam_id: Option<String> = None;
        let sam_owners: Vec<String> = data
            .state
            .players
            .keys()
            .filter(|id| *id != player_id)
            .cloned()
            .collect();
        'outer: for owner_id in sam_owners {
            for (sam_node_id, sam_node) in data.state.nodes.iter() {
                if sam_node.owner_id.as_deref() != Some(owner_id.as_str())
                    || sam_node.building != Some(BuildingType::Sam)
                {
                    continue;
                }
                let sam_coord = match data.node_repo.coords_by_id.get(sam_node_id) {
                    Some(c) => c,
                    None => continue,
                };
                if haversine_km(target_lat, target_lon, sam_coord.lat, sam_coord.lon) > SAM_RADIUS_KM {
                    continue;
                }
                if now - data.state.building_cooldowns.get(sam_node_id).copied().unwrap_or(0)
                    < SAM_COOLDOWN_MS
                {
                    continue;
                }
                intercepted = true;
                interceptor_id = Some(owner_id.clone());
                interceptor_sam_id = Some(sam_node_id.clone());
                break 'outer;
            }
        }

        if intercepted {
            if let Some(sam_node_id) = &interceptor_sam_id {
                data.state.building_cooldowns.insert(sam_node_id.clone(), now);
            }
            data.state.building_cooldowns.insert(ready_silo_id.clone(), now);
            let player = data.state.players.get_mut(player_id).unwrap();
            player.gold -= cost;
            data.state.version += 1;
            self.save_state_locked(&mut data.state)?;
            return Ok(json!({
                "ok": true,
                "intercepted": true,
                "interceptorId": interceptor_id,
                "targetLat": target_lat,
                "targetLon": target_lon,
                "radiusKm": radius_km,
            }));
        }

        let mut affected = Vec::<String>::new();
        for (node_id, node) in data.state.nodes.iter_mut() {
            if node.owner_id.as_deref() == Some(player_id) {
                continue;
            }
            if node.building.is_some() {
                continue;
            }
            let coord = match data.node_repo.coords_by_id.get(node_id) {
                Some(c) => c,
                None => continue,
            };
            if haversine_km(target_lat, target_lon, coord.lat, coord.lon) <= radius_km {
                node.army = 0;
                node.owner_id = Some(player_id.to_string());
                affected.push(node_id.clone());
            }
        }
        if !affected.is_empty() {
            data.player_nodes_dirty = true;
        }

        data.state.building_cooldowns.insert(ready_silo_id.clone(), now);
        let player = data.state.players.get_mut(player_id).unwrap();
        player.gold -= cost;
        self.recompute_player_economy_locked(data, player_id);
        data.state.version += 1;
        self.save_state_locked(&mut data.state)?;
        Ok(json!({
            "ok": true,
            "intercepted": false,
            "missileType": missile_type,
            "targetLat": target_lat,
            "targetLon": target_lon,
            "radiusKm": radius_km,
            "affectedNodeIds": affected,
            "affectedCount": affected.len(),
        }))
    }

    fn build_building_request(
        &self,
        token: &str,
        node_id: &str,
        building: BuildingType,
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        self.build_building_locked(&mut data, &session.player_id, node_id, building)
    }

    fn launch_missile_request(
        &self,
        token: &str,
        missile_type: &str,
        target_lat: f64,
        target_lon: f64,
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        self.launch_missile_locked(&mut data, &session.player_id, missile_type, target_lat, target_lon)
    }

    fn city_features_locked(&self, data: &AppData) -> Vec<Value> {
        data.state
            .nodes
            .iter()
            .filter_map(|(node_id, node)| {
                if node.building != Some(BuildingType::City) {
                    return None;
                }
                let coord = data.node_repo.coords_by_id.get(node_id)?;
                let owner = node.owner_id.as_ref().and_then(|pid| data.state.players.get(pid));
                let color = owner.map(|p| p.color.clone()).unwrap_or_else(|| "#94a3b8".to_string());
                Some(json!({
                    "type": "Feature",
                    "geometry": circle_polygon(coord.lat, coord.lon, CITY_RADIUS_KM),
                    "properties": {
                        "id": node_id,
                        "ownerId": node.owner_id,
                        "ownerColor": color,
                    }
                }))
            })
            .collect()
    }

    fn node_army_production_rate_locked(
        &self,
        data: &AppData,
        node_id: &str,
        player_id: &str,
    ) -> f64 {
        let mut rate = 1.0;
        if data.node_repo.coords_by_id.get(node_id).is_none() {
            return 0.0;
        }
        if let Some(neighbors) = data.node_repo.adjacency.get(node_id) {
            for edge in neighbors {
                let neighbor = match data.state.nodes.get(&edge.to) {
                    Some(n) => n,
                    None => continue,
                };
                if neighbor.owner_id.as_deref() != Some(player_id) {
                    continue;
                }
                if neighbor.building != Some(BuildingType::Barracks) {
                    continue;
                }
                let mut bonus = 1.0;
                if self.is_node_inside_player_city_locked(data, &edge.to, player_id) {
                    bonus *= 1.5;
                }
                rate += bonus;
            }
        }
        if self.is_node_inside_player_city_locked(data, node_id, player_id) {
            rate *= 1.5;
        }
        rate
    }

    fn require_session_locked(&self, data: &AppData, token: &str) -> Result<Session, String> {
        let session = data
            .state
            .sessions
            .get(token)
            .cloned()
            .filter(|session| data.state.players.contains_key(&session.player_id))
            .ok_or_else(|| "Invalid session.".to_string())?;
        if session.expires_at <= now_ms() {
            return Err("Session expired. Please log in again.".to_string());
        }
        Ok(session)
    }

    fn reconstruct_node_id_path(
        &self,
        prev: &HashMap<String, String>,
        start_id: &str,
        target_id: &str,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut cursor = Some(target_id.to_string());
        while let Some(current) = cursor {
            out.push(current.clone());
            if current == start_id {
                break;
            }
            cursor = prev.get(&current).cloned();
        }
        out.reverse();
        out
    }

    fn immediate_neighbor_targets_locked(
        &self,
        data: &mut AppData,
        from_node_id: &str,
    ) -> Result<Vec<String>, String> {
        self.ensure_node_repo_fresh(data, false)?;
        if !data.node_repo.nodes_by_id.contains_key(from_node_id) {
            return Err("Invalid nodes.".to_string());
        }
        let cache_key = format!("{}:{}", data.node_repo.graph_version, from_node_id);
        if let Some(targets) = data.node_repo.immediate_neighbor_cache.get(&cache_key) {
            return Ok(targets.clone());
        }

        let start = from_node_id.to_string();
        let mut open = BinaryHeap::<OpenNode>::new();
        open.push(OpenNode {
            node_id: start.clone(),
            score: 0.0,
        });
        let mut dist = HashMap::<String, f64>::new();
        let mut prev = HashMap::<String, String>::new();
        let mut targets = Vec::<String>::new();
        let mut seen_targets = HashSet::<String>::new();
        dist.insert(start.clone(), 0.0);

        while let Some(OpenNode {
            node_id: current,
            score: current_score,
        }) = open.pop()
        {
            let best_score = *dist.get(&current).unwrap_or(&f64::INFINITY);
            if current_score > best_score {
                continue;
            }
            if current != start && data.node_repo.nodes_by_id.contains_key(&current) {
                if seen_targets.insert(current.clone()) {
                    let path_node_ids = self.reconstruct_node_id_path(&prev, &start, &current);
                    let path_coords = path_node_ids
                        .iter()
                        .filter_map(|node_id| data.node_repo.coords_by_id.get(node_id))
                        .map(|coord| [coord.lon, coord.lat])
                        .collect::<Vec<_>>();
                    if path_coords.len() >= 2 {
                        let route_cache_key = format!(
                            "{}:{}:{}",
                            data.node_repo.graph_version, from_node_id, current
                        );
                        data.node_repo.route_cache.insert(route_cache_key, path_coords);
                        targets.push(current.clone());
                    }
                }
                continue;
            }

            if let Some(neighbors) = data.node_repo.adjacency.get(&current) {
                for edge in neighbors {
                    if edge.to == start {
                        continue;
                    }
                    let tentative = current_score + edge.weight;
                    let known = *dist.get(&edge.to).unwrap_or(&f64::INFINITY);
                    if tentative >= known {
                        continue;
                    }
                    dist.insert(edge.to.clone(), tentative);
                    prev.insert(edge.to.clone(), current.clone());
                    open.push(OpenNode {
                        node_id: edge.to.clone(),
                        score: tentative,
                    });
                }
            }
        }

        data.node_repo
            .immediate_neighbor_cache
            .insert(cache_key, targets.clone());
        Ok(targets)
    }

    fn register_player(
        &self,
        username: &str,
        password: &str,
        color: Option<&str>,
        start_node_id: Option<&str>,
    ) -> Result<Value, String> {
        let normalized = username.trim().to_lowercase();
        if normalized.len() < 3 {
            return Err("Username must be at least 3 characters.".to_string());
        }
        if password.len() < 4 {
            return Err("Password must be at least 4 characters.".to_string());
        }
        let start_node_id = start_node_id.map(|s| s.trim()).filter(|s| !s.is_empty());
        let mut data = app_lock(&self.inner);
        if data.state.usernames.contains_key(&normalized) {
            return Err("Username already exists.".to_string());
        }
        let player_id = random_id("player");
        let session_token = random_id("session");
        let start_nodes = if let Some(node_id) = start_node_id {
            self.choose_start_nodes_around_locked(&mut data, node_id)?
        } else {
            self.choose_start_nodes_locked(&mut data)?
        };
        for node_id in &start_nodes {
            data.state.nodes.insert(
                node_id.clone(),
                WorldNode {
                    owner_id: Some(player_id.clone()),
                    army: 10,
                    building: None,
                },
            );
        }
        data.player_nodes_dirty = true;
        let now = now_ms();
        data.state.players.insert(
            player_id.clone(),
            Player {
                id: player_id.clone(),
                username: normalized.clone(),
                password_hash: hash_password_argon2(password)?,
                created_at: now,
                start_node_ids: start_nodes.clone(),
                color: normalize_player_color(color),
                gold: 0,
                gold_income_per_sec: 0,
                gold_updated_at: now,
                damage_reduction_percent: 0,
                happiness_percent: 0,
            },
        );
        self.recompute_player_economy_locked(&mut data, &player_id);
        data.state.usernames.insert(normalized, player_id.clone());
        data.state.sessions.insert(
            session_token.clone(),
            Session {
                token: session_token.clone(),
                player_id: player_id.clone(),
                created_at: now,
                expires_at: now + 7 * 24 * 60 * 60 * 1000,
            },
        );
        self.purge_expired_sessions_locked(&mut data);
        self.save_state_locked(&mut data.state)?;
        Ok(json!({ "playerId": player_id, "token": session_token }))
    }

    fn login_player(&self, username: &str, password: &str) -> Result<Value, String> {
        let normalized = username.trim().to_lowercase();
        let mut data = app_lock(&self.inner);
        let player_id = data
            .state
            .usernames
            .get(&normalized)
            .cloned()
            .ok_or_else(|| "Invalid username or password.".to_string())?;
        let player = data
            .state
            .players
            .get(&player_id)
            .cloned()
            .ok_or_else(|| "Invalid username or password.".to_string())?;
        let (verified, is_old_hash) = verify_password(password, &player.password_hash)?;
        if !verified {
            return Err("Invalid username or password.".to_string());
        }
        let session_token = random_id("session");
        let now = now_ms();
        data.state.sessions.insert(
            session_token.clone(),
            Session {
                token: session_token.clone(),
                player_id: player_id.clone(),
                created_at: now,
                expires_at: now + 7 * 24 * 60 * 60 * 1000,
            },
        );
        if is_old_hash {
            if let Some(player_ref) = data.state.players.get_mut(&player_id) {
                player_ref.password_hash = hash_password_argon2(password)?;
            }
        }
        self.purge_expired_sessions_locked(&mut data);
        self.save_state_locked(&mut data.state)?;
        Ok(json!({ "playerId": player_id, "token": session_token }))
    }

    fn logout_player(&self, token: &str) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        data.state.sessions.remove(token);
        self.save_state_locked(&mut data.state)?;
        Ok(json!({ "ok": true }))
    }

    fn step_world_locked(&self, data: &mut AppData) -> Result<(), String> {
        self.sync_world_nodes_locked(data)?;

        // Update lazy gold for every player and recompute their economies once per tick.
        let player_ids: Vec<String> = data.state.players.keys().cloned().collect();
        for player_id in &player_ids {
            self.recompute_player_economy_locked(data, player_id);
        }

        // Building-aware army production.
        let production_rates: HashMap<String, i64> = data
            .state
            .nodes
            .iter()
            .filter_map(|(node_id, node)| {
                let player_id = node.owner_id.as_deref()?;
                if node.building.is_some() {
                    return None;
                }
                let rate = self.node_army_production_rate_locked(data, node_id, player_id);
                if rate > 0.0 {
                    Some((node_id.clone(), rate as i64))
                } else {
                    None
                }
            })
            .collect();
        for (node_id, rate) in production_rates {
            if let Some(node) = data.state.nodes.get_mut(&node_id) {
                node.army = (node.army + rate).min(ARMY_CAP);
            }
        }
        // Ensure building nodes have 0 army.
        for node in data.state.nodes.values_mut() {
            if node.building.is_some() {
                node.army = 0;
            }
        }

        let attack_ids = data.state.attacks.keys().cloned().collect::<Vec<_>>();
        for attack_id in attack_ids {
            let Some(attack) = data.state.attacks.get(&attack_id).cloned() else {
                continue;
            };

            let source = data.state.nodes.get(&attack.from_node_id).cloned();
            let target = data.state.nodes.get(&attack.to_node_id).cloned();

            let Some(source_snapshot) = source else {
                data.state.attacks.remove(&attack_id);
                continue;
            };
            let Some(target_snapshot) = target else {
                data.state.attacks.remove(&attack_id);
                continue;
            };

            if source_snapshot.owner_id.as_deref() != Some(attack.owner_id.as_str()) {
                data.state.attacks.remove(&attack_id);
                continue;
            }
            if attack.mode == "transfer"
                && target_snapshot.owner_id.as_deref() != Some(attack.owner_id.as_str())
            {
                data.state.attacks.remove(&attack_id);
                continue;
            }
            if attack.mode != "transfer"
                && target_snapshot.owner_id.as_deref() == Some(attack.owner_id.as_str())
            {
                if let Some(existing_attack) = data.state.attacks.get_mut(&attack_id) {
                    existing_attack.mode = "transfer".to_string();
                }
                continue;
            }
            if source_snapshot.army <= 0 {
                continue;
            }

            let flow = clamp_i64(attack.send_per_tick.max(1), 1, source_snapshot.army.max(1));

            if let Some(source_node) = data.state.nodes.get_mut(&attack.from_node_id) {
                source_node.army = (source_node.army - flow).max(0);
            }

            if attack.mode == "transfer" {
                if let Some(target_node) = data.state.nodes.get_mut(&attack.to_node_id) {
                    target_node.army = (target_node.army + flow).min(ARMY_CAP);
                }
                continue;
            }

            if let Some(target_node) = data.state.nodes.get_mut(&attack.to_node_id) {
                // Hospitals provide damage reduction against enemy attacks.
                let mut effective_flow = flow as f64;
                if let Some(owner_id) = target_node.owner_id.as_deref() {
                    if let Some(player) = data.state.players.get(owner_id) {
                        let reduction = (player.damage_reduction_percent as f64 / 100.0).min(0.99);
                        effective_flow *= 1.0 - reduction;
                    }
                }
                let damage = effective_flow as i64;
                target_node.army -= damage;
                if target_node.army < 0 {
                    let had_building = target_node.building.is_some();
                    target_node.owner_id = Some(attack.owner_id.clone());
                    target_node.army = target_node.army.abs().max(1);
                    // Destroy any building on a captured node. Only a captured
                    // node that actually held a building can change city
                    // membership; ownership flips always change the
                    // player-nodes index.
                    if had_building {
                        data.city_membership_dirty = true;
                        data.state.building_cooldowns.remove(&attack.to_node_id);
                    }
                    target_node.building = None;
                    data.player_nodes_dirty = true;
                    if let Some(existing_attack) = data.state.attacks.get_mut(&attack_id) {
                        existing_attack.mode = "transfer".to_string();
                    }
                }
            }
        }

        // Remove every outgoing attack from a node whose army has been drained
        // to 0. Incoming attacks are intentionally kept so the node can still
        // receive reinforcements while it rebuilds.
        let depleted_nodes: HashSet<String> = data
            .state
            .nodes
            .iter()
            .filter(|(_, node)| node.army <= 0)
            .map(|(node_id, _)| node_id.clone())
            .collect();
        data.state
            .attacks
            .retain(|_, attack| !depleted_nodes.contains(&attack.from_node_id));
        Ok(())
    }

    fn tick_world_locked(&self, data: &mut AppData) -> Result<(), String> {
        let current = now_ms();
        let last = if data.state.last_tick_at > 0 {
            data.state.last_tick_at
        } else {
            current
        };
        let elapsed_ticks = ((current - last).max(0) / WORLD_TICK_MS as i64) as usize;
        let elapsed_ticks = elapsed_ticks.min(MAX_CATCH_UP_TICKS);
        if elapsed_ticks == 0 {
            // Nothing to simulate. The background ticker keeps world nodes in sync,
            // so read-only requests do not need to pay for a full sync scan.
            return Ok(());
        }
        for _ in 0..elapsed_ticks {
            self.step_world_locked(data)?;
        }
        data.state.last_tick_at = current;
        // Avoid serializing the entire world to disk every single second.
        // The background ticker only persists every 5 seconds; mutating
        // endpoints still call save_state_locked directly when they change state.
        if current - data.state.last_persisted_at >= 5000 {
            self.save_state_locked(&mut data.state)?;
        }
        Ok(())
    }

    fn reconstruct_path_locked(&self, data: &AppData, prev: &HashMap<String, String>, target_id: &str) -> Vec<[f64; 2]> {
        let mut out = Vec::new();
        let mut cursor = Some(target_id.to_string());
        while let Some(current) = cursor {
            if let Some(coord) = data.node_repo.coords_by_id.get(&current) {
                out.push([coord.lon, coord.lat]);
            }
            cursor = prev.get(&current).cloned();
        }
        out.reverse();
        out
    }

    fn find_road_path_locked(
        &self,
        data: &mut AppData,
        from_node_id: &str,
        to_node_id: &str,
    ) -> Result<Vec<[f64; 2]>, String> {
        self.ensure_node_repo_fresh(data, false)?;
        let cache_key = format!(
            "{}:{}:{}",
            data.node_repo.graph_version, from_node_id, to_node_id
        );
        if let Some(path) = data.node_repo.route_cache.get(&cache_key) {
            return Ok(path.clone());
        }

        let start = from_node_id.to_string();
        let goal = to_node_id.to_string();
        data.node_repo
            .coords_by_id
            .get(&start)
            .copied()
            .ok_or_else(|| "Missing road coordinates for one of the nodes.".to_string())?;
        let goal_coord = data
            .node_repo
            .coords_by_id
            .get(&goal)
            .copied()
            .ok_or_else(|| "Missing road coordinates for one of the nodes.".to_string())?;

        let mut open = BinaryHeap::<OpenNode>::new();
        open.push(OpenNode {
            node_id: start.clone(),
            score: 0.0,
        });
        let mut g_score = HashMap::<String, f64>::new();
        let mut prev = HashMap::<String, String>::new();
        let mut visited = HashSet::<String>::new();
        g_score.insert(start, 0.0);

        while let Some(OpenNode {
            node_id: current, ..
        }) = open.pop()
        {
            if current == goal {
                let path = self.reconstruct_path_locked(data, &prev, &goal);
                data.node_repo.route_cache.insert(cache_key, path.clone());
                return Ok(path);
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            let current_score = *g_score.get(&current).unwrap_or(&f64::INFINITY);
            if let Some(neighbors) = data.node_repo.adjacency.get(&current) {
                for edge in neighbors {
                    let best_neighbor_score = *g_score.get(&edge.to).unwrap_or(&f64::INFINITY);
                    let tentative = current_score + edge.weight;
                    if tentative >= best_neighbor_score {
                        continue;
                    }
                    prev.insert(edge.to.clone(), current.clone());
                    g_score.insert(edge.to.clone(), tentative);
                    let heuristic = data
                        .node_repo
                        .coords_by_id
                        .get(&edge.to)
                        .copied()
                        .map(|coord| haversine_km(coord.lat, coord.lon, goal_coord.lat, goal_coord.lon))
                        .unwrap_or(0.0);
                    open.push(OpenNode {
                        node_id: edge.to.clone(),
                        score: tentative + heuristic,
                    });
                }
            }
        }

        Err(format!(
            "No road path found between node {} and node {}.",
            from_node_id, to_node_id
        ))
    }

    fn connection_preview_feature(
        &self,
        from_node_id: &str,
        to_node_id: &str,
        mode: &str,
        owner_state: &str,
        path: &[[f64; 2]],
    ) -> Value {
        let coordinates = path
            .iter()
            .map(|pair| json!([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        json!({
            "type": "Feature",
            "properties": {
                "fromNodeId": from_node_id,
                "toNodeId": to_node_id,
                "mode": mode,
                "ownerState": owner_state
            },
            "geometry": {
                "type": "LineString",
                "coordinates": coordinates
            }
        })
    }

    fn simplify_connection_path(&self, path: &[[f64; 2]]) -> Vec<[f64; 2]> {
        if path.len() <= 2 {
            return path.to_vec();
        }
        let line: LineString<f64> = path
            .iter()
            .map(|pair| GeoCoord {
                x: pair[0],
                y: pair[1],
            })
            .collect();
        let simplified = line.simplify(&0.00002);
        let simplified_pairs = simplified
            .0
            .into_iter()
            .map(|coord| [coord.x, coord.y])
            .collect::<Vec<_>>();
        if simplified_pairs.len() >= 2 {
            simplified_pairs
        } else {
            path.to_vec()
        }
    }

    fn resolve_connection_locked(
        &self,
        data: &mut AppData,
        player_id: &str,
        from_node_id: &str,
        to_node_id: &str,
    ) -> Result<(String, Vec<[f64; 2]>, String), String> {
        if from_node_id == to_node_id {
            return Err("Select a different target node.".to_string());
        }
        let source = data
            .state
            .nodes
            .get(from_node_id)
            .cloned()
            .ok_or_else(|| "Invalid nodes.".to_string())?;
        let target = data
            .state
            .nodes
            .get(to_node_id)
            .cloned()
            .ok_or_else(|| "Invalid nodes.".to_string())?;
        if source.owner_id.as_deref() != Some(player_id) {
            return Err("You can only connect from your own node.".to_string());
        }
        let immediate_targets = self.immediate_neighbor_targets_locked(data, from_node_id)?;
        if !immediate_targets.iter().any(|node_id| node_id == to_node_id) {
            return Err("Connectable nodes must be directly next to your node on a single road.".to_string());
        }
        let mode = if target.owner_id.as_deref() == Some(player_id) {
            "transfer"
        } else {
            "attack"
        };
        let owner_state = if mode == "transfer" { "self" } else { "enemy" };
        let path = self.simplify_connection_path(&self.find_road_path_locked(data, from_node_id, to_node_id)?);
        if path.len() < 2 {
            return Err("No road path found between those nodes.".to_string());
        }
        Ok((mode.to_string(), path, owner_state.to_string()))
    }

    fn world_feature_locked(
        &self,
        data: &AppData,
        node_id: &str,
        player_id: Option<&str>,
    ) -> Option<Value> {
        let repo_node = data.node_repo.nodes_by_id.get(node_id)?;
        let world_node = data.state.nodes.get(node_id)?;
        let owner_state = match (world_node.owner_id.as_deref(), player_id) {
            (Some(owner_id), Some(player_id)) if owner_id == player_id => "self",
            (Some(_), _) => "enemy",
            _ => "neutral",
        };
        let production_rate = if owner_state == "self" {
            world_node
                .owner_id
                .as_deref()
                .map(|pid| self.node_army_production_rate_locked(data, node_id, pid))
                .unwrap_or(0.0)
        } else {
            0.0
        };
        let owner = world_node
            .owner_id
            .as_deref()
            .and_then(|owner_id| data.state.players.get(owner_id));
        let owner_username = owner.map(|player| player.username.clone());
        let owner_color = owner.map(|player| player.color.clone());
        let display_name = data
            .node_repo
            .display_names
            .get(node_id)
            .cloned()
            .unwrap_or_else(|| format!("Node {}", node_id));
        Some(json!({
            "type": "Feature",
            "properties": {
                "id": node_id,
                "displayName": display_name,
                "latitude": repo_node.lat,
                "longitude": repo_node.lon,
                "degree": repo_node.degree,
                "army": world_node.army,
                "armyLabel": format_army_label(world_node.army),
                "ownerId": world_node.owner_id,
                "ownerUsername": owner_username,
                "ownerColor": owner_color,
                "ownerState": owner_state,
                "building": world_node.building.as_ref().map(|b| b.to_label()),
                "productionRate": production_rate,
                "canAttack": player_id.map(|id| world_node.owner_id.as_deref() != Some(id)).unwrap_or(false)
            },
            "geometry": {
                "type": "Point",
                "coordinates": [repo_node.lon, repo_node.lat]
            }
        }))
    }

    fn attack_feature(&self, data: &AppData, attack: &Attack, player_id: Option<&str>) -> Value {
        let coordinates = attack
            .path
            .iter()
            .map(|pair| json!([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        let owner_color = data
            .state
            .players
            .get(&attack.owner_id)
            .map(|player| player.color.clone())
            .unwrap_or_else(random_player_color);
        json!({
            "type": "Feature",
            "properties": {
                "id": attack.id,
                "ownerId": attack.owner_id,
                "ownerState": if player_id == Some(attack.owner_id.as_str()) { "self" } else { "enemy" },
                "ownerColor": owner_color,
                "mode": attack.mode,
                "createdAt": attack.created_at,
                "sendPerTick": attack.send_per_tick,
                "fromNodeId": attack.from_node_id,
                "toNodeId": attack.to_node_id
            },
            "geometry": {
                "type": "LineString",
                "coordinates": coordinates
            }
        })
    }

    fn owned_node_features_locked(&self, data: &AppData, player_id: Option<&str>) -> Vec<Value> {
        let Some(player_id) = player_id else {
            return Vec::new();
        };
        data.state
            .nodes
            .iter()
            .filter_map(|(node_id, node)| {
                (node.owner_id.as_deref() == Some(player_id))
                    .then(|| self.world_feature_locked(data, node_id, Some(player_id)))
                    .flatten()
            })
            .collect()
    }

    fn game_state_response(&self, token: Option<&str>, include_nodes: bool) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = token
            .map(|token| self.require_session_locked(&data, token))
            .transpose()?;
        // The background ticker advances the world, so read-only responses
        // do not need to pay for a full lock-held tick.
        let player_id = session.as_ref().map(|session| session.player_id.as_str());
        self.ensure_city_membership_locked(&mut data);
        let owned_nodes = self.owned_node_features_locked(&data, player_id);
        let node_features = if include_nodes {
            data.state
                .nodes
                .keys()
                .filter_map(|node_id| self.world_feature_locked(&data, node_id, player_id))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let attack_features = data
            .state
            .attacks
            .values()
            .map(|attack| self.attack_feature(&data, attack, player_id))
            .collect::<Vec<_>>();
        Ok(json!({
            "player": player_id.and_then(|player_id| data.state.players.get(player_id)).map(|player| json!({
                "id": player.id,
                "username": player.username,
                "color": player.color,
                "gold": player.gold,
                "goldIncomePerSec": player.gold_income_per_sec,
                "damageReductionPercent": player.damage_reduction_percent,
                "happinessPercent": player.happiness_percent
            })),
            "buildStatus": self.current_build_status(&mut data),
            "cityFeatures": {
                "type": "FeatureCollection",
                "features": self.city_features_locked(&data)
            },
            "ownedNodes": {
                "type": "FeatureCollection",
                "features": owned_nodes
            },
            "nodes": {
                "type": "FeatureCollection",
                "features": node_features
            },
            "attacks": {
                "type": "FeatureCollection",
                "features": attack_features
            }
        }))
    }

    fn is_test_or_bot_username(username: &str) -> bool {
        let lower = username.to_lowercase();
        if lower.contains("test")
            || lower.contains("probe")
            || lower.contains("perf")
            || lower.contains("dummy")
            || lower.starts_with("qa_")
            || lower.contains("loginprobe")
            || lower.contains("trae_")
            || lower.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
        let unique: HashSet<char> = lower.chars().collect();
        username.len() >= 12 && unique.len() <= 4
    }

    fn leaderboard_response(&self, token: Option<&str>) -> Result<Value, String> {
        let data = app_lock(&self.inner);
        let session = token
            .map(|token| self.require_session_locked(&data, token))
            .transpose()?;
        let mut scores = HashMap::<String, (usize, i64)>::new();
        for node in data.state.nodes.values() {
            if let Some(owner_id) = node.owner_id.as_ref() {
                let entry = scores.entry(owner_id.clone()).or_insert((0, 0));
                entry.0 += 1;
                entry.1 += node.army.max(0);
            }
        }
        let mut leaderboard = Vec::new();
        for (player_id, (nodes, army)) in scores {
            if let Some(player) = data.state.players.get(&player_id) {
                if Self::is_test_or_bot_username(&player.username) {
                    continue;
                }
                leaderboard.push(json!({
                    "playerId": player_id,
                    "username": player.username,
                    "nodes": nodes,
                    "army": army,
                }));
            }
        }
        leaderboard.sort_by(|a, b| {
            let a_army = a.get("army").and_then(Value::as_i64).unwrap_or(0);
            let b_army = b.get("army").and_then(Value::as_i64).unwrap_or(0);
            b_army
                .cmp(&a_army)
                .then_with(|| {
                    let a_nodes = a.get("nodes").and_then(Value::as_u64).unwrap_or(0);
                    let b_nodes = b.get("nodes").and_then(Value::as_u64).unwrap_or(0);
                    b_nodes.cmp(&a_nodes)
                })
                .then_with(|| {
                    let a_name = a.get("username").and_then(Value::as_str).unwrap_or("");
                    let b_name = b.get("username").and_then(Value::as_str).unwrap_or("");
                    a_name.cmp(b_name)
                })
        });
        leaderboard.truncate(20);
        let current_player_id = session.as_ref().map(|session| session.player_id.as_str());
        let current_rank = current_player_id.and_then(|player_id| {
            leaderboard
                .iter()
                .position(|entry| entry.get("playerId").and_then(Value::as_str) == Some(player_id))
                .map(|index| index + 1)
        });
        Ok(json!({
            "leaderboard": leaderboard,
            "currentRank": current_rank,
        }))
    }

    fn node_ids_for_tile_locked(
        &self,
        data: &mut AppData,
        z: i64,
        x: i64,
        y: i64,
    ) -> Result<Vec<String>, String> {
        self.ensure_node_repo_fresh(data, false)?;
        let zoom = clamp_i64(z, 0, 22);
        let tiles_per_side = 1_i64 << zoom;
        if x < 0 || y < 0 || x >= tiles_per_side || y >= tiles_per_side {
            return Err("Invalid tile coordinates.".to_string());
        }
        let key = format!("{}:{}:{}:{}", data.node_repo.graph_version, zoom, x, y);
        if let Some(ids) = data.node_repo.tile_node_id_cache.get(&key) {
            return Ok(ids.clone());
        }
        let bounds = tile_bounds(zoom, x, y);
        // Cheap reject: tiles outside the loaded region's overall bbox cannot
        // contain any nodes, so skip the S2 cover recursion entirely.
        if let Some(region) = data.boundary_repo.overall_bbox {
            let intersects = bounds.west <= region.east
                && bounds.east >= region.west
                && bounds.south <= region.north
                && bounds.north >= region.south;
            if !intersects {
                return Ok(Vec::new());
            }
        }
        // Clamp the cover to the region: a low-zoom tile (z=0 covers the whole
        // planet) must not make the S2 cover recursion walk cells that cannot
        // contain any node. `bounds` itself stays unclamped so the exact
        // per-node filter below keeps its tile-border semantics.
        let cover_bounds = match &data.boundary_repo.overall_bbox {
            Some(region) => BBox {
                west: bounds.west.max(region.west),
                east: bounds.east.min(region.east),
                south: bounds.south.max(region.south),
                north: bounds.north.min(region.north),
            },
            None => bounds.clone(),
        };
        let level = data.node_repo.s2_index_level;
        let cover = s2_cover_for_bounds(&cover_bounds, level);
        let mut node_ids = Vec::new();
        for cell in cover {
            if let Some(&(start, end)) = data.node_repo.s2_cell_ranges.get(&cell.0) {
                for index in start..=end {
                    if let Some((_, node_id)) = data.node_repo.s2_sorted_nodes.get(index) {
                        let node = data.node_repo.nodes_by_id.get(node_id).ok_or_else(|| {
                            format!("S2 index references missing node {}", node_id)
                        })?;
                        if node.lon < bounds.west
                            || node.lon >= bounds.east
                            || node.lat < bounds.south
                            || node.lat > bounds.north
                        {
                            continue;
                        }
                        node_ids.push(node_id.clone());
                    }
                }
            }
        }
        data.node_repo.tile_node_id_cache.insert(key, node_ids.clone());
        Ok(node_ids)
    }

    fn node_tile_response(
        &self,
        token: &str,
        z: i64,
        x: i64,
        y: i64,
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        let player_id = Some(session.player_id.as_str());
        let node_ids = self.node_ids_for_tile_locked(&mut data, z, x, y)?;
        // The repo may have been reloaded above; refresh caches before
        // world_feature_locked consults them.
        self.ensure_city_membership_locked(&mut data);
        let features = node_ids
            .iter()
            .filter_map(|node_id| self.world_feature_locked(&data, node_id, player_id))
            .collect::<Vec<_>>();
        Ok(json!({
            "z": z,
            "x": x,
            "y": y,
            "nodes": {
                "type": "FeatureCollection",
                "features": features
            }
        }))
    }

    fn connectable_nodes_request(
        &self,
        token: &str,
        from_node_id: &str,
        candidate_node_ids: &[String],
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        if candidate_node_ids.len() > 20_000 {
            return Err("Too many candidate nodes requested.".to_string());
        }
        let source = data
            .state
            .nodes
            .get(from_node_id)
            .cloned()
            .ok_or_else(|| "Invalid nodes.".to_string())?;
        if source.owner_id.as_deref() != Some(session.player_id.as_str()) {
            return Err("You can only connect from your own node.".to_string());
        }
        let immediate_targets = self.immediate_neighbor_targets_locked(&mut data, from_node_id)?;
        // The repo may have been reloaded above; refresh caches before
        // world_feature_locked consults them.
        self.ensure_city_membership_locked(&mut data);
        let candidate_filter = if candidate_node_ids.is_empty() || candidate_node_ids.len() > 20_000 {
            None
        } else {
            Some(candidate_node_ids.iter().cloned().collect::<HashSet<_>>())
        };
        let mut features = Vec::new();
        for node_id in immediate_targets {
            if node_id == from_node_id {
                continue;
            }
            if let Some(filter) = &candidate_filter {
                if !filter.contains(&node_id) {
                    continue;
                }
            }
            if let Some(feature) =
                self.world_feature_locked(&data, &node_id, Some(session.player_id.as_str()))
            {
                features.push(feature);
            }
        }
        Ok(json!({
            "fromNodeId": from_node_id,
            "targets": {
                "type": "FeatureCollection",
                "features": features
            }
        }))
    }

    fn connection_remove_request(
        &self,
        token: &str,
        from_node_id: &str,
        to_node_id: &str,
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        let matching_ids = data
            .state
            .attacks
            .iter()
            .filter_map(|(attack_id, attack)| {
                (attack.owner_id == session.player_id
                    && attack.from_node_id == from_node_id
                    && attack.to_node_id == to_node_id)
                    .then(|| attack_id.clone())
            })
            .collect::<Vec<_>>();
        if matching_ids.is_empty() {
            return Err("Connection not found.".to_string());
        }
        let removed_count = matching_ids.len();
        for attack_id in matching_ids {
            data.state.attacks.remove(&attack_id);
        }
        self.save_state_locked(&mut data.state)?;
        Ok(json!({
            "ok": true,
            "removed": true,
            "removedCount": removed_count,
            "fromNodeId": from_node_id,
            "toNodeId": to_node_id
        }))
    }

    fn connection_preview_request(
        &self,
        token: &str,
        from_node_id: &str,
        to_node_id: &str,
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        let (mode, path, owner_state) =
            self.resolve_connection_locked(&mut data, &session.player_id, from_node_id, to_node_id)?;
        Ok(json!({
            "ok": true,
            "mode": mode,
            "path": {
                "type": "FeatureCollection",
                "features": [self.connection_preview_feature(from_node_id, to_node_id, &mode, &owner_state, &path)]
            }
        }))
    }

    fn attack_node_request(
        &self,
        token: &str,
        from_node_id: &str,
        to_node_id: &str,
        send_per_tick: i64,
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        let existing_attack_id = data
            .state
            .attacks
            .iter()
            .find_map(|(attack_id, attack)| {
                (attack.owner_id == session.player_id
                    && attack.from_node_id == from_node_id
                    && attack.to_node_id == to_node_id)
                    .then(|| attack_id.clone())
            });
        if let Some(attack_id) = existing_attack_id {
            data.state.attacks.remove(&attack_id);
            self.save_state_locked(&mut data.state)?;
            return Ok(json!({ "ok": true, "removed": true, "attackId": attack_id }));
        }
        let (mode, path, _) =
            self.resolve_connection_locked(&mut data, &session.player_id, from_node_id, to_node_id)?;
        let attack_id = random_id("attack");
        data.state.attacks.insert(
            attack_id.clone(),
            Attack {
                id: attack_id.clone(),
                owner_id: session.player_id.clone(),
                mode,
                from_node_id: from_node_id.to_string(),
                to_node_id: to_node_id.to_string(),
                path,
                send_per_tick: sanitize_send_per_tick(send_per_tick),
                created_at: now_ms(),
            },
        );
        self.save_state_locked(&mut data.state)?;
        let attack_feature = data
            .state
            .attacks
            .get(&attack_id)
            .map(|attack| self.attack_feature(&data, attack, Some(session.player_id.as_str())));
        Ok(json!({
            "ok": true,
            "attackId": attack_id,
            "sendPerTick": sanitize_send_per_tick(send_per_tick),
            "attack": attack_feature
        }))
    }

    fn connection_rate_request(
        &self,
        token: &str,
        from_node_id: &str,
        to_node_id: &str,
        send_per_tick: i64,
    ) -> Result<Value, String> {
        let mut data = app_lock(&self.inner);
        let session = self.require_session_locked(&data, token)?;
        let next_rate = sanitize_send_per_tick(send_per_tick);
        let attack = data
            .state
            .attacks
            .values_mut()
            .find(|attack| {
                attack.owner_id == session.player_id
                    && attack.from_node_id == from_node_id
                    && attack.to_node_id == to_node_id
            })
            .ok_or_else(|| "Connection not found.".to_string())?;
        attack.send_per_tick = next_rate;
        self.save_state_locked(&mut data.state)?;
        Ok(json!({ "ok": true, "sendPerTick": next_rate }))
    }

    fn background_tick(&self) -> Result<(), String> {
        let mut data = app_lock(&self.inner);
        self.tick_world_locked(&mut data)?;
        // Periodically drop expired sessions so state.json does not grow
        // forever; persist only when something was actually removed.
        data.ticks_since_session_purge += 1;
        if data.ticks_since_session_purge >= 60 {
            data.ticks_since_session_purge = 0;
            if self.purge_expired_sessions_locked(&mut data) {
                self.save_state_locked(&mut data.state)?;
            }
        }
        let version = data.state.version;
        let update = json!({
            "type": "world-update",
            "version": version,
            "timestamp": now_ms()
        })
        .to_string();
        let _ = self.ws_update_sender.try_send(update);
        Ok(())
    }

    fn serve_static(&self, pathname: &str) -> Response<std::io::Cursor<Vec<u8>>> {
        let relative = match pathname {
            "/" => PathBuf::from("openfreemap_viewer.html"),
            "/openfreemap_viewer.html" => PathBuf::from("openfreemap_viewer.html"),
            "/sw.js" => PathBuf::from("sw.js"),
            "/vendor/maplibre-gl.js" => PathBuf::from("vendor/maplibre-gl.js"),
            "/vendor/maplibre-gl.css" => PathBuf::from("vendor/maplibre-gl.css"),
            _ => return error_json_response(404, &json!({ "error": "Not found." })),
        };
        let file_path = self.root.join(relative);
        match fs::read(&file_path) {
            Ok(data) => {
                let cache_control = if pathname.contains("build_status")
                    || pathname.contains("preview_intersections")
                    || pathname.contains("query_tiles_status")
                {
                    "no-store"
                } else {
                    "public, max-age=60"
                };
                Response::from_data(data)
                    .with_status_code(StatusCode(200))
                    .with_header(header("Content-Type", content_type(&file_path)))
                    .with_header(header("Cache-Control", cache_control))
            }
            Err(_) => error_json_response(404, &json!({ "error": "Not found." })),
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

// Tolerate mutex poisoning: a panicking request handler must not take down
// every subsequent request with "PoisonError" unwraps.
fn app_lock(m: &Mutex<AppData>) -> std::sync::MutexGuard<'_, AppData> {
    m.lock().unwrap_or_else(|err| err.into_inner())
}

fn file_mtime_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as i64)
}

fn read_json_file<T>(path: &Path) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn read_json_value(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_json_file<T>(path: &Path, value: &T) -> Result<(), String>
where
    T: Serialize,
{
    // Compact serialization: this helper only writes game_data/state.json,
    // which is large and machine-read only.
    let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
    let bytes = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    fs::write(&tmp_path, bytes).map_err(|err| err.to_string())?;
    fs::rename(&tmp_path, path).map_err(|err| err.to_string())
}

fn random_id(prefix: &str) -> String {
    let bytes: [u8; 12] = random();
    let suffix = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("{prefix}_{suffix}")
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn hash_password_argon2(password: &str) -> Result<String, String> {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    let argon2 = Argon2::default();
    let salt = SaltString::generate(&mut OsRng);
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| err.to_string())
}

fn verify_password(password: &str, hash: &str) -> Result<(bool, bool), String> {
    if hash.starts_with("$argon2") {
        let parsed_hash = match PasswordHash::new(hash) {
            Ok(hash) => hash,
            Err(_) => return Ok((false, false)),
        };
        match Argon2::default().verify_password(password.as_bytes(), &parsed_hash) {
            Ok(()) => Ok((true, false)),
            Err(_) => Ok((false, false)),
        }
    } else {
        let expected = sha256_hex(password);
        let verified = expected.len() == hash.len()
            && expected.as_bytes().ct_eq(hash.as_bytes()).into();
        Ok((verified, true))
    }
}

fn haversine_km(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    let to_rad = std::f64::consts::PI / 180.0;
    let d_lat = (b_lat - a_lat) * to_rad;
    let d_lon = (b_lon - a_lon) * to_rad;
    let lat1 = a_lat * to_rad;
    let lat2 = b_lat * to_rad;
    let sin_d_lat = (d_lat / 2.0).sin();
    let sin_d_lon = (d_lon / 2.0).sin();
    let h = sin_d_lat * sin_d_lat + lat1.cos() * lat2.cos() * sin_d_lon * sin_d_lon;
    6371.0 * 2.0 * h.sqrt().atan2((1.0 - h).sqrt())
}

fn circle_polygon(center_lat: f64, center_lon: f64, radius_km: f64) -> Value {
    // Approximate a circle as a 32-sided polygon.
    let points: Vec<Vec<f64>> = (0..=32)
        .map(|i| {
            let angle = (i as f64) * 2.0 * std::f64::consts::PI / 32.0;
            // Convert radius in km to degrees (roughly).
            let d_lat = (radius_km / 111.0) * angle.cos();
            let d_lon = (radius_km / (111.0 * center_lat.to_radians().cos())) * angle.sin();
            vec![center_lon + d_lon, center_lat + d_lat]
        })
        .collect();
    json!({
        "type": "Polygon",
        "coordinates": [points]
    })
}

fn format_army_label(value: i64) -> String {
    let army = value.max(0) as f64;
    if army >= ARMY_CAP as f64 {
        let millions = (army / 100_000.0).round() / 10.0;
        return if (millions.fract() - 0.0).abs() < f64::EPSILON {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        };
    }
    if army >= 1_000.0 {
        let thousands = (army / 100.0).round() / 10.0;
        return if (thousands.fract() - 0.0).abs() < f64::EPSILON {
            format!("{thousands:.0}K")
        } else {
            format!("{thousands:.1}K")
        };
    }
    format!("{}", army.round() as i64)
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn pair_from_value(value: &Value) -> Option<(f64, f64)> {
    let items = value.as_array()?;
    if items.len() < 2 {
        return None;
    }
    Some((items[0].as_f64()?, items[1].as_f64()?))
}

fn update_bbox_from_coordinates(value: &Value, bbox: &mut BBox) {
    if let Some((lon, lat)) = pair_from_value(value) {
        bbox.west = bbox.west.min(lon);
        bbox.east = bbox.east.max(lon);
        bbox.south = bbox.south.min(lat);
        bbox.north = bbox.north.max(lat);
        return;
    }
    if let Some(items) = value.as_array() {
        for item in items {
            update_bbox_from_coordinates(item, bbox);
        }
    }
}

fn ring_from_value(value: &Value) -> Vec<(f64, f64)> {
    value
        .as_array()
        .map(|points| points.iter().filter_map(pair_from_value).collect::<Vec<_>>())
        .unwrap_or_default()
}

fn point_in_ring(lon: f64, lat: f64, ring: &[(f64, f64)]) -> bool {
    if ring.is_empty() {
        return false;
    }
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > lat) != (yj > lat) {
            let x_intersection = xi + (lat - yi) * (xj - xi) / (yj - yi);
            if lon < x_intersection {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

fn point_in_polygon_geometry(lon: f64, lat: f64, coordinates: &Value) -> bool {
    let Some(rings) = coordinates.as_array() else {
        return false;
    };
    if rings.is_empty() {
        return false;
    }
    let outer_ring = ring_from_value(&rings[0]);
    if !point_in_ring(lon, lat, &outer_ring) {
        return false;
    }
    for ring in rings.iter().skip(1) {
        let hole = ring_from_value(ring);
        if point_in_ring(lon, lat, &hole) {
            return false;
        }
    }
    true
}

fn point_in_geometry(lon: f64, lat: f64, geometry_type: &str, coordinates: &Value) -> bool {
    match geometry_type {
        "Polygon" => point_in_polygon_geometry(lon, lat, coordinates),
        "MultiPolygon" => coordinates
            .as_array()
            .map(|polygons| {
                polygons
                    .iter()
                    .any(|polygon| point_in_polygon_geometry(lon, lat, polygon))
            })
            .unwrap_or(false),
        _ => false,
    }
}

fn fallback_boundary_features(region: &Value) -> Vec<GenericFeature> {
    region
        .get("overview_features")
        .and_then(Value::as_array)
        .map(|features| {
            features
                .iter()
                .filter_map(|feature| {
                    let coordinates = feature.get("coordinates")?.clone();
                    let name = feature
                        .get("name")
                        .or_else(|| feature.get("id"))
                        .and_then(value_to_string)
                        .unwrap_or_else(|| "region".to_string());
                    let mut properties = Map::new();
                    properties.insert("name".to_string(), Value::String(name));
                    Some(GenericFeature {
                        properties,
                        geometry: GeometryValue {
                            type_name: "Polygon".to_string(),
                            coordinates,
                        },
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn cache_signature(cache_dir: &Path) -> String {
    let mut items = fs::read_dir(cache_dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            let is_json = path.extension().and_then(|ext| ext.to_str()) == Some("json");
            if !is_json {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_string();
            let meta = fs::metadata(&path).ok()?;
            Some(format!(
                "{}:{}:{}",
                name,
                meta.len(),
                meta.modified()
                    .ok()
                    .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis())
                    .unwrap_or(0)
            ))
        })
        .collect::<Vec<_>>();
    items.sort();
    items.join("|")
}

fn clamp_i64(value: i64, min: i64, max: i64) -> i64 {
    value.max(min).min(max)
}

fn default_send_per_tick() -> i64 {
    1
}

fn sanitize_send_per_tick(value: i64) -> i64 {
    clamp_i64(value, 1, 250)
}

fn tile_x_to_lon(x: i64, zoom: i64) -> f64 {
    (x as f64 / 2_f64.powi(zoom as i32)) * 360.0 - 180.0
}

fn tile_y_to_lat(y: i64, zoom: i64) -> f64 {
    let n = std::f64::consts::PI - (2.0 * std::f64::consts::PI * y as f64) / 2_f64.powi(zoom as i32);
    (0.5 * (n.exp() - (-n).exp())).atan().to_degrees()
}

fn tile_bounds(z: i64, x: i64, y: i64) -> BBox {
    BBox {
        west: tile_x_to_lon(x, z),
        east: tile_x_to_lon(x + 1, z),
        north: tile_y_to_lat(y, z),
        south: tile_y_to_lat(y + 1, z),
    }
}

fn s2_cover_for_bounds(bounds: &BBox, level: u64) -> Vec<CellID> {
    let rect = S2Rect::from_degrees(bounds.south, bounds.west, bounds.north, bounds.east);
    let mut cover = Vec::new();
    for face in 0..6 {
        let face_cell = CellID::from_face(face);
        s2_cover_recursive(face_cell, level, &rect, &mut cover);
    }
    cover
}

fn s2_cover_recursive(cell: CellID, level: u64, rect: &S2Rect, cover: &mut Vec<CellID>) {
    if cell.level() > level {
        return;
    }
    if !s2_cell_intersects_rect(&cell, rect) {
        return;
    }
    if cell.level() == level {
        cover.push(cell);
        return;
    }
    for child in cell.children() {
        s2_cover_recursive(child, level, rect, cover);
    }
}

fn s2_cell_intersects_rect(cell: &CellID, rect: &S2Rect) -> bool {
    let bound = s2_cell_rect(cell);
    bound.intersects(rect)
}

fn s2_cell_rect(cell: &CellID) -> S2Rect {
    s2::cell::Cell::from(*cell).rect_bound()
}

fn run_websocket_server(app: Arc<App>, update_rx: crossbeam_channel::Receiver<String>, port: u16, bind_addr: String) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Failed to create tokio runtime for WebSocket server: {err}");
            return;
        }
    };

    rt.block_on(async move {
        let (broadcast_tx, _) = tokio::sync::broadcast::channel::<String>(64);

        // Bridge the crossbeam channel into the broadcast channel on a
        // dedicated plain thread; recv() blocks, so it must not sit on a
        // tokio worker. broadcast::Sender::send is sync-safe.
        let broadcast_tx2 = broadcast_tx.clone();
        thread::spawn(move || {
            while let Ok(msg) = update_rx.recv() {
                let _ = broadcast_tx2.send(msg);
            }
        });

        let addr = format!("{bind_addr}:{port}");
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("Failed to bind WebSocket server to {addr}: {err}");
                return;
            }
        };
        println!("WebSocket server listening on ws://{addr}/ws");

        while let Ok((stream, _)) = listener.accept().await {
            let broadcast_rx = broadcast_tx.subscribe();
            let app = app.clone();
            tokio::spawn(handle_ws_client(stream, app, broadcast_rx));
        }
    });
}

async fn handle_ws_client(
    stream: tokio::net::TcpStream,
    app: Arc<App>,
    mut broadcast_rx: tokio::sync::broadcast::Receiver<String>,
) {
    // eprintln!("DEBUG ws client connected");
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws_stream) => ws_stream,
        Err(err) => {
            eprintln!("WebSocket handshake failed: {err}");
            return;
        }
    };
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let auth_timeout = tokio::time::Duration::from_secs(10);
    let auth_result = tokio::time::timeout(auth_timeout, async {
        while let Some(msg) = ws_rx.next().await {
            let msg = match msg {
                Ok(msg) => msg,
                Err(_) => return None,
            };
            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if value.get("type").and_then(Value::as_str) == Some("auth") {
                        if let Some(token) = value.get("token").and_then(Value::as_str) {
                            let data = app_lock(&app.inner);
                            // Same rules as HTTP auth: session exists, is
                            // unexpired, and references an existing player.
                            if let Ok(session) = app.require_session_locked(&data, token) {
                                return Some(session.player_id.clone());
                            }
                        }
                    }
                }
                break;
            }
        }
        None
    })
    .await;

    let _player_id = match auth_result {
        Ok(Some(pid)) => {
            // eprintln!("DEBUG ws auth ok player={pid}");
            let response = json!({
                "type": "authResult",
                "success": true,
                "playerId": pid
            })
            .to_string();
            let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(response)).await;
            pid
        }
        Ok(None) => {
            // eprintln!("DEBUG ws auth no player");
            let response = json!({
                "type": "authResult",
                "success": false
            })
            .to_string();
            let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(response)).await;
            return;
        }
        Err(_) => {
            // eprintln!("DEBUG ws auth timeout");
            let response = json!({
                "type": "authResult",
                "success": false
            })
            .to_string();
            let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(response)).await;
            return;
        }
    };

    let mut ping_interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            result = broadcast_rx.recv() => {
                let msg = match result {
                    Ok(msg) => msg,
                    Err(_) => break,
                };
                if ws_tx
                    .send(tokio_tungstenite::tungstenite::Message::Text(msg))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ = ping_interval.tick() => {
                if ws_tx
                    .send(tokio_tungstenite::tungstenite::Message::Ping(vec![]))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

fn parse_body_json(request: &mut Request) -> Result<Value, String> {
    let mut bytes = Vec::new();
    request
        .as_reader()
        .take(2_000_001)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() > 2_000_000 {
        return Err("Request body too large.".to_string());
    }
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|_| "Invalid JSON.".to_string())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

fn json_response<T>(status: u16, payload: &T) -> Response<std::io::Cursor<Vec<u8>>>
where
    T: Serialize,
{
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
}

fn error_json_response(status: u16, payload: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(status, payload)
}

fn content_type(file_path: &Path) -> &'static str {
    match file_path.extension().and_then(|ext| ext.to_str()).unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "geojson" => "application/geo+json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "pbf" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

fn parse_url(url: &str) -> (String, HashMap<String, String>) {
    let mut parts = url.splitn(2, '?');
    let pathname = parts.next().unwrap_or("/").to_string();
    let query = parts.next().unwrap_or_default();
    let params = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    (pathname, params)
}

fn extract_bearer_token(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().to_string().eq_ignore_ascii_case("Authorization"))
        .and_then(|header| std::str::from_utf8(header.value.as_bytes()).ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| token.trim().to_string())
}

fn handle_request(app: &Arc<App>, mut request: Request) {
    let (pathname, query) = parse_url(request.url());
    let response = (|| -> Result<Response<std::io::Cursor<Vec<u8>>>, String> {
        match (request.method(), pathname.as_str()) {
            (&Method::Post, "/api/register") => {
                let body = parse_body_json(&mut request)?;
                let username = body.get("username").and_then(Value::as_str).unwrap_or_default();
                let password = body.get("password").and_then(Value::as_str).unwrap_or_default();
                let start_node_id = body.get("startNodeId").and_then(Value::as_str);
                // The server assigns a random color automatically.
                Ok(json_response(200, &app.register_player(username, password, None, start_node_id)?))
            }
            (&Method::Post, "/api/login") => {
                let body = parse_body_json(&mut request)?;
                let username = body.get("username").and_then(Value::as_str).unwrap_or_default();
                let password = body.get("password").and_then(Value::as_str).unwrap_or_default();
                Ok(json_response(200, &app.login_player(username, password)?))
            }
            (&Method::Get, "/api/game-state") => {
                let token = extract_bearer_token(&request);
                let include_nodes = query.get("view").map(String::as_str) != Some("summary");
                Ok(json_response(200, &app.game_state_response(token.as_deref(), include_nodes)?))
            }
            (&Method::Get, "/api/leaderboard") => {
                let token = extract_bearer_token(&request);
                Ok(json_response(200, &app.leaderboard_response(token.as_deref())?))
            }
            (&Method::Get, "/api/node-tile") => {
                let token = extract_bearer_token(&request)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let z = query
                    .get("z")
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or_else(|| "z, x, and y are required.".to_string())?;
                let x = query
                    .get("x")
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or_else(|| "z, x, and y are required.".to_string())?;
                let y = query
                    .get("y")
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or_else(|| "z, x, and y are required.".to_string())?;
                Ok(json_response(200, &app.node_tile_response(&token, z, x, y)?))
            }
            (&Method::Post, "/api/logout") => {
                let token = extract_bearer_token(&request).or_else(|| {
                    let body = parse_body_json(&mut request).ok()?;
                    body.get("token").and_then(Value::as_str).map(String::from)
                }).ok_or_else(|| "Invalid session.".to_string())?;
                Ok(json_response(200, &app.logout_player(&token)?))
            }
            (&Method::Post, "/api/attack") => {
                let body = parse_body_json(&mut request)?;
                let token = body
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let from_node_id = body
                    .get("fromNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                let to_node_id = body
                    .get("toNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                let send_per_tick = body
                    .get("sendPerTick")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(default_send_per_tick);
                Ok(json_response(
                    200,
                    &app.attack_node_request(token, &from_node_id, &to_node_id, send_per_tick)?,
                ))
            }
            (&Method::Post, "/api/build") => {
                if !BUILDINGS_ENABLED {
                    return Ok(json_response(403, &json!({"error": "building system disabled"})));
                }
                let body = parse_body_json(&mut request)?;
                let token = body
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let node_id = body
                    .get("nodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid node.".to_string())?;
                let building = body
                    .get("building")
                    .and_then(Value::as_str)
                    .and_then(BuildingType::from_str)
                    .ok_or_else(|| "Unknown building type.".to_string())?;
                Ok(json_response(
                    200,
                    &app.build_building_request(token, &node_id, building)?,
                ))
            }
            (&Method::Post, "/api/launch-missile") => {
                if !BUILDINGS_ENABLED {
                    return Ok(json_response(403, &json!({"error": "building system disabled"})));
                }
                let body = parse_body_json(&mut request)?;
                let token = body
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let missile_type = body
                    .get("missileType")
                    .and_then(Value::as_str)
                    .unwrap_or("nuke");
                let target_lat = body
                    .get("targetLat")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| "Invalid target.".to_string())?;
                let target_lon = body
                    .get("targetLon")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| "Invalid target.".to_string())?;
                Ok(json_response(200, &app.launch_missile_request(token, missile_type, target_lat, target_lon)?))
            }
            (&Method::Post, "/api/connection-rate") => {
                let body = parse_body_json(&mut request)?;
                let token = body
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let from_node_id = body
                    .get("fromNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                let to_node_id = body
                    .get("toNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                let send_per_tick = body
                    .get("sendPerTick")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(default_send_per_tick);
                Ok(json_response(
                    200,
                    &app.connection_rate_request(token, &from_node_id, &to_node_id, send_per_tick)?,
                ))
            }
            (&Method::Post, "/api/connection-remove") => {
                let body = parse_body_json(&mut request)?;
                let token = body
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let from_node_id = body
                    .get("fromNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                let to_node_id = body
                    .get("toNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                Ok(json_response(
                    200,
                    &app.connection_remove_request(token, &from_node_id, &to_node_id)?,
                ))
            }
            (&Method::Post, "/api/connectable-nodes") => {
                let body = parse_body_json(&mut request)?;
                let token = body
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let from_node_id = body
                    .get("fromNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                let candidate_node_ids = body
                    .get("nodeIds")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(value_to_string).collect::<Vec<_>>())
                    .unwrap_or_default();
                Ok(json_response(
                    200,
                    &app.connectable_nodes_request(token, &from_node_id, &candidate_node_ids)?,
                ))
            }
            (&Method::Post, "/api/connection-preview") => {
                let body = parse_body_json(&mut request)?;
                let token = body
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Invalid session.".to_string())?;
                let from_node_id = body
                    .get("fromNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                let to_node_id = body
                    .get("toNodeId")
                    .and_then(value_to_string)
                    .ok_or_else(|| "Invalid nodes.".to_string())?;
                Ok(json_response(
                    200,
                    &app.connection_preview_request(token, &from_node_id, &to_node_id)?,
                ))
            }
            (&Method::Get, "/api/build-status") => {
                let mut data = app_lock(&app.inner);
                Ok(json_response(200, &app.current_build_status(&mut data)))
            }
            _ => Ok(app.serve_static(&pathname)),
        }
    })()
    .unwrap_or_else(|error| error_json_response(400, &json!({ "error": error })));

    let _ = request.respond(response);
}

fn main() {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (ws_update_sender, ws_update_receiver) = crossbeam_channel::unbounded::<String>();
    let app = Arc::new(App::new(root, ws_update_sender).expect("failed to initialize app"));

    // BIND_ADDR selects the interface both servers bind to (default keeps the
    // historical behavior of listening on every interface).
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0".to_string());
    if bind_addr == "0.0.0.0" {
        println!("WARNING: BIND_ADDR=0.0.0.0 exposes the API on all network interfaces; front it with TLS/a reverse proxy in production.");
    }

    let ws_app = app.clone();
    let ws_port = std::env::var("WS_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8003);
    let ws_bind_addr = bind_addr.clone();
    thread::spawn(move || run_websocket_server(ws_app, ws_update_receiver, ws_port, ws_bind_addr));

    let ticker_app = app.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(WORLD_TICK_MS));
        if let Err(error) = ticker_app.background_tick() {
            eprintln!("Background tick failed: {error}");
        }
    });

    if let Err(error) = app.warm_node_repo() {
        eprintln!("Failed to warm node repository: {error}");
    }

    let port = std::env::var("PORT")
        .ok()
        .or_else(|| std::env::args().nth(1))
        .unwrap_or_else(|| "8002".to_string());
    let address = format!("{bind_addr}:{port}");
    let server = Server::http(&address).unwrap_or_else(|error| panic!("failed to bind {address}: {error}"));
    println!("Game server listening on http://{address}");
    println!("WebSocket server listening on ws://{bind_addr}:{ws_port}/ws");
    println!("Interfaces/ports configurable via BIND_ADDR, PORT, WS_PORT env vars.");

    // Bound the number of in-flight request threads: acquire a slot before
    // spawning; when saturated this blocks and tiny_http queues connections.
    let worker_slots = Arc::new((Mutex::new(MAX_HTTP_WORKERS), Condvar::new()));
    for request in server.incoming_requests() {
        {
            let (lock, cvar) = &*worker_slots;
            let mut available = lock.lock().unwrap_or_else(|err| err.into_inner());
            while *available == 0 {
                available = cvar.wait(available).unwrap_or_else(|err| err.into_inner());
            }
            *available -= 1;
        }
        let app = app.clone();
        let worker_slots = worker_slots.clone();
        thread::spawn(move || {
            handle_request(&app, request);
            let (lock, cvar) = &*worker_slots;
            let mut available = lock.lock().unwrap_or_else(|err| err.into_inner());
            *available += 1;
            cvar.notify_one();
        });
    }
}
