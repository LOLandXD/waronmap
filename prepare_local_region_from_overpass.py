from __future__ import annotations

import argparse
import csv
import json
import hashlib
import sys
import time
import urllib.parse
from collections import Counter
from pathlib import Path

import requests

from build_intersection_tiles import build_tiles


OVERPASS_URLS = [
    "https://overpass.kumi.systems/api/interpreter",
    "https://lz4.overpass-api.de/api/interpreter",
    "https://overpass-api.de/api/interpreter",
]
ROAD_FILTER_VERSION = "v2"
MAX_SPLIT_DEPTH = 2
MIN_SPLIT_DEG = 0.05
HIGHWAY_FILTER = (
    '["highway"]["area"!="yes"]'
    '["highway"!~"footway|path|cycleway|steps|proposed|construction|corridor|pedestrian|bridleway"]'
)


def render_progress(prefix: str, current: int, total: int, suffix: str = "") -> None:
    total = max(total, 1)
    current = max(0, min(current, total))
    width = 28
    filled = int(width * current / total)
    bar = "#" * filled + "-" * (width - filled)
    percent = (current / total) * 100.0
    line = f"\r{prefix} [{bar}] {current}/{total} {percent:5.1f}%"
    if suffix:
        line += f"  {suffix}"
    sys.stdout.write(line)
    sys.stdout.flush()


def finish_progress() -> None:
    sys.stdout.write("\n")
    sys.stdout.flush()


def bbox_tiles(bounds: dict, step_deg: float) -> list[tuple[float, float, float, float]]:
    tiles = []
    south = bounds["south"]
    while south < bounds["north"]:
        north = min(bounds["north"], south + step_deg)
        west = bounds["west"]
        while west < bounds["east"]:
            east = min(bounds["east"], west + step_deg)
            tiles.append((south, west, north, east))
            west = east
        south = north
    return tiles


def region_query_tiles(region: dict, step_deg: float) -> list[tuple[float, float, float, float]]:
    query_bounds = region.get("query_bounds")
    if query_bounds:
      tiles: list[tuple[float, float, float, float]] = []
      for bounds in query_bounds:
          tiles.extend(bbox_tiles(bounds, step_deg))
      return tiles
    return bbox_tiles(region["bounds"], step_deg)


def build_query(south: float, west: float, north: float, east: float, timeout_s: int) -> str:
    return f"""
[out:json][timeout:{timeout_s}];
(
  way{HIGHWAY_FILTER}({south},{west},{north},{east});
);
out body;
>;
out skel qt;
""".strip()


def error_summary(exc: Exception) -> str:
    text = str(exc).strip().replace("\n", " ")
    return text if len(text) <= 220 else text[:217] + "..."


def merge_overpass_payloads(payloads: list[dict]) -> dict:
    seen: set[tuple[str | None, int | None]] = set()
    elements: list[dict] = []
    for payload in payloads:
        for element in payload.get("elements", []):
            key = (element.get("type"), element.get("id"))
            if key in seen:
                continue
            seen.add(key)
            elements.append(element)
    return {
        "version": 0.6,
        "generator": "merged-overpass",
        "elements": elements,
    }


def split_bbox(south: float, west: float, north: float, east: float) -> list[tuple[float, float, float, float]]:
    lat_span = north - south
    lon_span = east - west
    if lat_span <= MIN_SPLIT_DEG and lon_span <= MIN_SPLIT_DEG:
        return []

    mid_lat = south + (lat_span / 2.0)
    mid_lon = west + (lon_span / 2.0)
    return [
        (south, west, mid_lat, mid_lon),
        (south, mid_lon, mid_lat, east),
        (mid_lat, west, north, mid_lon),
        (mid_lat, mid_lon, north, east),
    ]


