#!/usr/bin/env python3
"""Validate the dormant writer surface census against exact Rust declarations.

This bounded lexical contract uses ``rust_lex`` to ignore comments/strings and
removes ``macro_rules!`` bodies before matching declarations. It does not prove
call reachability, follow aliases/re-exports, or parse cfg predicates; Rust tests
own the typed disposition grammar while this census pins current named surfaces.
"""
from __future__ import annotations

import json, re, sys
from pathlib import Path
from rust_lex import StripState, strip_line

MANIFEST = Path("scripts/writer_surface_manifest.jsonl")
FIELDS = {"id","file","symbol","provider","artifact","origin","path_builder","open","append","reopen","rotate","truncate","cleanup","disposition","w1"}
# id -> exact identity/classification plus an operation field that must stay nonempty.
EXPECTED = {
"process-create":("src/services/session_backend.rs","create_session_with_command_env","Claude|Codex","RelayJsonl","AgentDeskManaged","DormantManaged","open"),
"rotating-open":("src/services/tmux_common.rs","RotatingJsonlWriter","Claude|Codex|Qwen","RelayJsonl","AgentDeskManaged","DormantManaged","reopen"),
"rotating-append":("src/services/tmux_common.rs","write_line","Claude|Codex|Qwen","RelayJsonl","AgentDeskManaged","DormantManaged","append"),
"rotating-reopen":("src/services/tmux_common.rs","reopen_if_path_replaced","Claude|Codex|Qwen","RelayJsonl","AgentDeskManaged","DormantManaged","reopen"),
"jsonl-open":("src/services/tmux_common.rs","open_jsonl_append_file","Claude|Codex|Qwen","RelayJsonl","AgentDeskManaged","DormantManaged","open"),
"temp-cleanup":("src/services/tmux_common.rs","cleanup_session_temp_files","Claude|Codex|Qwen","RelayJsonl|Prompt|InputFifo|OwnerMarker|WrapperScript|RuntimeMarker","SessionAuxiliary","DormantManaged","cleanup"),
"authority-cleanup":("src/services/tmux_common.rs","cleanup_session_temp_files_under_source_authority","Claude|Codex|Qwen","RelayJsonl|Prompt|InputFifo|OwnerMarker|WrapperScript|RuntimeMarker","SessionAuxiliary","DormantManaged","cleanup"),
"claude-cleanup":("src/services/claude.rs","cleanup_stale_claude_tui_session","Claude","RelayJsonl|Prompt|InputFifo|OwnerMarker|WrapperScript|RuntimeMarker","SessionAuxiliary","DormantManaged","cleanup"),
"codex-cleanup":("src/services/codex.rs","cleanup_existing_codex_tui_session","Codex","RelayJsonl|Prompt|InputFifo|OwnerMarker|WrapperScript|RuntimeMarker","SessionAuxiliary","DormantManaged","cleanup"),
"qwen-create-cleanup":("src/services/qwen.rs","execute_streaming_local_tmux","Qwen","RelayJsonl|Prompt|InputFifo|OwnerMarker","AgentDeskManaged","DormantManaged","cleanup"),
"watcher-rotate":("src/services/discord/tmux_watcher/jsonl_rotation.rs","rotate_watcher_jsonl_if_due","Claude|Codex|Qwen","RelayJsonl","AgentDeskManaged","DormantManaged","rotate"),
"owned-rotate":("src/services/discord/tmux_watcher/jsonl_rotation.rs","rotate_owned_jsonl","Claude|Codex|Qwen","RelayJsonl","AgentDeskManaged","DormantManaged","rotate"),
"truncate":("src/services/tmux_common.rs","truncate_jsonl_head_safe","Claude|Codex|Qwen","RelayJsonl","AgentDeskManaged","DormantManaged","truncate"),
"owner-classify":("src/services/tmux_common.rs","classify_watcher_jsonl_owner","Claude|Codex|Qwen","RelayJsonl|NativeTranscript|NativeRollout","AgentDeskManaged|ProviderNative","DormantManaged|Observed","path_builder"),
"claude-native-path":("src/services/claude_tui/transcript_tail.rs","claude_transcript_path","Claude","NativeTranscript","ProviderNative","Observed","path_builder"),
"claude-native-candidates":("src/services/claude_tui/transcript_tail.rs","claude_transcript_path_candidates","Claude","NativeTranscript","ProviderNative","Observed","path_builder"),
"codex-rollout-find":("src/services/codex_tui/rollout_tail.rs","find_rollout_by_session_id","Codex","NativeRollout","ProviderNative","Observed","path_builder"),
"codex-rollout-find-under":("src/services/codex_tui/rollout_tail.rs","find_rollout_by_session_id_under","Codex","NativeRollout","ProviderNative","Observed","path_builder"),
"codex-rollout-tail":("src/services/codex_tui/rollout_tail.rs","tail_latest_rollout_for_cwd_with_handoff_for_tmux","Codex","NativeRollout","ProviderNative","Observed","open"),
"codex-rollout-tail-offset":("src/services/codex_tui/rollout_tail.rs","tail_rollout_file_from_offset","Codex","NativeRollout","ProviderNative","Observed","open"),
"codex-rollout-tail-tmux":("src/services/codex_tui/rollout_tail.rs","tail_rollout_file_from_offset_for_tmux","Codex","NativeRollout","ProviderNative","Observed","open"),
"hook-queue-lock-type":("src/services/claude_tui/hook_relay/ordered_queue.rs","RelayQueueFileLock","Claude","HookRelayQueueLock","AgentDeskManaged","DormantManaged","open"),
"hook-queue-lock":("src/services/claude_tui/hook_relay/ordered_queue.rs","lock_relay_queue_file","Claude","HookRelayQueueLock","AgentDeskManaged","DormantManaged","open"),
"hook-queue-lock-mode":("src/services/claude_tui/hook_relay/ordered_queue.rs","lock_relay_queue_file_with_mode","Claude","HookRelayQueueLock","AgentDeskManaged","DormantManaged","open"),
"hook-queue-publish":("src/services/claude_tui/hook_relay/ordered_queue.rs","publish_atomic_file","Claude","HookRelayQueueRecord","AgentDeskManaged","DormantManaged","rotate"),
"hook-queue-quarantine":("src/services/claude_tui/hook_relay/ordered_queue.rs","quarantine_path","Claude","HookRelayQueueRecord","AgentDeskManaged","DormantManaged","rotate"),
"hook-queue-retention":("src/services/claude_tui/hook_relay/queue_retention.rs","prune_artifact_dir","Claude","HookRelayQueueRecord","AgentDeskManaged","DormantManaged","cleanup"),
"gemini-no-local":("src/services/writer_protocol.rs","classify_writer","Gemini","NoManagedLocalTranscript","AgentDeskManaged","Observed","path_builder"),
"opencode-no-local":("src/services/writer_protocol.rs","classify_writer","OpenCode","NoManagedLocalTranscript","AgentDeskManaged","Observed","path_builder"),
"unsupported-unknown":("src/services/writer_protocol.rs","classify_writer","Unsupported","Unknown","Unsupported","Unsupported","path_builder"),
}

