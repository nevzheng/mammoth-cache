#!/usr/bin/env python3
"""Summarizes the LCOV report from `bazel coverage` as a Markdown table of line coverage.

Keeps the server's own sources (crates/*/src/), leaving out the test files (instrumented by
--instrument_test_targets so integration tests report the library code they reach) and the
generated REAPI code. Writes the filtered report to --lcov for genhtml and other tools.

    scripts/coverage-summary.py bazel-out/_coverage/_coverage_report.dat --lcov lcov.info
"""

import argparse
import re
import sys
from collections import defaultdict

KEEP = re.compile(r"^crates/[^/]+/src/")
SKIP = re.compile(r"/src/reapi/proto/")


def parse(path):
    """Yields (source file, its record's lines) for each record in an LCOV file."""
    record = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            record.append(line)
            if line == "end_of_record":
                yield record[0].removeprefix("SF:"), record
                record = []


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("report", help="LCOV file from bazel coverage --combined_report=lcov")
    parser.add_argument("--lcov", help="write the filtered LCOV report here")
    args = parser.parse_args()

    hits = defaultdict(dict)
    kept = []
    for source, record in parse(args.report):
        if not KEEP.match(source) or SKIP.search(source):
            continue
        kept.extend(record)
        lines = hits[source]
        for line in record:
            if line.startswith("DA:"):
                number, count = line[3:].split(",")[:2]
                lines[number] = max(lines.get(number, 0), int(count))

    if args.lcov:
        with open(args.lcov, "w", encoding="utf-8") as f:
            f.write("\n".join(kept) + "\n")

    if not hits:
        sys.exit(f"{args.report}: no records for crates/*/src/")

    rows = []
    total = covered = 0
    for source in sorted(hits):
        lines = hits[source].values()
        if not lines:
            continue
        n, c = len(lines), sum(1 for count in lines if count > 0)
        total += n
        covered += c
        rows.append(f"| `{source}` | {c} / {n} | {100 * c / n:.1f}% |")

    print("## Line coverage\n")
    print(f"**{100 * covered / total:.1f}%** of {total} lines in `crates/*/src/` ", end="")
    print("(Rust unit, integration and doc tests; generated REAPI code left out).\n")
    print("<details><summary>By file</summary>\n")
    print("| File | Lines hit | Coverage |")
    print("| --- | ---: | ---: |")
    print("\n".join(rows))
    print("\n</details>")


if __name__ == "__main__":
    main()
