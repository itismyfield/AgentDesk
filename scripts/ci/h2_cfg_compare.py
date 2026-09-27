#!/usr/bin/env python3
"""Compare compiler-emitted cfg lists for H2 lanes without interpreting Rust source."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


# Accept rustc's one-atom-per-line output; keep quoted values opaque, including escapes.
CFG_ATOM = re.compile(r'(?:[^\W\d]|_)\w*(?:="(?:[^"\\\x00-\x1f\x7f]|\\[^\x00-\x1f\x7f])*")?')


def read_cfg(path: Path) -> set[str]:
    """Read a nonempty UTF-8 compiler snapshot, ignoring blank lines, order and duplicates."""
    atoms = set()
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        atom = raw.strip()
        if not atom:
            continue
        if not CFG_ATOM.fullmatch(atom):
            raise ValueError(f"{path}:{number}: expected a compiler cfg atom, got {atom!r}")
        atoms.add(atom)
    if not atoms:
        raise ValueError(f"{path}: empty compiler cfg list")
    return atoms


def compare_cfgs(linux: set[str], macos: set[str]) -> dict[str, list[str]]:
    """Report shared and lane-only atoms; no target, feature or custom cfg is exempted."""
    return {
        "common": sorted(linux & macos),
        "linux_only": sorted(linux - macos),
        "macos_only": sorted(macos - linux),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux", type=Path, required=True, help="Linux compiler --print cfg output")
    parser.add_argument("--macos", type=Path, required=True, help="macOS compiler --print cfg output")
    args = parser.parse_args(argv)
    snapshots = {}
    for lane in ("linux", "macos"):
        try:
            snapshots[lane] = read_cfg(getattr(args, lane))
        except (OSError, UnicodeError, ValueError) as exc:
            print(f"h2-cfg-compare: {lane}: {exc}", file=sys.stderr)
            return 2
    report = compare_cfgs(snapshots["linux"], snapshots["macos"])
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 1 if report["linux_only"] or report["macos_only"] else 0


if __name__ == "__main__":
    sys.exit(main())
