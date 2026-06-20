from __future__ import annotations

import argparse
import csv
import json
import math
import sys
from collections import defaultdict
from pathlib import Path


def clamp(value: float, lower: float, upper: float) -> float:
    return max(lower, min(upper, value))


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


def lonlat_to_tile(lon: float, lat: float, zoom: int) -> tuple[int, int]:
    lat = clamp(lat, -85.05112878, 85.05112878)
    n = 2 ** zoom
    xtile = int((lon + 180.0) / 360.0 * n)
    lat_rad = math.radians(lat)
    ytile = int((1.0 - math.asinh(math.tan(lat_rad)) / math.pi) / 2.0 * n)
    xtile = max(0, min(n - 1, xtile))
    ytile = max(0, min(n - 1, ytile))
    return xtile, ytile


def iter_csv_points(input_path: Path):
    with input_path.open("r", encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle)
        required = {"id", "latitude", "longitude"}
        if not required.issubset(reader.fieldnames or set()):
            missing = ", ".join(sorted(required - set(reader.fieldnames or [])))
            raise ValueError(f"CSV is missing required columns: {missing}")

        for row in reader:
            try:
                node_id = row["id"]
                lat = float(row["latitude"])
                lon = float(row["longitude"])
            except (KeyError, TypeError, ValueError):
                continue

            degree_raw = row.get("degree", "")
            try:
                degree = int(degree_raw) if degree_raw not in ("", None) else None
            except ValueError:
                degree = None

            yield {
                "id": node_id,
                "latitude": lat,
                "longitude": lon,
                "degree": degree,
            }


def feature_from_point(point: dict) -> dict:
    properties = {
        "id": point["id"],
        "latitude": point["latitude"],
        "longitude": point["longitude"],
    }
    if point["degree"] is not None:
        properties["degree"] = point["degree"]

    return {
        "type": "Feature",
        "geometry": {
            "type": "Point",
            "coordinates": [point["longitude"], point["latitude"]],
        },
        "properties": properties,
    }


def write_geojson(path: Path, features: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "type": "FeatureCollection",
        "features": features,
    }
    with path.open("w", encoding="utf-8") as handle:
        json.dump(payload, handle, separators=(",", ":"))


def build_tiles(
    input_csv: Path,
    output_dir: Path,
    min_zoom: int,
    max_zoom: int,
    progress_callback=None,
) -> dict:
    if min_zoom > max_zoom:
        raise ValueError("min_zoom must be <= max_zoom")

    tile_buckets: dict[tuple[int, int, int], list[dict]] = defaultdict(list)
    total_points = 0
    min_lat = math.inf
    max_lat = -math.inf
    min_lon = math.inf
    max_lon = -math.inf

    for point in iter_csv_points(input_csv):
        total_points += 1
        min_lat = min(min_lat, point["latitude"])
        max_lat = max(max_lat, point["latitude"])
        min_lon = min(min_lon, point["longitude"])
        max_lon = max(max_lon, point["longitude"])

        feature = feature_from_point(point)
        for zoom in range(min_zoom, max_zoom + 1):
            x, y = lonlat_to_tile(point["longitude"], point["latitude"], zoom)
            tile_buckets[(zoom, x, y)].append(feature)

    if total_points == 0:
        raise ValueError("No valid points found in input CSV")

    if output_dir.exists():
        for existing in output_dir.glob("**/*.geojson"):
            existing.unlink()
        for existing in output_dir.glob("*.json"):
            existing.unlink()

    tiles_written = 0
    tile_index: dict[str, int] = {}
    total_tile_files = len(tile_buckets)
    for (zoom, x, y), features in tile_buckets.items():
        tile_path = output_dir / str(zoom) / str(x) / f"{y}.geojson"
        write_geojson(tile_path, features)
        tile_index[f"{zoom}/{x}/{y}"] = len(features)
        tiles_written += 1
        if progress_callback is not None:
            progress_callback(tiles_written, total_tile_files, total_points)
        render_progress(
            "Writing tiles",
            tiles_written,
            total_tile_files,
            suffix=f"nodes={total_points:,}",
        )

    finish_progress()

    metadata = {
        "format": "intersection_tiles_v1",
        "indexing_method": "web_mercator_tile_grid",
        "source_csv": str(input_csv.name),
        "min_zoom": min_zoom,
        "max_zoom": max_zoom,
        "total_points": total_points,
        "bounds": {
            "min_lat": min_lat,
            "max_lat": max_lat,
            "min_lon": min_lon,
            "max_lon": max_lon,
        },
        "tiles_written": tiles_written,
    }

    output_dir.mkdir(parents=True, exist_ok=True)
    metadata_path = output_dir / "metadata.json"
    with metadata_path.open("w", encoding="utf-8") as handle:
        json.dump(metadata, handle, indent=2)

    tile_index_path = output_dir / "tile_index.json"
    with tile_index_path.open("w", encoding="utf-8") as handle:
        json.dump(tile_index, handle, separators=(",", ":"))

    return metadata


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build static web tiles from a prepared street-intersection CSV."
    )
    parser.add_argument(
        "--input-csv",
        required=True,
        help="CSV with columns id,latitude,longitude and optional degree.",
    )
    parser.add_argument(
        "--output-dir",
        default="prepared_nodes",
        help="Directory where tile GeoJSON files and metadata.json will be written.",
    )
    parser.add_argument(
        "--min-zoom",
        type=int,
        default=8,
        help="Lowest tile zoom to prepare.",
    )
    parser.add_argument(
        "--max-zoom",
        type=int,
        default=14,
        help="Highest tile zoom to prepare.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    metadata = build_tiles(
        input_csv=Path(args.input_csv).expanduser().resolve(),
        output_dir=Path(args.output_dir).expanduser().resolve(),
        min_zoom=args.min_zoom,
        max_zoom=args.max_zoom,
    )
    print(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    main()