def fetch_overpass_bbox(
    south: float,
    west: float,
    north: float,
    east: float,
    timeout_s: int,
    retries: int,
    status_callback=None,
    split_depth: int = 0,
) -> dict:
    query = build_query(south, west, north, east, timeout_s)
    last_error: Exception | None = None
    split_attempted = False
    cycle = 0

    while True:
        cycle += 1
        for endpoint in OVERPASS_URLS:
            url = endpoint + "?data=" + urllib.parse.quote(query)
            for attempt in range(1, retries + 1):
                try:
                    response = requests.get(
                        url,
                        timeout=timeout_s + 120,
                        headers={"User-Agent": "new-england-four-node-builder/1.0"},
                    )
                    response.raise_for_status()
                    return response.json()
                except Exception as exc:  # noqa: BLE001
                    last_error = exc
                    wait_s = min(6 * attempt, 18)
                    if status_callback:
                        status_callback(
                            kind="retry",
                            endpoint=urllib.parse.urlparse(endpoint).netloc,
                            wait_s=wait_s,
                            split_depth=split_depth,
                            error=error_summary(exc),
                        )
                    time.sleep(wait_s)

        if not split_attempted and split_depth < MAX_SPLIT_DEPTH:
            child_tiles = split_bbox(south, west, north, east)
            if child_tiles:
                split_attempted = True
                if status_callback:
                    status_callback(
                        kind="split",
                        endpoint="subdivide",
                        wait_s=0,
                        split_depth=split_depth + 1,
                        error=error_summary(last_error or RuntimeError("Unknown Overpass failure")),
                    )
                return merge_overpass_payloads(
                    [
                        fetch_overpass_bbox(
                            child_south,
                            child_west,
                            child_north,
                            child_east,
                            timeout_s=timeout_s,
                            retries=retries,
                            status_callback=status_callback,
                            split_depth=split_depth + 1,
                        )
                        for child_south, child_west, child_north, child_east in child_tiles
                    ]
                )

        wait_s = min(30 * cycle, 180)
        if status_callback:
            status_callback(
                kind="cycle",
                endpoint="all-endpoints",
                wait_s=wait_s,
                split_depth=split_depth,
                error=error_summary(last_error or RuntimeError("Unknown Overpass failure")),
            )
        time.sleep(wait_s)


def normalize_edge(a: int, b: int) -> tuple[int, int]:
    return (a, b) if a < b else (b, a)


def process_overpass_elements(
    data: dict,
    node_coords: dict[int, tuple[float, float]],
    edge_set: set[tuple[int, int]],
    degree_counter: Counter[int],
) -> None:
    elements = data.get("elements", [])
    for element in elements:
        if element.get("type") == "node":
            lat = element.get("lat")
            lon = element.get("lon")
            if isinstance(lat, (int, float)) and isinstance(lon, (int, float)):
                node_coords[int(element["id"])] = (float(lat), float(lon))

    for element in elements:
        if element.get("type") != "way":
            continue
        refs = element.get("nodes") or []
        if len(refs) < 2:
            continue
        for pos in range(len(refs) - 1):
            a = int(refs[pos])
            b = int(refs[pos + 1])
            edge = normalize_edge(a, b)
            if edge in edge_set:
                continue
            edge_set.add(edge)
            degree_counter[a] += 1
            degree_counter[b] += 1


def write_build_status(status_path: Path, payload: dict) -> None:
    status_path.write_text(json.dumps(payload, indent=2), encoding="utf-8")


def build_status_payload(
    region: dict,
    *,
    phase: str,
    current: int,
    total: int,
    cached_tiles: int,
    node_count: int,
    edge_count: int,
    message: str,
) -> dict:
    return {
        "phase": phase,
        "region_id": region["id"],
        "region_name": region["name"],
        "states": region["states"],
        "current": current,
        "total": total,
        "cached_tiles": cached_tiles,
        "node_count": node_count,
        "edge_count": edge_count,
        "progress_kind": "nodes",
        "progress_nodes_current": node_count,
        "progress_nodes_total": node_count if phase in {"tiling", "complete"} else None,
        "message": message,
    }


def tile_feature(tile_id: int, south: float, west: float, north: float, east: float, completed: bool) -> dict:
    return {
        "type": "Feature",
        "properties": {
            "tile_id": tile_id,
            "completed": completed,
        },
        "geometry": {
            "type": "Polygon",
            "coordinates": [[
                [west, south],
                [east, south],
                [east, north],
                [west, north],
                [west, south],
            ]],
        },
    }


