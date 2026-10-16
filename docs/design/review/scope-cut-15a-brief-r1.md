# 범위 축소 단위 15A 구현 r1 — watcher·reaper·UI·health 의 잔여 input fence 소비자 제거 (미니 계정1 Opus, 코디네이터 맥 14:2x KST 10-16)
역할: 구현자. 독립 리뷰는 맥 코디네이터가 배정한다. 다른 에이전트에게 다시 위임하지 않는다.

## 배경
- 사용자 10-14 범위 축소: TUI 릴레이는 세 목표(세션 즉시 입력 주입, transcript 단위 Discord 전달, transcript 파일 추종)만 남기고 쓰지 않는 main 코드는 지운다. 입력 runtime(`input_runtime/`)은 운영에서 꺼진 휴면 계층이다. fence gate 등록은 `Gate::protect` 하나이고 생산 등록처 `input_runtime/supervisor/reserve.rs` `reserve_boot`는 시험에서만 호출된다. 그래서 운영에서 `fence::lookup`·`boot_skip`·`any_protected`·`health_reasons`는 항상 "보호 없음"이다(단위 14 리뷰 CLEAN이 독립 확인).
- 사양(먼저 읽는다): `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/scope-cut/u15/design-u15-spec-r1.answer.md` 전체. 특히 1절 생산 참조 전수표, 2절 시험 판정, 3절 15A, 4절 증명 시험·뮤턴트, 6절 첫 명령. 좌표 기준은 `54ea7dd76`.
- 기준: `refs/review/scope-cut-14-r1` = `54ea7dd76f257e7a89a8d07e9e12beb01e1acaa2`(main b50ab97c7 + 단위 14, 리뷰 CLEAN·착지 대기). 15A는 단위 14와 같은 PR로 묶어 착지할 수 있게 그 위에 쌓는다.

