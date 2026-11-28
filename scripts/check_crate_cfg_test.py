#!/usr/bin/env python3
"""Reject new test-only cfg sites in dependency-owned Rust sources."""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
from hashlib import sha256
import json
from pathlib import Path
import re
import subprocess
import sys

if __package__:
    from .ci.rust_test_sources import walk, workspace_lib_roots
    from .generate_inventory_docs import scan_balanced
    from .rust_cfg import cfg_requires_test
    from .rust_lex import CODE, COMMENT, LITERAL, ParseError, StripState, blank_segments, cfg_item_end, lex_segments
else:
    from ci.rust_test_sources import walk, workspace_lib_roots
    from generate_inventory_docs import scan_balanced
    from rust_cfg import cfg_requires_test
    from rust_lex import CODE, COMMENT, LITERAL, ParseError, StripState, blank_segments, cfg_item_end, lex_segments


BASELINE = "scripts/crate_cfg_test_baseline.txt"
INITIAL_BASELINE_SHA256 = "9076d8a1f7b2aa2fd4d93dd93b0817b88dae200a4fefb615f6a926dad9ed6a1b"
ALLOW = "// cfg-test-in-crate: allow "
CLOSER = {"(": ")", "[": "]", "{": "}"}
CANDIDATE = re.compile(
    r"#\s*(?P<inner>!)?\s*\[\s*cfg\s*\("
    r"|\b(?P<macro>cfg|stringify)\s*!\s*[([{]"
)
NEXT_ATTR = re.compile(r"\s*#\s*\[")
VISIBILITY = r"(?:pub\s*(?:\([^)]*\)\s*)?)?"
IDENT = r"(?:r#)?[^\W\d]\w*"
MODULE = re.compile(r"\s*" + VISIBILITY + rf"mod\s+{IDENT}\s*([;{{])")
ITEM = re.compile(
    r"\s*" + VISIBILITY + r"(?:(?:async|const|unsafe|default)\s+)*"
    r"(?P<head>fn|mod|struct|enum|union|trait|impl|type|static|const|use|extern|macro_rules|macro)\b"
    r"(?!\s*\{)"
)
NEGATED_TEST = re.compile(r"not\s*\(\s*test\s*,?\s*\)")
TEST_WORD = re.compile(r"\btest\b")
HAS_CFG = re.compile(r"\bcfg\b")
HEADER_DELIMITER = re.compile(r"[{;([]")
KEY_TOKEN = re.compile(rf"{IDENT}|\d+|\S")


@dataclass(frozen=True)
class Finding:
    path: str
    line: int
    kind: str
    signature: str
    allowed: bool


def crate_sources(root: Path) -> set[Path]:
    sources = set()
    crates = root / "crates"
    libraries = [(manifest, lib) for manifest, lib in workspace_lib_roots(root).values()
                 if manifest.is_relative_to(crates)]
    if not libraries:
        raise ValueError("no library crates found under crates/")
    for manifest, lib in libraries:
        inventory = walk(lib, root, manifest_dir=manifest.parent)
        if inventory.errors:
            raise ValueError("\n".join(inventory.errors))
        for path, chains in inventory.file_cfg_texts.items():
            if not path.is_relative_to(root):
                raise ValueError(f"crate source outside worktree: {path}")
            if not path.name.endswith("_tests.rs") and any(
                not any(requires_test(cfg, dependency_test=False) for cfg in chain) for chain in chains
            ):
                sources.add(path)
    return sources


def requires_test(predicate: str, *, dependency_test: bool = True) -> bool:
    return bool(TEST_WORD.search(predicate)) and cfg_requires_test(
        predicate.strip().removesuffix(","), strict=not dependency_test, dependency_test=dependency_test
    )


def end_group(code: str, opening: int) -> int:
    delimiter = code[opening]
    return scan_balanced(code, opening, delimiter, CLOSER[delimiter])[1]


def target_start(code: str, after: int) -> int:
    while attribute := NEXT_ATTR.match(code, after):
        after = end_group(code, attribute.end() - 1)
    return after


def target_end(code: str, start: int, item: re.Match | None) -> int:
    if item and item["head"] not in {"use", "const", "static", "type"}:
        cursor = start
        while match := HEADER_DELIMITER.search(code, cursor):
            if match[0] in "{;":
                return match.start()
            cursor = end_group(code, match.start())
    try:
        return cfg_item_end(code, start, scan_balanced)
    except ParseError:
        next_line = code.find("\n", start)
        return len(code) if next_line < 0 else next_line


def allowed_lines(source: str) -> set[int]:
    if ALLOW not in source:
        return set()
    state = StripState()
    lines = set()
    for line, text in enumerate(source.splitlines(), 1):
        continued_block = bool(state.block_depth)
        segments = lex_segments(text, state)
        for kind, segment in segments[continued_block:]:
            if kind == COMMENT and segment.startswith(ALLOW) and segment[len(ALLOW):].strip():
                lines.add(line)
    return lines


