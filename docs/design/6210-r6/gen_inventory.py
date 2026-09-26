#!/usr/bin/env python3
r"""#6210 r6 선행 산출물: relay 부작용 호출 지점의 기계적 인벤토리.

사용법
    python3 gen_inventory.py <worktree> [--citations r5_citations.tsv] > inventory.tsv

같은 worktree 내용이면 항상 같은 바이트를 출력한다(정렬 고정, 시각·절대경로·해시 순서 없음).
Rust 소스는 저장소의 frontier gate lexer(scripts/check_durable_frontier_writer_call_sites.py)로
읽는다. 주석·문자열·#[cfg(test)] 항목·*tests.rs·핀 고정 test-only 모듈은 공백으로 지워진
production text만 본다. 줄 번호는 원본과 같다. rustc 없이 이름으로만 매칭한다(타입 미해결).

======================================================================================
찾는 기준 (여기가 정본이다. 바꾸면 inventory.tsv와 pin baseline을 같이 재생성한다)
======================================================================================
스캔 범위: <worktree>/src/**/*.rs production text (lexer의 _scan_inputs 결과와 같다).

A. Discord HTTP 송신·수정·삭제
   A-seed (serenity/HTTP primitive, production text):
     A_METHOD_RE   `.<m>(`  m ∈ A_PRIMITIVES
     A_CTX_RE      `.edit(` `.delete(` `.react(` `.pin(` `.unpin(` 중 첫 인자가 http/ctx/cache 계열
     A_REST_RE     원본 줄의 문자열 리터럴 `channels/{..}/messages` 또는 `discord.com/api`
                   (주석 줄 제외, production 줄만; A_REST_EXCLUDE_FILE_RE 파일과 `.route(` 줄 제외)
   A-wrapper: A-site(seed 또는 wrapper 호출)를 품은 fn 중
     (1) src/services/discord/http.rs 의 fn 전부
     (2) formatting.rs·formatting/** 에서 이름에 `long` 이 들어간 fn
     (3) outbound/** 의 fn
     (4) 이름이 A_WRAPPER_PREFIX_RE 로 시작하거나 이름 토큰이 A_WRAPPER_TOKENS 와 겹치는 fn
     (cmd_* slash 엔트리포인트 제외)
   그 fn 이름의 호출부(`\bNAME\s*(::<..>)?\s*\(`, `fn NAME(` 정의 제외)를 다시 A-site로 추가하고
   고정점까지 반복한다. 즉 wrapper 뒤로 최소 한 단계 이상 실제 호출부까지 내려간다.

B. inflight 행 삭제·재작성
   B-seed: B_PRIMITIVE_RE (`remove_file(` `rename(` `atomic_write(` `fs::write(`) 가
     inflight.rs·inflight/** 안에 있거나, 같은 statement 에 `inflight` 가 나오거나,
     감싸는 fn 본문이 B_ROW_PATH_RE(inflight 행 경로 API)를 부르는 곳,
     또는 원본 직전 B_RAW_WINDOW 줄에 B_RAW_HINT_RE(행 경로 리터럴·삭제 로그 target)가 있는 곳.
   B-seed(지연 삭제 요청): B_DEFERRED_CLEAR_RE `clear_inflight: true` 구조체 리터럴(struct 선언 제외).
   B-wrapper: B-site를 품고 이름이 B_WRAPPER_PREFIX_RE 로 시작하거나 토큰이 B_WRAPPER_TOKENS 와
     겹치는 fn. 고정점까지 호출부 추적.

C. transcript·spool 원본 삭제·truncate
   C-seed: C_NAMED_SEEDS 호출, `.set_len(`·`.truncate(true)` (C_TRUNCATE_SCOPE 안, 또는 fn 이
     C_SOURCE_HINT_RE 를 언급할 때),
     tmux_session_files.rs 의 `rename(`,
     그리고 감싸는 fn 본문이 C_SOURCE_HINT_RE 를 언급하는 `remove_file(`/`remove_dir_all(`.
   C-wrapper: C-site를 품고 이름이 C_WRAPPER_PREFIX_RE 로 시작하거나 토큰이 C_WRAPPER_TOKENS 와
     겹치는 fn. 고정점까지.
   (spool 전용 원본 파일은 src/ 에서 0건이다. scripts/ 쪽 삭제는 summary.md 에 따로 적는다.)

D. offset·frontier·배달 경계 writer
   D_FRONTIER_SYMBOLS 호출(= 기존 frontier gate EXPECTED_CALL_SITES 키),
   D_FIELD_ASSIGN_RE 필드 대입(`=`, `+=`, `-=`), D_DEREF_ASSIGN_RE `*field =` (&mut 매개변수),
   D_LOCAL_ASSIGN_RE 같은 이름 지역 변수 재대입(let 선언 제외), D_FIELD_INIT_RE 구조체 리터럴 초기화
   (struct 정의 안의 필드 선언은 제외), D_FIELD_SHORTHAND_RE 축약 초기화 `Foo { field, .. }`
   (struct_literal_at: 가장 안쪽 괄호가 `TypeName {` 이고 뒤에 `=`·`=>` 가 없는 리터럴만), `InflightTurnState::new(`,
   D_ATOMIC_RE (offset/frontier 원자값 store/fetch_max/compare_exchange),
   D_LEASE_RE (lease 상태 전이 호출: try_acquire*, commit*, release*, renew, reclaim_if_expired;
   D_LEASE_SCOPE 안에서 파일명에 lease 가 있거나 직전 80자에 lease/cell 이 있을 때), D_LEASE_STATE_RE (`.lease = LeaseState::X`).
   D는 이름 규칙 wrapper 폐포를 돌리지 않는다(frontier gate 심볼이 이미 wrapper 목록이다).
   아래 "한 단계 규칙"만 적용한다.

한 단계 규칙(모든 범주): site 를 품은 fn 이 이름 규칙(wrapper)에 안 걸려도 그 fn 의 호출부는
   한 번 site 로 센다(seed 종류 `unnamed-fn-caller`). 그 호출부를 품은 fn 은 이름 규칙에 걸릴 때만
   더 올라간다. 전 호출 그래프 폐포(9,765행·깊이 36, C 가 2,826행으로 번짐)는 쓰지 않는다.
함수 값 참조: `(atomic_write,` `, fs::remove_file)` 처럼 인자로 넘긴 primitive(B_FNREF_RE)와 wrapper
   이름(call_re fn_value)도 호출부로 센다. `use a::{x, y};` 재수출 목록(in_use_decl)은 제외한다.
fn 경계: fn·struct 범위는 주석·문자열만 지운 전체 코드(shape)의 중괄호로 잰다. production text 는
   `#[cfg(test)] let x = if .. {..} else {..};` 에서 첫 줄만 지워 중괄호 균형이 깨지기 때문이다.
   macro_rules! 본문 안의 site 는 fn 이 `<module>` 로 나오고 매크로 호출부는 추적하지 않는다.

wrapper 호출부 매칭의 한정(오탐 폭증 방지, 이름 기반 매칭의 한계):
   밑줄 없는 한 단어 이름과 GENERIC_NAME_EXCLUDE 는 `<정의 모듈명>::NAME(` 형태의 호출만 센다
   (예: fresh_send::deliver). 메서드 호출 `.deliver(` 로만 불리면 놓친다 → summary.md 에 목록.
   A_PRIMITIVES 와 같은 이름의 wrapper 는 `.NAME(` 이 이미 seed 이므로 `.` 없는 호출만 추적한다.

판정 열(관리 채널 도달·(B) 요소·retire 주체)은 아래 RULES 표에서 기계적으로 나온다.
  - r5 인용 태그(Sxx/Mxx)가 붙으면 그 태그의 (B) 요소를 쓴다(S_ELEMENTS, M_ELEMENTS).
  - wrapper 내부 행(generic 모듈)은 호출자 행들에서 도달을 유도한다: 하나라도 예면 예,
    전부 아니오면 아니오, 그 밖은 불명.
  - 어느 규칙에도 걸리지 않으면 '불명'으로 남긴다(추정으로 채우지 않는다).
"""
from __future__ import annotations

