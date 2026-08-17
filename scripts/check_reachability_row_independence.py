#!/usr/bin/env python3
"""I14 row-independence source lint for `health/reachability/**` (#5071 T4-B1).

4987 §-1.5 withdrew the claim that invariant I14 ("obligation production is
independent of the inflight row") is compiler-enforced: `InflightTurnState` is
`pub(in crate::services::discord)`, so the compiler accepts an import from the
reachability tree without complaint. The design chose a source gate instead, in
the genre of `check_contract_symbol_refs.py`, and this is it.

**This is a lint, not a type proof.** That sentence is in 4987 §-1.5, in the
reachability module's own docs, in the `relay_reachability` change surface, and
here — the four places a reader could form the opposite belief. Real enforcement
needs a crate boundary and is out of this series' scope.

Two halves, both required:

1. **Row independence.** No `inflight` path segment may appear in the tree,
   neither in a `use` item nor in a fully-qualified path expression, and no
   name that a `pub use` elsewhere under `src/**` launders out of the inflight
   module may appear either. Scanning all three is deliberate: a lint that
   reads only `use` lines is bypassed by writing
   `crate::services::discord::inflight::foo()` at the call site, and one that
   reads only the text of this tree is bypassed by re-exporting the row from a
   third file under a name that never says "inflight". For the same reason the
   scan set is the text the tree compiles rather than the files `rglob` finds:
   `include!("...")` is followed, transitively.
2. **Change-surface ownership.** `docs/agent-maintenance/change-surfaces.md`
   must carry the `relay_reachability` surface with the canonical globs that
   cover every file in the tree, the `scripts/relay_watchdog.py` companion-edit
   requirement of 4987 §2.4/§9.4, and the lint-not-type-proof statement. A tree
   with no owner entry is how the next slice adds a file nobody reviews as part
   of this surface.

WHAT THIS DOES NOT SEE. It is a lexical scan over neutralized Rust source, not
a Rust parser and not a resolver:

  * comments and string/char literals are blanked first, so this tree's own
    module docs (which discuss "the inflight row" in prose) are not violations
    — and, symmetrically, a path a macro assembles from string fragments is
    invisible;
  * the third-file re-export closure is by NAME, not by resolution. Every
    `pub use` under `src/**` is expanded leaf by leaf; a leaf whose path names
    `inflight` publishes a forbidden name (its alias, where it is renamed), a
    `*` leaf forbids the laundering module's own name instead because its
    exports cannot be enumerated, and the set is closed transitively so a
    second `pub use` hop under a third name does not escape it. Exactly one
    sub-resolution runs before that closure reads a leaf: a `self::<name>::…`
    head is rewritten through the same FILE's own `use` bindings — `use path as
    alias;` and the bare `use path::Name;` alike, chained to a fixpoint —
    because `use ...::inflight as rows;` plus `pub use self::rows::Row as R;`
    reaches the row without either item spelling `inflight` in one path. That
    rewrite is file-local by construction and stops there. What still escapes:
    a launderer outside `src/**`, one a macro generates, one that arrives
    through a glob import (`use x::*;` binds a set this scan cannot enumerate,
    so a `pub use` over a glob-bound name is not resolved) or a `#[path]`
    redirection, and a re-export this lexical `use`-tree expander misreads —
    this is a bounded lexical closure, not a name resolver. Symmetrically, a
    tree item that merely SHARES a laundered name is a false positive this
    gate accepts rather than a hole it leaves;
  * `#[path = "..."]` module redirections are NOT followed, so a `#[path]`
    aimed outside the tree is still invisible. Resolving one correctly depends
    on inline-module nesting and on whether the containing file is a `mod.rs`,
    which this lexical scan does not track; the tree's own `#[path]` uses point
    at files in its own directory, which the directory scan already owns.
    `include!("...")` IS followed — it is unconditionally relative to the
    including file's directory — and an argument that does not resolve to a
    file on disk is a violation rather than a silent gap;
  * future-slice markers are associated lexically: `not yet on disk` exempts
    only the concrete `src/**.rs` path immediately before it in the same bullet,
    before another concrete path appears;
  * it says nothing about runtime reachability, only about source text.

The neutralizer is `scripts/check_clippy_allow_ratchet.py`'s, not a second copy:
one Rust lexical pre-pass in this repo, with one set of edge cases. It reports
lexically unterminated files, and this gate FAILS CLOSED on them rather than
scanning text it could not lex.
"""