## 범위 (15A, 접두사 src/services/discord/)
watchers/lifecycle.rs watchers/lifecycle/restore.rs tmux_reaper.rs tmux_session_files.rs runtime_bootstrap/queued_recovery.rs terminal_ui_obligation.rs placeholder_sweeper.rs health/snapshot.rs health/watcher_respawn.rs health/watcher_respawn/idle_relay_absence.rs health/watcher_respawn/live_bridge_guard.rs health/turn_deliver/inject.rs recovery_engine/manual_rebind/live_bridge_guard.rs recovery_engine/manual_rebind/coordinate_adoption.rs turn_bridge/mod.rs turn_bridge/retry_state.rs turn_bridge/watcher_orphan_cleanup.rs turn_bridge/followup_requeue.rs voice_barge_in/runtime_lifecycle.rs tui_prompt_relay/synthetic_start.rs shared_state.rs tui_direct_pending_start/restore_gate.rs 
- 각 `boot_skip`/`lookup`/`any_protected`/`health_reasons` 가지를 현재 운영 경로(보호 없음·None)로 펴고, `effect::*` 래퍼는 안쪽 future/closure를 직접 실행한다. input_runtime 타입을 공개 시그니처로 들고 있는 함수는 이 소단위에서 바꾸지 않는다(15C).
- `placeholder_sweeper.rs`는 fence 두 줄만 지운다. stall/abandon/probe 본체는 그대로.
- 범위 밖: `recovery_engine/restore_inflight.rs`·`runtime_bootstrap/orphan_recovery.rs`(15B, #6858 뒤), `session_transition.rs`·`mailbox_finish.rs`·`turn_orchestrator/**`·`turn_finalizer*`·`turn_bridge/before_start_stop*`·`inflight/**`·`tui_followup.rs`·`headless_turn*`(15C·13A/13B), `input_runtime/**`·`tui_input/**`(16). 단 `input_runtime` 안의 함수가 호출자 0이 되면 지우지 말고 보고서 "16 넘김" 표에 적는다(16에서 모듈째 삭제).
- 1단계(빌드 전): 사양 6절 exact-ref census 두 명령을 이 worktree에서 실행해 결과를 보고서 첫 절에 붙이고, 사양 전수표에 없는 생산 참조가 나오면 15A/B/C 중 어디에 둘지 적는다. 15A에 넣어야 하는데 위 범위 밖 파일이면 쓰기 전에 `FILES <파일> — <이유>`로 멈춘다.

## 금지 파일 (다른 레인·열린 PR 소유)
위 "범위 밖" 전부, 그리고 `router/intake_gate*`·`router/message_handler/intake_turn*`·`intake_dispatch*`·`commands/**`·`adk_session*`·`claude_tui/**`·`runtime_bootstrap/gateway_runtime.rs`·`tmux_watcher/terminal_relay_plan*`. 생성 manifest·ARCHITECTURE.md·skip-pin은 공식 생성기로만 재생성한다(`/Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/merge_main_regen.sh` 규칙).

## 시험
- 사양 2절 표대로: fence 시험 파일을 통째로 지우지 않는다. 각 시험이 함께 단언하던 보존 계약(restore 순서, reaper host guard, orphan GC, manual rebind, queued old-format decoding, terminal UI 일반 reconcile, respawn, pending start claim/reclaim, inject ordering, voice route)을 어떤 남는 시험이 덮는지 표로 적고, 덮는 시험이 없으면 fence 없이 그 계약만 단언하도록 고쳐 남긴다(단위 12 교훈: 유일한 계약 시험을 함께 지워 P1).
- placeholder sweeper: 실제 sweep pass에서 stall 가지를 지우는 뮤턴트를 잡는 pass-level 시험이 있는지 먼저 확인하고, 없으면 최소 functional 시험 하나를 남긴다(사양 4절 끝).
- 뮤턴트(남는 시험이 RED여야 함, 사양 4절에서 최소 5개): ① watcher restore의 pending watcher 생성 생략 ② reaper가 host guard 무시하고 kill ③ orphan file GC unlink 생략 ④ terminal UI obligation HTTP edit 또는 성공 뒤 clear 생략 ⑤ turn deliver pane send 생략. 원복 뒤 GREEN, 원복 blob이 커밋과 같음을 `git diff`로 확인.
- 대상: 바뀐 파일의 모듈 시험 전체(watchers, tmux_reaper, tmux_session_files, runtime_bootstrap, terminal_ui_obligation, placeholder_sweeper, health, recovery_engine::manual_rebind, turn_bridge, voice_barge_in, tui_prompt_relay::synthetic_start, tui_direct_pending_start). PG 시험은 `bash /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/pg_throwaway.sh cargo test --lib -- <filters>`. 기대 시험 수를 먼저 적고 `running N`과 대조한다. full-lib은 돌리지 않는다.
- `cargo check --all-targets`, clippy 변경 hunk 진단 0, fmt.

## 크기
- 리뷰 단위 상한 30파일/순증 +800(`python3 /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py 54ea7dd76 HEAD`). 설계 추정 생산 20–22파일, 순삭제 −450~−850.
- giant(1000줄 초과) 파일 raw 순증 0 이하 — synthetic_start.rs·health/snapshot.rs·watcher_respawn.rs·tmux_session_files.rs·tmux_reaper.rs·followup_requeue.rs·placeholder_sweeper.rs·retry_state.rs 등. 파일별 줄 수 전후 표. dead-code baseline은 감소만 반영한다.

## 순서·규칙
1. worktree 생성 뒤 `bash /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/seed_target.sh <worktree>` → `bash /Users/itismyfield/.adk/release/workspaces/agentdesk/scripts/wait_headroom.sh` 한 번 → 첫 빌드. 기다리는 동안 1단계 census·전수표를 쓴다.
2. 셸마다 `unset TMUX TMUX_PANE`, `export CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none`, cargo 앞 PATH에 `/Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/cargo-shim`. cargo·시험을 nohup·&로 던지지 말고 끝까지 기다린다.
3. 금지: 새 `#[allow]`·baseline 인상·`RUST_MIN_STACK`·`--no-verify`·force·`rm -rf`·rebase·reset·맨 stash, PR 브랜치 push, 운영 설정·DB 접근, agentdesk.yaml 읽기·쓰기. 새 dead_code 경고를 allow·stub·cfg(test) 한정으로 숨기지 않는다. 주석은 무엇/왜 1~2줄.
4. 리뷰 집중 항목(조기 return·에러 경로가 보호 없음 경로와 같은지, 동시성 경합, 옛 형식 복원, 잘못된 0/PASS)을 커밋 전에 보고서에 한 줄씩 적고, 각 줄에 막은 방법이나 남은 반례를 쓴다.
5. 통과하면 커밋(브랜치 `feat/scope-cut-15a-fence-consumers`) → `git push origin HEAD:refs/review/scope-cut-15a-r1` → `git ls-remote origin refs/review/scope-cut-15a-r1` read-back → `bash /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/prepush_gates.sh --static-only <worktree>`.

## 끝
보고서 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/scope-cut/u15/report-u15a-r1.md`: census 결과, 전수표(삭제·남김·운영 무발동), 시험 덮음 표, 뮤턴트 표, running N 대조, cap·giant 표, 16 넘김 표, 리뷰 집중 항목.
마지막 메시지 첫 줄 `REVIEW_READY scope-cut-15a head=<full sha> ref=refs/review/scope-cut-15a-r1 cap=<files>/<net> tests=<p/f/s> mutants=<n/n>`, 다음 줄 `LANE_DONE static=<OK|FAIL>`. 막히면 `BLOCKED <사유>` / `FILES …` / `DESIGN_DELTA …`.

## 보충(14:35) — 13A와 겹치는 hunk 금지
- `health/turn_deliver/inject.rs`의 reserve/hand_back/settle 호출 3곳 permit 인자는 13A가 지운다. 15A는 그 밖의 fence 가지만 편다.
- `turn_bridge/followup_requeue.rs`의 시험 `c1b_timeout_consumer_uses_preclose_bridge_effect_for_binding_and_all_sources`는 13A가 지운다. 15A는 다른 c1b_* fence 시험만 다룬다.

## 금지 hunk (맥 14:35 보충 — WSL 13A FILES 승인 범위, 15A는 건드리지 않는다)
- `health/turn_deliver/inject.rs`: reserve/hand_back/settle 호출 3곳의 permit 인자 제거는 13A 몫. 15A는 그 밖의 fence 가지(`lookup`·`boot_skip`·`effect::*` 래퍼 등)만 편다. `effect::admit`(:350 부근)·`effect::scope`(:767 부근)는 사양 5절대로 16 몫이면 남긴다.
- `turn_bridge/followup_requeue.rs`: 시험 `c1b_timeout_consumer_uses_preclose_bridge_effect_for_binding_and_all_sources` 삭제는 13A 몫. 15A는 다른 c1b_* fence 시험만 다룬다.