import argparse
import importlib.util
import re
import sys
from collections import defaultdict
from pathlib import Path

sys.dont_write_bytecode = True

# ----------------------------------------------------------------------------- 기준
A_PRIMITIVES = (
    "send_message", "send_files", "edit_message", "delete_message", "delete_messages",
    "create_message", "create_reaction", "delete_reaction", "delete_reactions",
    "delete_reaction_emoji", "create_thread", "create_public_thread", "create_private_thread",
    "create_thread_from_message", "edit_thread", "say", "reply", "reply_ping",
    "create_response", "edit_response", "create_interaction_response",
    "edit_original_interaction_response", "create_followup", "create_followup_message",
    "edit_followup", "delete_followup", "pin", "unpin", "pin_message", "unpin_message",
    "broadcast_typing", "start_typing", "crosspost",
)
A_METHOD_RE = re.compile(r"\.\s*(" + "|".join(A_PRIMITIVES) + r")\s*(?:::<[^>]*>)?\s*\(")
A_CTX_RE = re.compile(r"\.\s*(edit|delete|react|pin|unpin)\s*\(\s*&?\s*\(?\s*[\w.]*(?:http|ctx|cache)")
A_REST_RE = re.compile(r"channels/\{[^}\"]*\}/messages|discord\.com/api")
# AgentDesk 자체 API 라우트 정의·문서 인벤토리의 경로 문자열은 Discord 호출이 아니다.
A_REST_EXCLUDE_FILE_RE = re.compile(r"^src/server/routes/docs/")
A_WRAPPER_PREFIX_RE = re.compile(
    r"^(send|edit|delete|replace|post|patch|react|reply|relay|deliver|publish|rollback|"
    r"repost|resend|update_placeholder|update_status|finalize_placeholder|cleanup_placeholder)_"
)

B_PRIMITIVE_RE = re.compile(r"\b(remove_file|rename|atomic_write)\s*\(|\bfs::write\s*\(")
# 함수 값으로 넘겨 주입 closure(`write(&path, ..)`)로 부르는 곳: `(atomic_write,` `, fs::remove_file)`.
B_FNREF_RE = re.compile(r"[(,]\s*(?:(?:std\s*::\s*)?fs\s*::\s*)?(remove_file|rename|atomic_write)\s*(?=[,)])")


class _At:
    def __init__(self, off: int):
        self._off = off

    def start(self) -> int:
        return self._off


B_ROW_PATH_RE = re.compile(r"\binflight_state_path\s*\(|\bdiscord_inflight_root\s*\(|\binflight_root\b")
# 문자열 리터럴로 행 경로를 조립하는 곳(예: JS op `agentdesk.inflight.remove`)은 production text에서
# 문자열이 지워지므로, 원본의 직전 B_RAW_WINDOW 줄에서 행 경로 리터럴·삭제 로그 target을 찾는다.
B_RAW_HINT_RE = re.compile(r'"[^"\n]*runtime/discord_inflight[^"\n]*"|"agentdesk::inflight_remove"')
B_RAW_WINDOW = 30
B_WRAPPER_PREFIX_RE = re.compile(
    r"^(save|persist|clear|delete|archive|reap|abandon|invalidate|remove|write|upsert|rewrite|"
    r"force_clean|rebind|reset|purge|sweep|mark|stamp|rewind)_"
)

# 이름 토큰(밑줄로 나눈 단어) 중 하나라도 이 집합에 있으면 wrapper 후보다(접두 규칙과 OR).
A_WRAPPER_TOKENS = frozenset({
    "send", "edit", "delete", "replace", "post", "patch", "react", "reaction", "reply", "relay",
    "deliver", "delivery", "publish", "rollback", "repost", "resend", "cleanup", "notify",
    "teardown", "rollover", "fallback", "sweep", "announce", "footer", "panel", "placeholder",
})
B_WRAPPER_TOKENS = frozenset({
    "save", "persist", "clear", "delete", "archive", "reap", "abandon", "invalidate", "remove",
    "write", "upsert", "rewrite", "patch", "bump", "commit", "cleanup", "finalize", "sync",
    "stamp", "mark", "rewind", "refresh", "consume", "dispose", "adopt", "adoption", "reset",
    "set", "bind", "touch", "backfill", "mutate", "downgrade", "admit", "retire", "settle",
    "reanchor",
})
# 행 삭제를 다른 주체(finalizer 등)에게 메시지로 위임하는 요청 필드. 호출 그래프로는 이어지지 않으므로
# 요청을 만드는 지점을 B-site로 직접 센다.
B_DEFERRED_CLEAR_RE = re.compile(r"\bclear_inflight\s*:\s*true\b")
C_WRAPPER_TOKENS = frozenset({
    "cleanup", "sweep", "truncate", "remove", "delete", "reset", "rotate", "purge", "clear", "reap",
})
# 엔트리포인트(프레임워크가 부르는 slash command)는 호출부가 없으므로 wrapper로 보지 않는다.
WRAPPER_EXCLUDE_PREFIX = ("cmd_",)

