# #6210 r6 선행 산출물: relay 부작용 호출 지점 기계적 인벤토리 요약

대상: `/Users/itismyfield/.adk/release/worktrees/i6210-inv` @ `d834201262`(detached, 생성 시점 origin/main).
기준(무엇을 site로 세는지)의 정본은 `gen_inventory.py` 머리 docstring이다. 이 문서는 그 결과를 읽는 방법이다.
이 문서의 계수는 모두 `derive_tables.py`가 `inventory.tsv`에서 결정적으로 다시 뽑는 값이다(`stats.txt`).

## 0. 파일

| 파일 | 내용 |
|---|---|
| `gen_inventory.py` | 생성기. `python3 gen_inventory.py <worktree> --citations r5_citations.tsv --meta wrappers.tsv > inventory.tsv` |
| `inventory.tsv` | 3,128행. 열: 범주, file:line, 함수, 호출 경로 요약, 관리채널 도달, 도달 사유, (B) 요소, 현재 retire 주체, r5 행, 리뷰 태그, callee, 순번, 깊이, seed 종류 |
| `wrappers.tsv` | 폐포에 쓴 fn 목록. kind = `wrapper`(이름 규칙, 호출부 고정점까지) 554 / `one-level`(이름 규칙 밖, 호출부 한 번만 셈) 509 / `stop`(한 단계 호출부로만 잡혀 더 안 올라감) 471 |
| `r5_citations.tsv` | r5 초안(Sxx)과 r5 리뷰(Mxx·Rxx)의 file:line 인용을 옮긴 121행. `r5 행`·`리뷰 태그` 열의 근거 |
| `derive_tables.py` | `inventory.tsv`만 읽어 아래 네 파일을 만든다(소스 재스캔 없음) |
| `coverage.tsv` | 인용 121행 각각의 inventory 적중 수 |
| `unknown_by_fn.tsv` | 불명 행을 (도달 사유, 모듈, file::함수)로 묶은 381묶음 |
| `missing_by_fn.tsv` | r5 표 밖(`누락`) 행을 (범주, file::함수)로 묶은 1,577묶음 |
| `stats.txt` | 범주별 계수 |
| `pin_test_draft.py` | r6 PR-0용 CI 검사 초안(커밋 안 함). §7 |

## 1. 범주별 계수

| 범주 | 행 | 도달 예 | 아니오 | 불명 | r5 누락 | 누락 중 리뷰 태그도 없음 | 함수 수(S 태그 없는 함수) |
|---|---|---|---|---|---|---|---|
| A Discord HTTP 송신·수정·삭제 | 1,228 | 523 | 335 | 370 | 1,133 | 1,084 | 629 (606) |
| B inflight 행 삭제·재작성 | 720 | 668 | 0 | 52 | 615 | 530 | 476 (441) |
| C transcript·spool 삭제·truncate | 133 | 57 | 12 | 64 | 125 | 97 | 82 (76) |
| D offset·frontier·배달 경계·lease writer | 1,047 | 871 | 1 | 175 | 888 | 819 | 494 (454) |
| 계 | 3,128 | 2,119 | 348 | 661 | 2,761 | 2,530 | |

site가 있는 파일은 438개다. 그중 363개는 r5 초안·리뷰 어디에도 인용되지 않았고, 406개에는 S 태그 행이 하나도 없다.

seed 종류(행이 어떻게 잡혔는지):

- A: wrapper-call 654, serenity-method 295, unnamed-fn-caller 260, raw-rest 18, serenity-model 1
- B: wrapper-call 412, unnamed-fn-caller 274, inflight-scope 27, deferred-clear-request 6, raw-row-path-literal 1
- C: unnamed-fn-caller 46, fn-mentions-source 41, wrapper-call 29, named-seed 13, generation-rename 2, truncate 2
- D: unnamed-fn-caller 554, struct-init 220, field-assign 80, local-assign 68, frontier-gate-symbol 49, lease-transition 45, deref-assign 18, ctor 6, lease-state-write 4, atomic 3

