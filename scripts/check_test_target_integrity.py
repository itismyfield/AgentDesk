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

`--write-lib-inventory-manifest` is the explicit regeneration path: it writes
the sorted static lib-test identity manifest without invoking cargo. Verification
never writes the manifest; it compares the checked-in names with the current
source inventory and reports both sides of the diff.
"""

from __future__ import annotations

import argparse
import hashlib
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
JOB_HEADER = re.compile(r"^  ([A-Za-z0-9_-]+):(?:\s*#.*)?$")
RECIPE_HEADER = re.compile(r"^([A-Za-z0-9_-]+):(?:\s.*)?$")
EVIDENCE_LINE = re.compile(r"^selection-evidence: selected=(\d+) command=(.*)$")
SUMMARY_KEYS = {"invocations", "nonzero", "findings", "extraction_errors", "execution_errors"}

# The static parser intentionally sees platform-gated tests from every host.
# libtest only lists tests compiled for the current host, and include!()-based
# tests are outside this parser. These exact, named differences are reviewed
# data; any drift on either side fails --verify-lib-inventory.
LIB_INVENTORY_STATIC_ONLY_BASE = frozenset({
    "cli::discord_thread_create::tests::thread_create_lock_cancel_child_process",
    "cli::discord_thread_create::tests::windows_async_waiter_recovers_abandoned_owner",
    "cli::discord_thread_create::tests::windows_cancelled_async_holder_releases_before_runtime_exit",
    "cli::discord_thread_create::tests::windows_lock_uses_global_current_sid_named_mutex",
    "cli::discord_thread_create_lock::windows::tests::wait_status_accepts_normal_and_abandoned_but_reports_errors",
    "services::discord::placeholder_sweeper::abandon_guard::tests::claude_e_process_cleanup_is_fail_closed_without_unix_probe",
})
LIB_INVENTORY_STATIC_ONLY_BY_PLATFORM = {
    "darwin": frozenset({
        "services::process::simple_cancel_watcher_tests::linux_proc_stat_parser_handles_comm_with_spaces_and_parens",
    }),
    "linux": frozenset({
        "cli::init::launchd_plist_tests::clamp_launchd_nofile_soft_limit_never_exceeds_host_hard_limit",
        "cli::init::launchd_plist_tests::generate_launchd_plist_release_sets_clamped_soft_number_of_files_limit",
        "cli::init::launchd_plist_tests::generate_launchd_plist_uses_requested_fresh_home_and_root_only",
        "services::platform::binary_resolver::tests::codex_fallback_dirs_include_app_bundle_resources_on_macos",
    }),
}
LIB_INVENTORY_KNOWN_CARGO_ONLY = frozenset({
    "services::discord::tmux::restored_turn_injected_anchor_tests::task_notification_kind_restart_invariant_tests::task_notification_kind_restart_roundtrip_4253",
    "services::discord::tmux::tmux_output_stream::tests::provider_output_guard_tests::invariant_4371_raw_claude_jsonl_reaches_last_mile_guard_without_leaking",
    "services::discord::tmux::tmux_watcher::terminal_direct_fallback::tests::committed_cleanup_preserves_tracking_until_delete_commits_4508",
    "services::discord::tmux::tmux_watcher::terminal_direct_fallback::tests::committed_edit_failure_cleanup_has_controller_legacy_parity_4508",
    "services::discord::tmux::tmux_watcher::terminal_direct_fallback::tests::legacy_edit_failure_revalidation_precedes_fallback_post_4508",
    "services::discord::tmux::tmux_watcher::terminal_direct_fallback::tests::recovered_task_response_identity_is_stable_without_inflight_or_context",
    "services::discord::tmux::tmux_watcher::terminal_direct_fallback::tests::watcher_task_response_wiring_prepares_reference_before_send_and_marks_after_frontier",
})
LIB_INVENTORY_MANIFEST_REL = Path("scripts/lib_test_inventory_manifest.txt")
LIB_INVENTORY_MANIFEST_HEADER = (
    "# Generated by scripts/check_test_target_integrity.py "
    "--write-lib-inventory-manifest."
)
LIB_INVENTORY_MANIFEST_RULES = (
    "# Manifest rows are unique; cfg-disjoint source duplicates are "
    "canonicalized to one row.",
    "# Rows are sorted by bytewise UTF-8 ascending order (locale-independent).",
    "# Encoding is UTF-8, line endings are LF, and the file ends with one LF.",
)


def _byte_sort_key(value: str) -> bytes:
    """Use locale-independent byte ordering for manifest entries."""
    return value.encode("utf-8")


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


@dataclass(frozen=True)
class RustToken:
    value: str
    line: int
    kind: str = "punct"


def _rust_tokens(text: str) -> list[RustToken]:
    """Tokenize the Rust subset needed for attrs, items, and brace scopes."""
    tokens: list[RustToken] = []
    index = 0
    line = 1
    while index < len(text):
        char = text[index]
        if char.isspace():
            line += char == "\n"
            index += 1
            continue
        if text.startswith("//", index):
            end = text.find("\n", index)
            index = len(text) if end < 0 else end
            continue
        if text.startswith("/*", index):
            depth = 1
            index += 2
            while index < len(text) and depth:
                if text.startswith("/*", index):
                    depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    line += text[index] == "\n"
                    index += 1
            continue
        raw = re.match(r'r(#{0,255})"', text[index:])
        if raw:
            marker = '"' + raw.group(1)
            start_line = line
            start = index + raw.end()
            end = text.find(marker, start)
            if end < 0:
                end = len(text)
                index = len(text)
            else:
                index = end + len(marker)
            value = text[start:end]
            line += value.count("\n")
            tokens.append(RustToken(value, start_line, "string"))
            continue
        if char == '"':
            start_line = line
            index += 1
            value = []
            while index < len(text):
                if text[index] == "\\" and index + 1 < len(text):
                    value.extend(text[index:index + 2])
                    line += text[index + 1] == "\n"
                    index += 2
                elif text[index] == '"':
                    index += 1
                    break
                else:
                    value.append(text[index])
                    line += text[index] == "\n"
                    index += 1
            tokens.append(RustToken("".join(value), start_line, "string"))
            continue
        if char == "'":
            literal = re.match(r"'(?:\\.|[^\\'\n])'", text[index:])
            if literal:
                index += literal.end()
                continue
        ident = re.match(r"[A-Za-z_][A-Za-z0-9_]*", text[index:])
        if ident:
            tokens.append(RustToken(ident.group(), line, "ident"))
            index += ident.end()
            continue
        if char in "#[]{}();=:":
            tokens.append(RustToken(char, line))
        index += 1
    return tokens


@dataclass(frozen=True)
class StaticTestInventory:
    tests: dict[str, str]
    module_errors: dict[str, str]
    # Multiple cfg-disjoint source declarations can share one full Rust test
    # ID. The manifest pins the canonical set identity once; retain the source
    # sites for diagnostics instead of silently losing that fact.
    duplicate_tests: tuple[tuple[str, str, str], ...] = ()


def collect_static_tests(root: Path, repo_root: Path) -> StaticTestInventory:
    """Collect full #[test]/#[tokio::test] identities without compiling."""
    tests: dict[str, str] = {}
    module_errors: dict[str, str] = {}
    duplicate_tests: list[tuple[str, str, str]] = []
    queue: list[tuple[Path, tuple[str, ...], Path]] = [(root, (), root.parent)]
    seen: set[tuple[Path, tuple[str, ...]]] = set()
    while queue:
        source, outer, base_dir = queue.pop()
        identity = (source.resolve(), outer)
        if identity in seen or not source.is_file():
            continue
        seen.add(identity)
        tokens = _rust_tokens(source.read_text("utf-8"))
        scopes: list[tuple[int, str, Path]] = []
        depth = 0
        pending_path: str | None = None
        pending_test = False
        index = 0
        while index < len(tokens):
            token = tokens[index]
            current_names = outer + tuple(scope[1] for scope in scopes)
            current_dir = scopes[-1][2] if scopes else base_dir
            if token.value == "#" and index + 1 < len(tokens) \
                    and tokens[index + 1].value == "[":
                end = index + 2
                attr_depth = 1
                while end < len(tokens) and attr_depth:
                    attr_depth += tokens[end].value == "["
                    attr_depth -= tokens[end].value == "]"
                    end += 1
                attr = tokens[index + 2:end - 1]
                path_end = next((offset for offset, item in enumerate(attr)
                                 if item.value in ("(", "=", "]")), len(attr))
                attr_path = tuple(item.value for item in attr[:path_end]
                                  if item.kind == "ident")
                pending_test = pending_test or attr_path in {
                    ("test",), ("tokio", "test"),
                }
                if attr_path == ("path",):
                    string = next((item.value for item in attr
                                   if item.kind == "string"), None)
                    pending_path = string or pending_path
                index = end
                continue
            if token.value == "fn" and token.kind == "ident" \
                    and index + 1 < len(tokens):
                name = tokens[index + 1]
                if pending_test and name.kind == "ident":
                    test_id = "::".join(current_names + (name.value,))
                    site = f"{source.relative_to(repo_root)}:{token.line}"
                    previous = tests.get(test_id)
                    if previous is None:
                        tests[test_id] = site
                    else:
                        duplicate_tests.append((test_id, previous, site))
                pending_path = None
                pending_test = False
            elif token.value == "mod" and token.kind == "ident" \
                    and index + 2 < len(tokens) \
                    and tokens[index + 1].kind == "ident":
                name = tokens[index + 1].value
                cursor = index + 2
                while cursor < len(tokens) and tokens[cursor].value not in (";", "{"):
                    cursor += 1
                if cursor < len(tokens):
                    path_names = current_names + (name,)
                    site = f"{source.relative_to(repo_root)}:{token.line}"
                    if tokens[cursor].value == "{":
                        depth += 1
                        scopes.append((depth, name, current_dir / name))
                        index = cursor + 1
                        pending_path = None
                        pending_test = False
                        continue
                    candidates = ((source.parent / pending_path,
                                   current_dir / pending_path)
                                  if pending_path else
                                  (current_dir / f"{name}.rs",
                                   current_dir / name / "mod.rs"))
                    child = next((item for item in candidates if item.is_file()), None)
                    if child:
                        redirected = pending_path is not None
                        child_dir = (child.parent
                                     if redirected or child.name == "mod.rs"
                                     else child.parent / child.stem)
                        queue.append((child, path_names, child_dir))
                    else:
                        missing = ", ".join(
                            str(item.relative_to(repo_root))
                            if item.is_relative_to(repo_root) else str(item)
                            for item in candidates
                        )
                        module_errors["::".join(path_names)] = f"{site} (missing {missing})"
                    pending_path = None
                    pending_test = False
            if token.kind == "punct" and token.value == "{":
                depth += 1
            elif token.kind == "punct" and token.value == "}":
                depth -= 1
                while scopes and scopes[-1][0] > depth:
                    scopes.pop()
            elif token.kind == "punct" and token.value == ";":
                pending_path = None
                pending_test = False
            index += 1
    return StaticTestInventory(tests, module_errors, tuple(duplicate_tests))


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


