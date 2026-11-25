#!/usr/bin/env python3
"""Machine check of a KRX-TS conversion commit (#7353): the diff is test_support_codemod.py's output, nothing else.

    python3 scripts/crate_split/test_support_verify.py <base> <head> [--repo DIR]

<base> is the commit the codemod ran on (the tool commit), <head> the conversion. PASS needs all four:

  a diff      every changed file is a modified `.rs` file, and every hunk pairs each removed line with one
              added line that equals it except for one allowed substitution: the `test` atom of a leading
              `#[cfg(test)]`, or a direct argument `test` of a leading `#[cfg(all(..))]`, spelled
              `any(test, feature = "test-support")`. Indentation, spacing and trailing text stay byte for byte.
  b count     substitutions = the codemod's `scanned - excluded` on <base>, and the codemod's edits on <base>
              are exactly the changed lines (file, line, before, after).
  c exclude   test_support_exclude.json is the same at both commits, and every item it lists has the same
              attribute block and first item line at <head> as at <base>.
  d idem      the codemod on <head> has no edit left.

The first output line is `TS_VERIFY PASS ...` or `TS_VERIFY FAIL <checks>`; exit 0 only on PASS.
The trees are read from git (`git archive` into a temporary directory), never from the working tree.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import test_support_codemod as codemod  # noqa: E402

ATOM = 'any(test, feature = "test-support")'
EXCLUDE = "scripts/crate_split/test_support_exclude.json"
TREE_PATHS = ("Cargo.toml", "src", "crates", "scripts/crate_split/partition.json", EXCLUDE)
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")


def git(repo, *args, check=True):
    proc = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    if check and proc.returncode:
        raise RuntimeError(f"git {' '.join(args)}: {proc.stderr.strip()}")
    return proc.stdout


def extract(repo, sha, dest):
    present = [p for p in TREE_PATHS if git(repo, "ls-tree", "--name-only", sha, "--", p).strip()]
    archive = subprocess.Popen(["git", "-C", str(repo), "archive", "--format=tar", sha, "--", *present],
                               stdout=subprocess.PIPE)
    tar = subprocess.run(["tar", "-x", "-C", str(dest)], stdin=archive.stdout, capture_output=True)
    archive.stdout.close()
    if archive.wait() or tar.returncode:
        raise RuntimeError(f"extract {sha}: {tar.stderr.decode(errors='replace')[-300:]}")


def allowed(old, new):
    """Whether `new` is `old` with exactly one allowed `test` -> ATOM substitution (own rule, not the codemod's)."""
    m = re.match(r"^\s*#\[cfg\(", old)
    if not m or len(new) != len(old) + len(ATOM) - 4:
        return False
    for i in (k.start() for k in re.finditer(r"test", old)):
        if old[:i] + ATOM + old[i + 4:] != new:
            continue
        before, after = old[m.end():i], old[i + 4:]
        if re.match(r"[\w-]", after[:1]) or re.search(r"[\w\"-]$", before):
            return False
        if before.strip() == "" and re.match(r"^\s*\)\]", after):
            return True  # #[cfg(test)]
        depth = 0
        for ch in re.sub(r'"[^"]*"', '""', before):
            depth += (ch == "(") - (ch == ")")
        return depth == 1 and re.match(r"^\s*all\s*\(", before) is not None and \
            re.search(r"(?:\(|,)\s*$", before) is not None and re.match(r"^\s*[,)]", after) is not None
    return False


def diff_pairs(repo, base, head):
    """([(file, old line no, old, new)], [problems]) of the base..head diff."""
    problems, pairs = [], []
    for row in git(repo, "diff", "--no-renames", "--name-status", base, head).splitlines():
        status, path = row.split("\t", 1)
        if status != "M" or not path.endswith(".rs"):
            problems.append(f"{status} {path}: only modified .rs files may change")
    text = git(repo, "diff", "--no-renames", "--no-color", "--no-ext-diff", "-U0", base, head, "--", "*.rs")
    path, old, new, old_no = None, [], [], 0

    def flush():
        if path is None or not (old or new):
            return
        if len(old) != len(new):
            problems.append(f"{path}:{old_no}: hunk removes {len(old)} lines and adds {len(new)}")
            return
        for k, (a, b) in enumerate(zip(old, new)):
            if not allowed(a, b):
                problems.append(f"{path}:{old_no + k}: not an allowed substitution: {a.strip()!r} -> {b.strip()!r}")
            else:
                pairs.append((path, old_no + k, a, b))

    for line in text.split("\n"):
        if line.startswith("diff --git "):
            flush()
            path, old, new = None, [], []
        elif line.startswith("+++ b/"):
            path = line[6:]
        elif line.startswith("@@"):
            flush()
            old, new = [], []
            old_no = int(HUNK.match(line).group(1))
        elif line.startswith("-") and not line.startswith("--- "):
            old.append(line[1:])
        elif line.startswith("+") and not line.startswith("+++ "):
            new.append(line[1:])
        elif line.startswith("\\"):
            problems.append(f"{path}: end-of-file newline changed")
    flush()
    return pairs, problems


def item_texts(tree, entry):
    """Attribute block and first line of every item an exclusion entry names, as written."""
    try:
        found = codemod.cfg_items(tree, entry["file"]).get(entry["item"], [])
    except OSError:
        return None
    lines = tree.lines(entry["file"])
    return [lines[codemod.vt.attr_start(lines, k):k + 1] for k in found] or None


def verify(repo, base, head):
    base = git(repo, "rev-parse", "--verify", f"{base}^{{commit}}").strip()
    head = git(repo, "rev-parse", "--verify", f"{head}^{{commit}}").strip()
    rows, failed = [], []
    with tempfile.TemporaryDirectory(prefix="ts-verify-") as tmp:
        bdir, hdir = Path(tmp) / "base", Path(tmp) / "head"
        bdir.mkdir()
        hdir.mkdir()
        extract(repo, base, bdir)
        extract(repo, head, hdir)
        pairs, problems = diff_pairs(repo, base, head)
        rows.append(("a diff", "FAIL" if problems else "PASS", f"subs={len(pairs)} problems={len(problems)}",
                     problems))
        # c first: it needs only labels.
        bex, hex_ = (d / EXCLUDE for d in (bdir, hdir))
        problems = []
        if not bex.is_file() or not hex_.is_file() or bex.read_bytes() != hex_.read_bytes():
            problems.append(f"{EXCLUDE} differs between the commits or is missing")
            entries, bundles = [], []
        else:
            try:
                entries, bundles = codemod.load_exclude(bex)
            except codemod.CodemodError as err:
                problems += err.lines
                entries, bundles = [], []
        btree, htree = codemod.Tree(bdir), codemod.Tree(hdir)
        for e in entries:
            before, after = item_texts(btree, e), item_texts(htree, e)
            if before is None or before != after:
                problems.append(f"excluded item changed or missing: {e['file']} :: {e['item']}")
        rows.append(("c exclude", "FAIL" if problems else "PASS", f"entries={len(entries)} changed={len(problems)}",
                     problems))
        problems, counts = [], {}
        try:
            result = codemod.plan(btree, entries, bundles)
            counts = result["counts"]
            want = {(e["file"], e["line"], e["before"], e["after"]) for e in result["edits"]}
            got = set(pairs)
            if len(pairs) != counts["scanned"] - counts["excluded"]:
                problems.append(f"subs {len(pairs)} != scanned {counts['scanned']} - excluded {counts['excluded']}")
            problems += [f"codemod edit missing from the diff: {f}:{n}" for f, n, *_ in sorted(want - got)]
            problems += [f"diff line the codemod does not make: {f}:{n}" for f, n, *_ in sorted(got - want)]
        except codemod.CodemodError as err:
            problems.append(f"codemod on base failed (code {err.code}): {err.lines[:3]}")
        rows.append(("b count", "FAIL" if problems else "PASS",
                     " ".join(f"{k}={counts[k]}" for k in ("scanned", "excluded", "listed", "deferred", "subs")
                              if k in counts), problems))
        problems = []
        try:
            left = codemod.plan(htree, entries, bundles)["edits"]
            problems += [f"edit left at head: {e['file']}:{e['line']}" for e in left]
        except codemod.CodemodError as err:
            problems.append(f"codemod on head failed (code {err.code}): {err.lines[:3]}")
            left = None
        rows.append(("d idem", "FAIL" if problems else "PASS", f"edits={'?' if left is None else len(left)}",
                     problems))
    failed = [name.split()[0] for name, result, _, _ in rows if result != "PASS"]
    if failed:
        print(f"TS_VERIFY FAIL {','.join(failed)}")
    else:
        print(f"TS_VERIFY PASS subs={len(pairs)} scanned={counts['scanned']} excluded={counts['excluded']} idem=0")
    print(f"base={base} head={head}")
    print("| check | result | detail |\n|---|---|---|")
    for name, result, detail, _ in rows:
        print(f"| {name} | {result} | {detail} |")
    for name, _, _, problems in rows:
        for p in problems[:40]:
            print(f"{name.split()[0]}  {p}")
    return 1 if failed else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("base")
    parser.add_argument("head")
    parser.add_argument("--repo", default=".")
    args = parser.parse_args(argv)
    try:
        return verify(Path(args.repo).resolve(), args.base, args.head)
    except RuntimeError as err:
        print(f"TS_VERIFY FAIL git ({err})")
        return 2


if __name__ == "__main__":
    sys.exit(main())