`unnamed-fn-caller`는 "한 단계 규칙"의 산물이다. 이름 규칙에 안 걸린 fn(예: `reacquire_watcher_inflight_for_active_stream`)의 호출부를 한 번 센다.

## 2. r5 표 밖 site(`r5 행 = 누락`)

r5 초안의 S01~S20 인용 범위(파일·줄 범위 또는 `fn:` 지정)에 들지 않는 행은 **2,761/3,128**이다.
그중 관리 채널 도달이 `예`인 누락은 A 435, B 563, C 49, D 713이다.
함수 단위 목록은 `missing_by_fn.tsv`에 있다. 모듈별 상위(괄호는 도달 `예` 행 수):

- A: discord/commands 241(0), turn_bridge 112(112), router 89(0), outbound 61(52), tmux_watcher 51(51), server/routes 50(0), turn_view_reconciler 43(43), formatting 39(33), idle_recap_interaction 39(0), meeting_orchestrator 38(0), dispatches 36(0), gateway 22(0), recovery_engine 21(21), tui_prompt_relay 18(18)
- B: inflight 145(145), turn_bridge 105(105), recovery_engine 43(43), tui_prompt_relay 43(43), health 42(42), tmux_watcher 35(35), turn_finalizer 29(29), router 26(0), placeholder_sweeper 15(15), relay_recovery 14(14), tui_direct_pending_start 13(13), runtime_bootstrap 9(9), session_relay_sink 9(9), discord/mod 8(8)
- C: claude 18(18), router 17(0), codex 16(0), auto_queue 12(0), commands 12(0), qwen 11(0), tmux_session_files 10(10), tmux_common 7(7), runtime_bootstrap 5(5), tmux_reaper 3(3), turn_lifecycle 3(3)
- D: turn_bridge 254(254), tmux_watcher 103(103), inflight 87(87), outbound 51(0), tui_prompt_relay 50(50), recovery_engine 43(43), health 37(37), session_relay_sink 34(34), claude 18(18), router 15(0), codex 14(0), turn_finalizer 14(14), qwen 14(0), task_notification_delivery 11(11)

S 태그별 적중 행수: S01 7, S02 56, S03 38, S04 38, S05 117, S06 95, S07 25, S08 7, S09 17, S10 70, S11 29, S12 72, S13 74, S14 5, S15 87, S16 5, S17 11, S18 8, S19 3, S20 1.

해석 주의:

- `누락`은 "r5 표의 인용 범위 밖"이라는 뜻이다. 결함이라는 뜻이 아니다. 대부분은 표가 요약한 wrapper의 호출부다. 그래도 r6 표는 이 행들을 하나씩 소속시켜야 한다.
- 도달 `예`는 규칙표(생성기 `RULES`)가 모듈 단위로 준 판정이다. 개별 호출의 실제 도달은 r6 설계가 확정한다. 판정 근거는 `도달 사유` 열에 있다.

## 3. r5 리뷰가 지적한 site의 포함 확인

인용 121행(초안 S 50, 리뷰 M·R 71) 중 **96행은 인용 범위 안에 inventory 행이 있다**.
적중 0인 25행을 하나씩 확인했다. 전부 A~D 부작용 호출이 아니거나, 그 부작용이 같은 파일의 다른 행이나 호출부 행으로 이미 잡혀 있다.
스크립트 누락으로 판정된 행은 없다. 이 확인 과정에서 누락으로 드러난 것은 스크립트를 고쳐 포함시켰다:
한 단계 규칙, 함수 값 참조, `use` 재수출 제외, fn 경계 shape, D 축약 초기화, C truncate 범위.
그 수정으로 새로 잡힌 인용: S02, S08, S10(tmux.rs), S12, M15(tmux_watcher.rs:2446), M17(session_rotation_settle.rs:270 `delivered_frontier` 축약 초기화), S20(spawns.rs:261).

