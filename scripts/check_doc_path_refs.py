#!/usr/bin/env python3
"""Fail when a hand-written doc names a repo path that does not exist.

Run it to see the rules and the markers for removed, proposed, or external paths.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
BASELINE_REL = Path("scripts") / "doc_path_refs_baseline.txt"
EXCLUDED_PREFIXES = ("docs/generated/",)
# Untracked outputs built inside a checkout. Docs may name them (saying how to
# build them), but a Markdown link to one is dead on a fresh clone.
CHECKOUT_LOCAL = (
    "docs/generated/module-inventory.md",
    "docs/generated/giant-file-registry.md",
    "dashboard/dist",
)
ABSENT_DECL_RE = re.compile(r"<!--\s*doc-path-refs:\s*(?:removed|proposed)\s+(.*?)\s*-->")
EXTERNAL_DECL_RE = re.compile(r"<!--\s*doc-path-refs:\s*external\s+(.*?)\s*-->")

ANCHOR_DIRS = (
    "src", "docs", "scripts", "tests", "migrations", "policies", "dashboard",
    "skills", "tools", "assets", ".github",
)
CODE_EXT = (
    "rs|ts|tsx|js|mjs|cjs|jsx|py|sh|sql|toml|yaml|yml|json|md|css|html|txt"
)
_BOUNDARY = r"(?<![\w./~$@%*-])"
LINE_REF_RE = re.compile(
    _BOUNDARY
    + r"((?:\.\.?/)*(?:[\w.-]+/)*[\w-][\w.-]*\.(?:" + CODE_EXT + r"))"
    + r"(?::|#L)(\d+)(?:\s*[-–]\s*L?(\d+))?"
)
ANCHORED_RE = re.compile(
    _BOUNDARY + r"((?:" + "|".join(re.escape(d) for d in ANCHOR_DIRS) + r")/[\w./-]*)"
)
# A bare `:N` code span continues the previous `path:N` on the same line.
CONT_REF_RE = re.compile(r"`:(\d+)(?:\s*[-–]\s*(\d+))?`")
RUST_PARTIAL_RE = re.compile(_BOUNDARY + r"((?:[\w-]+/)*[\w-]+\.rs)(?![\w/])")
LINK_RE = re.compile(r"\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")
URL_RE = re.compile(r"[a-zA-Z][\w+.-]*://\S+")
PLACEHOLDER_RE = re.compile(r"[*<>{}$]|\.\.\.|…")
# `path` (removed|proposed|missing) / `path`(삭제됨|제안|없음): the file is gone or not created.
REMOVED_RE = re.compile(
    r"/?`?\s*\((?:removed|deleted|proposed|missing|삭제됨|제안|없음)(?:[ ,;][^)]*)?\)"
)
FENCE_RE = re.compile(r"^\s{0,3}(?:```|~~~)")
# Every test lives in the lib, so a `--bin agentdesk` test run passes with 0 tests.
ZERO_TEST_CMD_RE = re.compile(r"cargo\s+(?:test|nextest\s+run)\b[^`\n]*?--bin[\s=]+agentdesk\b")
ZERO_TEST_REASON = "runs 0 tests (all tests are in the lib); use cargo test --lib <module::path>"
CODE_SPAN_RE = re.compile(r"`[^`]+`")
HAS_EXT_RE = re.compile(r"\.[A-Za-z0-9]+$")


@dataclass(frozen=True)
class Finding:
    doc: str
    line: int
    ref: str
    reason: str

    def key(self) -> str:
        return f"{self.doc}\t{self.ref}"

    def render(self) -> str:
        return f"{self.doc}:{self.line}: {self.ref} -> {self.reason}"


class RepoIndex:
    def __init__(self, files: list[str], root: Path) -> None:
        self.root = root
        self.files = set(files)
        self.dirs: set[str] = set()
        self.suffixes: dict[str, list[str]] = defaultdict(list)
        for path in files:
            parts = path.split("/")
            for i in range(1, len(parts)):
                self.dirs.add("/".join(parts[:i]))
            for i in range(len(parts)):
                self.suffixes["/".join(parts[i:])].append(path)
        self._lines: dict[str, int] = {}

    def exists(self, path: str) -> bool:
        path = path.rstrip("/")
        return path in self.files or path in self.dirs

    def line_count(self, path: str) -> int:
        if path not in self._lines:
            data = (self.root / path).read_bytes()
            count = data.count(b"\n")
            if data and not data.endswith(b"\n"):
                count += 1
            self._lines[path] = count
        return self._lines[path]

    def resolve(self, doc: str, path: str) -> list[str]:
        """Exact, doc-relative, then suffix candidates for a file reference."""
        stripped = re.sub(r"^(?:\./|\.\./)+", "", path)
        relative = os.path.normpath(os.path.join(os.path.dirname(doc), path))
        for candidate in (relative, stripped):
            if candidate in self.files:
                return [candidate]
        return list(self.suffixes.get(stripped, ()))


def tracked_files(root: Path) -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True, capture_output=True,
    ).stdout.decode("utf-8")
    return [p for p in out.split("\0") if p and (root / p).exists()]


def _clean(token: str) -> str:
    return token.rstrip(".,;:)/")


def _marked_removed(line: str, end: int) -> bool:
    return bool(REMOVED_RE.match(line, end))


def _declared_removed(path: str, removed: list[str]) -> bool:
    path = re.sub(r"^(?:\./|\.\./)+", "", path).rstrip("/")
    return any(d == path or d.endswith("/" + path) or path.endswith("/" + d) for d in removed)


def _check_line_refs(doc, lineno, line, index, findings, seen, removed) -> None:
    """Check `path:N` refs and the bare `:N` refs that continue the last one."""
    events = [(m.start(), m) for m in LINE_REF_RE.finditer(line)]
    events += [(m.start(), m) for m in CONT_REF_RE.finditer(line)]
    candidates: list[str] = []
    label = ""
    for _, match in sorted(events, key=lambda e: e[0]):
        if match.re is LINE_REF_RE:
            seen.add(match.start(1))
            path, first, last = match.group(1), int(match.group(2)), match.group(3)
            candidates, label = [], match.group(0)
            if _marked_removed(line, match.end()) or _declared_removed(path, removed):
                continue
            candidates = index.resolve(doc, path)
            if not candidates:
                findings.append(Finding(doc, lineno, label, "file missing"))
                continue
            ref = label
        elif candidates:
            first, last = int(match.group(1)), match.group(2)
            ref = f"{label} … {match.group(0)}"
        else:
            continue
        need = max(first, int(last) if last else 0)
        if first == 0 or all(index.line_count(c) < need for c in candidates):
            longest = max(index.line_count(c) for c in candidates)
            findings.append(Finding(doc, lineno, ref, f"line past EOF ({longest} lines)"))


def _check_links(doc, lineno, raw, index, findings) -> None:
    for match in LINK_RE.finditer(raw):
        target = match.group(1)
        if re.match(r"[a-zA-Z][\w+.-]*:", target) or target.startswith("#"):
            continue
        target = target.split("#", 1)[0].split("?", 1)[0]
        if not target or PLACEHOLDER_RE.search(target):
            continue
        if target.startswith("/"):
            resolved = target.lstrip("/")
        else:
            resolved = os.path.normpath(os.path.join(os.path.dirname(doc), target))
        if not index.exists(resolved):
            findings.append(Finding(doc, lineno, target, "link target missing"))


def _is_deprecated(lines: list[str]) -> bool:
    """A `deprecated: true` front matter keeps the doc as context only."""
    if not lines or lines[0].strip() != "---":
        return False
    for line in lines[1:40]:
        if line.strip() == "---":
            return False
        if re.fullmatch(r"deprecated:\s*true\s*", line.strip()):
            return True
    return False


def _external_declarations(doc, lines, index, findings) -> list[str]:
    prefixes: list[str] = []
    for lineno, raw in enumerate(lines, 1):
        for match in EXTERNAL_DECL_RE.finditer(raw):
            for prefix in re.split(r"[\s,]+", match.group(1)):
                prefix = prefix.strip("`")
                if not prefix:
                    continue
                if index.exists(prefix.rstrip("/")):
                    findings.append(Finding(doc, lineno, prefix, "declared external but exists"))
                prefixes.append(prefix)
    return prefixes


def _absent_declarations(doc, lines, index, findings) -> list[str]:
    removed: list[str] = []
    for lineno, raw in enumerate(lines, 1):
        for match in ABSENT_DECL_RE.finditer(raw):
            for path in re.split(r"[\s,]+", match.group(1)):
                path = path.strip("`").rstrip("/")
                if not path:
                    continue
                if index.exists(path) or index.suffixes.get(path):
                    findings.append(Finding(doc, lineno, path, "declared removed/proposed but exists"))
                removed.append(path)
    return removed


def scan_doc(doc: str, text: str, index: RepoIndex) -> list[Finding]:
    findings: list[Finding] = []
    in_fence = False
    lines = text.splitlines()
    removed = _absent_declarations(doc, lines, index, findings)
    external = _external_declarations(doc, lines, index, findings)
    links_only = _is_deprecated(lines)
    for lineno, raw in enumerate(lines, 1):
        if ZERO_TEST_CMD_RE.search(raw):
            findings.append(Finding(doc, lineno, "cargo test --bin agentdesk", ZERO_TEST_REASON))
        if ABSENT_DECL_RE.search(raw) or EXTERNAL_DECL_RE.search(raw):
            continue
        for prefix in external:
            raw = re.sub(re.escape(prefix) + r"[\w./:#-]*", lambda m: " " * len(m.group(0)), raw)
        if FENCE_RE.match(raw):
            in_fence = not in_fence
            continue
        line = URL_RE.sub(lambda m: " " * len(m.group(0)), raw)
        spans = [m.span() for m in CODE_SPAN_RE.finditer(line)]
        seen: set[int] = set()
        _check_links(doc, lineno, raw, index, findings)
        if links_only:
            continue
        _check_line_refs(doc, lineno, line, index, findings, seen, removed)

        for match in ANCHORED_RE.finditer(line):
            token = _clean(match.group(1))
            end = match.start(1) + len(token)
            if match.start(1) in seen or "/" not in token or _marked_removed(line, end):
                continue
            if PLACEHOLDER_RE.search(token) or _declared_removed(token, removed):
                continue
            if PLACEHOLDER_RE.match(line, match.end()):
                # A glob/placeholder follows: its literal directory must exist.
                token = match.group(1).rsplit("/", 1)[0]
                if not index.exists(token):
                    findings.append(Finding(doc, lineno, token + "/", "directory missing"))
                continue
            in_code = in_fence or any(s < match.start(1) < e for s, e in spans)
            if not in_code and not HAS_EXT_RE.search(token):
                continue
            local = any(token == p or token.startswith(p + "/") for p in CHECKOUT_LOCAL)
            if not local and not index.exists(token):
                findings.append(Finding(doc, lineno, token, "path missing"))

        for match in RUST_PARTIAL_RE.finditer(line):
            token = match.group(1)
            if match.start(1) in seen or _marked_removed(line, match.end()):
                continue
            if _declared_removed(token, removed):
                continue
            if token.split("/", 1)[0] in ANCHOR_DIRS and "/" in token:
                continue
            if not index.resolve(doc, token):
                findings.append(Finding(doc, lineno, token, "no tracked .rs file ends with this"))
    return findings


def load_baseline(path: Path) -> set[str]:
    if not path.exists():
        return set()
    entries = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.startswith("#"):
            entries.add(line.rstrip("\n"))
    return entries


def scan_repo(root: Path) -> list[Finding]:
    files = tracked_files(root)
    index = RepoIndex(files, root)
    findings: list[Finding] = []
    for doc in sorted(files):
        if not doc.endswith(".md") or doc.startswith(EXCLUDED_PREFIXES):
            continue
        text = (root / doc).read_text(encoding="utf-8", errors="replace")
        findings.extend(scan_doc(doc, text, index))
    return findings


RULES = """\
Scope: tracked *.md outside docs/generated/. Checked shapes:
  - relative Markdown link targets must exist;
  - paths starting with a top-level dir (src/, docs/, scripts/, ...) must be
    tracked (extensionless ones only inside code spans or fences);
  - path:N and path#LN (and a following `:N` code span) must name an existing
    file (partial paths match by suffix) and a line inside it;
  - partial Rust paths (turn_bridge/mod.rs, outbox.rs) must match a tracked file;
  - no `cargo test --bin agentdesk` command: it runs 0 tests and exits 0. This
    one cannot be baselined.