C_NAMED_SEEDS = ("cleanup_session_temp_files", "sweep_orphan_session_files", "truncate_jsonl_head_safe")
C_SOURCE_HINT_RE = re.compile(r"jsonl|transcript|output_path|session_temp|generation_marker|session_generation|rollout|spool")
# truncate 계열은 services/ 전체를 본다(메모리 저장소 lock 파일은 원본이 아니므로 제외).
# services/ 밖(logging·worker ledger·secret file)은 fn 이 C_SOURCE_HINT_RE 를 언급할 때만.
C_TRUNCATE_SCOPE = ("src/services/",)
C_TRUNCATE_EXCLUDE = ("src/services/memory/",)
C_WRAPPER_PREFIX_RE = re.compile(r"^(cleanup|sweep|truncate|remove|delete|reset|rotate|purge|clear)_")

D_OFFSET_FIELDS = (
    "last_offset", "response_sent_offset", "turn_start_offset", "resume_offset",
    "last_relay_offset", "last_relayed_offset", "last_watcher_relayed_offset",
    "bridge_confirmed_response_sent_offset", "confirmed_end_offset", "confirmed_offset",
    "delivered_frontier", "tmux_last_offset", "current_offset", "committed_offset",
    "delivered_offset", "frontier", "relay_frontier", "handoff_offset", "start_offset",
    "watermark", "confirmed_end",
)
_D_FIELDS = "|".join(D_OFFSET_FIELDS)
D_FIELD_ASSIGN_RE = re.compile(r"\.\s*(" + _D_FIELDS + r")\s*(?:[+\-]?=)(?!=)")
D_DEREF_ASSIGN_RE = re.compile(r"\*\s*(" + _D_FIELDS + r")\s*(?:[+\-]?=)(?!=)")
D_LOCAL_ASSIGN_RE = re.compile(r"(?<![\w.*&])(" + _D_FIELDS + r")\s*(?:[+\-]?=)(?!=)")
D_FIELD_INIT_RE = re.compile(r"(?<![\w.:])(" + _D_FIELDS + r")\s*:\s*(?![:\s]*(?:u64|usize|i64|Option|Arc|AtomicU64|u32)\b)[^,}\n]")
# 필드 축약 초기화 `Foo { response_sent_offset, .. }`: 구조체 리터럴일 때만(패턴 분해 제외, struct_literal_at 참조).
D_FIELD_SHORTHAND_RE = re.compile(r"[{,]\s*(" + _D_FIELDS + r")\s*(?=[,}])")
D_ATOMIC_RE = re.compile(
    r"\b(" + _D_FIELDS + r")\s*\.\s*(store|fetch_max|fetch_add|fetch_update|swap|compare_exchange(?:_weak)?)\s*\("
)
D_LEASE_RE = re.compile(
    r"\.\s*(try_acquire\w*|commit|commit_exact|release|release_exact|release_owned_state|renew|reclaim_if_expired)\s*\("
)
# writer_protocol·calendar_sync·cluster capacity 등 다른 도메인의 lease 는 배달 경계가 아니다.
D_LEASE_SCOPE = ("src/services/discord/", "src/services/turn_orchestrator/")
D_LEASE_STATE_RE = re.compile(r"\.\s*lease\s*=(?!=)\s*LeaseState\s*::\s*(\w+)")
D_LEASE_FILE_HINT = re.compile(r"lease|DeliveryLease")
D_CTOR_RE = re.compile(r"\bInflightTurnState\s*::\s*new\s*\(")

GENERIC_NAME_EXCLUDE = frozenset({
    "new", "run", "send", "edit", "delete", "save", "clear", "remove", "write", "commit",
    "release", "apply", "handle", "process", "build", "update", "reply", "say",
    "save_state", "write_all", "remove_entry", "clear_all",
})

SCOPE_PREFIXES = ("src/",)

