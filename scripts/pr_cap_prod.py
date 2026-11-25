#!/usr/bin/env python3
"""Measure a PR against the prod cap: 30 files with a code change / net +1500 code lines (code added minus code deleted).

usage: pr_cap_prod.py <base> <head> [--repo <path>]
Blank and comment-only lines, tests, fixtures, generated files and docs are not counted.
Files matching a glob in scripts/cap_generated_data.tsv are tool output, not review surface: listed apart as GENERATED_DATA.
Rust files also skip the lines inside `#[cfg(test)] mod x { ... }` blocks; a top-of-file `mod tests;` or test helper does not hide the prod lines below it.
"""
import argparse
import fnmatch
import re
import subprocess
import sys

FILES, NET = 30, 1500
SLASH_COMMENTS = (".rs", ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".go", ".swift", ".kt", ".java", ".c", ".h", ".cpp")
HASH_COMMENTS = (".py", ".sh", ".bash", ".zsh", ".yml", ".yaml", ".toml", ".rb")
EXCLUDE = (
    "tests/*", "*/tests/*", "*_tests.rs", "*_test.rs", "*/fixtures/*", "*/testdata/*",
    "*.snap", "docs/*", "*.md", "scripts/lib_test_inventory_manifest.txt", "scripts/pg_test_lane_manifest.txt",
    "scripts/sql_execution_surface_inventory.json",
    "*/generated/*",
)
TEST_FILES = ("tests/*", "*/tests/*", "*_tests.rs", "*_test.rs")
# A test_*.py name alone is not a test; tool code without test definitions counts as prod.
PY_TEST_NAMES = ("*/test_*.py", "test_*.py")
PY_TEST_CONTENT = re.compile(r"^\s*(?:def test_\w+|async def test_\w+|class \w+\(.*TestCase\)|import pytest|import unittest|from unittest)", re.M)
# Rows are glob<TAB>generation command<TAB>verification command, read from the head commit.
GENERATED_LIST = "scripts/cap_generated_data.tsv"
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")
ATTR = re.compile(r"^\s*#\[")
MOD_BLOCK = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{\s*$")


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=True).stdout


def excluded(path):
    return any(fnmatch.fnmatch(path, pat) for pat in EXCLUDE)


def file_lines(repo, rev, path):
    try:
        return git(repo, "show", f"{rev}:{path}").splitlines()
    except subprocess.CalledProcessError:
        return []


def code_lines(lines, path):
    """1-based numbers of the lines that hold code: not blank and not only a comment."""
    slash, hashed = path.endswith(SLASH_COMMENTS), path.endswith(HASH_COMMENTS)
    code, in_block = set(), False
    for n, line in enumerate(lines, 1):
        s = line.strip()
        if in_block or (slash and s.startswith("/*")):
            body = s if in_block else s[2:]
            in_block = "*/" not in body
            rest = "" if in_block else body.split("*/", 1)[1].strip()
            if rest and not rest.startswith("//"):
                code.add(n)
        elif s and not (slash and s.startswith("//")) and not (hashed and s.startswith("#") and not s.startswith("#!")):
            code.add(n)
    if path.endswith(".rs"):
        code -= test_block_lines(lines)
    return code


def code_delta(repo, base, head, path):
    new, old = code_lines(file_lines(repo, head, path), path), code_lines(file_lines(repo, base, path), path)
    adds = dels = 0
    for line in git(repo, "diff", "-U0", "--no-renames", base, head, "--", path).splitlines():
        m = HUNK.match(line)
        if m:
            o, oc, h, hc = int(m.group(1)), int(m.group(2) or 1), int(m.group(3)), int(m.group(4) or 1)
            dels += sum(1 for k in range(o, o + oc) if k in old)
            adds += sum(1 for k in range(h, h + hc) if k in new)
    return adds, dels