| 태그 | 인용 | 적중 0 사유 |
|---|---|---|
| S07 | session_relay_sink/idle_jsonl.rs 19-40,133-150,235-249 | 타입 정의·inflight 게이트 판정·로그 시각. writer 없음 |
| S10 | recovery_engine/phase_policy.rs 32-104 | 술어(predicate). 같은 S10의 tmux.rs 행은 적중 |
| S13 | tmux_session_files.rs 613-640 | `committed_frontier_for_current_generation`: 원자값 load(reader) |
| S14 | outbound/delivery_record.rs 85-159 | `DeliveryRecord` struct 정의. 필드 쓰기는 호출부 D 행 |
| S15 | tmux_watcher.rs 2047-2195 | anchor 완료가 `note_tui_anchor_completed` → reconciler 메서드 `.note_turn_completed(`로 간접 호출된다(메서드 간접 호출 한계, §6). 같은 파일에 S15 행 71개 |
| S19 | tmux_watcher/utf8_chunk_decoder.rs 12-112 | 디코더(reader) |
| S19 | tmux_common.rs 15-110 | source-authority lock static 정의 |
| S20 | runtime_bootstrap/deferred_restart.rs 72-84,142-161,209-225 | 재시작 rollback·`publish_restart_terminal` sentinel. 재시작 제어라 A~D 밖 |
| S20 | task_supervisor/watcher_completion.rs 27-46 | 완료 대기·관측 ticket |
| S20 | scripts/deploy-release.sh 2847-2891 | 스캔 범위(src/**/*.rs) 밖. d834에서는 2859-2903이다(§8). 바이너리 `.prev` 백업·승격 |
| M1 | tmux_watcher/liveness.rs 172 | `should_probe_tmux_liveness` 가드 |
| M2 | turn_bridge/retry_state.rs 99-110 | `bridge_confirmed_response_sent_offset_seed` 값 계산기. 대입은 호출부 D 행 |
| M3 | turn_bridge/bridge_entry_persist.rs 294 | `streaming_rollover_frozen_msg_ids` deref 대입. offset 필드 아님. 감싸는 fn에 3행 |
| M4 | turn_bridge/retry_state.rs 203 | fn 시그니처 줄. 같은 fn의 215/219/220이 D 행 |
| M7 | recovery_engine/idle_captured_response.rs 36,62,65 | 36은 시그니처, 62/65는 가드. 같은 파일 A 행(:157) 포함 3행 |
| M7 | tui_prompt_relay/synthetic_start/bridge_handoff.rs 516 | `resume_unpublished`: `capture_dormant`의 custody try_lock·lease clone. A~D 밖 |
| M12 | placeholder_sweeper.rs 441-457 | `classify_age` 판정. 같은 fn 계열에 9행 |
| M16 | outbound/delivery_record.rs 1605-1633 | frontier gate 심볼 정의. 호출부가 D 행(frontier-gate-symbol) |
| M16 | recovery_engine/terminal_watcher.rs 124-139 | `recovery_watcher_start_offset` 계산기 |
| R11 | tmux.rs 256 | reader |
| R11 | tmux_watcher/liveness.rs 469-477 | `build_watcher_reacquire_inflight_state`: 메모리 상태 구성. 영속화하는 호출부는 B 행 |
| R11 | watchers/lifecycle/restore.rs 514-518 | 가드. 같은 파일 10행 |
| R11 | claude.rs 2100-2103,2513 | `output_path` 신원 비교. offset 아님. 같은 파일 11행 |
| R31 | tmux_common.rs 1471,1924-1971 | 1471은 `truncate_jsonl_head_safe` 정의(호출부가 C named-seed 행). 1924-1971은 분류기·reader |
| R31 | idle_recap_interaction.rs 561,571 | `send_followup_prompt`는 tmux 입력이다. Discord HTTP 아님 |

재현: `python3 derive_tables.py` 후 `awk -F'\t' 'NR>1 && $5==0' coverage.tsv`.

### 교차 확인

