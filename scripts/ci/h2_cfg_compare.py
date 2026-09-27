#!/usr/bin/env python3
"""Compare structured compiler cfg snapshots for H2 lanes without interpreting Rust source."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


Cfg = tuple[str, ...]


def read_cfg(path: Path, *, data: bytes | None = None) -> set[Cfg]:
    """Read JSON atoms [name] or [name, value]; rustc's unescaped text output is not an input format."""
    try:
        snapshot = json.loads((path.read_bytes() if data is None else data).decode("utf-8"))
    except json.JSONDecodeError as exc:
        raise ValueError(f"{path}:{exc.lineno}: expected structured cfg JSON: {exc.msg}") from exc
    if isinstance(snapshot, dict) and snapshot.get("schema") == 1:
        if not all(isinstance(snapshot.get(key), str) and len(snapshot[key]) == 32 for key in ("run_id", "nonce")):
            raise ValueError(f"{path}: invalid cfg run identity")
        snapshot = snapshot.get("atoms")
    if not isinstance(snapshot, list) or not snapshot:
        raise ValueError(f"{path}: expected a nonempty cfg array")
    atoms = set()
    for number, atom in enumerate(snapshot, 1):
        if (not isinstance(atom, list) or len(atom) not in (1, 2)
                or not all(isinstance(field, str) for field in atom) or not atom[0].isidentifier()):
            raise ValueError(f"{path}: atom {number}: expected [name] or [name, value] with an identifier name")
        for field in atom:
            field.encode("utf-8")  # JSON can contain lone surrogate escapes; Rust strings cannot.
        atoms.add(tuple(atom))
    return atoms


def compare_cfgs(linux: set[Cfg], macos: set[Cfg]) -> dict[str, list[Cfg]]:
    """Report shared and lane-only atoms; no target, feature or custom cfg is exempted."""
    return {
        "common": sorted(linux & macos),
        "linux_only": sorted(linux - macos),
        "macos_only": sorted(macos - linux),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, epilog="Raw --print cfg is ambiguous; supply structured JSON snapshots.",
    )
    parser.add_argument("--linux", type=Path, required=True, help="Linux structured cfg JSON snapshot")
    parser.add_argument("--macos", type=Path, required=True, help="macOS structured cfg JSON snapshot")
    args = parser.parse_args(argv)
    snapshots = {}
    for lane in ("linux", "macos"):
        try:
            snapshots[lane] = read_cfg(getattr(args, lane))
        except (OSError, UnicodeError, ValueError, RecursionError, MemoryError) as exc:
            print(f"h2-cfg-compare: {lane}: {type(exc).__name__}: {exc}", file=sys.stderr)
            return 2
    try:
        report = compare_cfgs(snapshots["linux"], snapshots["macos"])
        print(json.dumps(report, ensure_ascii=True, indent=2))
    except MemoryError:
        print("h2-cfg-compare: cannot report cfg comparison: MemoryError", file=sys.stderr)
        return 2
    return 1 if report["linux_only"] or report["macos_only"] else 0


if __name__ == "__main__":
    sys.exit(main())