from __future__ import annotations

import argparse
import fnmatch
import importlib.util
import re
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# The tree this gate owns. The module root is a sibling FILE of the directory
# (`health.rs` + `health/`, this repo's non-`mod.rs` layout), so it is listed
# explicitly: scanning the directory alone would leave the root unguarded.
TREE_ROOT_FILE = "src/services/discord/health/reachability.rs"
TREE_DIR = "src/services/discord/health/reachability"

# Where a third file could re-export the row from. Only `src/**` can be imported
# by the tree, so a `pub use` under `tests/` cannot launder anything into it.
SRC_ROOT = "src"

# The module segment the whole gate is about.
INFLIGHT_MODULE = "inflight"

CHANGE_SURFACES_DOC = "docs/agent-maintenance/change-surfaces.md"
SURFACE_HEADING = "### `relay_reachability`"

# Globs the surface entry must declare so every file in the tree has an owner.
REQUIRED_SURFACE_GLOBS = (TREE_ROOT_FILE, f"{TREE_DIR}/**")

# Statements the surface entry must carry. Compared after whitespace and
# markdown emphasis are normalized away, so the doc may wrap and bold freely.
REQUIRED_SURFACE_MARKERS = (
    # 4987 §2.4/§9.4: the obligation rule has two implementations and they must
    # move together, or the second oracle is born.
    "scripts/relay_watchdog.py",
    # 4987 §-1.5: the downgrade must survive in the operator-facing doc too.
    "lint, not a type proof",
)

# A concrete `src/**.rs` path in the surface entry that is not yet on disk must
# say so in the same bullet, naming the slice that lands it.
FUTURE_SLICE_MARKER = re.compile(r"not yet on disk")

# The body is captured because the launder closure reads a file's own `use`
# bindings through the same expander it reads `pub use` items with; `segment_hits`
# still matches on group(0).
USE_ITEM = re.compile(r"\buse\b([^;]*);", re.DOTALL)
# A re-export: only a `pub` one publishes a name a third file can be reached
# through. `pub(crate)`/`pub(in path)` count — the tree is inside the crate.
REEXPORT_ITEM = re.compile(r"\bpub\b(?:\s*\([^)]*\))?\s*\buse\b([^;]*);", re.DOTALL)
# The same head, run over RAW text to skip files that cannot re-export anything
# before paying for a neutralization pass. A commented-out `pub use` survives
# this filter, which only costs a pass that then finds nothing.
REEXPORT_PREFILTER = re.compile(r"\bpub\b(?:\s*\([^)]*\))?\s*\buse\b")
LEAF_ALIAS = re.compile(
    r"^(?P<path>.*?)\s+as\s+(?P<alias>[A-Za-z_][A-Za-z0-9_]*)$", re.DOTALL
)
INCLUDE_CALL = re.compile(r"\binclude!\s*[(\[{]")
# Read off the RAW text, because the neutralizer blanks literal interiors. Its
# output is length-preserving, so a match offset in one indexes the other.
INCLUDE_ARGUMENT = re.compile(
    r'\s*(?:r(?P<hashes>#*)"(?P<raw>.*?)"(?P=hashes)|"(?P<plain>[^"\\\n]*)")',
    re.DOTALL,
)
SURFACE_PATH = re.compile(r"`(src/[^`]+\.rs)`")
SURFACE_GLOB = re.compile(r"`(src/[^`]+)`")


def _load_neutralizer():
    """Reuse the repo's single Rust lexical pre-pass rather than forking it."""

    name = "check_clippy_allow_ratchet"
    if name in sys.modules:
        return sys.modules[name].neutralize_source
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).resolve().parent / "check_clippy_allow_ratchet.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/check_clippy_allow_ratchet.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module.neutralize_source