- **inflight 행 unlink**: B의 `remove_file` seed는 16행이다.
  - Rust 쪽 15행: inflight.rs·clear_store/**·rebind_reap.rs·removal.rs·boot_reaper.rs
  - 정책 op `agentdesk.inflight.remove`의 구현 1행: engine/ops/exec_ops.rs:303
  - 여기에 archive `rename` 1행이 더해진다(clear_store/mod.rs:274).
  - r5 리뷰의 "17"과의 차이는 test-only 후보(status_panel_singleton_store.rs:309, inflight.rs:5962, removal.rs:1338)의 집계 방식이다. `log_inflight_remove` 호출 위치 대조와 일치한다.
- **JS 정책**: `policies/` 아래에 `agentdesk.inflight.remove` 호출자는 0이다.
- **이름 충돌**: `clear_inflight_by_tmux_name`이 세 곳에 정의된다(turn_lifecycle.rs:639, discord/mod.rs:511, inflight.rs:471). 이름 매칭이라 세 정의의 호출부가 합쳐진다. `wrappers.tsv`의 `files` 열에서 정의 파일 목록을 볼 수 있다.
- **test-only**: save_store.rs:79 `save_inflight_state`는 `#[cfg(test)]`이고, tmux_common.rs:2233 `set_len`은 테스트 모듈 안이다. 둘 다 production text에서 지워지므로 행이 없다. 정상이다.

## 4. 불명 661행: 무엇이 있어야 정해지는가

전체 목록은 `unknown_by_fn.tsv`에 있다. 묶음 단위는 file::함수다.

| 도달 사유(= inventory 열 값) | 함수 | 행 | 주 모듈 | 판정에 필요한 것 |
|---|---|---|---|---|
| services 기타; 호출 채널 추적 필요 | 121 | 180 | dispatches 36, codex 30, qwen 25, auto_queue 21, routines 12, codex_tui 9, cluster 8, onboarding 6 | 채널 ID 인자의 출처 추적. 해당 서비스가 TUI-direct 관리 채널(provider 세션이 붙은 채널)에 보낼 수 있는지. dispatch·routine 대상 채널이 관리 채널과 겹칠 수 있는지에 대한 설계 결정 |
| discord 기타 모듈; 채널 선택 인자 추적 필요 | 72 | 103 | gateway 22, tmux_placeholder_suppression 20, terminal_ui_obligation 8, monitoring_status 7, startup_reclaim 7, relay_health 7, destructive_cancel_gate 6, mailbox_finish 5, delivery_lease_key 5 | 모듈별 규칙 1줄씩. 그 모듈이 관리 채널 턴을 다루는지는 모듈 담당 코드 확인으로 정한다. gateway는 전 채널 공용이라 호출별 판정이 필요하다 |
| services 밖(서버 라우트·CLI 등); 대상 채널 인자 추적 필요 | 52 | 88 | server/routes 58, cli/discord 6 | API 요청의 channel_id가 관리 채널일 수 있는지. 운영자 수동 API(R=수동 재생 포함)를 r6이 관리 채널에 허용하는지에 대한 결정 |
| 같은 채널의 Discord 발 턴; 합성 턴 행·ID와의 상호작용 판정 필요 | 51 | 147 | router | 관리 채널에 Discord 사용자 메시지가 들어올 때 router가 합성 턴의 inflight 행·placeholder ID를 건드리는지. r6의 "관리 채널 소유권" 정의가 있어야 정해진다 |
| wrapper 내부인데 추적된 호출자가 없음(이름 제외 목록 또는 간접 호출) | 30 | 55 | outbound 52 | 호출부가 한 단어·generic 이름(§6 목록)이나 메서드 간접 호출이라 추적이 끊겼다. 호출부를 손으로 찾아 행을 붙이면 호출자 규칙에서 도달이 유도된다 |
| 호출자 도달이 불명 포함 | 16 | 17 | outbound, discord_io, http, formatting | 위 불명 묶음이 정해지면 자동으로 정해진다(유도 규칙) |
| 관리 채널에 recap을 올리는지 채널 선택 인자 추적 필요 | 13 | 37 | idle_recap_interaction 29, idle_recap 8 | idle recap이 TUI-direct 세션 채널에도 게시되는지(현재 설정·게이트 확인) |
| Discord 발 큐 턴; 관리 채널 큐 공유 여부 판정 필요 | 11 | 14 | queue_dispatch, queue_io, queue_marker | 관리 채널의 Discord 큐와 합성 턴이 같은 mailbox·큐 마커를 쓰는지 |
| Discord 발 턴 placeholder와 공용; 관리 턴 경로 사용 여부 호출자 추적 필요 | 9 | 10 | placeholder_controller | 합성 턴 경로가 이 placeholder API를 쓰는지 호출자 확인 |
| 사용자 메시지 reaction; 합성 턴 prompt anchor 대상 여부 판정 필요 | 6 | 10 | reaction_lifecycle | 합성 턴 prompt anchor 메시지에 reaction이 붙는지(anchor가 봇 메시지인지 사용자 메시지인지) |

추정으로 채운 불명은 없다. 규칙에 안 걸리면 불명으로 남겼다.

## 5. 한 단계 규칙과 전체 폐포를 쓰지 않은 이유

이름 규칙 없이 전 호출 그래프를 무조건 폐포로 돌리면 결과가 **9,765행**이 된다(A 3,584, B 2,862, C 2,826, D 493, 깊이 최대 36).
C가 2,826행으로 번지는 것은 `main`·서버 부트스트랩까지 올라가기 때문이다. 부작용 site 판정에는 쓸모가 없다.
그래서 다음을 택했다:

- 이름 규칙 wrapper: 호출부를 고정점까지 따라간다.
- 이름 밖 fn: 호출부를 한 번 센다(`one-level` 509개).
- 한 단계 호출부로만 잡힌 fn(`stop` 471개): 더 올라가지 않는다.

`stop` fn이 다시 primitive를 감싸는 새 경로가 되면 pin 검사(§7)가 `wrappers.tsv` 차이로 잡는다.

## 6. 한계(이름 기반 lexer)

- rustc 타입 해석이 없다. 이름으로만 매칭하므로 같은 이름의 정의가 여럿이면 호출부가 합쳐진다(§3 교차 확인).
- 메서드 간접 호출(`.note_turn_completed(` 같은 trait·reconciler 메서드), actor·mpsc·queue를 통한 디스패치, 콜백 등록은 추적하지 않는다.
- `macro_rules!` 본문 안의 site는 fn이 `<module>`로 나온다(예: turn_bridge/stream_loop/types.rs:251 `dispatch_pinned_terminal!`). 매크로 호출부는 추적하지 않는다.
- 한 단어·generic 이름(`GENERIC_NAME_EXCLUDE`) wrapper는 `<정의 모듈>::NAME(` 호출만 센다. 메서드 `.NAME(`으로만 불리면 놓친다. 해당 fn 50개:
  - A wrapper: `delete`(mod.rs, queued_card_gate.rs, queued_placeholders.rs), `deliver`(outbox_actionable_delivery.rs, fresh_send.rs, terminal_handoff.rs, stream_loop/types.rs), `edit`·`send`(status_panel.rs, cli/discord.rs)
  - B wrapper: `commit`(completion_delivery.rs)
  - one-level·stop: A `act begin channels create drain resolve router start transition`, B `apply capture claim drain drop evaluate reconcile router`, C `activate resolve`, D `acquire activate advance allows bootstrap capture checkpoint claim commit decision decode deliver drop execute external from load message new observe persist reason reconcile record release spawn`
  - 전체는 `wrappers.tsv`에서 이름에 `_`가 없는 행이다.
- lexer 결함 우회: `#[cfg(test)] let x = if … {`에서 lexer가 첫 줄만 지워 중괄호 균형이 깨진다(task_notification_context.rs:472). fn 경계는 주석·문자열만 지운 shape 텍스트로 잰다.
- 스캔 범위 밖(src/**/*.rs 아님)이지만 A~C 부작용이 있는 스크립트:
  - scripts/e2e/run_tui_relay.py: inflight 디렉터리 조작 1049·1472, 강제 cancel API 1123
  - scripts/e2e/tui_relay/discord.py: e2e Discord API
  - scripts/voice-channel-migrate.sh: Discord REST(권한 변경만)
  - scripts/deploy-release.sh: 바이너리 백업·승격, 재시작 제어
  - 운영 코드 경로가 아니므로 pin 대상에서 뺀다. r6이 e2e를 관리 채널 계약 검증에 쓰면 따로 본다.

