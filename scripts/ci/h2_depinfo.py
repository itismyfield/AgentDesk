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

def parse_depinfo(text: str, depinfo: Path) -> list[tuple[str, set[str]]]:
    """(target, prerequisites) of every Makefile rule; `\\ ` unescapes to a space, `#` lines are skipped."""
    rules = []
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        target, sep, rest = line.partition(": ")
        if not sep and line.endswith(":"):
            target, rest = line[:-1], ""  # an empty per-file rule
        elif not sep:
            raise m.MeasureError(f"root lib dep-info {depinfo} has an unsupported line {line[:120]!r}")
        split = re.split(r"(?<!\\)\s+", rest.strip())
        rules.append((target.replace("\\ ", " "), {token.replace("\\ ", " ") for token in split if token}))
    return rules

def depinfo_inputs(root: Path, depinfo: Path) -> set[str]:
    """Every prerequisite of a readable dep-info whose own compile rule reads src/lib.rs."""
    try:
        text = depinfo.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise m.MeasureError(f"cannot read root lib dep-info {depinfo}: {exc}") from exc
    rules = parse_depinfo(text, depinfo)
    # rustc names the .d and the .rmeta/.rlib it emits by absolute path; match by file name
    own = {depinfo.name, f"lib{depinfo.stem}.rmeta", f"lib{depinfo.stem}.rlib"}
    lib = os.path.realpath(root / "src/lib.rs")
    if not any(Path(target).name in own and lib in {os.path.realpath(root / dep) for dep in deps} for target, deps in rules):
        raise m.MeasureError(f"root lib dep-info {depinfo} has no compile rule reading src/lib.rs")
    return set().union(*(deps for _, deps in rules))

def classify(root: Path, deps) -> tuple[dict[str, str], set[str]]:
    """({repo-relative realpath: path as written, `..` collapsed}, realpaths outside the repo)."""
    real_root = Path(os.path.realpath(root))
    inside, outside = {}, set()
    for dep in sorted(deps):
        path = Path(os.path.realpath(root / dep))  # an absolute dep replaces root
        try:
            rel = path.relative_to(real_root).as_posix()
        except ValueError:
            outside.add(path.as_posix())
            continue
        written = Path(os.path.normpath(root / dep))
        inside.setdefault(rel, next((written.relative_to(r).as_posix() for r in (root, real_root)
                                     if written.is_relative_to(r)), rel))
    return inside, outside

def canonical_modules(root: Path) -> set[str]:
    """The module tree as repo-relative realpaths, so a symlinked module matches the file rustc read."""
    real_root = Path(os.path.realpath(root))
    paths = (Path(os.path.realpath(root / rel)) for rel in m._module_table(root))
    return {path.relative_to(real_root).as_posix() for path in paths if path.is_relative_to(real_root)}

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
    """R-O over the lib compile inputs and duplicate_mod; a missing, unreadable or invalid dep-info is itself a problem."""
    problems = duplicate_mod_problems(lines)
    try:
        inside, outside = classify(root, depinfo_inputs(root, root_lib_depinfo(root, lines)))
    except m.MeasureError as exc:
        return problems + [f"R-O: {exc}"]
    modules = canonical_modules(root)
    problems += [f"R-O: lib compile input {path} is outside the repo" for path in sorted(outside)]
    for rel, written in sorted(inside.items()):
        # `.rs` must be a module; anything else must be data even when `#[path]` mounts it
        if rel.endswith(".rs") and rel not in modules:
            problems.append(f"R-O: {written} is compiled into the lib but is not in the module tree")
        elif not rel.endswith(".rs") and not DATA_INPUT_RE.fullmatch(rel):
            problems.append(f"R-O: lib compile input {written} is not in the data allowlist")
    return problems
