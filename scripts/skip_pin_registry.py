#!/usr/bin/env python3
"""The writer-gate skip pin: legacy resolver paths plus `.d` fragments.

Rows are `basename | src/...rs` or `resolver | src/...rs`, sorted by path, in
`scripts/test_only_module_skip_pin.d/<node>-<issue>-<slice>.txt`. The split resolver
literal and old combined basename/resolver literals stay readable in reviewed order;
they are checked for literals, path syntax and duplicates, never for order. Basename
rows remain readable for compatibility; new syncs register only resolver exclusions.
"""

from __future__ import annotations

import ast
import hashlib
import importlib.util
import sys
from dataclasses import dataclass
from pathlib import Path

LEGACY_REL = Path("scripts/test_only_module_skip_pin.py")
LEGACY_PATHS_REL = Path("scripts/test_only_module_skip_pin_paths.py")
FRAGMENTS_REL = Path("scripts/test_only_module_skip_pin.d")
KINDS = {"basename": "PINNED_BASENAME_TEST_FILES", "resolver": "PINNED_RESOLVER_TEST_ONLY_FILES"}
ROW_HINT = "expected `basename | src/...rs` or `resolver | src/...rs`"


def _load_fragments():
    name = "registration_fragments"
    if name not in sys.modules:
        spec = importlib.util.spec_from_file_location(
            name, Path(__file__).resolve().with_name("registration_fragments.py")
        )
        if spec is None or spec.loader is None:
            raise RuntimeError("cannot load scripts/registration_fragments.py")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    return sys.modules[name]


FRAGMENTS = _load_fragments()


def path_problem(path: str) -> str | None:
    parts = path.split("/")
    if (
        parts[0] != "src"
        or not path.endswith(".rs")
        or any(part in ("", ".", "..") for part in parts)
        or any(char.isspace() or char in '|\\"' for char in path)
    ):
        return f"`{path}` is no normalized repo-relative src/**/*.rs path"
    return None


def legacy_rows(source: str, label: str):
    """Literal rows in file order; resolver-only layouts need no basename assignment."""
    try:
        tree = ast.parse(source, label)
    except SyntaxError as error:
        return [], [(f"{label}:{error.lineno}", f"does not parse: {error.msg}")]
    rows, problems = [], []
    assigned = {kind: 0 for kind in KINDS}
    for node in tree.body:
        target = node.targets[0] if isinstance(node, ast.Assign) and len(node.targets) == 1 else None
        kind = next((k for k, name in KINDS.items() if getattr(target, "id", None) == name), None)
        if kind is None:
            continue
        assigned[kind] += 1
        call = node.value
        literal = (
            call.args[0]
            if isinstance(call, ast.Call) and getattr(call.func, "id", None) == "frozenset"
            and len(call.args) == 1 and not call.keywords
            else None
        )
        if isinstance(literal, ast.Dict) and not literal.keys:
            continue
        if not isinstance(literal, ast.Set):
            problems.append((f"{label}:{node.lineno}", f"{KINDS[kind]} must stay `frozenset({{...}})`"))
            continue
        for element in literal.elts:
            location = f"{label}:{element.lineno}"
            if not (isinstance(element, ast.Constant) and isinstance(element.value, str)):
                problems.append((location, f"{KINDS[kind]} entries must be string literals"))
                continue
            if problem := path_problem(element.value):
                problems.append((location, problem))
            rows.append(FRAGMENTS.Row(element.value, kind, location))
    for kind, count in assigned.items():
        if count != 1 and not (kind == "basename" and count == 0):
            problems.append((label, f"{KINDS[kind]} must be assigned once, found {count}"))
    return rows, problems


def fragment_rows(lines):
    """Rows of fragment lines; each fragment must already be sorted by path."""
    rows, problems = [], []
    last: dict[str, str] = {}
    for line in lines:
        kind, _, path = line.text.partition(" | ")
        if kind not in KINDS or not path:
            problems.append((line.location, ROW_HINT))
            continue
        if problem := path_problem(path):
            problems.append((line.location, problem))
        file = line.location.rsplit(":", 1)[0]
        if file in last and last[file] >= path:
            problems.append((line.location, "rows must be unique and sorted by path in a fragment"))
        last[file] = path
        rows.append(FRAGMENTS.Row(path, kind, line.location))
    return rows, problems


@dataclass(frozen=True)
class SkipPin:
    rows: dict

    def frozensets(self) -> tuple[frozenset, frozenset]:
        return tuple(
            frozenset(path for path, row in self.rows.items() if row.payload == kind)
            for kind in KINDS
        )

    def legacy(self, path: str) -> bool:
        source = self.rows[path].location.rsplit(":", 1)[0]
        return source in (LEGACY_REL.as_posix(), LEGACY_PATHS_REL.as_posix())