## 7. pin 검사 초안(`pin_test_draft.py`, r6 PR-0용, 커밋 안 함)

- 키: (범주, 파일, 함수, callee, 순번). 순번은 (범주, 파일, 함수, callee) 안에서 줄 순서로 매긴 번호다.
  - 줄 번호는 키에 넣지 않는다. 무관한 편집으로 줄이 밀려도 통과한다.
  - 같은 함수 안에서 같은 callee 호출이 늘거나 줄면 잡힌다.
- 실패 조건:
  - 새 키: "classify it in inventory.tsv"
  - 사라진 키: "update baseline"
  - `wrappers.tsv`의 (kind, 범주, 이름) 증감
  - baseline 판정 열 공란
  - 불명 행 수가 상한(661)을 넘음(래칫)
- precommit-block 3-1 근거: 호출 지점 집합은 런타임 경로가 아니라 소스의 성질이라, 동작 테스트는 자기가 아는 경로만 밟는다. r3~r5에서 매번 표 밖 sender가 나온 공백이 그것이다.
- 확인:
  - 이 worktree에 대해 실행하면 `relay side-effect inventory: OK`, exit 0이다(약 100초).
  - baseline을 교란한 복사본으로도 실행했다: A 1행 삭제, 가짜 키 1행 추가, wrapper 1개 삭제.
  - 그 결과 `new relay side-effect site src/cli/dcserver.rs:1270 …`, `inventory key no longer in source …`, `new wrapper fn [A] adopt_watcher_singleton_panel_after_fresh_bind …`, 3 problems, exit 1이 나왔다.
