#!/usr/bin/env python3
"""H2 R-O compile-input check: the root lib's rustc dep-info against the module tree.

An input spelled or resolving to `.rs` must be a module file the text walker opened under the lib build's
cfg, any other must resolve to allowlisted data, every such module file must be an input (the walker's
self-check), and `clippy::duplicate_mod` (one file mounted twice) must not fire.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path

import h2_measure as m

# Non-Rust lib inputs rustc may read (sqlx::migrate!, include_str!, cargo manifest, clippy config).
# `*` stays within one path segment.
DATA_INPUTS = ("migrations/postgres/*.sql", "Cargo.toml", "clippy.toml", "defaults.json", "assets/runner-entry.html")
DATA_INPUT_RE = re.compile("|".join(re.escape(p).replace(r"\*", "[^/]*") for p in DATA_INPUTS))
ARTIFACT_RE = re.compile(r"lib(\w+)-([0-9a-f]+)\.(?:rmeta|rlib)")
# cfg names rustc knows without `--check-cfg` additions; any other name cannot be decided here
WELL_KNOWN_CFG = frozenset({"test", "doc", "doctest", "miri", "proc_macro", "feature", "clippy", "debug_assertions",
                            "unix", "windows", "panic", "overflow_checks", "ub_checks"})

def rustc_cfg(root: Path) -> str:
    """`rustc --print cfg` of the host, run in `root` so its rust-toolchain applies."""
    try:
        proc = subprocess.run(["rustc", "--print", "cfg"], cwd=root, capture_output=True, text=True, check=False)
    except OSError as exc:
        raise m.MeasureError(f"cannot run rustc --print cfg: {exc}") from exc
    if proc.returncode != 0:
        raise m.MeasureError(f"rustc --print cfg failed ({proc.returncode}): {proc.stderr[-2000:]}")
    return proc.stdout

def lib_cfg(root: Path, features) -> frozenset:
    """The cfg set of this run's lib build, the one source for it: host cfg, `clippy`, the artifact's features, no `test`."""
    cfg = {("clippy", None), *(("feature", name) for name in features)}
    for line in rustc_cfg(root).splitlines():
        name, sep, value = line.strip().partition("=")
        if name:
            cfg.add((name, value.strip('"') if sep else None))
    return frozenset(cfg)

def cfg_holds(cfg: frozenset, tokens) -> bool:
    """Value of a `( predicate )` token list under `cfg`; raises CfgError for an unknown name or bad syntax."""
    tokens, pos = list(tokens), 0

    def take(expected: str | None = None) -> str:
        nonlocal pos
        if pos >= len(tokens) or (expected and tokens[pos] != expected):
            raise m.CfgError(f"expected {expected or 'a token'} in {' '.join(tokens)}")
        pos += 1
        return tokens[pos - 1]

    def predicate() -> bool:
        name = take()
        if name in ("all", "any", "not") and tokens[pos:pos + 1] == ["("]:
            take("(")
            values = []  # every argument is evaluated, so an unknown name fails even after a decided one
            while tokens[pos:pos + 1] != [")"]:
                values.append(predicate())
                if tokens[pos:pos + 1] != [")"]:
                    take(",")
            take(")")
            if name == "not" and len(values) != 1:
                raise m.CfgError(f"not() takes one predicate in {' '.join(tokens)}")
            return all(values) if name == "all" else any(values) if name == "any" else not values[0]
        if name in ("true", "false"):
            return name == "true"
        known = {key for key, _ in cfg}
        if not re.fullmatch(r"[A-Za-z_]\w*", name) or not (name in WELL_KNOWN_CFG | known or name.startswith("target_")):
            raise m.CfgError(f"unknown cfg {name!r} in {' '.join(tokens)}")
        if tokens[pos:pos + 1] != ["="]:
            return (name, None) in cfg
        take("=")
        value = take()
        if not re.fullmatch(r'"[^"\\]*"', value):
            raise m.CfgError(f"cfg {name} needs a plain string value in {' '.join(tokens)}")
        return (name, value[1:-1]) in cfg

    take("(")
    value = predicate()
    take(")")
    if pos != len(tokens):
        raise m.CfgError(f"trailing tokens in {' '.join(tokens)}")
    return value

def root_lib_depinfo(root: Path, lines) -> tuple[Path, list[str]]:
    """(the `deps/<crate>-<hash>.d` whose hash matches the root lib artifact of this clippy run, its features)."""
    found = {}
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
                found[Path(filename).parent / f"{match.group(1)}-{match.group(2)}.d"] = event.get("features") or []
    if len(found) != 1:
        raise m.MeasureError(f"expected one root lib artifact dep-info, found {sorted(map(str, found))}")
    depinfo, features = found.popitem()
    if not depinfo.is_file():
        raise m.MeasureError(f"root lib dep-info {depinfo} does not exist")
    return depinfo, features

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
    """Every prerequisite of a dep-info whose own compile rule reads src/lib.rs."""
    rules = parse_depinfo(depinfo.read_text(encoding="utf-8"), depinfo)
    # rustc names the .d and the .rmeta/.rlib it emits by absolute path; match by file name
    own = {depinfo.name, f"lib{depinfo.stem}.rmeta", f"lib{depinfo.stem}.rlib"}
    lib = os.path.realpath(root / "src/lib.rs")
    if not any(Path(target).name in own and lib in {os.path.realpath(root / dep) for dep in deps} for target, deps in rules):
        raise m.MeasureError(f"root lib dep-info {depinfo} has no compile rule reading src/lib.rs")
    return set().union(*(deps for _, deps in rules))

def classify(root: Path, deps) -> tuple[list[tuple[str, str]], set[str]]:
    """([(path as written, repo-relative realpath)] per input, realpaths outside the repo).
    Aliases of one file stay separate entries so each spelling keeps its own rule."""
    real_root = Path(os.path.realpath(root))
    inside, outside = [], set()
    for dep in sorted(deps):
        path = Path(os.path.realpath(root / dep))  # an absolute dep replaces root
        try:
            rel = path.relative_to(real_root).as_posix()
        except ValueError:
            outside.add(path.as_posix())
            continue
        written = root / dep
        inside.append((next((written.relative_to(r).as_posix() for r in (root, real_root)
                             if written.is_relative_to(r)), dep), rel))
    return inside, outside

def canonical_modules(root: Path, cfg: frozenset) -> tuple[set[str], list[str]]:
    """(module files active under `cfg` as repo-relative realpaths, so a symlinked module matches the file rustc read;
    cfg problems)."""
    real_root = Path(os.path.realpath(root))
    _, opened, problems = m._module_walk(root, lambda tokens: cfg_holds(cfg, tokens))
    paths = (Path(os.path.realpath(path)) for path in opened)
    return {path.relative_to(real_root).as_posix() for path in paths if path.is_relative_to(real_root)}, problems

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
    try:  # selecting, reading and parsing the .d share one error boundary
        depinfo, features = root_lib_depinfo(root, lines)
        inside, outside = classify(root, depinfo_inputs(root, depinfo))
    except (OSError, UnicodeError) as exc:
        return problems + [f"R-O: cannot read root lib dep-info: {exc}"]
    except m.MeasureError as exc:
        return problems + [f"R-O: {exc}"]
    try:
        modules, cfg_problems = canonical_modules(root, lib_cfg(root, features))
    except m.MeasureError as exc:
        return problems + [f"R-O: {exc}"]
    problems += [f"R-O: {problem}" for problem in cfg_problems]
    problems += [f"R-O: lib compile input {path} is outside the repo" for path in sorted(outside)]
    # the walker's self-check: a module it calls active that rustc never read means its cfg view is wrong
    problems += [f"R-O: cfg evaluation puts {rel} in the lib build but rustc did not read it"
                 for rel in sorted(modules - {rel for _, rel in inside})]
    found = set()
    for written, rel in inside:
        # the written and the resolved extension each bring their rule, so an alias cannot trade one for the other;
        # `.rs` must be a module, anything else must be data even when `#[path]` mounts it
        rust = {written.endswith(".rs"), rel.endswith(".rs")}
        if True in rust and rel not in modules:
            found.add(f"R-O: {written} is compiled into the lib but is not in the module tree")
        if False in rust and not DATA_INPUT_RE.fullmatch(rel):
            found.add(f"R-O: lib compile input {written} is not in the data allowlist")
    return problems + sorted(found)
