#!/usr/bin/env python3
"""Validate the bounded Discord publication manifest (#71/#5191).

The gate is deliberately lexical: it proves an exact checked-in file/row closure,
not repository-wide Rust semantics.  It strips comments and literals, attributes
calls to brace-delimited real functions, and requires every declared contract
symbol to be linked to the row entry or an explicit cross-file path.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

DEFAULT_MANIFEST = Path("scripts/discord_publication_boundaries.json")
DIRECT_OPERATIONS = ["send_message", "send_files", "edit_message", "create_message"]
_OPS = "|".join(DIRECT_OPERATIONS)
DIRECT_SEND = re.compile(
    rf"\.\s*(?P<method>{_OPS})\s*\("
    rf"|(?:(?:\b[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)|(?:<[^>{{}};]+>))"
    rf"\s*::\s*(?P<ufcs>{_OPS})\s*\("
)
FUNCTION = re.compile(
    r"\b(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:<[^>{;]*>)?[^;{]*\{",
    re.DOTALL,
)
ALLOWED_FAILURES = {
    "DNS", "AMBIG", "PARTIAL", "SBU", "POSTCOMMIT_AMBIG", "TRANSIENT", "PERMANENT",
}
UNCERTAIN_FAILURES = {"AMBIG", "PARTIAL", "SBU", "POSTCOMMIT_AMBIG"}
SPAWN_CONTRACT_APIS = {
    "tokio_spawn_future": "tokio::spawn",
    "spawn_observed_future": "task_supervisor::spawn_observed",
    "spawn_observed_tmux_watcher_future": "task_supervisor::spawn_observed_tmux_watcher",
}
SEND_CONTRACTS = set(SPAWN_CONTRACT_APIS)
GENERIC_SETTLEMENTS = {"commit", "advance", "remove", "clear", "save", "release"}
CANONICAL_SCOPE = "Phase-A listed-file closure for #5191 answer publication and named status/cleanup families at base c052d386c23eaef5f439d98821bbb71cf23598fd; not repository-wide or family-complete"
CANONICAL_IDENTITY = "path_id; file and entry_symbol locate code; line numbers are diagnostics only"
CANONICAL_EXCLUDED = (
    "Unlisted files and adapters, including new wrappers outside closure.scope_files",
    "Discord command and interaction replies unrelated to answer publication",
    "delete_message cleanup operations (non-publication; logical cleanup symbols remain documented in rows)",
    "reaction-only traffic and thread creation",
    "test-only transports and non-Discord notification backends",
)
CANONICAL_SCOPE_FILES = (
    "src/services/discord/formatting/long_send_rollback.rs",
    "src/services/discord/formatting/replace_long_message.rs",
    "src/services/discord/outbound/send_api.rs",
    "src/services/discord/outbound/turn_output_controller.rs",
    "src/services/discord/outbound/turn_output_controller/fresh_send.rs",
    "src/services/discord/restart_report.rs",
    "src/services/discord/session_relay_sink.rs",
    "src/services/discord/task_notification_delivery/card_post.rs",
    "src/services/discord/task_notification_delivery/response_chunks.rs",
    "src/services/discord/tmux_watcher/jsonl_rotation.rs",
    "src/services/discord/tmux_watcher/no_result_exits.rs",
    "src/services/discord/tmux_watcher/orphan_status_panel_cleanup.rs",
    "src/services/discord/tmux_watcher/pre_emit_guard.rs",
    "src/services/discord/tmux_watcher/single_message_footer.rs",
    "src/services/discord/tmux_watcher/streaming_status_tick.rs",
    "src/services/discord/tmux_watcher/terminal_abort_exits.rs",
    "src/services/discord/tmux_watcher/terminal_direct_fallback.rs",
    "src/services/discord/tmux_watcher/terminal_long_chunks.rs",
    "src/services/discord/tmux_watcher/terminal_send.rs",
    "src/services/discord/tmux_watcher/two_message_panel.rs",
    "src/services/discord/turn_bridge/headless_delivery.rs",
    "src/services/discord/turn_bridge/single_message_footer.rs",
    "src/services/discord/turn_bridge/status_panel.rs",
    "src/services/discord/turn_bridge/status_panel/fallback.rs",
    "src/services/discord/turn_bridge/terminal_outcome_delivery.rs",
    "src/services/discord/turn_bridge/terminal_outcome_delivery/empty_response_recovery/handler.rs",
    "src/services/discord/turn_bridge/terminal_outcome_delivery/recovery_retry.rs",
    "src/services/discord/turn_bridge/two_message_panel.rs",
    "src/services/message_outbox.rs",
)
CANONICAL_PATH_IDS = (
    "outbound-v3-dm", "format-long-rollback", "format-replace-rollback",
    "controller-transport", "controller-fresh-send", "task-card-post",
    "task-response-chunks", "session-relay-answer", "plain-bridge-answer",
    "watcher-streaming", "watcher-no-result", "watcher-pre-emit", "watcher-rollover",
    "watcher-abort", "watcher-two-message", "watcher-completion-edit",
    "watcher-answer-replace", "watcher-answer-long", "watcher-direct-fallback",
    "watcher-footer", "bridge-headless", "bridge-status-panel-edit",
    "bridge-status-fallback", "bridge-two-message-create", "bridge-two-message-rollover",
    "bridge-footer-create", "bridge-recovery-retry", "bridge-empty-fallback",
    "message-outbox", "restart-report-legacy",
)


class ManifestError(ValueError):
    """The checked-in publication manifest is invalid or stale."""


@dataclass(frozen=True)
class FunctionBody:
    name: str
    start: int
    end: int
    text: str


def _nonempty(value: object) -> bool:
    return type(value) is str and bool(value.strip())


def _strings(value: object, *, allow_empty: bool = True) -> bool:
    return type(value) is list and (allow_empty or bool(value)) and all(_nonempty(x) for x in value)


def _exact_keys(value: object, expected: set[str], label: str) -> None:
    if type(value) is not dict:
        raise ManifestError(f"{label} must be an object")
    actual = set(value)
    if actual != expected:
        raise ManifestError(f"{label} fields mismatch; missing={sorted(expected-actual)} unexpected={sorted(actual-expected)}")


def _lexical_stream(text: str) -> str:
    """Blank Rust comments/string/char literals while preserving offsets/newlines."""
    out = list(text)
    i, n = 0, len(text)
    while i < n:
        if text.startswith("//", i):
            end = text.find("\n", i + 2)
            end = n if end < 0 else end
            out[i:end] = " " * (end - i)
            i = end
        elif text.startswith("/*", i):
            start, depth = i, 1
            i += 2
            while i < n and depth:
                if text.startswith("/*", i): depth += 1; i += 2
                elif text.startswith("*/", i): depth -= 1; i += 2
                else: i += 1
            for j in range(start, i):
                if out[j] != "\n": out[j] = " "
        else:
            raw = re.match(r"(?:br|r)(?P<h>#{0,255})\"", text[i:])
            regular = re.match(r"(?:b)?\"", text[i:])
            if raw:
                start, hashes = i, raw.group("h")
                i += raw.end()
                close = '"' + hashes
                end = text.find(close, i)
                i = n if end < 0 else end + len(close)
                for j in range(start, i):
                    if out[j] != "\n": out[j] = " "
            elif regular:
                start = i
                i += regular.end()
                while i < n:
                    if text[i] == "\\": i += 2
                    elif text[i] == '"': i += 1; break
                    else: i += 1
                for j in range(start, min(i, n)):
                    if out[j] != "\n": out[j] = " "
            elif text[i] == "'":
                # A Rust char closes nearby; a lifetime does not.
                match = re.match(r"'(?:\\.|[^\\'\n])'", text[i:])
                if match:
                    end = i + match.end()
                    out[i:end] = " " * (end - i)
                    i = end
                else:
                    i += 1
            else:
                i += 1
    return "".join(out)


def _function_bodies(text: str) -> list[FunctionBody]:
    clean = _lexical_stream(text)
    bodies: list[FunctionBody] = []
    for match in FUNCTION.finditer(clean):
        brace = match.end() - 1
        depth = 0
        for cursor in range(brace, len(clean)):
            if clean[cursor] == "{": depth += 1
            elif clean[cursor] == "}":
                depth -= 1
                if depth == 0:
                    bodies.append(FunctionBody(match.group(1), brace + 1, cursor, clean[brace + 1:cursor]))
                    break
    return bodies


def _body_at(bodies: list[FunctionBody], offset: int) -> FunctionBody | None:
    candidates = [body for body in bodies if body.start <= offset < body.end]
    return min(candidates, key=lambda body: body.end - body.start) if candidates else None


def discover_direct_sends(repo_root: Path, files: set[str]) -> Counter[tuple[str, str]]:
    found: Counter[tuple[str, str]] = Counter()
    for rel in sorted(files):
        text = (repo_root / rel).read_text(encoding="utf-8")
        clean = _lexical_stream(text)
        bodies = _function_bodies(text)
        for match in DIRECT_SEND.finditer(clean):
            owner = _body_at(bodies, match.start())
            found[(rel, owner.name if owner else "<module>")] += 1
    return found


def _safe_source(repo_root: Path, rel: object, label: str, *, canonical: bool = False) -> Path:
    if not _nonempty(rel):
        raise ManifestError(f"{label} must be a non-empty path")
    assert isinstance(rel, str)
    pure = PurePosixPath(rel)
    if pure.is_absolute() or ".." in pure.parts or pure.as_posix() != rel:
        raise ManifestError(f"{label} must be a normalized repository-relative path: {rel}")
    if not rel.startswith("src/") or not rel.endswith(".rs"):
        raise ManifestError(f"{label} must match src/**/*.rs: {rel}")
    root = repo_root.resolve()
    candidate = (root / rel).resolve()
    if candidate == root or root not in candidate.parents:
        raise ManifestError(f"{label} escapes repository root: {rel}")
    if not candidate.is_file():
        raise ManifestError(f"{label} source file missing: {rel}")
    if canonical and rel not in CANONICAL_SCOPE_FILES:
        raise ManifestError(f"{label} is outside canonical scope: {rel}")
    return candidate


def _claim_pattern(claim: str) -> re.Pattern[str]:
    pieces = re.split(r"(\.|::)", claim)
    pattern = ""
    for piece in pieces:
        if piece == ".": pattern += r"\s*\.\s*"
        elif piece == "::": pattern += r"\s*::\s*"
        else: pattern += re.escape(piece)
    return re.compile(rf"(?<![A-Za-z0-9_]){pattern}(?![A-Za-z0-9_])")


def _body_for(repo_root: Path, rel: str, entry: str, cache: dict[str, list[FunctionBody]]) -> FunctionBody:
    if rel not in cache:
        source = _safe_source(repo_root, rel, "contract path")
        cache[rel] = _function_bodies(source.read_text(encoding="utf-8"))
    matches = [body for body in cache[rel] if body.name == entry]
    if not matches:
        raise ManifestError(f"entry function {entry!r} missing from {rel}")
    if len(matches) != 1:
        raise ManifestError(f"entry function {entry!r} is ambiguous in {rel}")
    return matches[0]


def _spawn_call_contains(body: FunctionBody, api: str, target: str) -> bool:
    """Return true when target occurs inside an actual api(...) argument list."""
    target_pattern = _claim_pattern(target)
    for api_match in _claim_pattern(api).finditer(body.text):
        cursor = api_match.end()
        while cursor < len(body.text) and body.text[cursor].isspace():
            cursor += 1
        if cursor >= len(body.text) or body.text[cursor] != "(":
            continue
        depth = 0
        for end in range(cursor, len(body.text)):
            if body.text[end] == "(":
                depth += 1
            elif body.text[end] == ")":
                depth -= 1
                if depth == 0:
                    if target_pattern.search(body.text, cursor + 1, end):
                        return True
                    break
    return False


def _validate_graph(rows: list[dict[str, object]], by_id: dict[str, dict[str, object]]) -> None:
    edges: dict[str, list[str]] = {}
    for row in rows:
        path_id = str(row["path_id"])
        after = row["authority_order_after"]
        assert isinstance(after, list)
        unknown = [item for item in after if item not in by_id]
        if unknown: raise ManifestError(f"{path_id}: unknown authority predecessor(s): {unknown}")
        edges[path_id] = list(after)
    visiting: set[str] = set()
    done: set[str] = set()
    def visit(node: str, trail: list[str]) -> None:
        if node in visiting:
            start = trail.index(node)
            raise ManifestError("authority order cycle: " + " -> ".join(trail[start:] + [node]))
        if node in done: return
        visiting.add(node)
        for predecessor in edges[node]: visit(predecessor, trail + [node])
        visiting.remove(node); done.add(node)
    for node in edges: visit(node, [])


def load_and_validate(path: Path, repo_root: Path, *, enforce_canonical: bool = True) -> dict[str, object]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ManifestError(f"cannot read manifest {path}: {error}") from error
    _exact_keys(payload, {"schema_version", "scope", "identity", "closed_scope", "closure", "excluded", "rows"}, "manifest")
    if type(payload["schema_version"]) is not int or payload["schema_version"] != 1 or payload["closed_scope"] is not True:
        raise ManifestError("manifest must declare integer schema_version=1 and closed_scope=true")
    if not _nonempty(payload["scope"]) or not _nonempty(payload["identity"]) or not _strings(payload["excluded"], allow_empty=False):
        raise ManifestError("manifest must state identity, scope, and explicit exclusions")
    if enforce_canonical and (
        payload["scope"] != CANONICAL_SCOPE
        or payload["identity"] != CANONICAL_IDENTITY
        or tuple(payload["excluded"]) != CANONICAL_EXCLUDED
    ):
        raise ManifestError("manifest scope, identity, and exclusions must equal the pinned Phase-A contract")
    closure = payload["closure"]
    _exact_keys(closure, {"kind", "claim", "scope_files", "direct_operations", "excluded_operations"}, "closure")
    if closure["kind"] != "listed_files" or closure["claim"] != "direct_operations_only":
        raise ManifestError("closure must be listed_files/direct_operations_only")
    if closure["direct_operations"] != DIRECT_OPERATIONS or closure["excluded_operations"] != ["delete_message"]:
        raise ManifestError("closure operations differ from the fixed scanner contract")
    scope_files = closure["scope_files"]
    if not _strings(scope_files, allow_empty=False) or len(scope_files) != len(set(scope_files)):
        raise ManifestError("closure scope_files must be a non-empty unique string array")
    for rel in scope_files: _safe_source(repo_root, rel, "scope file", canonical=enforce_canonical)
    if enforce_canonical and tuple(scope_files) != CANONICAL_SCOPE_FILES:
        raise ManifestError("closure scope_files must equal the independently pinned canonical 29-file list")
    rows = payload["rows"]
    if type(rows) is not list or not rows: raise ManifestError("manifest rows must be a non-empty array")
    if enforce_canonical and (len(rows) != 30 or tuple(row.get("path_id") if type(row) is dict else None for row in rows) != CANONICAL_PATH_IDS):
        raise ManifestError("manifest rows must equal the independently pinned canonical 30-row path_id list")

    row_keys = {"path_id", "file", "entry_symbol", "transport_symbols", "contract_paths", "executor", "authorities", "timeout_retry", "failure_classes", "settlement_symbols", "post_success", "multi_op", "success_then_settlement", "direct_send_count", "authority_order_after"}
    by_id: dict[str, dict[str, object]] = {}
    declared: Counter[tuple[str, str]] = Counter()
    body_cache: dict[str, list[FunctionBody]] = {}
    scoped = set(scope_files)
    for index, raw in enumerate(rows):
        _exact_keys(raw, row_keys, f"row {index}")
        path_id, rel, entry = raw["path_id"], raw["file"], raw["entry_symbol"]
        if not all(_nonempty(x) for x in (path_id, rel, entry)): raise ManifestError(f"row {index} has an empty identity field")
        assert isinstance(path_id, str) and isinstance(rel, str) and isinstance(entry, str)
        if path_id in by_id: raise ManifestError(f"duplicate path_id: {path_id}")
        if rel not in scoped: raise ManifestError(f"{path_id}: row file is outside closure scope_files: {rel}")
        local_body = _body_for(repo_root, rel, entry, body_cache)
        for name in ("transport_symbols", "authorities", "failure_classes"):
            if not _strings(raw[name], allow_empty=False): raise ManifestError(f"{path_id}: {name} must be a non-empty string array")
        for name in ("settlement_symbols", "authority_order_after"):
            if not _strings(raw[name]): raise ManifestError(f"{path_id}: {name} must be a string array")
        for name in ("transport_symbols", "authorities", "failure_classes", "settlement_symbols", "authority_order_after"):
            if len(raw[name]) != len(set(raw[name])): raise ManifestError(f"{path_id}: duplicate value in {name}")
        failures = set(raw["failure_classes"])
        if not failures or failures - ALLOWED_FAILURES: raise ManifestError(f"{path_id}: unsupported or empty failure classes")
        if "DNS" in failures and failures & UNCERTAIN_FAILURES: raise ManifestError(f"{path_id}: DNS cannot coexist with publication uncertainty")
        if (raw["multi_op"] is True) != ("PARTIAL" in failures): raise ManifestError(f"{path_id}: multi_op and PARTIAL must imply each other")
        if (raw["success_then_settlement"] is True) != ("SBU" in failures): raise ManifestError(f"{path_id}: success_then_settlement and SBU must imply each other")
        if "POSTCOMMIT_AMBIG" in failures and "SBU" not in failures: raise ManifestError(f"{path_id}: POSTCOMMIT_AMBIG requires SBU")
        if type(raw["multi_op"]) is not bool or type(raw["success_then_settlement"]) is not bool:
            raise ManifestError(f"{path_id}: multi_op and success_then_settlement must be booleans")
        count = raw["direct_send_count"]
        if type(count) is not int or count < 0: raise ManifestError(f"{path_id}: direct_send_count must be an integer >= 0")

        timeout = raw["timeout_retry"]
        _exact_keys(timeout, {"timeout_ms", "policy"}, f"{path_id}.timeout_retry")
        if (timeout["timeout_ms"] is not None and (type(timeout["timeout_ms"]) is not int or timeout["timeout_ms"] < 0)) or not _nonempty(timeout["policy"]):
            raise ManifestError(f"{path_id}: invalid timeout_retry")
        executor = raw["executor"]
        _exact_keys(executor, {"owner", "mode", "send_contract", "spawn"}, f"{path_id}.executor")
        if not _nonempty(executor["owner"]) or executor["mode"] not in {"caller", "spawned"}: raise ManifestError(f"{path_id}: invalid executor owner/mode")
        if executor["mode"] == "caller":
            if executor["send_contract"] is not None or executor["spawn"] is not None: raise ManifestError(f"{path_id}: caller executor cannot declare spawned contract")
        else:
            if executor["send_contract"] not in SEND_CONTRACTS: raise ManifestError(f"{path_id}: spawned executor must use a fixed Send contract")
            spawn = executor["spawn"]
            _exact_keys(spawn, {"file", "entry_symbol", "spawn_api", "target_symbol"}, f"{path_id}.executor.spawn")
            if not all(_nonempty(spawn[name]) for name in ("file", "entry_symbol", "spawn_api", "target_symbol")):
                raise ManifestError(f"{path_id}: spawned executor evidence fields must be non-empty strings")
            expected_api = SPAWN_CONTRACT_APIS[executor["send_contract"]]
            if spawn["spawn_api"] != expected_api:
                raise ManifestError(f"{path_id}: spawned executor API does not match its fixed Send contract")
            spawn_body = _body_for(repo_root, spawn["file"], spawn["entry_symbol"], body_cache)
            if not _spawn_call_contains(spawn_body, spawn["spawn_api"], spawn["target_symbol"]):
                raise ManifestError(f"{path_id}: spawned executor evidence does not place target inside spawn call")

        paths = raw["contract_paths"]
        if type(paths) is not list: raise ManifestError(f"{path_id}: contract_paths must be an array")
        evidence = [local_body]
        seen_contract_paths: set[tuple[str, str]] = set()
        for path_index, item in enumerate(paths):
            _exact_keys(item, {"file", "entry_symbol"}, f"{path_id}.contract_paths[{path_index}]")
            if not _nonempty(item["file"]) or not _nonempty(item["entry_symbol"]):
                raise ManifestError(f"{path_id}: contract path fields must be non-empty strings")
            contract_key = (item["file"], item["entry_symbol"])
            if contract_key in seen_contract_paths:
                raise ManifestError(f"{path_id}: duplicate contract path {contract_key}")
            seen_contract_paths.add(contract_key)
            evidence.append(_body_for(repo_root, item["file"], item["entry_symbol"], body_cache))
        for symbol in [*raw["transport_symbols"], *raw["settlement_symbols"]]:
            if not any(_claim_pattern(symbol).search(body.text) for body in evidence):
                raise ManifestError(f"{path_id}: row-linked symbol {symbol!r} missing from entry/contract paths")
        for symbol in raw["settlement_symbols"]:
            if symbol in GENERIC_SETTLEMENTS: raise ManifestError(f"{path_id}: generic settlement symbol {symbol!r} must be qualified")

        post = raw["post_success"]
        if raw["success_then_settlement"]:
            _exact_keys(post, {"file", "entry_symbol", "transport_symbol", "settlement_symbol"}, f"{path_id}.post_success")
            if not all(_nonempty(post[name]) for name in ("file", "entry_symbol", "transport_symbol", "settlement_symbol")):
                raise ManifestError(f"{path_id}: post-success evidence fields must be non-empty strings")
            if post["transport_symbol"] not in raw["transport_symbols"] or post["settlement_symbol"] not in raw["settlement_symbols"]:
                raise ManifestError(f"{path_id}: post-success evidence must reference row claims")
            post_body = _body_for(repo_root, post["file"], post["entry_symbol"], body_cache)
            transports = list(_claim_pattern(post["transport_symbol"]).finditer(post_body.text))
            settlements = list(_claim_pattern(post["settlement_symbol"]).finditer(post_body.text))
            if not any(settlement.start() > transport.start() for transport in transports for settlement in settlements):
                raise ManifestError(f"{path_id}: settlement must appear after transport in post-success evidence")
        elif post is not None:
            raise ManifestError(f"{path_id}: post_success must be null when success_then_settlement=false")

        by_id[path_id] = raw
        declared[(rel, entry)] += count
    _validate_graph(rows, by_id)
    discovered = discover_direct_sends(repo_root, scoped)
    orphaned, missing = discovered - declared, declared - discovered
    if orphaned or missing:
        details = [f"orphan direct send {f}:{s} count={c}" for (f, s), c in sorted(orphaned.items())]
        details += [f"missing direct send {f}:{s} count={c}" for (f, s), c in sorted(missing.items())]
        raise ManifestError("; ".join(details))
    return payload


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--manifest", type=Path)
    args = parser.parse_args(argv)
    root = args.repo_root.resolve()
    try:
        payload = load_and_validate(args.manifest or root / DEFAULT_MANIFEST, root, enforce_canonical=True)
    except ManifestError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print(f"Discord publication boundary manifest passed ({len(payload['rows'])} paths, {len(payload['closure']['scope_files'])} files)")
    return 0


if __name__ == "__main__": raise SystemExit(main())
