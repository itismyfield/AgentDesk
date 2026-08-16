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
   neither in a `use` item nor in a fully-qualified path expression. Scanning
   both is deliberate: a lint that reads only `use` lines is bypassed by
   writing `crate::services::discord::inflight::foo()` at the call site.
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
  * a re-export laundering the module under another name in a THIRD file, then
    used here under that name, is invisible: the segment never appears here.
    The `use`-item half does catch the launderings visible from inside the tree
    — a bare trailing `inflight` segment and an `as` rename;
  * `#[path = "..."]` redirections are not followed. The scan set is the tree's
    files as they sit on disk;
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

INFLIGHT_QUALIFIED = re.compile(r"\binflight\b\s*::")
INFLIGHT_SEGMENT = re.compile(r"\binflight\b")
USE_ITEM = re.compile(r"\buse\b[^;]*;", re.DOTALL)
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


def scan_row_independence(repo_root: Path) -> list[Violation]:
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

    violations: list[Violation] = []
    for path in files:
        rel = path.relative_to(repo_root).as_posix()
        cleaned, ambiguous = neutralize_source(path.read_text(encoding="utf-8"))
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

        for match in INFLIGHT_QUALIFIED.finditer(cleaned):
            violations.append(
                Violation(
                    rel,
                    line_of(cleaned, match.start()),
                    "inflight-path",
                    "a qualified `inflight::` path breaks 4987 I14 row independence",
                )
            )

        for item in USE_ITEM.finditer(cleaned):
            segment = INFLIGHT_SEGMENT.search(item.group(0))
            if segment is None:
                continue
            absolute = item.start() + segment.start()
            if INFLIGHT_QUALIFIED.match(cleaned, absolute):
                continue  # already reported by the qualified-path scan
            violations.append(
                Violation(
                    rel,
                    line_of(cleaned, absolute),
                    "inflight-import",
                    "a `use` item names the inflight module; 4987 I14 forbids the "
                    "reachability tree from depending on the inflight row",
                )
            )

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


def run(repo_root: Path) -> list[Violation]:
    return scan_row_independence(repo_root) + scan_change_surface(repo_root)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--repo-root",
        default=str(REPO_ROOT),
        help="repository root to scan (default: this script's repository)",
    )
    args = parser.parse_args(argv)
    repo_root = Path(args.repo_root).resolve()

    violations = run(repo_root)
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
        "carry no inflight path, and the relay_reachability change surface owns "
        "them (source lint, not a type proof)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
