from __future__ import annotations

import argparse
import csv
from collections import Counter
from pathlib import Path

import osmium


EXCLUDED_HIGHWAYS = {
    "bridleway",
    "bus_guideway",
    "construction",
    "corridor",
    "cycleway",
    "footway",
    "path",
    "pedestrian",
    "platform",
    "planned",
    "proposed",
    "raceway",
    "service",
    "steps",
    "track",
}


def is_relevant_road(tags: osmium.osm.TagList) -> bool:
    highway = tags.get("highway")
    if not highway:
      return False
    if highway in EXCLUDED_HIGHWAYS:
      return False
    if tags.get("area") == "yes":
      return False
    return True


class RoadNodeCounter(osmium.SimpleHandler):
    def __init__(self) -> None:
        super().__init__()
        self.node_counts: Counter[int] = Counter()

    def way(self, way: osmium.osm.Way) -> None:
        if not is_relevant_road(way.tags):
            return

        refs = [node.ref for node in way.nodes]
        if len(refs) < 2:
            return

        # Count segment connectivity instead of only counting ways.
        for index in range(len(refs) - 1):
            self.node_counts[refs[index]] += 1
            self.node_counts[refs[index + 1]] += 1


class IntersectionWriter(osmium.SimpleHandler):
    def __init__(self, output_csv: Path, node_counts: Counter[int], min_degree: int) -> None:
        super().__init__()
        self.output_csv = output_csv
        self.node_counts = node_counts
        self.min_degree = min_degree
        self.rows_written = 0
        self.handle = output_csv.open("w", encoding="utf-8", newline="")
        self.writer = csv.writer(self.handle)
        self.writer.writerow(["id", "latitude", "longitude", "degree"])

    def node(self, node: osmium.osm.Node) -> None:
        degree = self.node_counts.get(node.id, 0)
        if degree < self.min_degree:
            return
        if not node.location.valid():
            return

        self.writer.writerow(
            [
                node.id,
                f"{node.location.lat:.7f}",
                f"{node.location.lon:.7f}",
                degree,
            ]
        )
        self.rows_written += 1

    def close(self) -> None:
        self.handle.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Extract street-intersection nodes from a U.S. OSM PBF into CSV."
    )
    parser.add_argument(
        "--input-pbf",
        required=True,
        help="Path to a nationwide OSM PBF, such as a Geofabrik U.S. extract.",
    )
    parser.add_argument(
        "--output-csv",
        default="usa_intersections.csv",
        help="CSV output path.",
    )
    parser.add_argument(
        "--min-degree",
        type=int,
        default=3,
        help="Minimum connected road-segment degree required to keep a node.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    input_pbf = Path(args.input_pbf).expanduser().resolve()
    output_csv = Path(args.output_csv).expanduser().resolve()

    if not input_pbf.exists():
        raise FileNotFoundError(f"Input PBF not found: {input_pbf}")
    if args.min_degree < 2:
        raise ValueError("--min-degree must be at least 2")

    print(f"Counting road-node connectivity in {input_pbf} ...")
    counter = RoadNodeCounter()
    counter.apply_file(str(input_pbf), locations=False)

    candidate_nodes = sum(1 for degree in counter.node_counts.values() if degree >= args.min_degree)
    print(f"Candidate intersections with degree >= {args.min_degree}: {candidate_nodes:,}")

    print(f"Writing coordinates to {output_csv} ...")
    writer = IntersectionWriter(output_csv=output_csv, node_counts=counter.node_counts, min_degree=args.min_degree)
    try:
        writer.apply_file(str(input_pbf), locations=True)
    finally:
        writer.close()

    print(f"Finished. Wrote {writer.rows_written:,} intersections to {output_csv}")


if __name__ == "__main__":
    main()