def code_without_macros(text: str) -> str:
    state=StripState(); code="\n".join(strip_line(line,state) for line in text.splitlines())
    chars=list(code)
    for match in list(re.finditer(r"\bmacro_rules\s*!\s*\w+\s*\{",code)):
        depth=0
        for i in range(match.start(),len(chars)):
            if chars[i]=="{": depth+=1
            elif chars[i]=="}":
                depth-=1
                if depth==0:
                    chars[match.start():i+1]=" "*(i+1-match.start()); break
    return "".join(chars)

def declaration_exists(text: str, symbol: str) -> bool:
    code=code_without_macros(text)
    return bool(re.search(rf"\b(?:fn|struct|enum)\s+{re.escape(symbol)}\b",code))

def check_manifest_text(text: str, root: Path|None=None) -> list[str]:
    errors=[]; rows={}
    for number,line in enumerate(text.splitlines(),1):
        try: row=json.loads(line)
        except (ValueError,TypeError) as exc: errors.append(f"line {number}: invalid JSON: {exc}"); continue
        if set(row)!=FIELDS: errors.append(f"line {number}: fields differ")
        ident=row.get("id")
        if ident in rows: errors.append(f"duplicate id {ident}")
        rows[ident]=row
    if set(rows)!=set(EXPECTED): errors.append(f"IDs differ: missing={sorted(set(EXPECTED)-set(rows))}, extra={sorted(set(rows)-set(EXPECTED))}")
    for ident,expected in EXPECTED.items():
        row=rows.get(ident)
        if not row: continue
        actual=tuple(row.get(k) for k in ("file","symbol","provider","artifact","origin","disposition"))
        if actual!=expected[:6]: errors.append(f"{ident}: identity/classification differs")
        if not row.get(expected[6]): errors.append(f"{ident}: {expected[6]} must be nonempty")
        if row.get("origin")=="ProviderNative" and row.get("disposition")!="Observed": errors.append(f"{ident}: provider-native must be observation-only")
        if root:
            path=root/row["file"]
            try: source=path.read_text(encoding="utf-8")
            except OSError as exc: errors.append(f"{ident}: cannot read {path}: {exc}")
            else:
                if not declaration_exists(source,row["symbol"]): errors.append(f"{ident}: declaration not found")
    return errors

def check(root: Path) -> list[str]:
    try: text=(root/MANIFEST).read_text(encoding="utf-8")
    except OSError as exc: return [f"cannot read {MANIFEST}: {exc}"]
    return check_manifest_text(text,root)

def main() -> int:
    errors=check(Path(__file__).resolve().parents[1])
    if errors:
        for error in errors: print(f"ERROR: writer surface manifest: {error}",file=sys.stderr)
        return 1
    print(f"writer surface manifest check passed: {len(EXPECTED)} exact rows")
    return 0
if __name__=="__main__": raise SystemExit(main())
