#!/usr/bin/env python3
"""Static equivalence gate for one crate-split slice (standard library only).

    python3 scripts/crate_split/equiv_check.py --base <sha> --head <sha> [--repo .]
        [--skip-cargo] [--windows auto|always|never] [--require move|vis]
        [--exclude GLOB ...] [--allow-added-tests] [--log-dir DIR]

The first output line is the verdict, `EQUIV_OK class=<move|vis|C>` or
`EQUIV_FAIL <item>[,<item>...]`, followed by a detail table and line evidence.

  a cargo     `cargo check --workspace --all-targets` at head; also for
              x86_64-pc-windows-gnu when the diff touches a unix/windows cfg.
  b class     move: every changed line is moved (`git diff --color-moved=blocks`)
              or a use/mod item, a path-only or visibility-only rewrite, a comment
              or generated docs. vis: only visibility modifiers changed. C: anything
              else; each C line is printed as evidence. Fails only with --require.
  c tests     the static list of test function names is the same before and after
              (moved tests are listed with their old and new file).
  d features  every `feature = "..."` in a file that received moved code is
              declared in the `[features]` of that file's nearest Cargo.toml.
  e logtarget strings naming a moved-from module's `<crate>::<module>` log target
              in non-Rust files or Rust string literals that the move would stop
              matching, when the moved code logs.
  f macros    path-sensitive macros in code moved across files (include_str!,
              include_bytes!, env!("CARGO_MANIFEST_DIR"), file!, module_path!, type_name).

Module paths come from file paths; `#[path]` overrides are not followed.
"""

from __future__ import annotations

import argparse
import collections
import fnmatch
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11 falls back to a [features] key scan.
    tomllib = None

GENERATED = ("ARCHITECTURE.md", "docs/generated/*")
MANIFESTS = ("Cargo.toml", "*/Cargo.toml", "Cargo.lock")
WINDOWS_TARGET = "x86_64-pc-windows-gnu"
OLD_MOVED, NEW_MOVED = "\x1b[35m", "\x1b[36m"
ANSI = re.compile(r"\x1b\[[0-9;]*m")
HUNK = re.compile(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@")
USE_START = re.compile(r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?use\s")
MOD_DECL = re.compile(r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+\w+\s*;")
ATTR_LINE = re.compile(r"^\s*#\[.*\]\s*$")
TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|::|\S")
PLATFORM_CFG = re.compile(
    r"cfg!?\s*\(.*\b(?:unix|windows|target_os|target_family)\b|std::os::(?:unix|windows)"
)
TEST_ATTR = re.compile(
    r"^\s*#\[\s*(?:(?:tokio|async_std|sqlx|actix_rt|test_log)::)?(?:test|rstest|test_case)\b"
)
FN_NAME = re.compile(
    r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:const|async|unsafe)\s+)*fn\s+([A-Za-z_]\w*)"
)
FEATURE = re.compile(r'feature\s*=\s*"([^"]+)"')
LOG_CALL = re.compile(
    r"\b(?:trace|debug|info|warn|error|event|(?:trace|debug|info|warn|error)_span|span)!\s*[(\[{]"
    r"|#\[\s*(?:tracing::)?instrument\b|\btracing::|\blog::"
)
PATH_MACRO = re.compile(
    r'\binclude_str!|\binclude_bytes!|\benv!\s*\(\s*"CARGO_MANIFEST_DIR"|\bfile!\s*\('
    r"|\bmodule_path!\s*\(|\btype_name\b"
)


class GitError(RuntimeError):
    pass


@dataclass
class Line:
    side: str  # "-" (base) or "+" (head)
    path: str
    lineno: int
    text: str
    moved: bool
    kind: str = ""


@dataclass
class Run:
    path: str
    lines: list = field(default_factory=list)
    verdict: str = ""


def git(repo: str, *args: str, check: bool = True) -> str:
    proc = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)
    if check and proc.returncode:
        raise GitError(f"git {' '.join(args)}: {proc.stderr.strip()}")
    return proc.stdout if proc.returncode == 0 else ""