def load(root: Path) -> SkipPin:
    """Validate every legacy and fragment row, index them by path, then build the pin."""
    legacy = LEGACY_PATHS_REL if (root / LEGACY_PATHS_REL).is_file() else LEGACY_REL
    rows, problems = legacy_rows((root / legacy).read_text("utf-8"), legacy.as_posix())
    lines, read_problems = FRAGMENTS.read_lines(root / FRAGMENTS_REL, FRAGMENTS_REL.as_posix())
    added, row_problems = fragment_rows(lines)
    indexed, index_problems = FRAGMENTS.index([*rows, *added])
    problems = [*problems, *read_problems, *row_problems, *index_problems]
    if problems:
        raise RuntimeError(
            "FAIL: writer-gate skip pin is invalid; no set was built.\n"
            + "\n".join(f"  {location}: {detail}" for location, detail in problems)
        )
    return SkipPin(indexed)


def fragment_file(name: str) -> str:
    file = name if name.endswith(".txt") else name + ".txt"
    if not FRAGMENTS.NAME.fullmatch(file):
        raise RuntimeError(f"fragment `{name}` must read {FRAGMENTS.NAME_HINT}")
    return file


def auto_fragment(path: str) -> str:
    """The fragment an unnamed sync writes for one source, unique per path across lanes."""
    return f"auto-0-{hashlib.sha256(path.encode()).hexdigest()[:12]}.txt"


def sync(root: Path, measured: dict, existing: set, promoted=(), fragment=None, *, deleted=()):
    """Write new resolver pins and drop explicitly promoted or deleted fragment rows.

    `measured` maps every test-only path to its kind. Refusals raise before any write. A
    legacy removal returns its manual deletion step and nothing is written, because this
    writer never rewrites legacy sources. Returns `(manual steps, written fragment paths)`."""
    target = fragment_file(fragment) if fragment else None
    pin = load(root)
    promoted = set(promoted)
    deleted = set(deleted)
    for rel in sorted(deleted):
        if (root / rel).exists() or (root / rel).is_symlink():
            raise RuntimeError("deletion path still exists: " + rel)
        if rel not in pin.rows:
            raise RuntimeError("deletion is not pinned: " + rel)
    for rel in sorted(promoted):
        if rel not in existing:
            raise RuntimeError("promotion path missing: " + rel)
        if rel in measured:
            raise RuntimeError("promotion remains test-only: " + rel)
        if rel not in pin.rows:
            raise RuntimeError("promotion is not pinned: " + rel)
    if set(pin.rows) - set(measured) - promoted - deleted:
        raise RuntimeError("refusing pin removal")
    removed = promoted | deleted
    manual = [
        f"{pin.rows[rel].location}: delete the legacy pin `{rel}` by hand; --sync-skips "
        f"never rewrites {pin.rows[rel].location.rsplit(':', 1)[0]}"
        for rel in sorted(removed) if pin.legacy(rel)
    ]
    if manual:
        return manual, []
    edits: dict[str, tuple[set, list]] = {}
    for rel in sorted(removed):
        file, number = pin.rows[rel].location.rsplit(":", 1)
        edits.setdefault(Path(file).name, (set(), []))[0].add(int(number))
    for rel in sorted(set(measured) - set(pin.rows)):
        if measured[rel] == "basename":
            continue
        edits.setdefault(target or auto_fragment(rel), (set(), []))[1].append(
            f"{measured[rel]} | {rel}"
        )
    written = []
    for name, (dropped, added) in sorted(edits.items()):
        _rewrite(root / FRAGMENTS_REL / name, dropped, added)
        written.append((FRAGMENTS_REL / name).as_posix())
    return [], written


def _rewrite(path: Path, dropped: set, added: list) -> None:
    """Drop rows by line number, insert rows in path order, and remove a file left rowless."""
    text = path.read_text("utf-8") if path.exists() else ""
    kept = [line for number, line in enumerate(text.split("\n")[:-1], 1) if number not in dropped]

    def key(line: str):
        return line.partition(" | ")[2] if line.strip() and not line.lstrip().startswith("#") else None

    for row in added:
        new = key(row)
        at = next((i for i, line in enumerate(kept) if (k := key(line)) and k > new), len(kept))
        kept.insert(at, row)
    if not any(key(line) for line in kept):
        path.unlink(missing_ok=True)
        return
    path.parent.mkdir(exist_ok=True)
    path.write_text("".join(line + "\n" for line in kept), "utf-8")