def normalized(text: str) -> str:
    state = StripState()
    parts = []
    for line in text.splitlines(keepends=True):
        continuing = state.in_string or state.raw_hashes is not None
        for kind, segment in lex_segments(line, state):
            if kind == CODE:
                parts.extend(KEY_TOKEN.findall(segment))
            elif kind == LITERAL:
                if continuing:
                    parts[-1] += segment
                else:
                    parts.append(segment)
            continuing = False
    return " ".join(parts)


def file_findings(path: str, source: str):
    if not HAS_CFG.search(source) or not TEST_WORD.search(source):
        return
    code, comments_removed = blank_segments(source, {COMMENT, LITERAL}, {COMMENT})
    allowances = allowed_lines(source)
    skip_until = 0
    for match in CANDIDATE.finditer(code):
        if match.start() < skip_until:
            continue
        opening = match.end() - 1
        end = end_group(code, opening)
        if match["macro"] == "stringify":
            skip_until = end
            continue
        predicate = comments_removed[opening + 1:end - 1].strip().removesuffix(",").rstrip()
        negated = NEGATED_TEST.fullmatch(predicate)
        test_only = requires_test(predicate)
        if not (negated or test_only):
            continue
        if match["macro"]:
            kind, target = "cfg-bang-test", ""
        else:
            after = re.match(r"\s*\]", code[end:])
            if not after:
                continue
            end += after.end()
            start = target_start(code, end)
            if match["inner"]:
                kind, target = "cfg-not-test" if negated else "cfg-test-item", ""
            elif test_only and (module := MODULE.match(code, start)):
                if module[1] == "{" and requires_test(predicate, dependency_test=False):
                    skip_until = end_group(code, module.end() - 1)
                continue
            else:
                item = ITEM.match(code, start)
                kind = "cfg-not-test" if negated else "cfg-test-item" if item else "cfg-test-statement"
                target = normalized(comments_removed[start:target_end(code, start, item)])
        line = source.count("\n", 0, match.start()) + 1
        signature = normalized(comments_removed[match.start():end])
        if target:
            signature += " " + target
        yield Finding(path, line, kind, signature, line in allowances)


def scan(root: Path) -> dict[str, Finding]:
    counts = Counter()
    found = {}
    for path in sorted(crate_sources(root)):
        for site in file_findings(path.relative_to(root).as_posix(), path.read_text("utf-8")):
            identity = (site.path, site.kind, site.signature, "allow" if site.allowed else "site")
            counts[identity] += 1
            found[json.dumps((*identity, counts[identity]), ensure_ascii=False)] = site
    return found


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *args], capture_output=True, text=True, check=True
    ).stdout


def baseline_rows(text: str) -> set[str]:
    rows = text.splitlines()
    if rows != sorted(set(rows)) or "" in rows:
        raise ValueError("baseline rows must be sorted, nonempty, and unique")
    return set(rows)


def check(root: Path, found: dict[str, Finding], base_ref: str) -> int:
    baseline = baseline_rows((root / BASELINE).read_text("utf-8"))
    base = git(root, "rev-parse", "--verify", f"{base_ref}^{{commit}}").strip()
    base_has_baseline = git(root, "ls-tree", base, "--", BASELINE)
    growth = set()
    if base_has_baseline:
        growth = baseline - baseline_rows(git(root, "show", f"{base}:{BASELINE}"))
    else:
        digest = sha256("".join(row + "\n" for row in sorted(baseline)).encode("utf-8")).hexdigest()
        if digest != INITIAL_BASELINE_SHA256:
            print(f"CFG_TEST_BASELINE_FAIL initial baseline differs from approved snapshot ({digest})")
            return 1

    for row in sorted(growth):
        print(f"CFG_TEST_BASELINE_FAIL added row {row}")
    stale = baseline - found.keys()
    for row in sorted(stale):
        print(f"CFG_TEST_BASELINE_FAIL stale row {row}")
    fresh = [site for key, site in found.items() if key not in baseline - growth]
    for site in fresh:
        print(f"CFG_TEST_IN_CRATE {site.path}:{site.line} {site.kind}")
    print(f"CFG_TEST_IN_CRATE_SUMMARY baseline={len(baseline)} sites={len(found)} "
          f"new={len(fresh)} baseline_growth={len(growth)} baseline_stale={len(stale)}")
    return int(bool(fresh or growth or stale))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="worktree or source archive to scan")
    parser.add_argument("--base-ref", default="HEAD^1", help="Git base for the no-growth check")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--list", action="store_true", help="list all findings without a baseline")
    mode.add_argument("--snapshot", action="store_true", help="print baseline keys without editing the baseline")
    args = parser.parse_args()
    try:
        root = args.root.resolve()
        found = scan(root)
        if args.snapshot:
            for row in sorted(found):
                print(row)
        elif args.list:
            for site in found.values():
                print(f"CFG_TEST_IN_CRATE {site.path}:{site.line} {site.kind} {site.signature}")
        else:
            return check(root, found, args.base_ref)
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        print(f"CFG_TEST_SCAN_FAIL {getattr(exc, 'stderr', None) or exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