neutralize_source = _load_neutralizer()


@dataclass(frozen=True)
class Violation:
    path: str
    line: int
    kind: str
    detail: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: {self.kind}: {self.detail}"


def tree_files(repo_root: Path) -> list[Path]:
    """Every `.rs` file the gate owns, root file first, then sorted children."""

    files: list[Path] = []
    root_file = repo_root / TREE_ROOT_FILE
    if root_file.is_file():
        files.append(root_file)
    tree_dir = repo_root / TREE_DIR
    if tree_dir.is_dir():
        files.extend(sorted(p for p in tree_dir.rglob("*.rs") if p.is_file()))
    return files


def line_of(text: str, index: int) -> int:
    return text.count("\n", 0, index) + 1


def relative_to(repo_root: Path, path: Path) -> str:
    """Repo-relative POSIX path, falling back to the absolute one.

    An `include!` may point outside the repo, and a violation on such a file
    still has to render.
    """

    try:
        return path.relative_to(repo_root).as_posix()
    except ValueError:
        return path.as_posix()


def split_top_level(body: str) -> list[str]:
    """Split a `use`-tree brace body on the commas that are not nested."""

    parts: list[str] = []
    current: list[str] = []
    depth = 0
    for char in body:
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(char)
    parts.append("".join(current))
    return [part.strip() for part in parts if part.strip()]


def _use_leaf(text: str, prefix: tuple[str, ...]) -> tuple[tuple[str, ...], str | None]:
    """One `use`-tree leaf as `(full path segments, published name)`.

    The name is `None` for a `*` leaf: a glob publishes a set this scan cannot
    enumerate, and the caller has to fall back to naming the module instead.
    """

    alias: str | None = None
    match = LEAF_ALIAS.match(text)
    if match:
        text, alias = match.group("path").strip(), match.group("alias")
    segments = tuple(part.strip() for part in text.split("::") if part.strip())
    if segments == ("self",):
        # `use a::b::{self, c}` re-publishes `b` itself.
        return prefix, alias or (prefix[-1] if prefix else None)
    full = prefix + segments
    if segments and segments[-1] == "*":
        return full, None
    return full, alias or (segments[-1] if segments else None)


def expand_use_tree(
    tree: str, prefix: tuple[str, ...] = ()
) -> list[tuple[tuple[str, ...], str | None]]:
    """Flatten a `use` tree into one entry per leaf.

    Per-leaf is the point: `use self::store::{Handle, load_inflight_row};`
    re-exports one name that reaches the row and one that does not, and taking
    the whole item as tainted would cascade the forbidden-name set across
    unrelated exports until it swallowed the crate.
    """

    tree = tree.strip()
    if not tree:
        return []
    open_at = tree.find("{")
    if open_at == -1:
        return [_use_leaf(tree, prefix)]

    head = tuple(part.strip() for part in tree[:open_at].split("::") if part.strip())
    depth = 0
    close_at = -1
    for index in range(open_at, len(tree)):
        if tree[index] == "{":
            depth += 1
        elif tree[index] == "}":
            depth -= 1
            if depth == 0:
                close_at = index
                break
    if close_at == -1:
        return []  # unbalanced braces; the neutralizer's own check owns this

    leaves: list[tuple[tuple[str, ...], str | None]] = []
    for part in split_top_level(tree[open_at + 1 : close_at]):
        leaves.extend(expand_use_tree(part, prefix + head))
    return leaves


def file_local_bindings(cleaned: str) -> dict[str, tuple[str, ...]]:
    """Every name this file's own `use` items bind, mapped to the path it names.

    Both spellings bind: `use path as alias;` binds `alias`, and the bare
    `use path::Name;` binds `Name` just as much. A `*` leaf binds a set this
    scan cannot enumerate and an `as _` leaf binds nothing nameable, so neither
    is recorded. Private and `pub` items alike are read — a private `use` still
    binds a name a `pub use` in the same file can be laid over, which is the
    whole point of this map.
    """

    bindings: dict[str, tuple[str, ...]] = {}
    for item in USE_ITEM.finditer(cleaned):
        for full, name in expand_use_tree(item.group(1)):
            if name is None or name == "_":
                continue
            bindings[name] = full
    return bindings


