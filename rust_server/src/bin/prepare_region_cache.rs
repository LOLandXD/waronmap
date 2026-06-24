use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let root = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(env::current_dir().map_err(|err| err.to_string())?);
    let manifest_path = root.join("local_node_store").join("region_manifest.json");
    let manifest = read_json_value(&manifest_path)?;
    let region = manifest
        .get("active_region")
        .cloned()
        .ok_or_else(|| "Missing active_region in region manifest.".to_string())?;
    let prepared_root = root.join(
        region
            .get("prepared_root")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing prepared_root in region manifest.".to_string())?,
    );
    fs::create_dir_all(&prepared_root).map_err(|err| err.to_string())?;

    let metadata_path = prepared_root.join("metadata.json");
    let build_status_path = prepared_root.join("build_status.json");
    let query_tiles_status_path = prepared_root.join("query_tiles_status.geojson");
    let preview_path = prepared_root.join("preview_intersections.geojson");
    let intersections_csv_path = prepared_root.join("intersections.csv");
    let cache_dir = prepared_root.join("_overpass_cache");
    let prepared_cache_path = prepared_root.join("prepared_cache.json");

    if !metadata_path.exists() && !intersections_csv_path.exists() && !preview_path.exists() {
        return Err(format!(
            "Prepared region data missing in {}. This Rust-only runtime expects cached prepared data there.",
            prepared_root.display()
        ));
    }

    let existing_metadata = read_json_value_optional(&metadata_path).unwrap_or_else(|| json!({}));
    let total_points = existing_metadata
        .get("total_points")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| count_csv_rows(&intersections_csv_path).unwrap_or(0));
    let cached_tiles = count_cache_files(&cache_dir);
    let query_tiles = query_tile_boxes(&region, existing_metadata.get("batch_step_deg").and_then(Value::as_f64).unwrap_or(0.2));
    let existing_build_status = read_json_value_optional(&build_status_path).unwrap_or_else(|| json!({}));
    let edge_count = existing_build_status
        .get("edge_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let indexing_method = existing_build_status
        .get("indexing_method")
        .cloned()
        .or_else(|| existing_metadata.get("indexing_method").cloned())
        .unwrap_or_else(|| json!("web_mercator_tile_grid"));
    let s2_index_level = existing_build_status
        .get("s2_index_level")
        .cloned()
        .or_else(|| existing_metadata.get("s2_index_level").cloned());

    let metadata = merged_metadata(&existing_metadata, &region, total_points, query_tiles.len());
    write_json_pretty(&metadata_path, &metadata)?;
    let mut build_status = json!({
        "phase": "complete",
        "region_id": metadata.get("region_id").cloned().unwrap_or_else(|| json!("unknown")),
        "region_name": metadata.get("region_name").cloned().unwrap_or_else(|| json!("Unknown Region")),
        "states": metadata.get("states").cloned().unwrap_or_else(|| json!([])),
        "current": total_points,
        "total": total_points,
        "cached_tiles": cached_tiles,
        "node_count": total_points,
        "edge_count": edge_count,
        "progress_kind": "nodes",
        "progress_nodes_current": total_points,
        "progress_nodes_total": total_points,
        "message": format!("Prepared node dataset cached with {} nodes.", total_points)
    });
    if let Value::String(method) = &indexing_method {
        build_status["indexing_method"] = indexing_method.clone();
        if method == "s2_hilbert" {
            build_status["message"] = json!(format!("Prepared S2/Hilbert-sorted node dataset with {} nodes.", total_points));
        }
    }
    if let Some(level) = s2_index_level {
        build_status["s2_index_level"] = level;
    }
    write_json_pretty(&build_status_path, &build_status)?;
    write_json_compact(
        &query_tiles_status_path,
        &json!({
            "type": "FeatureCollection",
            "features": query_tiles.into_iter().enumerate().map(|(index, bbox)| {
                json!({
                    "type": "Feature",
                    "properties": {
                        "tile_id": index + 1,
                        "completed": true
                    },
                    "geometry": {
                        "type": "Polygon",
                        "coordinates": [[
                            [bbox.west, bbox.south],
                            [bbox.east, bbox.south],
                            [bbox.east, bbox.north],
                            [bbox.west, bbox.north],
                            [bbox.west, bbox.south]
                        ]]
                    }
                })
            }).collect::<Vec<_>>()
        }),
    )?;
    write_json_pretty(
        &prepared_cache_path,
        &json!({
            "prepared_root": prepared_root.display().to_string(),
            "generated_at": now_ms(),
            "total_points": total_points,
            "cached_tiles": cached_tiles,
            "query_tiles": metadata.get("query_tiles").cloned().unwrap_or_else(|| json!(0)),
            "has_preview": preview_path.exists(),
            "has_intersections_csv": intersections_csv_path.exists()
        }),
    )?;

    println!("{}", prepared_cache_path.display());
    Ok(())
}

