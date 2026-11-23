#!/usr/bin/env python3
"""Non-authoritative per-test V8 edge union prototype. Never enables Rust skips.

Reports must come from isolated successful native runs, one exact file/name
identity each. This transformer cannot establish that provenance or completeness.
Its output deliberately carries no runtime-authority claim.
"""
import json
import runpy
import sys
from pathlib import Path

EXTRACTOR = Path(__file__).resolve().parents[1] / "scratch/optA/vitest/extract_v8.py"


def identity(row):
    if not isinstance(row, dict) or set(row) != {"file", "name"}:
        raise ValueError("exact file/name identity required")
    if any(not isinstance(row[key], str) or not row[key] for key in row):
        raise ValueError("empty identity")
    return row["file"], row["name"]


def union(root, inventory, static, observations):
    """Every report file, even zero-hit, is additive to static file edges."""
    expected = {identity(row) for row in inventory}
    if not expected or len(expected) != len(inventory):
        raise ValueError("empty or duplicate inventory")
    extract = runpy.run_path(str(EXTRACTOR))["extract"]
    merged = {}
    for row in static:
        key = identity(row["test"])
        if key not in expected or key in merged:
            raise ValueError("unknown or duplicate static identity")
        if not isinstance(row["files"], list) or any(not isinstance(f, str) for f in row["files"]):
            raise ValueError("invalid static edges")
        merged[key] = set(row["files"])
    if set(merged) != expected:
        raise ValueError("incomplete static inventory")
    seen = set()
    for row in observations:
        key = identity(row["test"])
        if key not in expected or key in seen:
            raise ValueError("unknown or duplicate coverage identity")
        seen.add(key)
        # No trust is inferred from caller-controlled provenance strings.
        files = extract(root, {"isolated": row["report"]})["isolated"]
        merged[key].update(files)
    if seen != expected:
        raise ValueError("missing per-test observation: caller must run full")
    return {"schema": "kioku/vitest/v8-prototype/v1", "authority": "unverified-prototype",
            "entries": [{"test": {"file": file, "name": name}, "files": sorted(files)}
                        for (file, name), files in sorted(merged.items())]}


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3:
            raise ValueError("usage: vitest_v8_edges.py ROOT INPUT.json")
        payload = json.loads(Path(sys.argv[2]).read_text())
        print(json.dumps(union(sys.argv[1], **payload), sort_keys=True))
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"V8 prototype failed (full required): {error}", file=sys.stderr)
        sys.exit(1)