def _command_lines(text: str) -> list[list[str]]:
    commands: list[list[str]] = []
    for line in text.splitlines():
        try:
            words = shlex.split(line.strip(), comments=True)
        except ValueError as error:
            raise RuntimeError(f"could not parse command: {line.strip()}") from error
        if words[:1] == ["run:"]:
            words = words[1:]
        if any(words[index:index + 2] == ["cargo", "test"]
               for index in range(len(words) - 1)):
            commands.append(words)
    return commands


def _recipe_commands(justfile: Path, recipe: str) -> list[list[str]]:
    body: list[str] = []
    active = False
    for line in justfile.read_text("utf-8").splitlines():
        header = RECIPE_HEADER.match(line)
        if header and not line[0].isspace():
            if active:
                break
            active = header.group(1) == recipe
        elif active:
            body.append(line)
    return _command_lines("\n".join(body))


def curated_commands(repo_root: Path, workflow: Path,
                     jobs: set[str]) -> list[list[str]]:
    selected: list[str] = []
    active = False
    for line in workflow.read_text("utf-8").splitlines():
        header = JOB_HEADER.match(line)
        if header:
            active = header.group(1) in jobs
        if active:
            selected.append(line)
    text = "\n".join(selected)
    commands = _command_lines(text)
    for line in text.splitlines():
        try:
            words = shlex.split(line.strip(), comments=True)
        except ValueError:
            continue
        if words[:1] == ["run:"]:
            words = words[1:]
        if words[:2] == ["just", "test-postgres"]:
            commands.extend(_recipe_commands(repo_root / "justfile", words[1]))
    return commands