#[derive(Clone, Copy)]
struct BBox {
    west: f64,
    east: f64,
    south: f64,
    north: f64,
}

fn merged_metadata(existing: &Value, region: &Value, total_points: u64, query_tile_count: usize) -> Value {
    let mut metadata = existing.clone();
    if !metadata.is_object() {
        metadata = json!({});
    }
    metadata["format"] = metadata
        .get("format")
        .cloned()
        .unwrap_or_else(|| json!("intersection_cache_v2"));
    metadata["total_points"] = json!(total_points);
    metadata["region_id"] = region
        .get("id")
        .cloned()
        .unwrap_or_else(|| json!("unknown_region"));
    metadata["region_name"] = region
        .get("name")
        .cloned()
        .unwrap_or_else(|| json!("Unknown Region"));
    metadata["states"] = region.get("states").cloned().unwrap_or_else(|| json!([]));
    metadata["query_tiles"] = json!(query_tile_count);
    metadata["intersections_csv"] = json!("intersections.csv");
    metadata
}

fn query_tile_boxes(region: &Value, step_deg: f64) -> Vec<BBox> {
    let query_bounds = region.get("query_bounds").and_then(Value::as_array);
    if let Some(bounds_list) = query_bounds {
        let mut boxes = Vec::new();
        for bounds in bounds_list {
            boxes.extend(split_bbox(
                bounds.get("west").and_then(Value::as_f64).unwrap_or(0.0),
                bounds.get("south").and_then(Value::as_f64).unwrap_or(0.0),
                bounds.get("east").and_then(Value::as_f64).unwrap_or(0.0),
                bounds.get("north").and_then(Value::as_f64).unwrap_or(0.0),
                step_deg,
            ));
        }
        return boxes;
    }
    split_bbox(
        region.get("bounds").and_then(|value| value.get("west")).and_then(Value::as_f64).unwrap_or(0.0),
        region.get("bounds").and_then(|value| value.get("south")).and_then(Value::as_f64).unwrap_or(0.0),
        region.get("bounds").and_then(|value| value.get("east")).and_then(Value::as_f64).unwrap_or(0.0),
        region.get("bounds").and_then(|value| value.get("north")).and_then(Value::as_f64).unwrap_or(0.0),
        step_deg,
    )
}

fn split_bbox(west: f64, south: f64, east: f64, north: f64, step_deg: f64) -> Vec<BBox> {
    let mut boxes = Vec::new();
    let mut current_south = south;
    while current_south < north {
        let current_north = (current_south + step_deg).min(north);
        let mut current_west = west;
        while current_west < east {
            let current_east = (current_west + step_deg).min(east);
            boxes.push(BBox {
                west: current_west,
                east: current_east,
                south: current_south,
                north: current_north,
            });
            current_west = current_east;
        }
        current_south = current_north;
    }
    boxes
}

fn count_csv_rows(path: &Path) -> Result<u64, String> {
    let file = fs::File::open(path).map_err(|err| err.to_string())?;
    let reader = BufReader::new(file);
    let mut count = 0_u64;
    for (index, line) in reader.lines().enumerate() {
        line.map_err(|err| err.to_string())?;
        if index == 0 {
            continue;
        }
        count += 1;
    }
    Ok(count)
}

fn count_cache_files(path: &Path) -> usize {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .count()
}

fn read_json_value(path: &Path) -> Result<Value, String> {
    read_json_value_optional(path).ok_or_else(|| format!("Failed to read JSON from {}", path.display()))
}

fn read_json_value_optional(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    fs::write(path, bytes).map_err(|err| err.to_string())
}

fn write_json_compact(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    fs::write(path, bytes).map_err(|err| err.to_string())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