def resolve_file_local(
    segments: tuple[str, ...], bindings: dict[str, tuple[str, ...]]
) -> tuple[str, ...]:
    """Rewrite a `use`-tree leaf's head through the bindings of its own file.

    `use ...::inflight as rows;` followed by `pub use self::rows::Row as R;`
    reaches the row without either item spelling `inflight` in one path, and the
    closure below matches path SEGMENTS, so the head has to be substituted
    before it looks. Only a `self::`-relative head is substituted: that is the
    one form that names an item of THIS file, and Rust forbids a module from
    binding the same name twice (E0255), so in source that compiles `self::X`
    has exactly one meaning and the substitution is not a guess. Chained to a
    fixpoint so an alias of an alias resolves too, with the visited set as the
    cycle guard.

    Every segment this splices in is literal text of the same file, which is
    what keeps `collect_launderers`' raw-text prefilter a superset rather than
    a miss. Deliberately NOT a resolver: a head that is not `self::`-relative
    names another module, and a glob import binds an unenumerable set; neither
    is followed.
    """

    seen: set[tuple[str, ...]] = set()
    while segments not in seen:
        seen.add(segments)
        if segments[:1] != ("self",):
            break
        head = segments[1:]
        target = bindings.get(head[0]) if head else None
        if target is None:
            break
        segments = target + head[1:]
    return segments


def module_name_of(rel: str) -> str:
    """The module a source file declares, in this repo's `foo.rs` + `foo/` layout."""

    name = rel.rsplit("/", 1)[-1]
    if name in ("mod.rs", "lib.rs", "main.rs"):
        parent = rel.rsplit("/", 2)
        return parent[-2] if len(parent) > 1 else ""
    return name[: -len(".rs")] if name.endswith(".rs") else name


@dataclass(frozen=True)
class Launder:
    """A name a third file publishes that reaches the inflight module."""

    name: str
    origin: str
    glob: bool


def collect_launderers(repo_root: Path) -> tuple[list[Launder], list[Violation]]:
    """Names any `pub use` under `src/**` launders out of the inflight module.

    Seeded with the `inflight` segment and closed to a fixpoint, so a re-export
    of a re-export is caught too. The tree's own files are excluded: a `pub use`
    there naming the row is already an `inflight-import`, and reporting it twice
    would only make the fix look like two problems.
    """

    src_root = repo_root / SRC_ROOT
    if not src_root.is_dir():
        return [], []

    tree_rels = {relative_to(repo_root, path) for path in tree_files(repo_root)}
    candidates = [
        path
        for path in sorted(src_root.rglob("*.rs"))
        if path.is_file() and relative_to(repo_root, path) not in tree_rels
    ]

    raw_cache: dict[Path, str] = {}
    leaf_cache: dict[Path, list[tuple[tuple[str, ...], str | None, int]]] = {}
    violations: list[Violation] = []

    def raw_of(path: Path) -> str:
        if path not in raw_cache:
            raw_cache[path] = path.read_text(encoding="utf-8", errors="replace")
        return raw_cache[path]

    def leaves_of(path: Path) -> list[tuple[tuple[str, ...], str | None, int]]:
        if path in leaf_cache:
            return leaf_cache[path]
        cleaned, ambiguous = neutralize_source(raw_of(path))
        if ambiguous:
            violations.append(
                Violation(
                    relative_to(repo_root, path),
                    0,
                    "unlexable-reexport-source",
                    "a comment or literal is unterminated, so this file's "
                    "re-exports cannot be read; failing closed",
                )
            )
            leaf_cache[path] = []
            return []
        # A re-export may be laid over a name the same file bound privately, so
        # each leaf's head is rewritten through this file's own bindings before
        # the closure matches segments against it.
        bindings = file_local_bindings(cleaned)
        found: list[tuple[tuple[str, ...], str | None, int]] = []
        for item in REEXPORT_ITEM.finditer(cleaned):
            line = line_of(cleaned, item.start())
            for full, name in expand_use_tree(item.group(1)):
                found.append((resolve_file_local(full, bindings), name, line))
        leaf_cache[path] = found
        return found

    tainted = {INFLIGHT_MODULE}
    frontier = set(tainted)
    laundered: dict[str, Launder] = {}
    while frontier:
        # Neutralizing is the expensive step, so only files whose RAW text
        # already contains a tainted word can contribute. Neutralizing only
        # ever blanks text, so this prefilter is a superset — never a miss.
        round_files = [
            path
            for path in candidates
            if any(word in raw_of(path) for word in frontier)
            and REEXPORT_PREFILTER.search(raw_of(path))
        ]
        frontier = set()
        for path in round_files:
            rel = relative_to(repo_root, path)
            for full, name, line in leaves_of(path):
                if not tainted.intersection(full):
                    continue
                if name is None:
                    # A `*` re-export publishes an unenumerable set; the module
                    # that holds it becomes the forbidden segment instead.
                    name = module_name_of(rel)
                    if not name:
                        continue
                    glob = True
                else:
                    glob = False
                if name in tainted or name == "_":
                    continue
                laundered[name] = Launder(name, f"{rel}:{line}", glob)
                tainted.add(name)
                frontier.add(name)

    return [laundered[name] for name in sorted(laundered)], violations