def _test_ids(output: str) -> set[str]:
    return {line.rsplit(": ", 1)[0].strip() for line in output.splitlines()
            if line.strip().endswith(": test")}


def render_lib_inventory_manifest(test_ids: set[str] | frozenset[str] |
                                  list[str] | tuple[str, ...]) -> str:
    """Render the canonical UTF-8/LF lib-test manifest."""
    entries = sorted(test_ids, key=_byte_sort_key)
    if len(entries) != len(set(entries)):
        raise ValueError("lib inventory manifest cannot contain duplicate test IDs")
    lines = [LIB_INVENTORY_MANIFEST_HEADER, *LIB_INVENTORY_MANIFEST_RULES,
             "[tests]", *entries]
    return "\n".join(lines) + "\n"


def parse_lib_inventory_manifest(text: str, source: str = "manifest") \
        -> frozenset[str]:
    """Parse and validate the canonical lib-test manifest text.

    Entries are deliberately unique and bytewise sorted. The parser accepts
    comments/blanks only before the [tests] section, matching the checked-in
    PG manifest convention while keeping the identity section deterministic.
    """
    lines = text.splitlines(keepends=True)
    if not lines or not lines[-1].endswith("\n"):
        raise ValueError(f"{source}: manifest must end with a final LF")
    if any(not line.endswith("\n") or "\r" in line for line in lines):
        raise ValueError(f"{source}: manifest must use LF line endings (no CR)")
    logical = [line[:-1] for line in lines]
    if logical[0] != LIB_INVENTORY_MANIFEST_HEADER:
        raise ValueError(f"{source}: invalid generated-manifest header")
    current: str | None = None
    entries: list[str] = []
    for lineno, line in enumerate(logical, 1):
        if lineno == 1:
            continue
        if line in LIB_INVENTORY_MANIFEST_RULES:
            if current is not None:
                raise ValueError(
                    f"{source}:{lineno}: comments/blanks are not allowed in [tests]"
                )
            continue
        if not line or line.startswith("#"):
            if current is not None:
                raise ValueError(
                    f"{source}:{lineno}: comments/blanks are not allowed in [tests]"
                )
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1]
            if section != "tests" or current is not None:
                raise ValueError(f"{source}:{lineno}: expected one [tests] section")
            current = section
            continue
        if current != "tests":
            raise ValueError(f"{source}:{lineno}: test ID outside [tests]")
        if line != line.strip() or any(char.isspace() for char in line):
            raise ValueError(f"{source}:{lineno}: test ID must be one non-whitespace token")
        entries.append(line)
    if current != "tests":
        raise ValueError(f"{source}: missing [tests] section")
    if len(entries) != len(set(entries)):
        duplicates = sorted(
            {entry for entry in entries if entries.count(entry) > 1},
            key=_byte_sort_key,
        )
        raise ValueError(
            f"{source}: duplicate test IDs are forbidden: {', '.join(duplicates)}"
        )
    expected_order = sorted(entries, key=_byte_sort_key)
    if entries != expected_order:
        raise ValueError(
            f"{source}: [tests] entries must be sorted by bytewise UTF-8 order"
        )
    return frozenset(entries)


