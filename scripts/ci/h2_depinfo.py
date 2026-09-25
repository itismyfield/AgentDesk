#!/usr/bin/env python3
"""H2 R-O compile-input check: the root lib's rustc dep-info against the module tree.

Every `.rs` rustc read for the lib must be a module the text walker knows, every other
input must be allowlisted data, and `clippy::duplicate_mod` (one file mounted twice) must not fire.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path

import h2_measure as m

# Non-Rust lib inputs rustc may read (sqlx::migrate!, include_str!, cargo manifest, clippy config).
# `*` stays within one path segment.
DATA_INPUTS = ("migrations/postgres/*.sql", "Cargo.toml", "clippy.toml", "defaults.json", "assets/runner-entry.html")
DATA_INPUT_RE = re.compile("|".join(re.escape(p).replace(r"\*", "[^/]*") for p in DATA_INPUTS))
ARTIFACT_RE = re.compile(r"lib(\w+)-([0-9a-f]+)\.(?:rmeta|rlib)")

def root_lib_depinfo(root: Path, lines) -> Path:
    """The `deps/<crate>-<hash>.d` whose hash matches the root lib artifact of this clippy run."""
    found = set()
    for raw in (line.strip() for line in lines):
        if not raw.startswith("{"):
            continue
        event = json.loads(raw)
        target = event.get("target") or {}
        if (event.get("reason") != "compiler-artifact" or target.get("kind") != ["lib"]
                or (event.get("profile") or {}).get("test") or target.get("name") != m.CRATE
                or Path(target.get("src_path", "")).resolve() != (root / "src/lib.rs").resolve()):
            continue
        for filename in event.get("filenames", []):
            if match := ARTIFACT_RE.fullmatch(Path(filename).name):
                found.add(Path(filename).parent / f"{match.group(1)}-{match.group(2)}.d")
    if len(found) != 1:
        raise m.MeasureError(f"expected one root lib artifact dep-info, found {sorted(map(str, found))}")
    depinfo = found.pop()
    if not depinfo.is_file():
        raise m.MeasureError(f"root lib dep-info {depinfo} does not exist")
    return depinfo

def parse_depinfo(text: str) -> set[str]:
    """Every prerequisite of every Makefile rule; `\\ ` unescapes to a space, `#` lines are skipped."""
    deps = set()
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        target, sep, rest = line.partition(": ")
        if not sep and line.endswith(":"):
            continue  # an empty per-file rule
        deps.update(token.replace("\\ ", " ") for token in re.split(r"(?<!\\)\s+", rest.strip()) if token)
    return deps

def classify(root: Path, deps) -> tuple[set[str], set[str]]:
    """(repo-relative posix paths, absolute paths outside the repo), symlinks and `..` resolved."""
    real_root = Path(os.path.realpath(root))
    inside, outside = set(), set()
    for dep in deps:
        path = Path(os.path.realpath(root / dep))  # an absolute dep replaces root
        try:
            inside.add(path.relative_to(real_root).as_posix())
        except ValueError:
            outside.add(path.as_posix())
    return inside, outside

def duplicate_mod_problems(lines) -> list[str]:
    problems = []
    for raw in (line.strip() for line in lines):
        if not raw.startswith("{"):
            continue
        event = json.loads(raw)
        message = event.get("message") or {}
        if (message.get("code") or {}).get("code") in m.RO_LINTS and "lib" in (event.get("target") or {}).get("kind", []):
            spans = sorted({f"{s['file_name']}:{s['line_start']}" for s in message.get("spans", [])})
            problems.append(f"R-O: clippy::duplicate_mod: one file is mounted as several modules ({', '.join(spans)})")
    return problems

def ro_problems(root: Path, lines) -> list[str]:
    """R-O over the lib compile inputs and duplicate_mod; a missing or ambiguous dep-info is itself a problem."""
    problems = duplicate_mod_problems(lines)
    try:
        depinfo = root_lib_depinfo(root, lines)
    except m.MeasureError as exc:
        return problems + [f"R-O: {exc}"]
    inside, outside = classify(root, parse_depinfo(depinfo.read_text(encoding="utf-8")))
    modules = m._module_table(root)
    problems += [f"R-O: lib compile input {path} is outside the repo" for path in sorted(outside)]
    problems += [f"R-O: {rel} is compiled into the lib but is not in the module tree" if rel.endswith(".rs")
                 else f"R-O: lib compile input {rel} is not in the data allowlist"
                 for rel in sorted(inside) if rel not in modules and not DATA_INPUT_RE.fullmatch(rel)]
    return problems