def segment_hits(cleaned: str, segment: str) -> list[tuple[int, bool]]:
    """`(line, qualified)` for every use of `segment` as a module path segment.

    Qualified means a `segment::` prefix anywhere in the file; the other channel
    is a bare `segment` inside a `use` item, which is how `use a::{b, segment};`
    and `use a::segment as rows;` reach the module without ever spelling
    `segment::`.
    """

    qualified = re.compile(rf"\b{re.escape(segment)}\b\s*::")
    bare = re.compile(rf"\b{re.escape(segment)}\b")
    hits: list[tuple[int, bool]] = []
    for match in qualified.finditer(cleaned):
        hits.append((line_of(cleaned, match.start()), True))
    for item in USE_ITEM.finditer(cleaned):
        found = bare.search(item.group(0))
        if found is None:
            continue
        absolute = item.start() + found.start()
        if qualified.match(cleaned, absolute):
            continue  # already reported by the qualified-path scan
        hits.append((line_of(cleaned, absolute), False))
    return hits


def included_paths(
    repo_root: Path, path: Path, raw: str, cleaned: str
) -> tuple[list[Path], list[Violation]]:
    """Files `path` pulls in with `include!`, plus the arguments that did not resolve.

    `include!` is unconditionally relative to the directory of the including
    file, which is why this one macro can be followed lexically while `#[path]`
    cannot.
    """

    rel = relative_to(repo_root, path)
    targets: list[Path] = []
    violations: list[Violation] = []
    for call in INCLUDE_CALL.finditer(cleaned):
        line = line_of(cleaned, call.start())
        argument = INCLUDE_ARGUMENT.match(raw, call.end())
        literal = None
        if argument is not None:
            literal = argument.group("raw")
            if literal is None:
                literal = argument.group("plain")
        if not literal:
            violations.append(
                Violation(
                    rel,
                    line,
                    "unreadable-include",
                    "the `include!` argument is not a plain path literal, so the "
                    "included text cannot be scanned; failing closed",
                )
            )
            continue
        target = path.parent / literal
        if not target.is_file():
            violations.append(
                Violation(
                    rel,
                    line,
                    "unresolved-include",
                    f"`include!` names {literal!r}, which is not a file on disk; "
                    "the included text would be compiled into the tree unscanned",
                )
            )
            continue
        targets.append(target.resolve())
    return targets, violations


