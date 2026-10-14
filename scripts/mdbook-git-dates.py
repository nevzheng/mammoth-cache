#!/usr/bin/env python3
"""mdBook preprocessor: puts "Created <date> · Last updated <date>" under each page's title.

Dates come from git history (`git log --follow`), so the build needs the full history; CI checks out
with `fetch-depth: 0`. A page with no commits yet shows today's date.
"""

import datetime
import json
import subprocess
import sys


def git_dates(path):
    out = subprocess.run(
        ["git", "log", "--follow", "--format=%as", "--", path],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.split()
    today = datetime.date.today().isoformat()
    return (out[-1], out[0]) if out else (today, today)


def stamp(chapter, src):
    if not chapter.get("path"):
        return
    created, updated = git_dates(f"{src}/{chapter['path']}")
    line = f"_Created {created} · Last updated {updated}_"
    lines = chapter["content"].split("\n")
    title = next((i for i, l in enumerate(lines) if l.startswith("# ")), None)
    if title is None:
        lines[:0] = [line, ""]
    else:
        lines[title + 1 : title + 1] = ["", line]
    chapter["content"] = "\n".join(lines)


def walk(items, src):
    for item in items:
        chapter = item.get("Chapter") if isinstance(item, dict) else None
        if chapter:
            stamp(chapter, src)
            walk(chapter.get("sub_items", []), src)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "supports":
        sys.exit(0)
    context, book = json.load(sys.stdin)
    src = context["config"]["book"].get("src", "src")
    walk(book.get("items", book.get("sections", [])), src)
    json.dump(book, sys.stdout)


if __name__ == "__main__":
    main()
