#!/usr/bin/env python3
"""Prints line coverage of the server's sources as a Markdown table, one column per LCOV report.

Counts the server's own sources (crates/*/src/), leaving out test files and the generated REAPI
code. With more than one report, an "All" column counts a line as covered when any report covers
it, which is what merging the reports (adding their counts) gives.

    scripts/coverage-summary.py "Rust tests=rust.info" "E2E=e2e.info"
"""

import argparse
import re
from collections import defaultdict

KEEP = re.compile(r"^crates/[^/]+/src/")
SKIP = re.compile(r"/src/reapi/proto/")


def read(path):
    """Returns {source file: {line: hit count}} for the kept files in an LCOV file."""
    hits = defaultdict(dict)
    source = None
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line.startswith("SF:"):
                source = line[3:]
                if not KEEP.match(source) or SKIP.search(source):
                    source = None
            elif line.startswith("DA:") and source:
                number, count = line[3:].split(",")[:2]
                lines = hits[source]
                lines[number] = lines.get(number, 0) + int(count)
            elif line == "end_of_record":
                source = None
    return hits


def cell(lines):
    if not lines:
        return "–"
    covered = sum(1 for count in lines.values() if count > 0)
    return f"{100 * covered / len(lines):.1f}%"


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("reports", nargs="+", metavar="NAME=LCOV")
    args = parser.parse_args()

    names, reports = [], []
    for arg in args.reports:
        name, _, path = arg.rpartition("=")
        names.append(name or path)
        reports.append(read(path))
    if len(reports) > 1:
        merged = defaultdict(dict)
        for report in reports:
            for source, lines in report.items():
                for number, count in lines.items():
                    merged[source][number] = merged[source].get(number, 0) + count
        names.append("All")
        reports.append(merged)

    sources = sorted({source for report in reports for source in report})
    totals = []
    for report in reports:
        total = {}
        for source, lines in report.items():
            total.update({(source, number): count for number, count in lines.items()})
        totals.append(total)

    print("## Line coverage\n")
    print("Lines of `crates/*/src/` (generated REAPI code left out) that each suite runs.\n")
    print("| | " + " | ".join(names) + " |")
    print("| --- |" + " ---: |" * len(names))
    print("| **Total** | " + " | ".join(f"**{cell(total)}**" for total in totals) + " |")
    print("| Lines | " + " | ".join(str(len(total)) for total in totals) + " |")
    print("\n<details><summary>By file</summary>\n")
    print("| File | " + " | ".join(names) + " |")
    print("| --- |" + " ---: |" * len(names))
    for source in sources:
        row = " | ".join(cell(report.get(source, {})) for report in reports)
        print(f"| `{source.removeprefix('crates/')}` | {row} |")
    print("\n</details>")


if __name__ == "__main__":
    main()
