#!/usr/bin/env python3
"""Test-target integrity gate (#5003 S1).

cargo exits 0 when a libtest filter matches zero tests, so a curated CI lane
pairing the wrong target flag (e.g. `--bin agentdesk`) with a lib-only module
filter runs 0 tests while its required check stays green. This gate statically
cross-checks each workflow `cargo test` command's `--lib`/`--bin`/`--test`
selection against where the filtered module is declared (module tree walked
from Cargo.toml target roots, following `#[path = "..."]` redirections; no
compilation). A filtered command whose selected target declares no modules at
all is always flagged. Default mode is warn-only
(rc=0) until the known offenders are repaired; `--enforce` makes violations
fatal, and opt-in `--run-list-check` additionally runs
`cargo test ... -- --list` (compiles) to flag lanes selecting 0 tests.
Legitimately-empty lanes (platform `#[cfg]`) are excused via
scripts/test_target_integrity_allowlist.txt (normalized command per line).
"""

from __future__ import annotations

import argparse
import re
import shlex
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

MOD_DECL = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*([;{])"
)
ATTR_PATH = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\s*\]')
ATTR_LINE = re.compile(r"^\s*#\[")
# Options consuming a value; their value must not be read as a filter.
CARGO_VALUE_OPTIONS = {
    "-p", "--package", "--exclude", "-j", "--jobs", "--features", "--profile",
    "--target", "--target-dir", "--manifest-path", "--color", "--config",
    "--bin", "--test", "--bench", "--example",
}
TARGET_VALUE_OPTIONS = {"--bin", "--test"}
# Target selectors we cannot statically map to a module tree (skipped).
UNSUPPORTED_TARGET_OPTIONS = {"--bins", "--tests", "--bench", "--benches",
                              "--example", "--examples", "--doc"}
LIST_SUMMARY = re.compile(r"(\d+) tests?, \d+ benchmarks")


@dataclass(frozen=True)
class Violation:
    workflow: str
    line: int
    command: str
    kind: str
    detail: str

    def render(self) -> str:
        # Single line: GitHub `::warning::` annotations only surface the
        # first line, so the command must live on the same line.
        return (f"{self.workflow}:{self.line}: [{self.kind}] {self.detail} "
                f"(command: {self.command})")


def load_allowlist(path: Path) -> set[str]:
    lines = path.read_text("utf-8").splitlines() if path.is_file() else []
    return {ln.strip() for ln in lines
            if ln.strip() and not ln.strip().startswith("#")}


def discover_targets(repo_root: Path) -> dict[str, Path]:
    """Map target keys ('lib', 'bin:<name>') to their crate-root source file."""
    manifest = tomllib.loads((repo_root / "Cargo.toml").read_text("utf-8"))
    targets: dict[str, Path] = {}
    lib_path = repo_root / manifest.get("lib", {}).get("path", "src/lib.rs")
    if lib_path.is_file():
        targets["lib"] = lib_path
    package_name = manifest.get("package", {}).get("name", "")
    for bin_table in manifest.get("bin", []):
        name, path = bin_table.get("name"), bin_table.get("path")
        if name and path and (repo_root / path).is_file():
            targets[f"bin:{name}"] = repo_root / path
    main_rs = repo_root / "src/main.rs"
    if package_name and main_rs.is_file() \
            and not any(key.startswith("bin:") for key in targets):
        targets[f"bin:{package_name}"] = main_rs
    for auto_bin in sorted((repo_root / "src/bin").glob("*.rs")):
        targets.setdefault(f"bin:{auto_bin.stem}", auto_bin)
    return targets