class Tree:
    """File contents at one commit, read lazily."""

    def __init__(self, repo: str, sha: str):
        self.repo, self.sha, self.cache = repo, sha, {}

    def text(self, path: str):
        if path not in self.cache:
            proc = subprocess.run(
                ["git", "-C", self.repo, "cat-file", "-p", f"{self.sha}:{path}"],
                capture_output=True,
            )
            self.cache[path] = proc.stdout.decode("utf-8", "replace") if proc.returncode == 0 else None
        return self.cache[path]

    def lines(self, path: str) -> list:
        text = self.text(path)
        return text.splitlines() if text is not None else []


def matches(path: str, globs) -> bool:
    return any(fnmatch.fnmatch(path, g) for g in globs)


def changed_files(repo: str, base: str, head: str, exclude) -> list:
    out = git(repo, "diff", "--no-renames", "--name-status", base, head)
    rows = [line.split("\t", 1) for line in out.splitlines() if line]
    return [(status, path) for status, path in rows if not matches(path, exclude)]


def diff_lines(repo: str, base: str, head: str, paths) -> list:
    """Every changed line with whether git's moved-block detection marked it."""
    if not paths:
        return []
    colors = {
        "color.diff.oldMoved": "magenta", "color.diff.newMoved": "cyan",
        "color.diff.oldMovedAlternative": "magenta", "color.diff.newMovedAlternative": "cyan",
        "color.diff.old": "red", "color.diff.new": "green",
    }
    config = [arg for key, value in colors.items() for arg in ("-c", f"{key}={value}")]
    out = subprocess.run(
        ["git", "-C", repo, *config, "diff", "--no-renames", "--no-ext-diff", "--no-relative",
         "--src-prefix=a/", "--dst-prefix=b/", "--color=always", "--color-moved=blocks",
         "--color-moved-ws=allow-indentation-change", "-U0", base, head, "--", *paths],
        capture_output=True, text=True, errors="replace",
    )
    if out.returncode:
        raise GitError(out.stderr.strip())
    result, old_path, new_path, in_header, old_no, new_no = [], "", "", False, 0, 0
    for raw in out.stdout.splitlines():
        plain = ANSI.sub("", raw)
        if plain.startswith("diff --git "):
            in_header = True
            continue
        if in_header:
            if plain.startswith("--- "):
                old_path = plain[6:] if plain.startswith("--- a/") else ""
            elif plain.startswith("+++ "):
                new_path = plain[6:] if plain.startswith("+++ b/") else ""
            elif plain.startswith("@@"):
                in_header = False
            if in_header:
                continue
        hunk = HUNK.match(plain)
        if hunk:
            old_no, new_no = int(hunk.group(1)), int(hunk.group(2))
            result.append(None)  # run boundary
            continue
        if plain.startswith("-"):
            result.append(Line("-", old_path, old_no, plain[1:], raw.startswith(OLD_MOVED)))
            old_no += 1
        elif plain.startswith("+"):
            result.append(Line("+", new_path, new_no, plain[1:], raw.startswith(NEW_MOVED)))
            new_no += 1
    return result


def use_mod_lines(lines: list) -> set:
    """1-based numbers of lines inside use items, mod declarations and their attributes."""
    marked, i = set(), 0
    while i < len(lines):
        start = i
        while i < len(lines) and ATTR_LINE.match(lines[i]):
            i += 1
        if i < len(lines) and (USE_START.match(lines[i]) or MOD_DECL.match(lines[i])):
            end = i
            while end < len(lines) and ";" not in lines[end]:
                end += 1
            marked.update(range(start + 1, end + 2))
            i = end + 1
        else:
            i = max(i, start + 1)
    return marked


def strip_visibility(tokens: list) -> list:
    out, i = [], 0
    while i < len(tokens):
        if tokens[i] == "pub":
            i += 1
            if i < len(tokens) and tokens[i] == "(":
                depth = 0
                while i < len(tokens):
                    depth += {"(": 1, ")": -1}.get(tokens[i], 0)
                    i += 1
                    if depth == 0:
                        break
            continue
        out.append(tokens[i])
        i += 1
    return out


def strip_paths(tokens: list) -> list:
    """Collapses `a::b::c` to `c`, so only the named item has to stay the same."""
    out = []
    for tok in tokens:
        if tok == "::":
            if out and out[-1] != "::":
                out.pop()
            out.append("::")
        elif out and out[-1] == "::":
            out[-1] = tok
        else:
            out.append(tok)
    return [tok for tok in out if tok != "::"]


def tokens_of(lines: list) -> list:
    return TOKEN.findall(" ".join(line.text for line in lines))


