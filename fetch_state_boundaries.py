from __future__ import annotations

import argparse
import json
import urllib.request
from pathlib import Path

SOURCE_URL = "https://raw.githubusercontent.com/PublicaMundi/MappingAPI/master/data/geojson/us-states.json"

DEFAULT_STATES = (
    ("Maine", "23"),
    ("New Hampshire", "33"),
    ("Vermont", "50"),
    ("Massachusetts", "25"),
)


def round_coords(value):
    if isinstance(value, list):
        return [round_coords(item) for item in value]
    if isinstance(value, float):
        return round(value, 6)
    return value


def fetch_source_features() -> list[dict]:
    request = urllib.request.Request(SOURCE_URL, headers={"User-Agent": "Mozilla/5.0 solo-coder"})
    payload = json.loads(urllib.request.urlopen(request).read().decode("utf-8"))
    return payload["features"]


def fetch_state_feature(source_features: list[dict], name: str, fips: str) -> dict:
    item = next((feature for feature in source_features if feature.get("properties", {}).get("name") == name), None)
    if item is None:
        raise RuntimeError(f"No state boundary returned for {name}")
    geometry = item["geometry"]
    geometry["coordinates"] = round_coords(geometry["coordinates"])
    return {
        "type": "Feature",
        "properties": {
            "name": name,
            "statefp": fips,
        },
        "geometry": geometry,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Fetch state polygons and write a GeoJSON file.")
    parser.add_argument(
        "--output",
        default="local_node_store/us_state_boundaries.geojson",
        help="Output GeoJSON path.",
    )
    args = parser.parse_args()

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    source_features = fetch_source_features()
    features = [fetch_state_feature(source_features, name, fips) for name, fips in DEFAULT_STATES]
    output_path.write_text(
        json.dumps({"type": "FeatureCollection", "features": features}, separators=(",", ":")),
        encoding="utf-8",
    )
    print(output_path)


if __name__ == "__main__":
    main()