def collect_modules(root: Path, repo_root: Path) -> dict[str, str]:
    """Walk `mod` declarations from a crate root; name -> first decl site."""
    modules: dict[str, str] = {}
    queue, seen = [root], set()
    while queue:
        source = queue.pop()
        if source in seen or not source.is_file():
            continue
        seen.add(source)
        pending_path: str | None = None
        for lineno, line in enumerate(source.read_text("utf-8").splitlines(), 1):
            attr = ATTR_PATH.match(line)
            if attr:
                pending_path = attr.group(1)
                continue
            match = MOD_DECL.match(line)
            if not match:
                # Other attributes (#[cfg], ...) may sit between #[path] and
                # the mod item; any other non-blank line detaches the attr.
                if line.strip() and not ATTR_LINE.match(line):
                    pending_path = None
                continue
            name, terminator = match.groups()
            redirect, pending_path = pending_path, None
            modules.setdefault(name, f"{source.relative_to(repo_root)}:{lineno}")
            if terminator != ";":
                continue  # inline module: same-file lines are already scanned
            if redirect is not None:
                # #[path = "..."] outside inline blocks is relative to the
                # directory of the declaring source file.
                candidates: tuple[Path, ...] = (source.parent / redirect,)
            elif source.name in ("lib.rs", "main.rs", "mod.rs"):
                base = source.parent
                candidates = (base / f"{name}.rs", base / name / "mod.rs")
            else:
                base = source.parent / source.stem
                candidates = (base / f"{name}.rs", base / name / "mod.rs")
            for candidate in candidates:
                if candidate.is_file():
                    queue.append(candidate)
                    break
    return modules


def extract_commands(workflow: Path) -> list[tuple[int, list[str], str]]:
    """Yield (line, argv, normalized) for each `cargo test` line in a workflow."""
    commands: list[tuple[int, list[str], str]] = []
    for lineno, line in enumerate(workflow.read_text("utf-8").splitlines(), 1):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        start = stripped.find("cargo test")
        if start < 0:
            continue
        snippet = stripped[start:]
        try:
            words = shlex.split(snippet, comments=True)
        except ValueError:
            # `run: "... cargo test ..."` YAML quoting leaves a dangling
            # close quote after the command; retry without it.
            if not (snippet and snippet[-1] in "\"'"):
                continue
            try:
                words = shlex.split(snippet[:-1], comments=True)
            except ValueError:
                continue
        if words[:2] != ["cargo", "test"]:
            continue
        commands.append((lineno, words, " ".join(words)))
    return commands


@dataclass(frozen=True)
class CommandSpec:
    targets: tuple[str, ...]      # 'lib', 'bin:<name>', 'test:<name>'
    filters: tuple[str, ...]
    skipped: bool                 # cannot/should not be statically judged


def parse_command(words: list[str]) -> CommandSpec:
    args = words[2:]
    before, after = args, []
    if "--" in args:
        split = args.index("--")
        before, after = args[:split], args[split + 1:]
    targets: list[str] = []
    filters: list[str] = []
    unsupported = False
    index = 0
    while index < len(before):
        token = before[index]
        if token in TARGET_VALUE_OPTIONS and index + 1 < len(before):
            kind = "bin" if token == "--bin" else "test"
            targets.append(f"{kind}:{before[index + 1]}")
            index += 2
            continue
        if token == "--lib":
            targets.append("lib")
        elif token in UNSUPPORTED_TARGET_OPTIONS or token == "--all-targets":
            unsupported = unsupported or token != "--all-targets"
            if token == "--all-targets":
                return CommandSpec((), (), skipped=True)
        elif token in CARGO_VALUE_OPTIONS:
            index += 1
        elif not token.startswith("-"):
            filters.append(token)
        index += 1
    index = 0
    while index < len(after):
        token = after[index]
        if token in ("--skip", "--test-threads", "--format", "--color", "-Z"):
            index += 1
        elif not token.startswith("-"):
            filters.append(token)
        index += 1
    # No explicit selection (all default targets run) or a target family we do
    # not model: nothing to cross-check statically.
    skipped = unsupported or not targets
    return CommandSpec(tuple(targets), tuple(filters), skipped=skipped)