# ----------------------------------------------------------------------------- 판정 규칙
# (경로 접두 → 도달, 사유, 기본 (B) 요소, retire 주체). 먼저 맞는 규칙을 쓴다.
# 도달 값 'derive' = 호출자 행에서 유도한다.
D = "src/services/discord/"
RULES = [
    (D + "commands/", "아니오", "slash/interaction 응답 경로(릴레이 행·본문 ID를 만들지 않음)", "해당 없음", "interaction 수명(Discord 측)"),
    (D + "meeting", "아니오", "회의 전용 채널·상태기계", "해당 없음", "meeting_orchestrator"),
    (D + "voice", "아니오", "음성 경로", "해당 없음", "voice_lifecycle"),
    (D + "gateway_voice_queue", "아니오", "음성 경로", "해당 없음", "voice_lifecycle"),
    (D + "model_picker_interaction", "아니오", "interaction 응답", "해당 없음", "interaction 수명"),
    (D + "sidecar_interaction", "아니오", "interaction 응답", "해당 없음", "interaction 수명"),
    (D + "idle_recap", "불명", "관리 채널에 recap을 올리는지 채널 선택 인자 추적 필요", "N", "idle_recap"),
    (D + "http.rs", "derive", "HTTP wrapper", "호출자 따름", "호출자"),
    (D + "formatting", "derive", "long-message wrapper", "호출자 따름", "호출자"),
    (D + "discord_io", "derive", "Discord IO wrapper", "호출자 따름", "호출자"),
    (D + "outbound/", "derive", "outbound 전달 계층", "호출자 따름", "호출자"),
    (D + "delivery_lease_cell", "예", "배달 lease(관리 채널 relay 공용)", "D,G,C,R", "lease commit/release(S14)"),
    (D + "tmux_watcher", "예", "TUI-direct watcher relay", "D,G,C,R", "terminal_commit_epilogue(S15)·pane-death(S16)"),
    (D + "tmux.rs", "예", "watcher lifecycle·frontier", "G,C", "watcher lifecycle"),
    (D + "tmux/", "예", "watcher lifecycle", "G,C", "watcher lifecycle"),
    (D + "tmux_restart_handoff", "예", "restart handoff가 watcher 턴 행을 재생", "D,G,C", "자기 자신(무조건 clear)"),
    (D + "tmux_session_files", "예", "TUI 세션 sidecar/원본", "C,R", "세션 재생성·boot sweep"),
    (D + "tmux_output_stream", "예", "TUI 출력 원본", "C,R", "watcher"),
    (D + "session_relay_sink", "예", "SBR sink(TUI-direct)", "D,C,N,R", "sink 완료 clear"),
    (D + "standby_relay", "예", "standby relay", "D,G,C,R", "standby 완료 mirror/clear"),
    (D + "tui_prompt_relay", "예", "합성 턴 claim·refresh(owner 1)", "D,G,C,R", "terminal epilogue·settle"),
    (D + "tui_direct", "예", "TUI-direct 합성 턴", "D,G,C", "tui_direct_pending_start"),
    (D + "turn_bridge", "예", "idle bridge(S08)", "D,C,R", "bridge 완료"),
    (D + "recovery_engine", "예", "restore/rebind 복구", "G,C,R", "restore 완료 후 clear"),
    (D + "recovery_paths", "예", "복구 경로", "G,C,R", "recovery_paths 자신"),
    (D + "watchers/", "예", "lifecycle restore", "G,C,R", "watcher lifecycle"),
    (D + "inflight", "예", "inflight 행 저장소(관리 채널 행 포함)", "C", "clear_store/removal"),
    (D + "health", "예", "health 복구·rebind", "G,C", "health recovery"),
    (D + "relay_recovery", "예", "relay 자동 복구", "G,C", "relay_recovery"),
    (D + "relay_coord", "예", "relay 좌표·frontier", "D,G,C", "watcher"),
    (D + "placeholder_live_events", "예", "status panel(관리 채널)", "D,G,C,N", "panel cleanup(S05)"),
    (D + "placeholder_sweeper", "예", "orphan placeholder sweep", "C,R", "sweeper 자신"),
    (D + "placeholder_cleanup", "예", "placeholder 정리", "C,R", "cleanup 자신"),
    (D + "placeholder_controller", "불명", "Discord 발 턴 placeholder와 공용; 관리 턴 경로 사용 여부 호출자 추적 필요", "불명", "불명"),
    (D + "turn_finalizer", "예", "턴 finalize", "G,C,N", "finalizer"),
    (D + "streaming_finalizer", "예", "streaming finalize", "D,G,C", "finalizer"),
    (D + "terminal_delivery_custody", "예", "terminal custody", "C,R", "custody"),
    (D + "task_notification_delivery", "예", "알림 카드(관리 채널 포함)", "N", "task_notification store"),
    (D + "subagent_notification_card", "예", "알림 카드", "N", "호출자"),
    (D + "tui_task_card", "예", "TUI task 카드", "N", "호출자"),
    (D + "restart_report", "예", "restart 보고", "N", "restart_report"),
    (D + "restart_mode", "예", "drain restart", "D,G", "restart_mode"),
    (D + "runtime_bootstrap", "예", "부팅 복구·redelivery", "G,C,R", "boot"),
    (D + "catch_up", "불명", "catch-up 대상 채널 선택 추적 필요", "불명", "불명"),
    (D + "router/", "불명", "같은 채널의 Discord 발 턴; 합성 턴 행·ID와의 상호작용 판정 필요", "불명", "router 턴 수명"),
    (D + "queue_", "불명", "Discord 발 큐 턴; 관리 채널 큐 공유 여부 판정 필요", "불명", "queue"),
    (D + "reaction", "불명", "사용자 메시지 reaction; 합성 턴 prompt anchor 대상 여부 판정 필요", "N", "reaction_lifecycle"),
    (D + "stall_recovery", "예", "stall 복구", "C", "stall_recovery"),
    (D + "inflight_heartbeat_sweeper", "예", "heartbeat sweeper(행 abandon)", "C", "sweeper 자신"),
    (D + "status_panel", "예", "status panel 저장소", "D,G,C", "panel cleanup"),
    (D + "turn_view_reconciler", "예", "turn view 재조정", "G,C", "reconciler"),
    (D + "footer_view_reconciler", "예", "footer 재조정", "N", "reconciler"),
    (D + "single_message_panel", "불명", "panel 대상 채널 선택 추적 필요", "불명", "불명"),
    (D + "mod.rs", "예", "discord 루트 재수출·doctor", "C", "호출자"),
    (D + "turn_lease", "예", "턴 lease", "D,G", "turn_lease"),
    (D + "session_runtime", "예", "세션 런타임", "C", "session_runtime"),
    (D + "session_idle_cleanup", "예", "세션 idle 정리", "C", "session_idle_cleanup"),
    (D + "tmux_lifecycle", "예", "tmux 수명", "C", "tmux_lifecycle"),
    (D + "tmux_reaper", "예", "tmux reaper", "C", "tmux_reaper"),
    (D, "불명", "discord 기타 모듈; 채널 선택 인자 추적 필요", "불명", "불명"),
    ("src/services/turn_orchestrator/", "예", "턴 오케스트레이터(관리 채널 턴 포함)", "D,C", "turn_orchestrator"),
    ("src/services/turn_lifecycle", "예", "turn_lifecycle force-kill/cancel", "C", "turn_lifecycle"),
    ("src/services/claude", "예", "Claude TUI 세션 재생성", "C,R", "provider 세션 수명"),
    ("src/services/tmux_common", "예", "tmux 세션 공용", "C,R", "provider 세션 수명"),
    ("src/services/", "불명", "services 기타; 호출 채널 추적 필요", "불명", "불명"),
    ("src/", "불명", "services 밖(서버 라우트·CLI 등); 대상 채널 인자 추적 필요", "불명", "불명"),
]

S_ELEMENTS = {
    "S01": "D,G,C,N,R", "S02": "D,G,C,R", "S03": "D,G,C,N,R", "S04": "D,G,C,R",
    "S05": "D,G,C,R", "S06": "D,C,R", "S07": "D,C,N,R", "S08": "D,C,R", "S09": "D,G,C,N,R",
    "S10": "G,C,R", "S11": "G,C,N,R", "S12": "D,C", "S13": "G,C,N,R", "S14": "D,G,C,N,R",
    "S15": "G,C,N", "S16": "D,C,R", "S17": "G,C,N,R", "S18": "C", "S19": "C,R",
    "S20": "D,G,C,N,R",
}
# 리뷰 1.2의 '겹침' 열(F=동결 ID는 D/G 쪽 요소로 본다).
M_ELEMENTS = {
    "M1": "D,G,C", "M2": "D,G", "M3": "D,G", "M4": "D,G", "M5": "D", "M6": "C",
    "M7": "C", "M8": "D,C", "M9": "D,G", "M10": "C", "M11": "C", "M12": "C", "M13": "R",
    "M14": "C", "M15": "C", "M16": "G,C", "M17": "C", "M18": "C", "M19": "D",
}
ELEM_NAMES = {"D": "drain", "G": "재개", "C": "custody", "N": "알림", "R": "수동 재생"}

COLUMNS = (
    "범주", "file:line", "함수", "호출 경로 요약", "관리채널 도달", "도달 사유", "(B) 요소",
    "현재 retire 주체", "r5 행", "리뷰 태그", "callee", "순번", "깊이", "seed 종류",
)


