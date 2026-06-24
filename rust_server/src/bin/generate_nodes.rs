use s2::cellid::CellID;
use s2::latlng::LatLng;

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

const S2_LEAF_LEVEL: u64 = 30;
const S2_INDEX_LEVEL: u64 = 12;

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

    let intersections_csv_path = prepared_root.join("intersections.csv");
    let cache_dir = prepared_root.join("_overpass_cache");

    if !intersections_csv_path.exists() {
        return Err(format!(
            "Prepared intersections CSV missing at {}. Provide a valid OSM PBF or prepared CSV.",
            intersections_csv_path.display()
        ));
    }

    println!("Reading existing intersections from {}...", intersections_csv_path.display());
    let mut nodes = read_intersections_csv(&intersections_csv_path)?;
    println!("Read {} intersections.", nodes.len());

    println!("Computing S2 Cell IDs and sorting by Hilbert curve...");
    for node in &mut nodes {
        node.cell_id = CellID::from(LatLng::from_degrees(node.lat, node.lon));
    }
    nodes.sort_by_key(|node| node.cell_id.0);

    println!("Rewriting intersections.csv in Hilbert order...");
    write_intersections_csv(&intersections_csv_path, &nodes)?;

    let (tile_index, cell_ranges) = build_tile_index(&nodes)?;
    let tile_index_path = prepared_root.join("tile_index.json");
    write_json_pretty(&tile_index_path, &tile_index)?;

    let metadata_path = prepared_root.join("metadata.json");
    let edge_count = count_edges(&cache_dir)?;
    let metadata = build_metadata(&region, nodes.len() as u64, cell_ranges.len(), edge_count);
    write_json_pretty(&metadata_path, &metadata)?;

    let build_status_path = prepared_root.join("build_status.json");
    write_json_pretty(
        &build_status_path,
        &json!({
            "phase": "complete",
            "region_id": region.get("id").cloned().unwrap_or_else(|| json!("unknown")),
            "region_name": region.get("name").cloned().unwrap_or_else(|| json!("Unknown Region")),
            "states": region.get("states").cloned().unwrap_or_else(|| json!([])),
            "current": nodes.len(),
            "total": nodes.len(),
            "cached_tiles": count_cache_files(&cache_dir),
            "node_count": nodes.len(),
            "edge_count": edge_count,
            "progress_kind": "nodes",
            "progress_nodes_current": nodes.len(),
            "progress_nodes_total": nodes.len(),
            "indexing_method": "s2_hilbert",
            "s2_index_level": S2_INDEX_LEVEL,
            "message": format!("Re-sorted {} intersections using S2 Hilbert curve.", nodes.len())
        }),
    )?;

    let preview_path = prepared_root.join("preview_intersections.geojson");
    write_preview_geojson(&preview_path, &nodes)?;

    println!(
        "Done. Wrote {} S2/Hilbert-sorted intersections to {}.",
        nodes.len(),
        intersections_csv_path.display()
    );
    Ok(())
}

struct IntersectionNode {
    id: String,
    lat: f64,
    lon: f64,
    degree: i64,
    cell_id: CellID,
}

fn read_intersections_csv(path: &Path) -> Result<Vec<IntersectionNode>, String> {
    let file = fs::File::open(path).map_err(|err| err.to_string())?;
    let reader = BufReader::new(file);
    let mut nodes = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|err| err.to_string())?;
        if index == 0 {
            continue;
        }
        let mut parts = line.split(',');
        let id = parts.next().unwrap_or_default().trim().to_string();
        let lat = parts.next().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(f64::NAN);
        let lon = parts.next().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(f64::NAN);
        let degree = parts.next().and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0);
        if id.is_empty() || !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        nodes.push(IntersectionNode {
            id,
            lat,
            lon,
            degree,
            cell_id: CellID(0),
        });
    }
    Ok(nodes)
}