def load_lib_inventory_manifest(path: Path) -> frozenset[str]:
    """Read the manifest bytes and enforce UTF-8, LF, and final-LF rules."""
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError(f"{path}: manifest must be valid UTF-8 ({error})") from error
    return parse_lib_inventory_manifest(text, str(path))


def lib_inventory_regeneration_command() -> str:
    return (
        "python3 scripts/check_test_target_integrity.py "
        "--write-lib-inventory-manifest"
    )


@dataclass(frozen=True)
class InventoryComparison:
    static_only: frozenset[str]
    cargo_only: frozenset[str]
    static_ids: frozenset[str]
    cargo_ids: frozenset[str]
    module_errors: tuple[tuple[str, str], ...] = ()
    execution_error: str | None = None
    duplicate_tests: tuple[tuple[str, str, str], ...] = ()


def _identity_digest(test_ids: frozenset[str] | set[str]) -> str:
    return hashlib.sha256(
        "\n".join(sorted(test_ids, key=_byte_sort_key)).encode("utf-8")
    ).hexdigest()


def expected_lib_static_only(platform_name: str) -> frozenset[str] | None:
    platform_only = LIB_INVENTORY_STATIC_ONLY_BY_PLATFORM.get(platform_name)
    return None if platform_only is None else \
        LIB_INVENTORY_STATIC_ONLY_BASE | platform_only