# ----------------------------------------------------------------------------- 스캔
def load_lexer(worktree: Path):
    scripts = worktree / "scripts"
    sys.path.insert(0, str(scripts))
    path = scripts / "check_durable_frontier_writer_call_sites.py"
    spec = importlib.util.spec_from_file_location("_frontier_gate", path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["_frontier_gate"] = mod
    spec.loader.exec_module(mod)
    return mod


FN_DEF_RE = re.compile(r"\bfn\s+([A-Za-z_]\w*)\s*[<(]")
STRUCT_RE = re.compile(r"\b(?:struct|enum|union)\s+[A-Za-z_]\w*[^;{]*\{")


class SrcFile:
    def __init__(self, rel: str, prod: str, raw_lines: list[str], countable: set[int], shape: str):
        self.rel = rel
        self.prod = prod
        # fn·struct 경계는 shape(주석·문자열만 지운 전체 코드)로 잰다. production text 는
        # `#[cfg(test)] let x = if .. { .. } else { .. };` 에서 첫 `{` 만 지워 중괄호 균형이 깨진다.
        assert len(shape) == len(prod)
        self.shape = shape
        self.raw_lines = raw_lines
        self.countable = countable
        self.line_starts = [0]
        for i, ch in enumerate(prod):
            if ch == "\n":
                self.line_starts.append(i + 1)
        self.fns = self._fn_spans()
        self.structs = self._struct_spans()

    def line_of(self, off: int) -> int:
        lo, hi = 0, len(self.line_starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if self.line_starts[mid] <= off:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1

    def _match_brace(self, open_at: int) -> int:
        depth = 0
        for i in range(open_at, len(self.shape)):
            c = self.shape[i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    return i
        return len(self.shape) - 1

    def _fn_spans(self):
        spans = []
        for m in FN_DEF_RE.finditer(self.shape):
            i, paren = m.end() - 1, 0
            body = None
            while i < len(self.shape):
                c = self.shape[i]
                if c in "([":
                    paren += 1
                elif c in ")]":
                    paren -= 1
                elif c == ";" and paren == 0:
                    break
                elif c == "{" and paren == 0:
                    body = i
                    break
                i += 1
            if body is None:
                continue
            spans.append((m.start(), self._match_brace(body), m.group(1), body))
        return spans

    def _struct_spans(self):
        out = []
        for m in STRUCT_RE.finditer(self.shape):
            out.append((m.start(), self._match_brace(m.end() - 1)))
        return out

    def enclosing(self, off: int):
        best = None
        for s, e, name, body in self.fns:
            if s <= off <= e and (best is None or s >= best[0]):
                best = (s, e, name, body)
        return best

    def in_struct(self, off: int) -> bool:
        return any(s <= off <= e for s, e in self.structs)

    def statement(self, off: int) -> str:
        """호출 이름부터 그 호출의 닫는 괄호까지(괄호 균형)."""
        i = self.prod.find("(", off)
        if i == -1:
            return ""
        depth = 0
        for j in range(i, len(self.prod)):
            c = self.prod[j]
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if depth == 0:
                    return self.prod[off: j + 1]
        return self.prod[off:]

    def raw_window(self, line: int, before: int) -> str:
        lines = self.raw_lines[max(0, line - 1 - before): line]
        return "\n".join(x for x in lines if not x.lstrip().startswith(("//", "*", "/*")))


def load_files(worktree: Path, g) -> list[SrcFile]:
    files, skips = g._scan_inputs(worktree, g.PINNED_TEST_ONLY_MODULE_FILES)
    out = []
    for p in sorted(files, key=lambda x: str(x)):
        if p in skips:
            continue
        ap = p if p.is_absolute() else worktree / p
        rel = ap.relative_to(worktree).as_posix()
        if not rel.startswith(SCOPE_PREFIXES):
            continue
        countable = set()
        shape_lines = []
        for lineno, code, is_prod in g.production_lines(ap):
            shape_lines.append(code)
            if is_prod:
                countable.add(lineno)
        prod = g._production_text(ap)
        raw = ap.read_text(encoding="utf-8").splitlines()
        out.append(SrcFile(rel, prod, raw, countable, "\n".join(shape_lines)))
    return out


# ----------------------------------------------------------------------------- site 수집
class Site:
    __slots__ = ("cat", "rel", "line", "fn", "fn_span", "callee", "kind", "depth", "chain")

    def __init__(self, cat, f: SrcFile, off, callee, kind, depth, chain):
        self.cat, self.rel, self.line = cat, f.rel, f.line_of(off)
        enc = f.enclosing(off)
        self.fn = enc[2] if enc else "<module>"
        self.fn_span = (f.line_of(enc[0]), f.line_of(enc[1])) if enc else (self.line, self.line)
        self.callee, self.kind, self.depth, self.chain = callee, kind, depth, chain


def call_re(name: str, dotless: bool, fn_value: bool = False) -> re.Pattern:
    """`NAME(` 호출. fn_value 이면 인자 자리의 함수 값 참조(`(NAME,` `, path::NAME)`)도 센다.
    매치 위치는 이름 그룹 `n` 의 시작이다."""
    lead = r"(?<![\w.])" if dotless else r"(?<!\w)"
    rx = lead + r"(?P<n>" + re.escape(name) + r")\s*(?:::\s*<[^>]*>)?\s*\("
    if fn_value:
        rx += r"|[(,]\s*(?:\w+\s*::\s*)*(?P<v>" + re.escape(name) + r")\s*(?=[,)])"
    return re.compile(rx)


USE_DECL_RE = re.compile(
    r"(?:^|[;{}])\s*(?:#\s*!?\s*\[[^\]]*\]\s*)*(?:pub\s*(?:\([^)]*\))?\s*)?use\s+[\w:{}\s,*]*$"
)


def in_use_decl(prod: str, off: int) -> bool:
    """`use a::{x, b::{y}};` 목록 안이면 참(재수출은 호출이 아니다)."""
    return USE_DECL_RE.search(prod[prod.rfind(";", 0, off) + 1: off]) is not None


def name_start(m: re.Match) -> int:
    return m.start("n") if m.group("n") is not None else m.start("v")


def is_definition(prod: str, off: int) -> bool:
    return re.search(r"\bfn\s+$", prod[max(0, off - 16): off]) is not None


def seed_A(f: SrcFile):
    for m in A_METHOD_RE.finditer(f.prod):
        yield m.start(), "." + m.group(1), "serenity-method"
    for m in A_CTX_RE.finditer(f.prod):
        yield m.start(), "." + m.group(1) + "(http)", "serenity-model"
    for idx, raw in enumerate(f.raw_lines, start=1):
        s = raw.lstrip()
        if idx not in f.countable or s.startswith("//") or s.startswith("*"):
            continue
        m = A_REST_RE.search(raw)
        if m and (A_REST_EXCLUDE_FILE_RE.search(f.rel) or ".route(" in raw):
            continue
        if m:
            yield f.line_starts[idx - 1], "REST:" + m.group(0), "raw-rest"


def seed_B(f: SrcFile):
    in_scope = f.rel == D + "inflight.rs" or f.rel.startswith(D + "inflight/")
    hits = [(m.start(), m.group(1) or "fs::write") for m in B_PRIMITIVE_RE.finditer(f.prod)]
    hits += [(m.start(1), m.group(1) + "(fn값)") for m in B_FNREF_RE.finditer(f.prod)]
    for start, name in sorted(hits):
        m = _At(start)
        if is_definition(f.prod, m.start()) or in_use_decl(f.prod, m.start()):
            continue
        stmt = f.statement(m.start())
        enc = f.enclosing(m.start())
        fn_text = f.prod[enc[0]: enc[1]] if enc else ""
        name_base = name.removesuffix("(fn값)")
        reason = None
        if in_scope:
            reason = "inflight-scope"
        elif "inflight" in stmt:
            reason = "stmt-mentions-inflight"
        elif enc and B_ROW_PATH_RE.search(fn_text) and name_base in ("remove_file", "rename"):
            reason = "fn-uses-row-path"
        elif name_base in ("remove_file", "rename") and B_RAW_HINT_RE.search(
            f.raw_window(f.line_of(m.start()), B_RAW_WINDOW)
        ):
            reason = "raw-row-path-literal"
        if reason:
            yield m.start(), name, reason
    for m in B_DEFERRED_CLEAR_RE.finditer(f.prod):
        if not f.in_struct(m.start()):
            yield m.start(), "clear_inflight:true", "deferred-clear-request"


def seed_C(f: SrcFile):
    for name in C_NAMED_SEEDS:
        for m in call_re(name, False).finditer(f.prod):
            if not is_definition(f.prod, m.start()):
                yield m.start(), name, "named-seed"
    def fn_hint(off):
        enc = f.enclosing(off)
        if not enc:
            return False
        return bool(C_SOURCE_HINT_RE.search(f.prod[enc[0]: enc[1]]) or C_SOURCE_HINT_RE.search(enc[2]))

    trunc_scope = f.rel.startswith(C_TRUNCATE_SCOPE) and not f.rel.startswith(C_TRUNCATE_EXCLUDE)
    for m in re.finditer(r"\.\s*set_len\s*\(", f.prod):
        if trunc_scope or fn_hint(m.start()):
            yield m.start(), ".set_len", "truncate"
    for m in re.finditer(r"\.\s*truncate\s*\(\s*true\s*\)", f.prod):
        if trunc_scope or fn_hint(m.start()):
            yield m.start(), ".truncate(true)", "truncate"
    for m in re.finditer(r"\b(remove_file|remove_dir_all|rename)\s*\(", f.prod):
        if is_definition(f.prod, m.start()):
            continue
        name = m.group(1)
        if f.rel.endswith("tmux_session_files.rs") and name == "rename":
            yield m.start(), name, "generation-rename"
            continue
        if name == "rename":
            continue
        if f.rel == D + "inflight.rs" or f.rel.startswith(D + "inflight/"):
            continue
        enc = f.enclosing(m.start())
        stmt = f.statement(m.start())
        fn_text = f.prod[enc[0]: enc[1]] if enc else ""
        raw_fn = "\n".join(f.raw_lines[f.line_of(enc[0]) - 1: f.line_of(enc[1])]) if enc else ""
        if "inflight" in stmt:
            continue
        if C_SOURCE_HINT_RE.search(stmt) or C_SOURCE_HINT_RE.search(fn_text) or C_SOURCE_HINT_RE.search(
            (enc[2] if enc else "")
        ):
            yield m.start(), name, "fn-mentions-source"


def struct_literal_at(f: SrcFile, off: int) -> bool:
    """off 를 감싸는 가장 안쪽 괄호가 `TypeName {` 이고 그 `{..}` 가 패턴(뒤에 `=`·`=>`)이 아니면 참."""
    enc = f.enclosing(off)
    if not enc or off < enc[3] or f.in_struct(off):
        return False
    depth, i = 0, off - 1
    while i > enc[3]:
        c = f.prod[i]
        if c in ")]}":
            depth += 1
        elif c in "([{":
            if depth == 0:
                break
            depth -= 1
        i -= 1
    if i <= enc[3] or f.prod[i] != "{":
        return False
    if not re.search(r"\b[A-Z]\w*(?:\s*::\s*<[^>]*>)?\s*$", f.prod[max(0, i - 120): i]):
        return False
    close = f._match_brace(i)
    after = f.prod[close + 1: close + 40]
    return re.match(r"[\s)]*(?:=(?!=)|=>|\|)", after) is None


def seed_D(f: SrcFile, frontier_symbols):
    for sym in frontier_symbols:
        base = sym.split("::")[-1]
        for m in call_re(base, False).finditer(f.prod):
            if not is_definition(f.prod, m.start()):
                yield m.start(), base, "frontier-gate-symbol"
    for m in D_FIELD_ASSIGN_RE.finditer(f.prod):
        yield m.start(), "." + m.group(1) + "=", "field-assign"
    for m in D_DEREF_ASSIGN_RE.finditer(f.prod):
        yield m.start(), "*" + m.group(1) + "=", "deref-assign"
    for m in D_LOCAL_ASSIGN_RE.finditer(f.prod):
        pre = f.prod[max(0, m.start() - 12): m.start()]
        if re.search(r"\b(let|mut|const|static)\s+$", pre) or not f.enclosing(m.start()):
            continue  # 첫 바인딩(let)은 대입 writer가 아니라 선언이다
        yield m.start(), m.group(1) + "=", "local-assign"
    for m in D_FIELD_INIT_RE.finditer(f.prod):
        enc = f.enclosing(m.start())
        if f.in_struct(m.start()) or not enc or m.start() < enc[3]:
            continue  # struct 선언·fn 시그니처 매개변수는 writer가 아니다
        if re.search(r"\|[^|\n]*$", f.prod[f.line_starts[f.line_of(m.start()) - 1]: m.start()]):
            continue  # closure 매개변수 `|frontier: u64|`
        # match 팔(`Foo { last_offset: x } =>`)·패턴 분해는 writer가 아니다: 같은 줄 `=>`/`let` 패턴 제외
        line = f.raw_lines[f.line_of(m.start()) - 1]
        if "=>" in line and line.index("=>") > line.find(m.group(1)):
            continue
        yield m.start(), m.group(1) + ":", "struct-init"
    for m in D_FIELD_SHORTHAND_RE.finditer(f.prod):
        if struct_literal_at(f, m.start(1)):
            yield m.start(1), m.group(1) + ":", "struct-init"
    for m in D_ATOMIC_RE.finditer(f.prod):
        yield m.start(), m.group(1) + "." + m.group(2), "atomic"
    for m in D_CTOR_RE.finditer(f.prod):
        yield m.start(), "InflightTurnState::new", "ctor"
    lease_scope = f.rel.startswith(D_LEASE_SCOPE)
    for m in (D_LEASE_RE.finditer(f.prod) if lease_scope else ()):
        pre = f.prod[max(0, m.start() - 80): m.start()]
        if D_LEASE_FILE_HINT.search(f.rel) or re.search(r"lease|cell", pre, re.I):
            yield m.start(), "." + m.group(1), "lease-transition"
    for m in D_LEASE_STATE_RE.finditer(f.prod):
        yield m.start(), "lease=LeaseState::" + m.group(1), "lease-state-write"


ONE_LEVEL_KIND = "unnamed-fn-caller"


def wrapper_ok(cat: str, rel: str, fn: str) -> bool:
    if fn == "<module>" or fn.startswith(WRAPPER_EXCLUDE_PREFIX):
        return False
    toks = set(fn.split("_"))
    if cat == "A":
        return (
            rel == D + "http.rs"
            or ((rel == D + "formatting.rs" or rel.startswith(D + "formatting/")) and "long" in fn)
            or rel.startswith(D + "outbound/")
            or A_WRAPPER_PREFIX_RE.match(fn) is not None
            or bool(toks & A_WRAPPER_TOKENS)
        )
    if cat == "B":
        return B_WRAPPER_PREFIX_RE.match(fn) is not None or bool(toks & B_WRAPPER_TOKENS)
    if cat == "C":
        return C_WRAPPER_PREFIX_RE.match(fn) is not None or bool(toks & C_WRAPPER_TOKENS)
    return False


def module_stem(rel: str) -> str:
    p = Path(rel)
    return p.parent.name if p.name == "mod.rs" else p.stem


def wrapper_call_re(name: str, defs: set[str]) -> re.Pattern:
    """일반 이름(한 단어·GENERIC_NAME_EXCLUDE)은 정의 파일 모듈명으로 한정된 호출만 본다."""
    if "_" not in name or name in GENERIC_NAME_EXCLUDE:
        stems = sorted({module_stem(r) for r in defs})
        return re.compile(r"\b(?:" + "|".join(map(re.escape, stems)) + r")\s*::\s*(?P<n>" + re.escape(name) + r")\s*\(")
    return call_re(name, dotless=name in A_PRIMITIVES, fn_value=True)


def collect(files: list[SrcFile], g):
    sites: dict[tuple, Site] = {}

    def add(cat, f, off, callee, kind, depth, chain):
        s = Site(cat, f, off, callee, kind, depth, chain)
        key = (cat, s.rel, s.line, callee, off)
        if key not in sites:
            sites[key] = s
            return True
        return False

    seeders = {"A": seed_A, "B": seed_B, "C": seed_C}
    frontier_symbols = sorted(g.EXPECTED_CALL_SITES.keys())
    for f in files:
        for cat, fn in seeders.items():
            for off, callee, kind in fn(f):
                add(cat, f, off, callee, kind, 0, callee)
        for off, callee, kind in seed_D(f, frontier_symbols):
            add("D", f, off, callee, kind, 0, callee)

    wrappers: dict[tuple, dict] = {}  # (cat, name) -> {"defs": set(rel), "depth":, "chain":}
    traced: set[tuple] = set()
    untraced: dict[tuple, str] = {}
    one_level: dict[tuple, dict] = {}
    one_level_traced: set[tuple] = set()
    for _round in range(64):
        new_wrappers = []
        new_one_level = []
        for s in list(sites.values()):
            k = (s.cat, s.fn)
            if wrapper_ok(s.cat, s.rel, s.fn):
                w = wrappers.setdefault(k, {"defs": set(), "depth": s.depth + 1, "chain": s.chain})
                w["defs"].add(s.rel)
                if s.depth + 1 < w["depth"] or (s.depth + 1 == w["depth"] and s.chain < w["chain"]):
                    w["depth"], w["chain"] = s.depth + 1, s.chain
                if k not in traced:
                    new_wrappers.append(k)
            elif s.fn != "<module>" and not s.fn.startswith(WRAPPER_EXCLUDE_PREFIX):
                untraced.setdefault(k, s.rel)
                if s.kind != ONE_LEVEL_KIND:
                    # 이름 규칙에 안 걸린 fn도 호출부 한 단계는 센다(그 호출부의 fn은 이름 규칙에 걸릴 때만 더 올라간다).
                    w = one_level.setdefault(k, {"defs": set(), "depth": s.depth + 1, "chain": s.chain})
                    w["defs"].add(s.rel)
                    if s.depth + 1 < w["depth"] or (s.depth + 1 == w["depth"] and s.chain < w["chain"]):
                        w["depth"], w["chain"] = s.depth + 1, s.chain
                    if k not in one_level_traced:
                        new_one_level.append(k)
        if not new_wrappers and not new_one_level:
            break
        added = False
        for k in sorted(set(new_wrappers)):
            traced.add(k)
            cat, name = k
            w = wrappers[k]
            rx = wrapper_call_re(name, w["defs"])
            for f in files:
                if name not in f.prod:  # 빠른 거르기(정규식과 같은 결과)
                    continue
                for m in rx.finditer(f.prod):
                    if is_definition(f.prod, name_start(m)) or in_use_decl(f.prod, name_start(m)):
                        continue
                    chain = f"{name} → {w['chain']}"
                    if add(cat, f, name_start(m), name, "wrapper-call", w["depth"], chain):
                        added = True
        for k in sorted(set(new_one_level)):
            one_level_traced.add(k)
            cat, name = k
            w = one_level[k]
            rx = wrapper_call_re(name, w["defs"])
            for f in files:
                if name not in f.prod:
                    continue
                for m in rx.finditer(f.prod):
                    if is_definition(f.prod, name_start(m)) or in_use_decl(f.prod, name_start(m)):
                        continue
                    chain = f"{name} → {w['chain']}"
                    if add(cat, f, name_start(m), name, ONE_LEVEL_KIND, w["depth"], chain):
                        added = True
        if not added:
            break
    for k in traced:
        untraced.pop(k, None)
    # meta 표시: one-level = 호출부를 한 번 셌다, stop = 한 단계 호출부로만 잡혀 더 올라가지 않았다
    untraced = {k: (rel, "one-level" if k in one_level_traced else "stop") for k, rel in untraced.items()}
    return sites, wrappers, untraced


# ----------------------------------------------------------------------------- 인용·판정
def load_citations(path: Path):
    cites = defaultdict(list)  # rel -> [(tag, a, b | fn:NAME)]
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        tag, _src, rel, spans = line.split("\t")[:4]
        for sp in spans.split(","):
            sp = sp.strip()
            if not sp:
                continue
            if sp.startswith("fn:"):
                cites[rel].append((tag, "fn", sp[3:]))
            elif "-" in sp:
                a, b = sp.split("-")
                cites[rel].append((tag, int(a), int(b)))
            else:
                cites[rel].append((tag, int(sp), int(sp)))
    return cites


def tags_for(site: Site, cites) -> tuple[list[str], list[str]]:
    exact, via_fn = set(), set()
    for tag, a, b in cites.get(site.rel, []):
        if a == "fn":
            if site.fn == b:
                exact.add(tag)
            continue
        if a <= site.line <= b:
            exact.add(tag)
        elif not (b < site.fn_span[0] or a > site.fn_span[1]):
            via_fn.add(tag)
    via_fn -= exact
    return sorted(exact), sorted(via_fn)


def rule_for(rel: str):
    for prefix, reach, why, elems, retire in RULES:
        if rel.startswith(prefix):
            return reach, why, elems, retire
    return "불명", "규칙 없음", "불명", "불명"


def fmt_elems(codes: str) -> str:
    if codes in ("해당 없음", "불명", "호출자 따름"):
        return codes
    return "·".join(f"{c}({ELEM_NAMES[c]})" for c in codes.split(",") if c in ELEM_NAMES)


def judge(rows: list[dict], sites_by_callee: dict):
    # 1차: 규칙표
    for r in rows:
        reach, why, elems, retire = rule_for(r["rel"])
        r["reach"], r["why"], r["retire"] = reach, why, retire
        codes = set()
        for t in r["tags_all"]:
            src = S_ELEMENTS.get(t) or M_ELEMENTS.get(t)
            if src:
                codes.update(src.split(","))
        if codes:
            r["elems"] = ",".join(c for c in "DGCNR" if c in codes)
            r["elem_src"] = "태그"
        else:
            r["elems"] = elems
            r["elem_src"] = "모듈"
        if r["seed_kind"] in ("serenity-method",) and r["callee"] in (".say", ".reply", ".create_response",
                                                                       ".create_interaction_response",
                                                                       ".edit_original_interaction_response",
                                                                       ".create_followup", ".create_followup_message"):
            if r["rel"].startswith(D + "commands/") or "interaction" in r["rel"]:
                r["reach"], r["why"] = "아니오", "interaction 응답 primitive"
    # 2차: derive 행은 호출자에서 유도(고정점)
    by_fn = defaultdict(list)
    for r in rows:
        by_fn[(r["cat"], r["fn"])].append(r)
    for _ in range(32):
        changed = False
        for r in rows:
            if r["reach"] not in ("derive",) and not r["reach"].startswith("derive"):
                continue
            callers = sites_by_callee.get((r["cat"], r["fn"]), [])
            vals = sorted({c["reach"] for c in callers})
            if not callers:
                new = ("불명", "wrapper 내부인데 추적된 호출자가 없음(이름 제외 목록 또는 간접 호출)")
            elif "예" in vals:
                ex = sorted(f"{c['rel']}:{c['line']}" for c in callers if c["reach"] == "예")[0]
                new = ("예", f"호출자 중 예 존재(예: {ex})")
            elif vals == ["아니오"]:
                new = ("아니오", f"호출자 {len(callers)}곳 전부 아니오")
            elif any(v == "derive" for v in vals):
                continue
            else:
                new = ("불명", f"호출자 도달이 불명 포함({','.join(vals)})")
            r["reach"], r["why"] = new
            if r["elems"] == "호출자 따름":
                codes = set()
                for c in callers:
                    if c["elems"] not in ("호출자 따름", "불명", "해당 없음"):
                        codes.update(c["elems"].split(","))
                r["elems"] = ",".join(x for x in "DGCNR" if x in codes) or "불명"
            changed = True
        if not changed:
            break
    for r in rows:
        if r["reach"] == "derive":
            r["reach"], r["why"] = "불명", "wrapper 호출자 유도가 순환으로 수렴하지 않음"
        if r["elems"] == "호출자 따름":
            r["elems"] = "불명"


def build_rows(worktree: Path, citations: Path):
    g = load_lexer(worktree)
    files = load_files(worktree, g)
    sites, wrappers, untraced = collect(files, g)
    cites = load_citations(citations)
    ordered = sorted(sites.values(), key=lambda s: (s.cat, s.rel, s.line, s.callee, s.kind))
    ord_count = defaultdict(int)
    rows = []
    for s in ordered:
        k = (s.cat, s.rel, s.fn, s.callee)
        ord_count[k] += 1
        exact, via_fn = tags_for(s, cites)
        s_tags = [t for t in exact if t.startswith("S")] + [f"{t}(fn)" for t in via_fn if t.startswith("S")]
        o_tags = [t for t in exact if not t.startswith("S")] + [f"{t}(fn)" for t in via_fn if not t.startswith("S")]
        rows.append({
            "cat": s.cat, "rel": s.rel, "line": s.line, "fn": s.fn, "callee": s.callee,
            "ordinal": ord_count[k], "depth": s.depth, "seed_kind": s.kind,
            "chain": (f"{s.fn} → {s.chain}" if s.fn != "<module>" else s.chain),
            "s_tags": s_tags, "o_tags": o_tags, "tags_all": exact + via_fn,
        })
    sites_by_callee = defaultdict(list)
    for r in rows:
        if r["seed_kind"] == "wrapper-call":
            sites_by_callee[(r["cat"], r["callee"])].append(r)
    judge(rows, sites_by_callee)
    return rows, wrappers, untraced, files


def render(rows) -> str:
    out = ["\t".join(COLUMNS)]
    for r in rows:
        r5 = ",".join(r["s_tags"]) if r["s_tags"] else "누락"
        elems = fmt_elems(r["elems"])
        if r["elem_src"] == "모듈" and elems not in ("불명", "해당 없음"):
            elems += " [모듈규칙]"
        out.append("\t".join(str(x).replace("\t", " ") for x in (
            r["cat"], f"{r['rel']}:{r['line']}", r["fn"], r["chain"], r["reach"], r["why"], elems,
            r["retire"], r5, ",".join(r["o_tags"]) or "-", r["callee"], r["ordinal"], r["depth"],
            r["seed_kind"],
        )))
    return "\n".join(out) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("worktree", type=Path)
    ap.add_argument("--citations", type=Path, default=Path(__file__).with_name("r5_citations.tsv"))
    ap.add_argument("--meta", type=Path, help="fn 목록(wrapper / one-level / stop)을 TSV로 추가 출력")
    args = ap.parse_args()
    wt = args.worktree.resolve()
    rows, wrappers, untraced, _files = build_rows(wt, args.citations)
    sys.stdout.write(render(rows))
    if args.meta:
        lines = ["kind\tcat\tname\tfiles\tdepth\tchain"]
        for (cat, name), w in sorted(wrappers.items()):
            lines.append(f"wrapper\t{cat}\t{name}\t{','.join(sorted(w['defs']))}\t{w['depth']}\t{w['chain']}")
        for (cat, name), (rel, kind) in sorted(untraced.items()):
            lines.append(f"{kind}\t{cat}\t{name}\t{rel}\t-\t-")
        args.meta.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