Prefer `path` + symbol name over path:N; line numbers drift with every edit.
Markers for paths that intentionally do not exist:
  `path` (removed) / (proposed) / (missing), or (삭제됨) / (제안) / (없음);
  <!-- doc-path-refs: removed a/b.rs c.py --> or ... proposed ... per doc
  (a declared path that exists is an error);
  <!-- doc-path-refs: external packages/x/ --> for another repo's paths.
A doc with `deprecated: true` front matter is context only: links are checked.
Globs and placeholders (*, <x>, {x}, $X, ...) are skipped. Known breakage owned
by another lane is listed in scripts/doc_path_refs_baseline.txt.
"""


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        epilog=RULES,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    parser.add_argument("--baseline", type=Path, default=None)
    parser.add_argument("--write-baseline", action="store_true")
    args = parser.parse_args(argv)
    root = args.root.resolve()
    baseline_path = args.baseline or (root / BASELINE_REL)

    findings = scan_repo(root)
    if args.write_baseline:
        keys = sorted({f.key() for f in findings if f.reason != ZERO_TEST_REASON})
        header = (
            "# doc<TAB>ref pairs that scripts/check_doc_path_refs.py reports without failing.\n"
            "# Only test-run docs awaiting their rewrite belong here; delete lines as they get fixed.\n"
        )
        baseline_path.parent.mkdir(parents=True, exist_ok=True)
        baseline_path.write_text(header + "".join(k + "\n" for k in keys), encoding="utf-8")
        print(f"wrote {len(keys)} baseline entries to {baseline_path}")
        return 0

    baseline = load_baseline(baseline_path)
    new = [f for f in findings if f.key() not in baseline or f.reason == ZERO_TEST_REASON]
    tolerated = [f for f in findings if f not in new]
    stale = sorted(baseline - {f.key() for f in findings})
    for finding in tolerated:
        print(f"baseline: {finding.render()}")
    for key in stale:
        print(f"note: stale baseline entry (fixed; remove it): {key}")
    for finding in new:
        print(f"ERROR: {finding.render()}", file=sys.stderr)
    if new:
        print(f"{len(new)} broken doc path reference(s). Fix them as follows.", file=sys.stderr)
        print(RULES, file=sys.stderr)
        return 1
    print(f"doc path refs OK ({len(tolerated)} baseline, {len(stale)} stale)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