def classify_runs(entries: list, base: Tree, head: Tree):
    runs, current = [], None
    marks = {}
    for entry in entries:
        if entry is None:
            current = None
            continue
        if current is None or current.path != entry.path:
            current = Run(entry.path)
            runs.append(current)
        current.lines.append(entry)
        tree = base if entry.side == "-" else head
        stripped = entry.text.strip()
        if entry.moved:
            entry.kind = "moved"
        elif matches(entry.path, GENERATED):
            entry.kind = "generated"
        elif matches(entry.path, MANIFESTS):
            entry.kind = "manifest"
        elif not entry.path.endswith(".rs"):
            entry.kind = "other"
        elif not stripped or stripped.startswith("//"):
            entry.kind = "comment"
        else:
            key = (entry.side, entry.path)
            if key not in marks:
                marks[key] = use_mod_lines(tree.lines(entry.path))
            entry.kind = "usemod" if entry.lineno in marks[key] else "code"
    for run in runs:
        rest = [line for line in run.lines if line.kind in ("usemod", "code", "other")]
        removed = [line for line in rest if line.side == "-"]
        added = [line for line in rest if line.side == "+"]
        code_r = [line for line in removed if line.kind != "usemod"]
        code_a = [line for line in added if line.kind != "usemod"]
        if not rest:
            run.verdict = "neutral"
        elif any(line.kind == "other" for line in rest):
            run.verdict = "C"
        elif strip_visibility(tokens_of(removed)) == strip_visibility(tokens_of(added)):
            run.verdict = "vis"
        elif not code_r and not code_a:
            run.verdict = "usemod"
        elif strip_paths(strip_visibility(tokens_of(code_r))) == strip_paths(
            strip_visibility(tokens_of(code_a))
        ):
            run.verdict = "path"
        else:
            run.verdict = "C"
    return runs


def overall_class(runs: list, entries: list) -> str:
    verdicts = {run.verdict for run in runs}
    if "C" in verdicts:
        return "C"
    if any(e is not None and e.moved for e in entries) or verdicts & {"usemod", "path"}:
        return "move"
    return "vis" if "vis" in verdicts else "move"


def test_names(tree: Tree, path: str) -> list:
    lines, names = tree.lines(path), []
    for i, line in enumerate(lines):
        if TEST_ATTR.match(line):
            for follow in lines[i + 1 : i + 16]:
                found = FN_NAME.match(follow)
                if found:
                    names.append(found.group(1))
                    break
    return names


def check_tests(files, base: Tree, head: Tree):
    before, after = collections.defaultdict(list), collections.defaultdict(list)
    for status, path in files:
        if not path.endswith(".rs"):
            continue
        if status != "A":
            for name in test_names(base, path):
                before[name].append(path)
        if status != "D":
            for name in test_names(head, path):
                after[name].append(path)
    removed = sorted(n for n in before if len(after.get(n, [])) < len(before[n]))
    added = sorted(n for n in after if len(before.get(n, [])) < len(after[n]))
    moved = sorted(
        (n, ",".join(sorted(before[n])), ",".join(sorted(after[n])))
        for n in before
        if n in after and sorted(before[n]) != sorted(after[n])
    )
    count = (sum(map(len, before.values())), sum(map(len, after.values())))
    return removed, added, moved, count


def nearest_manifest(tree: Tree, path: str):
    parts = path.split("/")[:-1]
    while True:
        candidate = "/".join(parts + ["Cargo.toml"])
        if tree.text(candidate) is not None:
            return candidate
        if not parts:
            return None
        parts.pop()


def manifest_table(text: str, table: str) -> dict:
    if tomllib is not None:
        return tomllib.loads(text).get(table, {})
    section = re.search(rf"^\[{table}\]\s*$(.*?)(?=^\[|\Z)", text, re.M | re.S)
    keys = re.findall(r'^\s*"?([\w-]+)"?\s*=', section.group(1), re.M) if section else []
    return dict.fromkeys(keys, "")


def check_features(dest_files, head: Tree):
    missing = []
    for path in sorted(dest_files):
        manifest = nearest_manifest(head, path)
        declared = manifest_table(head.text(manifest), "features") if manifest else {}
        for feature in sorted(set(FEATURE.findall(head.text(path) or ""))):
            if feature not in declared:
                missing.append((path, feature, manifest or "(no Cargo.toml)"))
    return missing


