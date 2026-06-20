use geo::{Coord as GeoCoord, LineString, Simplify};
use rand::random;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use url::form_urlencoded;

const WORLD_TICK_MS: u64 = 3000;
const REPO_REFRESH_MS: i64 = 10_000;

#[derive(Clone, Copy)]
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
}

#[derive(Clone)]
struct RepoNode {
    id: String,
    lat: f64,
    lon: f64,
    degree: i64,
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
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Player {
    id: String,
    username: String,
    password_hash: String,
    created_at: i64,
    start_node_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Session {
    token: String,
    player_id: String,
    created_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct WorldNode {
    owner_id: Option<String>,
    army: i64,
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
}

impl GameState {
    fn new() -> Self {
        Self {
            version: 1,
            saved_at: now_ms(),
            players: HashMap::new(),
            usernames: HashMap::new(),
            sessions: HashMap::new(),
            nodes: HashMap::new(),
            attacks: HashMap::new(),
        }
    }
}

struct AppData {
    state: GameState,
    node_repo: NodeRepo,
    boundary_repo: BoundaryRepo,
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
    inner: Mutex<AppData>,
}

impl App {
    fn new(root: PathBuf) -> Result<Self, String> {
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
            inner: Mutex::new(AppData {
                state,
                node_repo: NodeRepo::default(),
                boundary_repo: BoundaryRepo::default(),
            }),
        })
    }

    fn save_state_locked(&self, state: &mut GameState) -> Result<(), String> {
        state.saved_at = now_ms();
        write_json_file(&self.state_path, state)
    }

    fn current_build_status(&self) -> Value {
        read_json_value(&self.build_status_path).unwrap_or_else(|| {
            json!({
                "phase": "missing",
                "current": 0,
                "total": 0,
                "node_count": 0,
                "edge_count": 0,
                "message": "Builder has not started yet."
            })
        })
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
            data.node_repo.nodes_by_id.insert(
                node_id.to_string(),
                RepoNode {
                    id: node_id.to_string(),
                    lat,
                    lon,
                    degree,
                },
            );
            data.node_repo
                .coords_by_id
                .insert(node_id.to_string(), Coord { lat, lon });
        }
        Ok(())
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
                data.node_repo.nodes_by_id.insert(
                    node_id.clone(),
                    RepoNode {
                        id: node_id.clone(),
                        lat,
                        lon,
                        degree,
                    },
                );
                data.node_repo.coords_by_id.insert(node_id, Coord { lat, lon });
            }
        } else if self.intersections_csv_path.exists() {
            self.load_repo_nodes_from_csv(data)?;
        } else {
            return Err("No prepared node source found.".to_string());
        }

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

        data.node_repo.preview_mtime_ms = node_source_mtime_ms;
        data.node_repo.cache_signature = cache_signature;
        data.node_repo.boundary_signature = data.boundary_repo.signature.clone();
        data.node_repo.graph_version += 1;
        data.node_repo.refreshed_at = now;
        Ok(())
    }

    fn sync_world_nodes_locked(&self, data: &mut AppData) -> Result<bool, String> {
        self.ensure_node_repo_fresh(data, false)?;
        let mut modified = false;
        let valid_node_ids = data.node_repo.nodes_by_id.keys().cloned().collect::<HashSet<_>>();

        for node_id in data.node_repo.nodes_by_id.keys() {
            if !data.state.nodes.contains_key(node_id) {
                data.state.nodes.insert(
                    node_id.clone(),
                    WorldNode {
                        owner_id: None,
                        army: 10,
                    },
                );
                modified = true;
            }
        }

        let existing_node_ids = data.state.nodes.keys().cloned().collect::<Vec<_>>();
        for node_id in existing_node_ids {
            if !valid_node_ids.contains(&node_id) {
                data.state.nodes.remove(&node_id);
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

    fn require_session_locked(&self, data: &AppData, token: &str) -> Result<Session, String> {
        data.state
            .sessions
            .get(token)
            .cloned()
            .filter(|session| data.state.players.contains_key(&session.player_id))
            .ok_or_else(|| "Invalid session.".to_string())
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
        let mut open = vec![(start.clone(), 0.0_f64)];
        let mut dist = HashMap::<String, f64>::new();
        let mut prev = HashMap::<String, String>::new();
        let mut targets = Vec::<String>::new();
        let mut seen_targets = HashSet::<String>::new();
        dist.insert(start.clone(), 0.0);

        while !open.is_empty() {
            open.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let (current, current_score) = open.remove(0);
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

            let neighbors = data
                .node_repo
                .adjacency
                .get(&current)
                .cloned()
                .unwrap_or_default();
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
                open.push((edge.to, tentative));
            }
        }

        data.node_repo
            .immediate_neighbor_cache
            .insert(cache_key, targets.clone());
        Ok(targets)
    }

    fn register_player(&self, username: &str, password: &str) -> Result<Value, String> {
        let normalized = username.trim().to_lowercase();
        if normalized.len() < 3 {
            return Err("Username must be at least 3 characters.".to_string());
        }
        if password.len() < 4 {
            return Err("Password must be at least 4 characters.".to_string());
        }
        let mut data = self.inner.lock().unwrap();
        if data.state.usernames.contains_key(&normalized) {
            return Err("Username already exists.".to_string());
        }
        let player_id = random_id("player");
        let session_token = random_id("session");
        let start_nodes = self.choose_start_nodes_locked(&mut data)?;
        for node_id in &start_nodes {
            data.state.nodes.insert(
                node_id.clone(),
                WorldNode {
                    owner_id: Some(player_id.clone()),
                    army: 10,
                },
            );
        }
        data.state.players.insert(
            player_id.clone(),
            Player {
                id: player_id.clone(),
                username: normalized.clone(),
                password_hash: sha256_hex(password),
                created_at: now_ms(),
                start_node_ids: start_nodes.clone(),
            },
        );
        data.state.usernames.insert(normalized, player_id.clone());
        data.state.sessions.insert(
            session_token.clone(),
            Session {
                token: session_token.clone(),
                player_id: player_id.clone(),
                created_at: now_ms(),
            },
        );
        self.save_state_locked(&mut data.state)?;
        Ok(json!({ "playerId": player_id, "token": session_token }))
    }

    fn login_player(&self, username: &str, password: &str) -> Result<Value, String> {
        let normalized = username.trim().to_lowercase();
        let mut data = self.inner.lock().unwrap();
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
        if player.password_hash != sha256_hex(password) {
            return Err("Invalid username or password.".to_string());
        }
        let session_token = random_id("session");
        data.state.sessions.insert(
            session_token.clone(),
            Session {
                token: session_token.clone(),
                player_id: player_id.clone(),
                created_at: now_ms(),
            },
        );
        self.save_state_locked(&mut data.state)?;
        Ok(json!({ "playerId": player_id, "token": session_token }))
    }

    fn step_world_locked(&self, data: &mut AppData) -> Result<(), String> {
        self.sync_world_nodes_locked(data)?;
        let owned_node_ids = data
            .state
            .nodes
            .iter()
            .filter(|(_, node)| node.owner_id.is_some())
            .map(|(node_id, _)| node_id.clone())
            .collect::<Vec<_>>();
        for node_id in owned_node_ids {
            if let Some(node) = data.state.nodes.get_mut(&node_id) {
                node.army = (node.army + 1).min(1_000_000);
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
                    target_node.army = (target_node.army + flow).min(1_000_000);
                }
                continue;
            }

            if let Some(target_node) = data.state.nodes.get_mut(&attack.to_node_id) {
                target_node.army -= flow;
                if target_node.army < 0 {
                    target_node.owner_id = Some(attack.owner_id.clone());
                    target_node.army = target_node.army.abs().max(1);
                    if let Some(existing_attack) = data.state.attacks.get_mut(&attack_id) {
                        existing_attack.mode = "transfer".to_string();
                    }
                }
            }
        }
        Ok(())
    }

    fn tick_world_locked(&self, data: &mut AppData) -> Result<(), String> {
        let current = now_ms();
        let last = if data.state.saved_at > 0 {
            data.state.saved_at
        } else {
            current
        };
        let elapsed_ticks = ((current - last).max(0) / WORLD_TICK_MS as i64) as usize;
        if elapsed_ticks == 0 {
            self.sync_world_nodes_locked(data)?;
            return Ok(());
        }
        for _ in 0..elapsed_ticks {
            self.step_world_locked(data)?;
        }
        self.save_state_locked(&mut data.state)?;
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

        let mut open = vec![(start.clone(), 0.0_f64)];
        let mut g_score = HashMap::<String, f64>::new();
        let mut prev = HashMap::<String, String>::new();
        let mut visited = HashSet::<String>::new();
        g_score.insert(start, 0.0);

        while !open.is_empty() {
            open.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let (current, _) = open.remove(0);
            if current == goal {
                let path = self.reconstruct_path_locked(data, &prev, &goal);
                data.node_repo.route_cache.insert(cache_key, path.clone());
                return Ok(path);
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            let neighbors = data
                .node_repo
                .adjacency
                .get(&current)
                .cloned()
                .unwrap_or_default();
            for edge in neighbors {
                let current_score = *g_score.get(&current).unwrap_or(&f64::INFINITY);
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
                open.push((edge.to, tentative + heuristic));
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
        let owner_username = world_node
            .owner_id
            .as_deref()
            .and_then(|owner_id| data.state.players.get(owner_id))
            .map(|player| player.username.clone());
        Some(json!({
            "type": "Feature",
            "properties": {
                "id": node_id,
                "latitude": repo_node.lat,
                "longitude": repo_node.lon,
                "degree": repo_node.degree,
                "army": world_node.army,
                "armyLabel": format_army_label(world_node.army),
                "ownerId": world_node.owner_id,
                "ownerUsername": owner_username,
                "ownerState": owner_state,
                "canAttack": player_id.map(|id| world_node.owner_id.as_deref() != Some(id)).unwrap_or(false)
            },
            "geometry": {
                "type": "Point",
                "coordinates": [repo_node.lon, repo_node.lat]
            }
        }))
    }

    fn attack_feature(&self, attack: &Attack, player_id: Option<&str>) -> Value {
        let coordinates = attack
            .path
            .iter()
            .map(|pair| json!([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        json!({
            "type": "Feature",
            "properties": {
                "id": attack.id,
                "ownerId": attack.owner_id,
                "ownerState": if player_id == Some(attack.owner_id.as_str()) { "self" } else { "enemy" },
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
        let mut data = self.inner.lock().unwrap();
        let session = token
            .map(|token| self.require_session_locked(&data, token))
            .transpose()?;
        self.tick_world_locked(&mut data)?;
        self.sync_world_nodes_locked(&mut data)?;
        let player_id = session.as_ref().map(|session| session.player_id.as_str());
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
            .map(|attack| self.attack_feature(attack, player_id))
            .collect::<Vec<_>>();
        Ok(json!({
            "player": player_id.and_then(|player_id| data.state.players.get(player_id)).map(|player| json!({
                "id": player.id,
                "username": player.username
            })),
            "buildStatus": self.current_build_status(),
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

    fn node_ids_for_tile_locked(
        &self,
        data: &mut AppData,
        z: i64,
        x: i64,
        y: i64,
    ) -> Result<Vec<String>, String> {
        self.ensure_node_repo_fresh(data, false)?;
        let zoom = clamp_i64(z, 0, 22);
        let key = format!("{}:{}:{}:{}", data.node_repo.graph_version, zoom, x, y);
        if let Some(ids) = data.node_repo.tile_node_id_cache.get(&key) {
            return Ok(ids.clone());
        }
        let bounds = tile_bounds(zoom, x, y);
        let mut node_ids = Vec::new();
        for node in data.node_repo.nodes_by_id.values() {
            if node.lon < bounds.west
                || node.lon >= bounds.east
                || node.lat < bounds.south
                || node.lat > bounds.north
            {
                continue;
            }
            node_ids.push(node.id.clone());
        }
        data.node_repo.tile_node_id_cache.insert(key, node_ids.clone());
        Ok(node_ids)
    }

    fn node_tile_response(
        &self,
        token: Option<&str>,
        z: i64,
        x: i64,
        y: i64,
    ) -> Result<Value, String> {
        let mut data = self.inner.lock().unwrap();
        let session = token
            .map(|token| self.require_session_locked(&data, token))
            .transpose()?;
        self.tick_world_locked(&mut data)?;
        self.sync_world_nodes_locked(&mut data)?;
        let player_id = session.as_ref().map(|session| session.player_id.as_str());
        let node_ids = self.node_ids_for_tile_locked(&mut data, z, x, y)?;
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
        let mut data = self.inner.lock().unwrap();
        let session = self.require_session_locked(&data, token)?;
        self.tick_world_locked(&mut data)?;
        self.sync_world_nodes_locked(&mut data)?;
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
        let mut data = self.inner.lock().unwrap();
        let session = self.require_session_locked(&data, token)?;
        self.tick_world_locked(&mut data)?;
        self.sync_world_nodes_locked(&mut data)?;
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
        let mut data = self.inner.lock().unwrap();
        let session = self.require_session_locked(&data, token)?;
        self.tick_world_locked(&mut data)?;
        self.sync_world_nodes_locked(&mut data)?;
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
        let mut data = self.inner.lock().unwrap();
        let session = self.require_session_locked(&data, token)?;
        self.tick_world_locked(&mut data)?;
        self.sync_world_nodes_locked(&mut data)?;
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
            .map(|attack| self.attack_feature(attack, Some(session.player_id.as_str())));
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
        let mut data = self.inner.lock().unwrap();
        let session = self.require_session_locked(&data, token)?;
        self.tick_world_locked(&mut data)?;
        self.sync_world_nodes_locked(&mut data)?;
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
        let mut data = self.inner.lock().unwrap();
        self.tick_world_locked(&mut data)
    }

    fn serve_static(&self, pathname: &str) -> Response<std::io::Cursor<Vec<u8>>> {
        let relative = if pathname == "/" {
            PathBuf::from("openfreemap_viewer.html")
        } else if let Some(path) = safe_relative_path(pathname) {
            path
        } else {
            return error_json_response(403, &json!({ "error": "Forbidden." }));
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
    let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
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

fn format_army_label(value: i64) -> String {
    let army = value.max(0) as f64;
    if army >= 1_000_000.0 {
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
        let intersects = ((yi > lat) != (yj > lat))
            && (lon < ((xj - xi) * (lat - yi)) / ((yj - yi).abs().max(f64::EPSILON)) + xi);
        if intersects {
            inside = !inside;
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

fn safe_relative_path(pathname: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(pathname.trim_start_matches('/')).components() {
        match component {
            Component::Normal(value) => out.push(value),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
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

fn handle_request(app: &Arc<App>, mut request: Request) {
    let (pathname, query) = parse_url(request.url());
    let response = (|| -> Result<Response<std::io::Cursor<Vec<u8>>>, String> {
        match (request.method(), pathname.as_str()) {
            (&Method::Post, "/api/register") => {
                let body = parse_body_json(&mut request)?;
                let username = body.get("username").and_then(Value::as_str).unwrap_or_default();
                let password = body.get("password").and_then(Value::as_str).unwrap_or_default();
                Ok(json_response(200, &app.register_player(username, password)?))
            }
            (&Method::Post, "/api/login") => {
                let body = parse_body_json(&mut request)?;
                let username = body.get("username").and_then(Value::as_str).unwrap_or_default();
                let password = body.get("password").and_then(Value::as_str).unwrap_or_default();
                Ok(json_response(200, &app.login_player(username, password)?))
            }
            (&Method::Get, "/api/game-state") => {
                let token = query.get("token").map(String::as_str);
                let include_nodes = query.get("view").map(String::as_str) != Some("summary");
                Ok(json_response(200, &app.game_state_response(token, include_nodes)?))
            }
            (&Method::Get, "/api/node-tile") => {
                let token = query.get("token").map(String::as_str);
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
                Ok(json_response(200, &app.node_tile_response(token, z, x, y)?))
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
            (&Method::Get, "/api/build-status") => Ok(json_response(200, &app.current_build_status())),
            _ => Ok(app.serve_static(&pathname)),
        }
    })()
    .unwrap_or_else(|error| error_json_response(400, &json!({ "error": error })));

    let _ = request.respond(response);
}

fn main() {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let app = Arc::new(App::new(root).expect("failed to initialize app"));

    let ticker_app = app.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(WORLD_TICK_MS));
        if let Err(error) = ticker_app.background_tick() {
            eprintln!("Background tick failed: {error}");
        }
    });

    let port = std::env::var("PORT")
        .ok()
        .or_else(|| std::env::args().nth(1))
        .unwrap_or_else(|| "8002".to_string());
    let address = format!("127.0.0.1:{port}");
    let server = Server::http(&address).unwrap_or_else(|error| panic!("failed to bind {address}: {error}"));
    println!("Game server listening on http://localhost:{port}");

    for request in server.incoming_requests() {
        let app = app.clone();
        thread::spawn(move || {
            handle_request(&app, request);
        });
    }
}