fn write_intersections_csv(path: &Path, nodes: &[IntersectionNode]) -> Result<(), String> {
    let file = fs::File::create(path).map_err(|err| err.to_string())?;
    let mut writer = BufWriter::new(file);
    writeln!(writer, "id,latitude,longitude,degree").map_err(|err| err.to_string())?;
    for node in nodes {
        writeln!(
            writer,
            "{},{:.7},{:.7},{}",
            node.id, node.lat, node.lon, node.degree
        )
        .map_err(|err| err.to_string())?;
    }
    writer.flush().map_err(|err| err.to_string())?;
    Ok(())
}

fn build_tile_index(nodes: &[IntersectionNode]) -> Result<(Value, BTreeMap<u64, (usize, usize)>), String> {
    let mut cell_ranges: BTreeMap<u64, (usize, usize)> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        let parent = node.cell_id.parent(S2_INDEX_LEVEL);
        let entry = cell_ranges.entry(parent.0).or_insert((index, index));
        entry.1 = index;
    }

    let mut tile_entries = Vec::new();
    for (cell_raw, (start, end)) in &cell_ranges {
        let cell = CellID(*cell_raw);
        tile_entries.push(json!({
            "cell_token": cell.to_token(),
            "range_min": cell.range_min().to_token(),
            "range_max": cell.range_max().to_token(),
            "start_index": start,
            "end_index": end
        }));
    }

    let index = json!({
        "indexing_method": "s2_hilbert",
        "s2_level": S2_INDEX_LEVEL,
        "leaf_level": S2_LEAF_LEVEL,
        "total_points": nodes.len(),
        "cell_count": cell_ranges.len(),
        "cells": tile_entries
    });
    Ok((index, cell_ranges))
}

fn build_metadata(region: &Value, total_points: u64, cell_count: usize, edge_count: usize) -> Value {
    json!({
        "format": "intersection_cache_v2",
        "total_points": total_points,
        "region_id": region.get("id").cloned().unwrap_or_else(|| json!("unknown_region")),
        "region_name": region.get("name").cloned().unwrap_or_else(|| json!("Unknown Region")),
        "states": region.get("states").cloned().unwrap_or_else(|| json!([])),
        "query_tiles": 1,
        "intersections_csv": "intersections.csv",
        "indexing_method": "s2_hilbert",
        "s2_index_level": S2_INDEX_LEVEL,
        "s2_cell_count": cell_count,
        "edge_count": edge_count
    })
}

fn write_preview_geojson(path: &Path, nodes: &[IntersectionNode]) -> Result<(), String> {
    let features: Vec<Value> = nodes
        .iter()
        .map(|node| {
            json!({
                "type": "Feature",
                "properties": {
                    "id": node.id.clone(),
                    "degree": node.degree
                },
                "geometry": {
                    "type": "Point",
                    "coordinates": [node.lon, node.lat]
                }
            })
        })
        .collect();
    write_json_pretty(
        path,
        &json!({
            "type": "FeatureCollection",
            "features": features
        }),
    )?;
    Ok(())
}

fn count_cache_files(path: &Path) -> usize {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .count()
}

fn count_edges(cache_dir: &Path) -> Result<usize, String> {
    let mut count = 0_usize;
    if !cache_dir.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(cache_dir).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).map_err(|err| err.to_string())?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
        if let Some(elements) = value.get("elements").and_then(Value::as_array) {
            for element in elements {
                if element.get("type").and_then(Value::as_str) == Some("way") {
                    if let Some(nodes) = element.get("nodes").and_then(Value::as_array) {
                        count += nodes.len().saturating_sub(1).max(0);
                    }
                }
            }
        }
    }
    Ok(count)
}

fn read_json_value(path: &Path) -> Result<Value, String> {
    let bytes = fs::read(path).map_err(|err| err.to_string())?;
    serde_json::from_slice(&bytes).map_err(|err| err.to_string())
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    fs::write(path, bytes).map_err(|err| err.to_string())
}