def scan_row_independence(
    repo_root: Path, laundered: list[Launder] | None = None
) -> list[Violation]:
    files = tree_files(repo_root)
    if not files:
        # An empty scan set must never read as a clean scan.
        return [
            Violation(
                TREE_ROOT_FILE,
                0,
                "missing-tree",
                "the reachability tree is absent; delete this gate in the same "
                "change that deletes the tree",
            )
        ]

    if laundered is None:
        laundered, _ = collect_launderers(repo_root)

    violations: list[Violation] = []
    queue = list(files)
    seen = {path.resolve() for path in files}
    while queue:
        path = queue.pop(0)
        rel = relative_to(repo_root, path)
        try:
            raw = path.read_text(encoding="utf-8")
        except OSError as error:
            violations.append(
                Violation(rel, 0, "unreadable-file", f"cannot be read: {error}")
            )
            continue
        cleaned, ambiguous = neutralize_source(raw)
        if ambiguous:
            violations.append(
                Violation(
                    rel,
                    0,
                    "unlexable",
                    "a comment or literal is unterminated, so the scan cannot "
                    "distinguish code from prose; failing closed",
                )
            )
            continue

        for line, qualified in segment_hits(cleaned, INFLIGHT_MODULE):
            violations.append(
                Violation(
                    rel,
                    line,
                    "inflight-path" if qualified else "inflight-import",
                    "a qualified `inflight::` path breaks 4987 I14 row independence"
                    if qualified
                    else "a `use` item names the inflight module; 4987 I14 forbids "
                    "the reachability tree from depending on the inflight row",
                )
            )

        for launder in laundered:
            if launder.glob:
                for line, _qualified in segment_hits(cleaned, launder.name):
                    violations.append(
                        Violation(
                            rel,
                            line,
                            "laundered-inflight-module",
                            f"`{launder.name}` glob-re-exports the inflight module "
                            f"at {launder.origin}, so reaching it here is the same "
                            "4987 I14 dependency under another path",
                        )
                    )
                continue
            for match in re.finditer(rf"\b{re.escape(launder.name)}\b", cleaned):
                violations.append(
                    Violation(
                        rel,
                        line_of(cleaned, match.start()),
                        "laundered-inflight-name",
                        f"`{launder.name}` is re-exported out of the inflight "
                        f"module at {launder.origin}; 4987 I14 forbids the row "
                        "whatever name it arrives under",
                    )
                )

        targets, include_violations = included_paths(repo_root, path, raw, cleaned)
        violations.extend(include_violations)
        for target in targets:
            if target not in seen:
                seen.add(target)
                queue.append(target)

    return violations


def surface_section(doc_text: str) -> list[str] | None:
    """The `relay_reachability` bullet block, or None when it is absent."""

    lines = doc_text.splitlines()
    try:
        start = lines.index(SURFACE_HEADING)
    except ValueError:
        return None
    body: list[str] = []
    for line in lines[start + 1 :]:
        if line.startswith(("### ", "## ")):
            break
        body.append(line)
    return body


def split_bullets(section: list[str]) -> list[tuple[int, str]]:
    """Group a bullet block into `(offset_of_first_line, joined_text)` pairs.

    The doc wraps its prose, so a check that needs a path and its qualifier
    together has to read the bullet rather than the line.
    """

    bullets: list[tuple[int, list[str]]] = []
    for offset, line in enumerate(section):
        if line.startswith("- "):
            bullets.append((offset, [line]))
        elif bullets:
            bullets[-1][1].append(line)
    return [(offset, "\n".join(lines)) for offset, lines in bullets]


def normalize_prose(text: str) -> str:
    """Collapse wrapping and markdown emphasis so a required sentence survives
    both. A gate must not dictate where the author's editor wrapped a line."""

    return re.sub(r"\s+", " ", text.replace("**", "").replace("`", ""))


