#!/usr/bin/env python3
"""Fail when a relative link in a directory tree has no target.

Used by scripts/package.sh on the staging tree, before archiving: a release
archive must be self-contained, so every relative link in the documents it
ships must resolve inside it. Usage:

    python3 scripts/check-archive-links.py STAGE_DIR

Exit 0 when every link resolves, 1 with one line per broken link otherwise.
"""
import re
import sys
from pathlib import Path
from urllib.parse import unquote

MARKDOWN = re.compile(r"!?\[[^\]]*\]\(\s*([^)\s]+)")
ATTRIBUTE = re.compile(r'''\b(?:src|srcset|href)\s*=\s*(?:"([^"]*)"|'([^']*)')''')
SCHEME = re.compile(r"[A-Za-z][A-Za-z0-9+.-]*:")


def relative_target(link):
    """Return the repo-relative path a link points at, or None when it is external."""
    link = unquote(link.split("#", 1)[0].split("?", 1)[0])
    if not link or link.startswith(("/", "#")) or SCHEME.match(link):
        return None
    return link


def links_on_line(line):
    for match in MARKDOWN.finditer(line):
        yield match.group(1)
    for match in ATTRIBUTE.finditer(line):
        value = match.group(1) if match.group(1) is not None else match.group(2)
        # srcset is a comma separated list of URLs, each with an optional descriptor.
        yield from (part.strip().split(" ", 1)[0] for part in value.split(","))


def broken_links(path, archive):
    for number, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
        for link in links_on_line(line):
            target = relative_target(link)
            if target is None:
                continue
            candidate = path.parent / target
            # A target that resolves outside the archive is as broken as a missing one.
            if not candidate.exists() or not candidate.resolve().is_relative_to(archive):
                yield number, target


def main(argv):
    if len(argv) != 2:
        print("usage: check-archive-links.py DIR", file=sys.stderr)
        return 2
    directory = Path(argv[1])
    if not directory.is_dir():
        print(f"{directory}: not a directory", file=sys.stderr)
        return 2
    archive = directory.resolve()
    broken = 0
    for path in sorted(directory.rglob("*.md")):
        for number, target in broken_links(path, archive):
            where = path.relative_to(directory).as_posix()
            print(f"{where}:{number}: cible introuvable: {target}")
            broken += 1
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