def compare_lib_inventory(repo_root: Path, runner=None) -> InventoryComparison:
    """Compare static full lib test IDs with compiled libtest `--list` IDs."""
    inventory = collect_static_tests(discover_targets(repo_root)["lib"], repo_root)
    static_ids = frozenset(inventory.tests)
    module_errors = tuple(sorted(inventory.module_errors.items()))
    duplicate_tests = tuple(inventory.duplicate_tests)
    runner = runner or subprocess.run
    argv = ["cargo", "test", "--manifest-path", str(repo_root / "Cargo.toml"),
            "--lib", "--", "--list"]
    try:
        proc = runner(argv, cwd=repo_root, capture_output=True, text=True)
    except OSError as error:
        return InventoryComparison(frozenset(), frozenset(), static_ids,
                                   frozenset(), module_errors,
                                   f"cargo could not start: {error}",
                                   duplicate_tests)
    if proc.returncode:
        detail = proc.stderr[-1000:].strip() or proc.stdout[-1000:].strip()
        return InventoryComparison(
            frozenset(), frozenset(), static_ids, frozenset(), module_errors,
            f"cargo list failed (rc={proc.returncode}): {detail}",
            duplicate_tests,
        )
    cargo_ids = frozenset(_test_ids(proc.stdout))
    return InventoryComparison(
        static_ids - cargo_ids, cargo_ids - static_ids,
        static_ids, cargo_ids, module_errors,
        duplicate_tests=duplicate_tests,
    )


def observe_curated(repo_root: Path, workflow: Path, jobs: set[str], runner=None
                    ) -> list[tuple[list[str], int, str | None]]:
    runner = runner or subprocess.run
    observations = []
    for words in curated_commands(repo_root, workflow, jobs):
        cargo_index = next(index for index in range(len(words) - 1)
                           if words[index:index + 2] == ["cargo", "test"])
        argv = list(words)
        if "--" not in argv[cargo_index + 2:]:
            argv.append("--")
        try:
            plain = runner(argv + ["--list"], cwd=repo_root,
                           capture_output=True, text=True)
            ignored = runner(argv + ["--list", "--ignored"], cwd=repo_root,
                             capture_output=True, text=True)
        except OSError as error:
            observations.append((words, 0, f"process could not start: {error}"))
            continue
        selected = _test_ids(plain.stdout) - _test_ids(ignored.stdout)
        detail = None
        if plain.returncode or ignored.returncode:
            detail = (f"list execution failed (plain rc={plain.returncode}, "
                      f"ignored rc={ignored.returncode})")
        elif not selected:
            detail = "selection has 0 non-ignored test ids"
        observations.append((words, len(selected), detail))
    return observations