def scan_change_surface(repo_root: Path) -> list[Violation]:
    doc = repo_root / CHANGE_SURFACES_DOC
    if not doc.is_file():
        return [
            Violation(CHANGE_SURFACES_DOC, 0, "missing-doc", "change-surfaces.md is absent")
        ]

    text = doc.read_text(encoding="utf-8")
    section = surface_section(text)
    if section is None:
        return [
            Violation(
                CHANGE_SURFACES_DOC,
                0,
                "missing-surface",
                f"{SURFACE_HEADING} is absent; 4987 §9.4 requires the "
                "`relay_reachability` surface to own the reachability tree",
            )
        ]

    heading_line = text.splitlines().index(SURFACE_HEADING) + 1
    body = "\n".join(section)
    violations: list[Violation] = []

    declared_globs = set(SURFACE_GLOB.findall(body))
    for required in REQUIRED_SURFACE_GLOBS:
        if required not in declared_globs:
            violations.append(
                Violation(
                    CHANGE_SURFACES_DOC,
                    heading_line,
                    "unowned-tree",
                    f"the surface must declare `{required}` in canonical_modules",
                )
            )

    prose = normalize_prose(body)
    for marker in REQUIRED_SURFACE_MARKERS:
        if normalize_prose(marker) not in prose:
            violations.append(
                Violation(
                    CHANGE_SURFACES_DOC,
                    heading_line,
                    "missing-marker",
                    f"the surface must state {marker!r}",
                )
            )

    # Ghost paths: each future-slice marker qualifies only the concrete path
    # immediately before it. A marker elsewhere in the bullet must not exempt
    # unrelated missing paths that happen to share that bullet.
    for offset, bullet in split_bullets(section):
        path_matches = list(SURFACE_PATH.finditer(bullet))
        for index, path_match in enumerate(path_matches):
            path = path_match.group(1)
            qualifier_end = (
                path_matches[index + 1].start()
                if index + 1 < len(path_matches)
                else len(bullet)
            )
            qualifier = bullet[path_match.end() : qualifier_end]
            if (repo_root / path).is_file() or FUTURE_SLICE_MARKER.search(qualifier):
                continue
            violations.append(
                Violation(
                    CHANGE_SURFACES_DOC,
                    heading_line + 1 + offset,
                    "ghost-path",
                    f"{path} is named by the surface but is missing from disk; "
                    "remove it or mark it 'not yet on disk' with its slice",
                )
            )

    # Every file in the tree must be covered by a declared glob.
    for path in tree_files(repo_root):
        rel = path.relative_to(repo_root).as_posix()
        if any(
            rel == glob or fnmatch.fnmatch(rel, glob) or fnmatch.fnmatch(rel, glob + "/*")
            for glob in declared_globs
        ):
            continue
        violations.append(
            Violation(
                CHANGE_SURFACES_DOC,
                heading_line,
                "uncovered-file",
                f"{rel} is in the reachability tree but no declared "
                "canonical_modules glob covers it",
            )
        )

    return violations


def scan(repo_root: Path) -> tuple[list[Violation], list[Launder]]:
    """Every violation, plus the launder closure the summary line reports.

    The closure is walked once and handed back rather than recomputed, because
    it is the only pass in this gate that reads the whole of `src/**`.
    """

    laundered, launder_violations = collect_launderers(repo_root)
    violations = (
        scan_row_independence(repo_root, laundered)
        + launder_violations
        + scan_change_surface(repo_root)
    )
    return violations, laundered


def run(repo_root: Path) -> list[Violation]:
    return scan(repo_root)[0]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--repo-root",
        default=str(REPO_ROOT),
        help="repository root to scan (default: this script's repository)",
    )
    args = parser.parse_args(argv)
    repo_root = Path(args.repo_root).resolve()

    violations, laundered = scan(repo_root)
    if violations:
        print(
            "reachability row-independence gate FAILED "
            f"({len(violations)} violation(s)); 4987 §-1.5 I14 + §9.4:",
            file=sys.stderr,
        )
        for violation in violations:
            print(f"  {violation.render()}", file=sys.stderr)
        return 1

    print(
        f"reachability row-independence OK: {len(tree_files(repo_root))} file(s) "
        f"carry no inflight path and none of the {len(laundered)} name(s) "
        "re-exported out of the inflight module elsewhere in src/, and the "
        "relay_reachability change surface owns them (source lint, not a type "
        "proof)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