def write_query_tile_status(status_geojson_path: Path, tiles: list[tuple[float, float, float, float]], completed_ids: set[int]) -> None:
    features = []
    for index, (south, west, north, east) in enumerate(tiles, start=1):
        features.append(tile_feature(index, south, west, north, east, index in completed_ids))

    status_geojson_path.write_text(
        json.dumps(
            {
                "type": "FeatureCollection",
                "features": features,
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )


def preview_feature(node_id: int, lat: float, lon: float, degree: int) -> dict:
    return {
        "type": "Feature",
        "properties": {
            "id": node_id,
            "latitude": round(lat, 7),
            "longitude": round(lon, 7),
            "degree": degree,
        },
        "geometry": {
            "type": "Point",
            "coordinates": [round(lon, 7), round(lat, 7)],
        },
    }


def write_preview_intersections(preview_path: Path, node_coords: dict[int, tuple[float, float]], degree_counter: Counter[int], min_degree: int) -> int:
    features = []
    for node_id, degree in degree_counter.items():
        if degree < min_degree:
            continue
        coords = node_coords.get(node_id)
        if not coords:
            continue
        lat, lon = coords
        features.append(preview_feature(node_id, lat, lon, degree))

    preview_path.write_text(
        json.dumps(
            {
                "type": "FeatureCollection",
                "features": features,
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    return len(features)


def cache_file_path(cache_dir: Path, south: float, west: float, north: float, east: float) -> Path:
    bbox_key = f"{ROAD_FILTER_VERSION}:{HIGHWAY_FILTER}:{south:.6f},{west:.6f},{north:.6f},{east:.6f}"
    digest = hashlib.sha1(bbox_key.encode("utf-8")).hexdigest()[:16]
    return cache_dir / f"{digest}.json"


def prepare_region(
    manifest_path: Path,
    region_id: str | None,
    batch_step_deg: float,
    query_timeout_s: int,
    retries: int,
    min_degree: int,
    min_zoom: int,
    max_zoom: int,
) -> dict:
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    region = manifest["active_region"]
    if region_id and region["id"] != region_id:
        raise ValueError(f"Manifest active region is {region['id']}, not {region_id}")

    prepared_root = (manifest_path.parent.parent / region["prepared_root"]).resolve()
    prepared_root.mkdir(parents=True, exist_ok=True)
    cache_dir = prepared_root / "_overpass_cache"
    cache_dir.mkdir(parents=True, exist_ok=True)
    status_path = prepared_root / "build_status.json"
    tile_status_path = prepared_root / "query_tiles_status.geojson"
    preview_path = prepared_root / "preview_intersections.geojson"

    node_coords: dict[int, tuple[float, float]] = {}
    edge_set: set[tuple[int, int]] = set()
    degree_counter: Counter[int] = Counter()

    tiles = region_query_tiles(region, batch_step_deg)
    total_tiles = len(tiles)
    cached_tiles = 0
    completed_tile_ids: set[int] = set()
    fetched_any_tiles = False

    for index, (south, west, north, east) in enumerate(tiles, start=1):
        cache_path = cache_file_path(cache_dir, south, west, north, east)
        if cache_path.exists():
            cached_tiles += 1
            completed_tile_ids.add(index)
            data = json.loads(cache_path.read_text(encoding="utf-8"))
            process_overpass_elements(data, node_coords, edge_set, degree_counter)

    preview_count = write_preview_intersections(preview_path, node_coords, degree_counter, min_degree)
    write_query_tile_status(tile_status_path, tiles, completed_tile_ids)

    write_build_status(
        status_path,
        build_status_payload(
            region,
            phase="fetching",
            current=len(completed_tile_ids),
            total=total_tiles,
            cached_tiles=cached_tiles,
            node_count=preview_count,
            edge_count=len(edge_set),
            message=(
                f"Resumed from {cached_tiles} cached road batches. "
                f"Preview intersections: {preview_count:,}."
                if cached_tiles
                else "Starting local road batch fetch."
            ),
        ),
    )

    for index, (south, west, north, east) in enumerate(tiles, start=1):
        if index in completed_tile_ids:
            continue
        cache_path = cache_file_path(cache_dir, south, west, north, east)
        render_progress(
            "Building dataset",
            len(completed_tile_ids),
            total_tiles,
            suffix=f"fetching bbox {south:.3f},{west:.3f},{north:.3f},{east:.3f}",
        )

        def report_fetch_issue(*, kind: str, endpoint: str, wait_s: int, split_depth: int, error: str) -> None:
            if kind == "retry":
                message = (
                    f"Retrying road batch {index} of {total_tiles} via {endpoint} in {wait_s}s. "
                    f"Last error: {error}"
                )
            elif kind == "split":
                message = (
                    f"Splitting road batch {index} of {total_tiles} into smaller queries "
                    f"(depth {split_depth}) after: {error}"
                )
            else:
                message = (
                    f"Still retrying road batch {index} of {total_tiles}. "
                    f"Waiting {wait_s}s before the next endpoint round. Last error: {error}"
                )

            write_build_status(
                status_path,
                build_status_payload(
                    region,
                    phase="fetching",
                    current=len(completed_tile_ids),
                    total=total_tiles,
                    cached_tiles=cached_tiles,
                    node_count=preview_count,
                    edge_count=len(edge_set),
                    message=message,
                ),
            )

        data = fetch_overpass_bbox(
            south=south,
            west=west,
            north=north,
            east=east,
            timeout_s=query_timeout_s,
            retries=retries,
            status_callback=report_fetch_issue,
        )
        fetched_any_tiles = True
        cache_path.write_text(json.dumps(data, separators=(",", ":")), encoding="utf-8")
        completed_tile_ids.add(index)
        process_overpass_elements(data, node_coords, edge_set, degree_counter)

        render_progress(
            "Building dataset",
            len(completed_tile_ids),
            total_tiles,
            suffix=(
                f"cached={cached_tiles} nodes={len(node_coords):,} "
                f"edges={len(edge_set):,}"
            ),
        )
        write_build_status(
            status_path,
            build_status_payload(
                region,
                phase="fetching",
                current=len(completed_tile_ids),
                total=total_tiles,
                cached_tiles=cached_tiles,
                node_count=len(node_coords),
                edge_count=len(edge_set),
                message=(
                    f"Processed {len(completed_tile_ids)} of {total_tiles} road batches. "
                    f"Cached: {cached_tiles}."
                ),
            ),
        )
        write_query_tile_status(tile_status_path, tiles, completed_tile_ids)
        preview_count = write_preview_intersections(preview_path, node_coords, degree_counter, min_degree)
        write_build_status(
            status_path,
            build_status_payload(
                region,
                phase="fetching",
                current=len(completed_tile_ids),
                total=total_tiles,
                cached_tiles=cached_tiles,
                node_count=preview_count,
                edge_count=len(edge_set),
                message=(
                    f"Processed {len(completed_tile_ids)} of {total_tiles} road batches. "
                    f"Cached: {cached_tiles}. Preview intersections: {preview_count:,}."
                ),
            ),
        )

    if fetched_any_tiles:
        finish_progress()

    intersection_csv = prepared_root / "intersections.csv"
    rows_written = 0
    with intersection_csv.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(["id", "latitude", "longitude", "degree"])
        for node_id, degree in degree_counter.items():
            if degree < min_degree:
                continue
            coords = node_coords.get(node_id)
            if not coords:
                continue
            lat, lon = coords
            writer.writerow([node_id, f"{lat:.7f}", f"{lon:.7f}", degree])
            rows_written += 1

    print(f"Intersections written to CSV: {rows_written:,}", flush=True)
    write_preview_intersections(preview_path, node_coords, degree_counter, min_degree)
    write_build_status(
        status_path,
        build_status_payload(
            region,
            phase="tiling",
            current=0,
            total=1,
            cached_tiles=cached_tiles,
            node_count=rows_written,
            edge_count=len(edge_set),
            message=f"Writing prepared node tiles for {rows_written:,} intersections.",
        ),
    )

    metadata = build_tiles(
        input_csv=intersection_csv,
        output_dir=prepared_root,
        min_zoom=min_zoom,
        max_zoom=max_zoom,
        progress_callback=lambda current, total, total_points: write_build_status(
            status_path,
            build_status_payload(
                region,
                phase="tiling",
                current=current,
                total=total,
                cached_tiles=cached_tiles,
                node_count=total_points,
                edge_count=len(edge_set),
                message=f"Writing tile {current} of {total}.",
            ),
        ),
    )
    metadata["region_id"] = region["id"]
    metadata["region_name"] = region["name"]
    metadata["states"] = region["states"]
    metadata["batch_step_deg"] = batch_step_deg
    metadata["query_tiles"] = total_tiles
    metadata["intersections_csv"] = str(intersection_csv.name)

    (prepared_root / "metadata.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")
    write_build_status(
        status_path,
        build_status_payload(
            region,
            phase="complete",
            current=metadata["total_points"],
            total=metadata["total_points"],
            cached_tiles=cached_tiles,
            node_count=metadata["total_points"],
            edge_count=len(edge_set),
            message=f"Prepared node dataset complete with {metadata['total_points']:,} nodes.",
        ),
    )
    return metadata


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Prepare a local region node store by querying Overpass in bounding-box batches."
    )
    parser.add_argument(
        "--manifest",
        default="local_node_store/region_manifest.json",
        help="Path to the local region manifest.",
    )
    parser.add_argument(
        "--region-id",
        default=None,
        help="Optional safety check for the active region id.",
    )
    parser.add_argument(
        "--batch-step-deg",
        type=float,
        default=0.35,
        help="Bounding-box batch size in degrees.",
    )
    parser.add_argument(
        "--query-timeout-s",
        type=int,
        default=80,
        help="Overpass query timeout in seconds.",
    )
    parser.add_argument(
        "--retries",
        type=int,
        default=3,
        help="Retries per batch.",
    )
    parser.add_argument(
        "--min-degree",
        type=int,
        default=3,
        help="Minimum road-segment degree required to keep a node.",
    )
    parser.add_argument(
        "--min-zoom",
        type=int,
        default=8,
        help="Lowest prepared tile zoom.",
    )
    parser.add_argument(
        "--max-zoom",
        type=int,
        default=14,
        help="Highest prepared tile zoom.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    metadata = prepare_region(
        manifest_path=Path(args.manifest).expanduser().resolve(),
        region_id=args.region_id,
        batch_step_deg=args.batch_step_deg,
        query_timeout_s=args.query_timeout_s,
        retries=args.retries,
        min_degree=args.min_degree,
        min_zoom=args.min_zoom,
        max_zoom=args.max_zoom,
    )
    print(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    main()