def evidence_verification_errors(rendered: str) -> list[str]:
    """Recompute the observer summary from its detailed evidence lines."""
    errors: list[str] = []
    summary_lines = [line for line in rendered.splitlines()
                     if line.startswith("selection-evidence summary:")]
    if len(summary_lines) != 1:
        return [f"expected exactly one summary, found {len(summary_lines)}"]
    fields = {}
    for word in summary_lines[0].split():
        key, separator, value = word.partition("=")
        if separator and value.isdigit():
            if key in fields:
                errors.append(f"duplicate summary counter: {key}")
            fields[key] = int(value)
    if set(fields) != SUMMARY_KEYS:
        errors.append("summary must contain exactly the five required counters")
    observations = []
    warnings = []
    for line in rendered.splitlines():
        if line.startswith("selection-evidence:"):
            match = EVIDENCE_LINE.fullmatch(line)
            if not match:
                errors.append(f"malformed evidence line: {line}")
            else:
                observations.append((int(match.group(1)), match.group(2)))
        elif line.startswith("::warning "):
            warnings.append(line.split("::", 2)[-1])
    unmatched = set(range(len(observations)))
    finding_observations: set[int] = set()
    execution_errors = 0
    internal_errors = 0
    for warning in warnings:
        if warning.startswith("observer internal error:"):
            internal_errors += 1
            execution_errors += 1
            continue
        candidates = sorted(unmatched,
                            key=lambda index: len(observations[index][1]),
                            reverse=True)
        matched = next((index for index in candidates
                        if warning.endswith(": " + observations[index][1])), None)
        if matched is None:
            errors.append(f"warning has no matching observation: {warning}")
            continue
        detail = warning[:-(len(observations[matched][1]) + 2)]
        selected = observations[matched][0]
        if detail == "selection has 0 non-ignored test ids":
            if selected != 0:
                errors.append("zero-selection warning contradicts selected count")
        elif detail.startswith(("list execution failed ",
                                "process could not start:")):
            execution_errors += 1
        else:
            errors.append(f"unknown observer warning: {detail}")
        unmatched.remove(matched)
        finding_observations.add(matched)
    for index in unmatched:
        if observations[index][0] == 0:
            errors.append("selected=0 observation is missing its warning")
    expected = {
        "invocations": len(observations),
        "nonzero": len(observations) - len(finding_observations),
        "findings": len(warnings),
        "extraction_errors": 0,
        "execution_errors": execution_errors,
    }
    if fields != expected:
        errors.append(f"summary counters {fields} do not match evidence {expected}")
    if internal_errors > 1 or (internal_errors and observations):
        errors.append("internal-error evidence cannot accompany observations")
    return errors


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
    parser.add_argument(
        "--write-lib-inventory-manifest",
        action="store_true",
        help=(
            "write the sorted static lib-test identity manifest without "
            "running cargo (explicit regeneration step)"
        ),
    )
    parser.add_argument("--verify-lib-inventory", action="store_true",
                        help="compare static lib test IDs with compiled --list IDs")
    parser.add_argument("--observe-selection", action="store_true",
                        help="warn-only execution evidence for curated jobs")
    parser.add_argument("--verify-selection-evidence", type=Path,
                        help="fail unless an observer log has a truthful summary")
    parser.add_argument("--job", action="append", default=None)
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    workflows = args.workflow or sorted(
        (repo_root / ".github/workflows").glob("*.yml"))
    if args.verify_selection_evidence:
        try:
            rendered = args.verify_selection_evidence.read_text("utf-8")
        except OSError as error:
            print(f"selection-evidence verifier could not read log: {error}",
                  file=sys.stderr)
            return 2
        errors = evidence_verification_errors(rendered)
        for error in errors:
            print(f"selection-evidence verifier: {error}", file=sys.stderr)
        return 1 if errors else 0
    if args.write_lib_inventory_manifest:
        target = discover_targets(repo_root).get("lib")
        if target is None:
            print("lib inventory manifest: lib target not found", file=sys.stderr)
            return 2
        inventory = collect_static_tests(target, repo_root)
        for module, site in sorted(inventory.module_errors.items()):
            print(f"lib inventory module-resolution: {module} at {site}",
                  file=sys.stderr)
        for test_id, first, duplicate in inventory.duplicate_tests:
            print(
                f"lib inventory duplicate: {test_id} "
                f"(first={first}, duplicate={duplicate}; canonicalized once)",
                file=sys.stderr,
            )
        if inventory.module_errors:
            print(
                "lib inventory manifest: refusing to write because the "
                "static inventory is not unambiguous",
                file=sys.stderr,
            )
            return 1
        static_ids = frozenset(inventory.tests)
        manifest_path = repo_root / LIB_INVENTORY_MANIFEST_REL
        manifest_path.parent.mkdir(parents=True, exist_ok=True)
        manifest_path.write_bytes(
            render_lib_inventory_manifest(static_ids).encode("utf-8")
        )
        print(
            f"lib inventory manifest: wrote {manifest_path} "
            f"entries={len(static_ids)} digest={_identity_digest(static_ids)}"
        )
        return 0
    if args.verify_lib_inventory:
        # Contract boundary for this specific inventory check: it compares test
        # identities, not whether their bodies assert anything or whether a CI
        # lane executes them. Consequently it does not reject (1) #[ignore],
        # (2) an empty but compiling test body, (3) `if: false` on the workflow
        # test step after that job's semantic hash is re-pinned, or (4) a
        # negative path-filter pattern that prevents the lane from being chosen.
        manifest_path = repo_root / LIB_INVENTORY_MANIFEST_REL
        try:
            manifest_ids = load_lib_inventory_manifest(manifest_path)
        except OSError as error:
            print(f"lib inventory manifest: cannot read {manifest_path}: {error}",
                  file=sys.stderr)
            print(
                f"lib inventory re-pin: run `{lib_inventory_regeneration_command()}`, "
                "review the manifest diff, then rerun --verify-lib-inventory",
                file=sys.stderr,
            )
            return 2
        except ValueError as error:
            print(f"lib inventory manifest: {error}", file=sys.stderr)
            print(
                f"lib inventory re-pin: run `{lib_inventory_regeneration_command()}`, "
                "review the manifest diff, then rerun --verify-lib-inventory",
                file=sys.stderr,
            )
            return 2
        expected_static_only = expected_lib_static_only(sys.platform)
        if expected_static_only is None:
            print(f"lib inventory comparison: unsupported platform {sys.platform}",
                  file=sys.stderr)
            return 2
        comparison = compare_lib_inventory(repo_root)
        if comparison.execution_error:
            print(f"lib inventory comparison: {comparison.execution_error}",
                  file=sys.stderr)
            return 2
        for module, site in comparison.module_errors:
            print(f"lib inventory module-resolution: {module} at {site}",
                  file=sys.stderr)
        for test_id, first, duplicate in comparison.duplicate_tests:
            print(
                f"lib inventory duplicate: {test_id} "
                f"(first={first}, duplicate={duplicate}; canonicalized once)",
                file=sys.stderr,
            )
        manifest_only = manifest_ids - comparison.static_ids
        actual_only = comparison.static_ids - manifest_ids
        for test_id in sorted(manifest_only, key=_byte_sort_key):
            print(
                f"lib inventory manifest-only (deleted from source): {test_id}",
                file=sys.stderr,
            )
        for test_id in sorted(actual_only, key=_byte_sort_key):
            print(
                f"lib inventory actual-only (added in source): {test_id}",
                file=sys.stderr,
            )
        for test_id in sorted(comparison.static_only):
            print(f"lib inventory static-only: {test_id}", file=sys.stderr)
        for test_id in sorted(comparison.cargo_only):
            print(f"lib inventory cargo-only: {test_id}", file=sys.stderr)
        static_digest = _identity_digest(comparison.static_ids)
        manifest_digest = _identity_digest(manifest_ids)
        static_count_delta = len(comparison.static_ids) - len(manifest_ids)
        print("lib inventory comparison: "
              f"manifest-only={len(manifest_only)} "
              f"actual-only={len(actual_only)} "
              f"static-only={len(comparison.static_only)} "
              f"cargo-only={len(comparison.cargo_only)} "
              f"module-errors={len(comparison.module_errors)} "
              f"duplicates={len(comparison.duplicate_tests)}")
        print("lib inventory identity: "
              f"manifest={'match' if not manifest_only and not actual_only else 'changed'} "
              f"manifest-digest={manifest_digest} "
              f"actual-digest={static_digest} "
              f"manifest-count={len(manifest_ids)} "
              f"actual-count={len(comparison.static_ids)} "
              f"delta={static_count_delta:+d}")
        failed = (
            manifest_only
            or actual_only
            or comparison.static_only != expected_static_only
            or comparison.cargo_only != LIB_INVENTORY_KNOWN_CARGO_ONLY
            or comparison.module_errors
        )
        if failed:
            print(
                f"lib inventory re-pin: run `{lib_inventory_regeneration_command()}`, "
                "review the named manifest diff, then rerun --verify-lib-inventory"
            )
        return 1 if failed else 0
    if args.observe_selection:
        if len(workflows) != 1 or not args.job:
            parser.error("--observe-selection requires one --workflow and --job")
        workflow = Path(workflows[0])
        if not workflow.is_absolute():
            workflow = repo_root / workflow
        # Scope is deliberately narrow, and the observer step and this verifier
        # fail independently -- do not attribute one to the other.
        #
        # This verifier's only input is the evidence log. It never sees the
        # observer's exit code, so anything that kills the observer without
        # corrupting that log -- a nonzero exit or a broken pipe after a
        # complete, truthful summary -- leaves this verifier at zero and turns
        # the required job red through the observer step instead (the step runs
        # under pipefail). Conversely, this verifier is what catches problems
        # visible in the log itself.
        #
        # This comment does not describe the exact set this verifier rejects.
        # Five rounds of trying produced a wrong or incomplete answer every
        # time. To find out whether a particular evidence defect is caught,
        # write it into a log and run --verify-selection-evidence on it; that
        # answer does not go stale.
        #
        # What this verifier does not check is observation sufficiency. A caught
        # internal exception is reported truthfully as execution_errors/findings
        # while this process returns zero, and with no invocation floor a
        # truthful all-zero summary passes.
        #
        # Structural tamper-resistance lives in check-ci-runner-hardening.sh,
        # not here -- see the comment above its `targets` table for what a
        # re-pinned semantic hash does and does not accept. New jobs outside
        # that registry, quoted job ids, and syntax changes that shrink
        # extraction remain review responsibilities.
        try:
            observations = observe_curated(
                repo_root, workflow.resolve(), set(args.job)
            )
        except Exception as error:
            print(f"::warning file={workflow}::observer internal error: "
                  f"{type(error).__name__}: {error}")
            observations = []
            internal_errors = 1
        else:
            internal_errors = 0
        findings = internal_errors
        execution_errors = internal_errors
        for words, selected, detail in observations:
            command = " ".join(words)
            print(f"selection-evidence: selected={selected} command={command}")
            if detail:
                findings += 1
                execution_errors += detail != "selection has 0 non-ignored test ids"
                print(f"::warning file={workflow}::{detail}: {command}")
        print("selection-evidence summary: "
              f"invocations={len(observations)} "
              f"nonzero={sum(detail is None for _, _, detail in observations)} "
              f"findings={findings} extraction_errors=0 "
              f"execution_errors={execution_errors} "
              "[warn-only #5008]")
        return 0
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