def crate_name(tree: Tree, path: str) -> str:
    manifest = nearest_manifest(tree, path)
    package = manifest_table(tree.text(manifest), "package") if manifest else {}
    return str(package.get("name", "crate")).replace("-", "_")


def module_of(tree: Tree, path: str) -> str:
    manifest = nearest_manifest(tree, path) or "Cargo.toml"
    root = os.path.dirname(manifest)
    rel = os.path.relpath(path, os.path.join(root, "src") if root else "src")
    parts = rel[:-3].split(os.sep)
    if parts[-1] == "mod" or (len(parts) == 1 and parts[0] in ("lib", "main")):
        parts = parts[:-1]
    return "::".join([crate_name(tree, path), *parts])


def in_string(text: str, index: int) -> bool:
    return text[:index].count('"') % 2 == 1


def check_log_targets(repo, head_sha, entries, base: Tree, head: Tree, touched):
    moved = [e for e in entries if e is not None and e.moved]
    sources = sorted({module_of(base, e.path) for e in moved if e.side == "-"})
    dests = sorted({module_of(head, e.path) for e in moved if e.side == "+"})
    logs = sum(1 for e in moved if e.side == "+" and LOG_CALL.search(e.text))
    refs = []
    for source in sources:
        if all(dest == source for dest in dests):
            continue
        grep = git(repo, "grep", "-n", "-I", "-F", source, head_sha, check=False)
        for row in grep.splitlines():
            _, path, lineno, text = row.split(":", 3)
            for found in re.finditer(re.escape(source) + r"(?![A-Za-z0-9_])", text):
                if path.endswith(".rs") and not in_string(text, found.start()):
                    continue
                exact = text[found.end() : found.end() + 1] == '"'
                broken = any(not (dest + "::").startswith(source + "::") for dest in dests)
                risky = broken or exact
                refs.append((path, lineno, source, "risky" if risky else "covered", path in touched))
    failing = [r for r in refs if r[3] == "risky" and not r[4]] if logs else []
    return sources, dests, logs, refs, failing


def check_macros(entries):
    moved = [e for e in entries if e is not None and e.moved]
    files = {(e.side, e.path) for e in moved}
    same_file = {p for _, p in files} and all(("-", p) in files and ("+", p) in files for _, p in files)
    hits = [e for e in moved if e.side == "+" and PATH_MACRO.search(e.text)]
    return ([] if same_file else hits), hits