def test_block_lines(lines):
    """1-based line numbers inside inline `#[cfg(test)] mod x {` blocks; rustfmt closes each at the mod's indent."""
    inside, i = set(), 0
    while i < len(lines):
        if CFG_TEST.match(lines[i]):
            j = i + 1
            while j < len(lines) and (ATTR.match(lines[j]) or not lines[j].strip()):
                j += 1
            m = MOD_BLOCK.match(lines[j]) if j < len(lines) else None
            if m:
                close = m.group(1) + "}"
                end = next((k for k in range(j + 1, len(lines)) if lines[k].rstrip() == close), len(lines) - 1)
                inside.update(range(i + 1, end + 2))
                i = end + 1
                continue
        i += 1
    return inside


def py_test_file(repo, rev, path):
    return any(fnmatch.fnmatch(path, pat) for pat in PY_TEST_NAMES) and PY_TEST_CONTENT.search("\n".join(file_lines(repo, rev, path))) is not None


def generated_rules(repo, head):
    rules = []
    for line in file_lines(repo, head, GENERATED_LIST):
        if line.strip() and not line.lstrip().startswith("#"):
            parts = line.split("\t")
            rules.append((parts[0].strip(), *(p.strip() for p in (parts[1:] + ["?", "?"])[:2])))
    return rules


def measure(repo, base, head):
    """prod maps a path to (code adds, code dels, raw adds, raw dels), or None for a binary file.
    generated maps a listed data path to (raw adds, generation command, verification command)."""
    prod, other, generated = {}, {}, {}
    rules = generated_rules(repo, head)
    for row in git(repo, "diff", "--numstat", "--no-renames", base, head).splitlines():
        a, d, path = row.split("\t", 2)
        if a == "-":
            prod[path] = None
            continue
        a, d = int(a), int(d)
        rev = head if file_lines(repo, head, path) else base
        test = py_test_file(repo, rev, path)
        rule = next((r for r in rules if fnmatch.fnmatch(path, r[0])), None)
        if rule and not test and not any(fnmatch.fnmatch(path, pat) for pat in TEST_FILES):
            generated[path] = (a, rule[1], rule[2])
            continue
        if excluded(path) or test:
            other[path] = (a, d)
            continue
        ca, cd = code_delta(repo, base, head, path)
        if ca or cd:
            prod[path] = (ca, cd, a, d)
        if a - ca:
            other[path + "#noncode"] = (a - ca, 0)
    return prod, other, generated


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base")
    ap.add_argument("head")
    ap.add_argument("--repo", default=".")
    args = ap.parse_args()
    # Measure what the PR adds, as GitHub does: a base that moved on must not count its own commits.
    base = git(args.repo, "merge-base", args.base, args.head).strip()
    if base != git(args.repo, "rev-parse", args.base).strip():
        print(f"base {args.base} advanced; measuring from merge-base {base[:10]}")
    prod, other, generated = measure(args.repo, base, args.head)
    binary = [p for p, v in prod.items() if v is None]
    rows = [v for v in prod.values() if v is not None]
    ca, cd = sum(r[0] for r in rows), sum(r[1] for r in rows)
    ra, rd = sum(r[2] for r in rows), sum(r[3] for r in rows)
    net = ca - cd
    print(f"prod {len(prod)} files net {net:+d} code (+{ca}/-{cd} code lines; raw +{ra}/-{rd}); "
          f"not counted {len(other)} entries +{sum(a for a, _ in other.values())}")
    if generated:
        print(f"GENERATED_DATA {len(generated)} files +{sum(v[0] for v in generated.values())} lines not counted (셈 밖 데이터):")
        for p, (a, gen, ver) in sorted(generated.items()):
            print(f"  {p} +{a} generate: {gen} verify: {ver}")
    if binary:
        print(f"CAP: FAIL (binary prod files: {', '.join(binary)})")
        return 1
    if len(prod) > FILES or net > NET:
        print(f"CAP: FAIL (limit {FILES} files/net +{NET} code; blank and comment-only lines not counted)")
        return 1
    print(f"CAP: PASS (remaining {FILES - len(prod)} files/+{NET - net} net code)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