- 이식할 때:
  - `gen_inventory.py`·`inventory.tsv`·`wrappers.tsv`·`r5_citations.tsv`를 `scripts/relay_inventory/`로, 검사를 `scripts/check_relay_side_effect_inventory.py`로 둔다(초안의 기본 경로).
  - 실행 시간(~100초)이 prepush 예산에 부담이면 CI 전용 job으로 둔다.

## 8. 기준 SHA 이후 relay 경로 변화

`git diff --stat 06f860d932 origin/main -- src/services/discord/ src/services/turn_orchestrator/ src/db/` 결과는 빈 출력이다.
src 밖 관련 변경은 `scripts/deploy-release.sh` +14/−2다:

- 1행: build token 중첩 거부 +11
- 655행: build token 아래 cargo clean +1

S20 인용 2847-2891은 d834에서 약 2859-2903에 해당한다. 내용(`.prev` 백업·승격)은 같다.
작업 중 origin/main이 두 커밋 전진했다(보고 시점 7c776629cb).

- d8b2e72f6c(#6266): `scripts/ci/h2_measure.py`와 그 테스트만 바뀌었다.
- 7c776629cb(#6274): src/db/campaigns.rs에 `NodeInput` 필드 선언 +5, 그 테스트 +40, API 문서 문자열 1줄(routes/docs, A_REST 제외 파일)이 바뀌었다.

두 커밋이 건드린 파일에는 inventory 행이 0개다. 추가된 코드도 seed 패턴에 걸리지 않는 필드 선언·문서 문자열이다. 따라서 7c776629cb에서도 pin 키 집합은 같다. 이 판단은 diff를 읽고 내린 것이고, 그 SHA로 생성기를 다시 돌리지는 않았다.