def run_cargo(repo, head_sha, log_dir, targets):
    results = []
    actual = git(repo, "rev-parse", "HEAD").strip()
    dirty = git(repo, "status", "--porcelain", "--untracked-files=no").strip()
    if actual != head_sha or dirty:
        return [("host", None, f"worktree is not a clean checkout of {head_sha[:10]} (HEAD {actual[:10]})", 0)]
    os.makedirs(log_dir, exist_ok=True)
    for target in targets:
        cmd = ["cargo", "check", "--workspace", "--all-targets"]
        name = "host"
        if target:
            cmd += ["--target", target]
            name = target
        log = os.path.join(log_dir, f"cargo-check-{name}.log")
        with open(log, "w") as handle:
            rc = subprocess.run(cmd, cwd=repo, stdout=handle, stderr=subprocess.STDOUT).returncode
        with open(log, errors="replace") as handle:
            warnings = sum(1 for line in handle if line.startswith("warning"))
        results.append((name, rc, log, warnings))
    return results


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--repo", default=".")
    parser.add_argument("--skip-cargo", action="store_true")
    parser.add_argument("--windows", choices=("auto", "always", "never"), default="auto")
    parser.add_argument("--require", choices=("move", "vis"))
    parser.add_argument("--exclude", action="append", default=[])
    parser.add_argument("--allow-added-tests", action="store_true")
    parser.add_argument("--log-dir")
    args = parser.parse_args(argv)
    repo = os.path.abspath(args.repo)
    try:
        base_sha = git(repo, "rev-parse", "--verify", f"{args.base}^{{commit}}").strip()
        head_sha = git(repo, "rev-parse", "--verify", f"{args.head}^{{commit}}").strip()
        files = changed_files(repo, base_sha, head_sha, args.exclude)
        base, head = Tree(repo, base_sha), Tree(repo, head_sha)
        entries = diff_lines(repo, base_sha, head_sha, [p for _, p in files])
    except GitError as err:
        print(f"EQUIV_FAIL git ({err})")
        return 2
    lines = [e for e in entries if e is not None]
    runs = classify_runs(entries, base, head)
    klass = overall_class(runs, entries)
    removed, added, moved_tests, test_count = check_tests(files, base, head)
    dest_files = {e.path for e in lines if e.moved and e.side == "+" and e.path.endswith(".rs")}
    dest_files |= {p for s, p in files if s == "A" and p.endswith(".rs")}
    missing = check_features(dest_files, head)
    touched = {p for _, p in files}
    sources, dests, logs, refs, risky_refs = check_log_targets(repo, head_sha, entries, base, head, touched)
    macro_fail, macro_hits = check_macros(entries)

    windows = args.windows == "always" or (
        args.windows == "auto" and any(e.path.endswith(".rs") and PLATFORM_CFG.search(e.text) for e in lines)
    )
    rows, failed = [], []
    if args.skip_cargo:
        rows.append(("a cargo", "NOT_RUN", f"--skip-cargo (windows target {'needed' if windows else 'not needed'})"))
        failed.append("a")
    else:
        log_dir = args.log_dir or os.path.join(repo, "target", "equiv_check")
        results = run_cargo(repo, head_sha, log_dir, [None] + ([WINDOWS_TARGET] if windows else []))
        ok = all(rc == 0 for _, rc, _, _ in results)
        detail = "; ".join(
            f"{name} rc={rc} warning_lines={warn} log={log}" if rc is not None else log
            for name, rc, log, warn in results
        )
        rows.append(("a cargo", "PASS" if ok else "FAIL", detail))
        if not ok:
            failed.append("a")
    counts = collections.Counter(e.kind for e in lines)
    verdicts = collections.Counter(run.verdict for run in runs)
    class_ok = args.require is None or klass == args.require
    rows.append(("b class", klass if class_ok else f"FAIL ({klass}, required {args.require})",
                 " ".join(f"{k}={v}" for k, v in sorted(counts.items())) + " | runs "
                 + " ".join(f"{k}={v}" for k, v in sorted(verdicts.items()))))
    if not class_ok:
        failed.append("b")
    tests_ok = not removed and (args.allow_added_tests or not added)
    rows.append(("c tests", "PASS" if tests_ok else "FAIL",
                 f"changed-file tests base={test_count[0]} head={test_count[1]} moved={len(moved_tests)}"
                 f" removed={removed or 0} added={added or 0}"))
    if not tests_ok:
        failed.append("c")
    rows.append(("d features", "FAIL" if missing else "PASS",
                 f"files={len(dest_files)} missing={[(p, f) for p, f, _ in missing] or 0}"))
    if missing:
        failed.append("d")
    rows.append(("e logtarget", "FAIL" if risky_refs else "PASS",
                 f"from={sources or '-'} to={dests or '-'} moved_log_calls={logs} refs={len(refs)} risky={len(risky_refs)}"))
    if risky_refs:
        failed.append("e")
    rows.append(("f macros", "FAIL" if macro_fail else "PASS",
                 f"in_moved_lines={len(macro_hits)} cross_file={len(macro_fail)}"))
    if macro_fail:
        failed.append("f")

    print(f"EQUIV_FAIL {','.join(failed)}" if failed else f"EQUIV_OK class={klass}")
    print(f"base={base_sha} head={head_sha} files={len(files)}")
    print("| check | result | detail |\n|---|---|---|")
    for check, result, detail in rows:
        print(f"| {check} | {result} | {detail} |")
    print("\n## b. non-moved line evidence (run verdict, side, file:line, text)")
    for run in runs:
        for line in run.lines:
            if line.kind in ("code", "usemod", "other"):
                print(f"{run.verdict:7s} {line.side} {line.path}:{line.lineno} [{line.kind}] {line.text.strip()}")
    for title, items in (
        ("c. moved tests (name, from, to)", moved_tests),
        ("d. missing features (file, feature, manifest)", missing),
        ("e. log target references (file, line, target, status, touched)", refs),
        ("f. path-sensitive macros in moved lines", [(e.path, e.lineno, e.text.strip()) for e in macro_hits]),
    ):
        if items:
            print(f"\n## {title}")
            for item in items:
                print("  " + " | ".join(map(str, item)))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
