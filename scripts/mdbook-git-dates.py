#!/usr/bin/env python3
"""mdBook preprocessor: puts "Created <date> · Last updated <date>" under each page's title.

Dates come from git history (`git log --follow`), so the build needs the full history; CI checks out
with `fetch-depth: 0`. A page with no commits yet shows today's date.
"""

import datetime
import json
import os
import subprocess
import sys


def git_dates(root, path):
    out = subprocess.run(
        ["git", "log", "--follow", "--format=%as", "--", path],
        capture_output=True,
        text=True,
        check=False,
        cwd=root,
    ).stdout.split()
    today = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
    return (out[-1], out[0]) if out else (today, today)


def stamp(chapter, root, src):
    if not chapter.get("path"):
        return
    created, updated = git_dates(root, os.path.join(src, chapter["path"]))
    line = f"_Created {created} · Last updated {updated}_"
    lines = chapter["content"].split("\n")
    title = next((i for i, l in enumerate(lines) if l.startswith("# ")), None)
    if title is None:
        lines[:0] = [line, ""]
    else:
        lines[title + 1 : title + 1] = ["", line]
    chapter["content"] = "\n".join(lines)


def walk(items, root, src):
    for item in items:
        chapter = item.get("Chapter") if isinstance(item, dict) else None
        if chapter:
            stamp(chapter, root, src)
            walk(chapter.get("sub_items", []), root, src)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "supports":
        sys.exit(0)
    context, book = json.load(sys.stdin)
    src = context["config"]["book"].get("src", "src")
    walk(book.get("items", book.get("sections", [])), context["root"], src)
    json.dump(book, sys.stdout)


if __name__ == "__main__":
    main()
