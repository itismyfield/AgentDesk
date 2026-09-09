#!/usr/bin/env python3
"""Derive the `cross_os_rust` selectors for the cfg-shimmed relay tree (#5832).

`cross_os_rust` in `.github/workflows/ci-pr.yml` deliberately does not select
`src/services/discord/**` wholesale, so that subtree needs an explicit list.
Enumerating it by hand is what left the #5828 file class unselected.

A *consumer* is a file the Windows target compiles that carries a platform
`cfg` (`unix` / `windows`) of its own, or names a module declared behind a
`#[cfg(unix)]` gate. Only such a file can reproduce #5828: drop or mis-cfg the
shim and the native Windows lane breaks on main. `--format globs` collapses a
directory into `dir/**` only when at least half of the Rust files below it are
Windows-compiled, so unix-only trees never join the required lane with nothing
to verify. Deterministic (sorted); stdlib only.
"""

from __future__ import annotations

import argparse
import os
import re
from pathlib import Path, PurePosixPath

MOD = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_]\w*)[ \t]*;")
INLINE_MOD = re.compile(r"^([ \t]*)(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_]\w*)[ \t]*\{")
PATH_ATTR = re.compile(r"#\[path[ \t]*=[ \t]*\"([^\"]+)\"\]")
PLATFORM_CFG = re.compile(r"cfg(?:_attr)?!?[ \t]*\([^)]*\b(?:unix|windows|target_os|target_family)\b")
NON_WINDOWS = re.compile(r"\bunix\b|not[ \t]*\([ \t]*windows")
NOT_UNIX = re.compile(r"not[ \t]*\([ \t]*unix")
CRATE_ROOTS = ("src/lib.rs", "src/main.rs")
DEFAULT_SCOPE = "src/services/discord"


def _gate_above(lines: list[str], index: int) -> tuple[bool, str | None]:
    """Read the attribute run above a `mod` line: (is unix-gated, #[path] value)."""
    attrs: list[str] = []
    for line in reversed(lines[:index]):
        stripped = line.strip()
        if stripped.startswith("#["):
            attrs.append(stripped)
        elif stripped and not stripped.startswith("//"):
            break
    blob = " ".join(attrs)
    # A positive `target_os` list that never names windows is equally unix-only.
    named = "target_os" in blob and "not" not in blob and "windows" not in blob
    gated = "cfg" in blob and not NOT_UNIX.search(blob) and (bool(NON_WINDOWS.search(blob)) or named)
    path_attr = PATH_ATTR.search(blob)
    return gated, path_attr.group(1) if path_attr else None


def _child(src: Path, name: str, attr: str | None, chain: tuple[str, ...], owned: bool) -> Path | None:
    """Resolve one `mod` declaration the way rustc's directory ownership does.

    A file reached through `#[path]` owns its own directory (rustc's `relative:
    None`), every other non-`mod.rs` file owns `dir/<stem>/`, and each enclosing
    inline `mod NAME {` block appends a further directory -- including for a
    nested `#[path]`, which is what hid `voice_barge_in/tests/` (#5834 r3 P1-1).
    """
    root = src.parent if owned or src.stem in {"mod", "lib", "main"} else src.parent / src.stem
    base = root.joinpath(*chain)
    if attr:
        return Path(os.path.normpath((base if chain else src.parent) / attr))
    return next((c for c in (base / f"{name}.rs", base / name / "mod.rs") if c.exists()), None)


def scan(root: Path) -> tuple[dict[str, bool], set[str]]:
    """Return {rust file: compiled on Windows} plus the unix-gated module names."""
    queue = [root / rel for rel in CRATE_ROOTS if (root / rel).exists()]
    windows: dict[Path, bool] = dict.fromkeys(queue, True)
    gated: set[str] = set()
    ungated: set[str] = set()
    seen: set[Path] = set()
    owned: set[Path] = set()
    while queue:
        src = queue.pop()
        if src in seen:
            continue
        seen.add(src)
        lines = src.read_text(encoding="utf-8", errors="replace").splitlines()
        stack: list[tuple[str, str]] = []
        for index, line in enumerate(lines):
            # rustfmt closes an inline block at the opener's own indentation.
            while stack and line.startswith(f"{stack[-1][0]}}}"):
                stack.pop()
            opened = INLINE_MOD.match(line)
            if opened is not None:
                stack.append((opened.group(1), opened.group(2)))
            declared = MOD.match(line)
            if declared is None:
                continue
            gate, path_attr = _gate_above(lines, index)
            (gated if gate else ungated).add(declared.group(1))
            chain = tuple(name for _, name in stack)
            child = _child(src, declared.group(1), path_attr, chain, src in owned)
            if child is None or not child.exists():
                continue
            if path_attr:
                owned.add(child)
            windows[child] = windows.get(child, False) or (windows[src] and not gate)
            queue.append(child)
    return {p.relative_to(root).as_posix(): v for p, v in windows.items()}, gated - ungated


def consumers(root: Path, compiled: dict[str, bool], gated: set[str], scope: str) -> list[str]:
    refs = [re.compile(rf"\b{name}[ \t]*::") for name in sorted(gated)]
    found = []
    for rel in sorted(compiled):
        if not compiled[rel] or not rel.startswith(f"{scope}/"):
            continue
        text = (root / rel).read_text(encoding="utf-8", errors="replace")
        if PLATFORM_CFG.search(text) or any(ref.search(text) for ref in refs):
            found.append(rel)
    return found


def globs(compiled: dict[str, bool], scope: str, paths: list[str]) -> list[str]:
    def majority(directory: str) -> bool:
        below = [v for rel, v in compiled.items() if rel.startswith(f"{directory}/")]
        return bool(below) and sum(below) * 2 >= len(below)

    selected = set()
    for rel in paths:
        best, current = None, str(PurePosixPath(rel).parent)
        while current.startswith(f"{scope}/"):
            best = current if majority(current) else best
            current = str(PurePosixPath(current).parent)
        selected.add(f"{best}/**" if best else rel)
    return sorted(selected)


def unreachable(root: Path, compiled: dict[str, bool], scope: str) -> list[str]:
    """Rust files under `scope` the module walk never reached -- the walk's own
    blind spots, pinned against a justified exception table by #5834 r3."""
    walked = {rel for rel in compiled if rel.startswith(f"{scope}/")}
    below = ((p, p.relative_to(root).as_posix()) for p in (root / scope).rglob("*.rs"))
    return sorted(rel for p, rel in below if p.is_file() and rel not in walked)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--scope", default=DEFAULT_SCOPE)
    parser.add_argument(
        "--format", choices=("paths", "globs", "unreachable"), default="paths"
    )
    args = parser.parse_args()
    scope = args.scope.rstrip("/")
    compiled, gated = scan(args.root)
    if args.format == "unreachable":
        print("\n".join(unreachable(args.root, compiled, scope)))
        return 0
    found = consumers(args.root, compiled, gated, scope)
    print("\n".join(found if args.format == "paths" else globs(compiled, scope, found)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