def validate_command(spec: CommandSpec, inventories: dict[str, dict[str, str]],
                     repo_root: Path) -> list[tuple[str, str]]:
    """Return (kind, detail) findings for one parsed cargo test command."""
    findings: list[tuple[str, str]] = []
    selected: dict[str, str] = {}
    for target in spec.targets:
        if target.startswith("test:"):
            name = target.partition(":")[2]
            path = repo_root / "tests" / f"{name}.rs"
            if not path.is_file():
                findings.append(("unknown-target",
                                 f"--test {name}: tests/{name}.rs not found"))
                continue
            inventories.setdefault(target, collect_modules(path, repo_root))
        if target not in inventories:
            findings.append(("unknown-target",
                             f"target `{target}` not found in Cargo.toml"))
            continue
        selected.update(inventories[target])
    for filt in spec.filters:
        lead = filt.split("::", 1)[0]
        if not lead or lead in selected:
            continue
        declared_in = {
            target: modules[lead]
            for target, modules in inventories.items() if lead in modules
        }
        if declared_in:
            sites = ", ".join(f"{t} ({site})" for t, site in
                              sorted(declared_in.items()))
            findings.append(("target-mismatch", (
                f"filter `{filt}` names module `{lead}` declared in {sites}, "
                f"but the command only selects {'/'.join(spec.targets)}; the "
                f"filter matches 0 tests there and cargo still exits 0")))
        elif "::" in filt:
            findings.append(("unknown-module", (
                f"module-path filter `{filt}`: leading segment `{lead}` is "
                f"not a module in any known target")))
    if spec.filters and not selected and not findings:
        # Decisive signal: the selected target declares no modules at all, so
        # ANY filter (typo'd, ::-less, whatever) selects 0 tests there.
        findings.append(("empty-target", (
            f"selected target(s) {'/'.join(spec.targets)} declare no modules; "
            f"every libtest filter runs 0 tests there and cargo still exits 0")))
    return findings


def run_list_check(words: list[str], repo_root: Path) -> str | None:
    """Run `<command> -- --list`; return a finding detail if 0 tests match."""
    args = words[:words.index("--")] if "--" in words else list(words)
    proc = subprocess.run(args + ["--", "--list"], cwd=repo_root,
                          capture_output=True, text=True)
    if proc.returncode != 0:
        return f"--list run failed (rc={proc.returncode}): {proc.stderr[-300:]}"
    total = sum(int(count) for count in LIST_SUMMARY.findall(proc.stdout))
    return None if total else \
        "command selects 0 tests (`-- --list` reported no matches)"


def check_workflows(repo_root: Path, workflows: list[Path], allowlist: set[str],
                    with_list_check: bool) -> list[Violation]:
    inventories = {
        target: collect_modules(root, repo_root)
        for target, root in discover_targets(repo_root).items()
    }
    violations: list[Violation] = []
    for workflow in workflows:
        rel = (str(workflow.relative_to(repo_root))
               if workflow.is_relative_to(repo_root) else str(workflow))
        for lineno, words, normalized in extract_commands(workflow):
            allowlisted = normalized in allowlist
            spec = parse_command(words)
            if spec.skipped:
                continue
            findings = validate_command(spec, inventories, repo_root)
            if allowlisted:
                # The allowlist only excuses legitimately-empty lanes; a
                # target-mismatch means the command itself is wrong and must
                # be fixed, never allowlisted (enforced here, not just docs).
                findings = [(kind, detail) for kind, detail in findings
                            if kind == "target-mismatch"]
            if with_list_check and not findings and not allowlisted:
                detail = run_list_check(words, repo_root)
                if detail:
                    findings.append(("zero-match", detail))
            violations.extend(Violation(rel, lineno, normalized, kind, detail)
                              for kind, detail in findings)
    return violations


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo-root", type=Path,
                        default=Path(__file__).resolve().parents[1])
    parser.add_argument("--workflow", action="append", type=Path, default=None,
                        help="workflow file(s); default .github/workflows/*.yml")
    parser.add_argument("--allowlist", type=Path, default=None)
    parser.add_argument("--enforce", action="store_true",
                        help="exit 1 on violations (default: warn only)")
    parser.add_argument("--run-list-check", action="store_true",
                        help="also run `cargo test ... -- --list` (compiles)")
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    workflows = args.workflow or sorted(
        (repo_root / ".github/workflows").glob("*.yml"))
    allowlist = load_allowlist(args.allowlist or (
        repo_root / "scripts/test_target_integrity_allowlist.txt"))
    violations = check_workflows(repo_root,
                                 [Path(w).resolve() for w in workflows],
                                 allowlist, args.run_list_check)
    for violation in violations:
        prefix = "ERROR" if args.enforce else "::warning::"
        print(f"{prefix} {violation.render()}")
    if violations:
        mode = "enforced" if args.enforce else "warn-only rollout (#5003)"
        print(f"test-target integrity: {len(violations)} violation(s) [{mode}]")
        return 1 if args.enforce else 0
    print("test-target integrity check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
