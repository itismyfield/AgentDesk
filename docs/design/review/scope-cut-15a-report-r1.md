# 단위 15A r1 보고 — watcher·reaper·UI·health 잔여 input fence 소비자 제거

- 브랜치 `feat/scope-cut-15a-fence-consumers`. head `c18f2b6a89c7e5af6e1dd8ecfaa9f54ba4607bfd`. 리뷰 ref `refs/review/scope-cut-15a-r1`. `git ls-remote` 회신 `c18f2b6a89c7e5af6e1dd8ecfaa9f54ba4607bfd`로 head와 같다.
- 커밋:
  - `69d05fc3a`: WIP.
  - `7f4cb1152`: 본 변경.
  - `5c4d1a643`: origin/main `2239d6c47` 병합.
  - `c18f2b6a8`: skip-pin 재고정.
- **기준이 바뀌었다.** 브리프 기준은 `54ea7dd76`(단위 14 리뷰 ref)였다. 작업 중에 단위 14가 `de739c72d`(#7027)로, 13A가 `c66bfcde5`(#7026)로 main에 착지했다.
  - `--static-only`가 origin/main과의 합성 병합 충돌로 FAIL해서 rebase 없이 `merge_main_regen.sh`로 main을 병합했다. `SQUASHED_PARENT=54ea7dd76`을 썼다.
  - 그래서 15A는 이제 main 바로 위에 있다. 크기·giant 수치는 merge-base `2239d6c47` 기준이다.
- 로그 디렉터리: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/scope-cut/u15/u15a-logs/`.

## 1. census (사양 6절 exact-ref 두 명령, 기준 54ea7dd76)

- 결과 파일:
  - 생산 참조: `u15a-logs/census-prod.txt`, 634줄, 156파일.
  - 시험 참조: `u15a-logs/census-test.txt`, 999줄.
- 사양 전수표에 없던 생산 참조:
  - `watchers/lifecycle/restore_core.rs:18` (`boot_skip`): `restore.rs`에서 나뉜 자식 모듈이다. 15A가 import를 지우면 컴파일이 깨지므로 15A에 넣었다. 범위 밖·금지 목록에 없는 파일이다.
  - 컴파일 때문에 함께 바뀐 비목록 파일:
    - `recovery_engine.rs`·`health/recovery.rs`: 생성처가 사라진 `RebindError::InputFenced` 변형과 그 match 가지·시험.
    - `tui_direct_pending_start.rs`: `restore_gate::spawn_admitted` 제거에 따른 호출부.
    - `queued_placeholders_store.rs`: 시험만 바뀌었다.
- 그 밖의 생산 참조는 사양 표대로 다른 단위 몫이다:
  - 15B: `orphan_recovery.rs`.
  - 15C·13A/13B: `turn_orchestrator/**`, `session_transition.rs`, `inflight/**`, `headless_turn*`.
  - 16 또는 금지 파일: `router/intake_gate/**`, `intake_turn/**`, `cluster/intake_worker.rs`, `busy_followup_retry_store.rs`.
- HEAD 기준 15A 범위 파일의 잔여 참조 수(같은 정규식, `effect::` 포함):

| 파일 | 54ea7dd76 | HEAD | 남은 이유 |
|---|---|---|---|
| watchers/lifecycle.rs | 1 | 0 | |
| watchers/lifecycle/restore.rs | 16 | 0 | |
| watchers/lifecycle/restore_core.rs | 1 | 0 | |
| tmux_reaper.rs | 15 | 0 | |
| tmux_session_files.rs | 5 | 0 | |
| runtime_bootstrap/queued_recovery.rs | 3 | 0 | |
| terminal_ui_obligation.rs | 4 | 0 | |
| placeholder_sweeper.rs | 3 | 0 | |
| health/snapshot.rs | 1 | 1 | `:1082 .chain(input_runtime::health_reasons())`. `health_reasons`의 유일한 생산 호출자다. 지우면 input_runtime/mod.rs에 새 dead_code 경고가 난다. 이 함수는 fence allow 밖이고, 숨기기는 금지다. → 16 |
| health/watcher_respawn.rs | 3 | 0 | |
| health/watcher_respawn/idle_relay_absence.rs | 2 | 0 | |
| health/watcher_respawn/live_bridge_guard.rs | 5 | 0 | |
| health/turn_deliver/inject.rs | 6 | 5 | 남은 것: `:22` import, `:350 effect::admit`, `:593 effect::synchronous`, `:650 effect::scope`, `:763 effect::scope`. 593·650은 queue_io의 `s2b_closing_waits…` 시험이 이 permit 래퍼에 기대므로 되돌렸다. 13A 보충에 따라 admit·scope는 남긴다. → 13A 잔여·16 |
| recovery_engine/manual_rebind/live_bridge_guard.rs | 4 | 0 | |
| recovery_engine/manual_rebind/coordinate_adoption.rs | 3 | 0 | |
| turn_bridge/mod.rs | 1 | 1 | `:271 effect::detached(effect::current(), …)`. 금지 파일 `headless_turn/entrypoints.rs`의 c1b 시험이 "bridge가 permit을 쥔다"를 단언한다. → 15C |
| turn_bridge/retry_state.rs | 3 | 0 | |
| turn_bridge/watcher_orphan_cleanup.rs | 2 | 0 | |
| turn_bridge/followup_requeue.rs | 4 | 3 | 남은 3줄(431·442·489)은 모두 시험 `c1b_timeout_consumer_uses_preclose_bridge_effect…` 안이다. 13A 보충에 따라 13A 몫이다. 생산 참조는 0이다 |
| voice_barge_in/runtime_lifecycle.rs | 4 | 4 | `:568~594 any_protected/channel_gate`. 가지를 펴면 `provider` 인자가 쓰이지 않아 새 경고가 난다. 그 시그니처의 호출부는 금지 파일 `router/intake_gate*`에 있다. → 15C/16 |
| tui_prompt_relay/synthetic_start.rs | 1 | 0 | |
| shared_state.rs | 2 | 0 | |
| tui_direct_pending_start/restore_gate.rs | 3 | 0 | |

## 2. 전수표 (삭제 / 남김 / 운영 무발동)

운영에서는 `Gate::protect`가 시험에서만 등록된다. 그래서 아래 "삭제" 행은 모두 운영 값이 고정된 가지다: `boot_skip=false`, `lookup=None`, `any_protected=false`, `admit=Ok(None)`, `current()=None`. permit이 None이면 `scope/run/detached`는 안쪽 future를 그대로 돌려주고, `synchronous`는 closure를 실행한다. 이 경로로 편 것이다.

| 위치 | 처리 | 운영 동작 |
|---|---|---|
| restore.rs의 `boot_skip` continue/return 블록 전부, `n3bc_restore_tests::at/attempt` 시험 hook | 삭제 | 무발동 → 직접 경로 |
| restore_core.rs의 boot_skip 블록, 시험 hook 루프 | 삭제 | 무발동 |
| tmux_reaper.rs의 `input_boundary!`/`input_guard!` 매크로, boot_skip 가드, `input_target` closure | 삭제 | 무발동. host guard·liveness 판정은 그대로 |
| tmux_session_files.rs의 orphan sweep 가드 | 삭제. `with_session_temp_source_authority` 안 unlink는 유지 | 무발동 |
| queued_recovery.rs의 `protected` closure·retain, restore_standby_channel의 boot_skip | 삭제 | 무발동 |
| terminal_ui_obligation.rs의 `terminal_ui_input_protected`와 호출 4곳 | 삭제 | 무발동 |
| placeholder_sweeper.rs의 `\|\| boot_skip(...)` 2곳과 import | 삭제. 본체는 그대로 | 무발동 |
| watcher_respawn.rs의 boot_skip, row_io 래퍼 | 삭제. `load_inflight_state`·`reclaim_watcherless_session_bound_relay_owner`를 직접 호출 | 무발동 |
| idle_relay_absence.rs의 boot_skip | 삭제 | 무발동 |
| watcher_respawn/live_bridge_guard.rs의 admit·record_failure·`effect::scope`·row_io | 삭제. `recovery.run(retry_pending_watcher_respawn_admitted(..))` | admit은 운영에서 Ok(None) |
| inject.rs 직접 가지의 `match fence::lookup` | `None`으로 폄 | 무발동 |
| inject.rs의 admit·synchronous·scope | 남김 | 위 표 참고(13A·16) |
| manual_rebind/live_bridge_guard.rs의 admit_input·`effect::run` | 삭제. 두 진입점 모두 `try_recovery → rebind_inflight_for_channel_inner → recovery.run` | 무발동 |
| coordinate_adoption.rs의 permit·`effect::synchronous`·`fence::blocking` | 삭제. spawn_blocking이 `adopt_and_lock_inflight_episode`를 직접 호출 | `blocking`은 thread-local 표시뿐이다. gate가 없으면 소비자도 없다 |
| recovery_engine.rs·health/recovery.rs의 `RebindError::InputFenced`와 match 가지 | 삭제(생성처 0) | 무발동 |
| retry_state.rs의 spawn_observed 2곳 | 래퍼 제거, async 블록을 바로 실행 | 무발동 |
| watcher_orphan_cleanup.rs의 spawn_observed | 래퍼 제거 | 무발동 |
| followup_requeue.rs `rewatch_legacy_draft_holds`의 lookup retain | 삭제 | 무발동 |
| synthetic_start.rs `restore_pending_starts`의 lookup skip | 삭제. doc 주석 갱신 | 무발동 |
| shared_state.rs `insert/remove_queued_placeholder_on_worker_locked` | `*_locked`를 직접 호출 | 무발동 |
| restore_gate.rs의 input_effect·spawn_admitted, 호출부 tui_direct_pending_start.rs | 삭제. `task_supervisor::spawn_observed`를 직접 호출 | 무발동 |
| snapshot.rs health_reasons chain, turn_bridge/mod.rs bridge detached, voice lookup | 남김 | 1절 표의 이유 참고 |

## 3. 시험 덮음 표

main(2239d6c47) 대비 lib inventory manifest에서 시험 45건이 빠지고 10건이 더해졌다.

| 보존 계약 | 지운 fence 시험(대표) | 남은·고친 시험 | 지키는 계약 한 줄 |
|---|---|---|---|
| watcher restore 순서·claim | n3bc_restore_* 5건, c2b_startup_cleanup_leaves_a_protected_channel_alone_pg | `n3bc_restore_off_registry_keeps_legacy_adoption_and_old_row_semantics_pg`(any_protected 단언만 지움)와 p4c3w1 keyed_teardown 시험들 | 옛 row 형식 restore가 registry에 watcher를 claim하고 legacy adoption을 유지한다 |
| reaper host guard | c2b_dead_session_reaper…, c2b_orphan_cleanup…, n3bc_* 7건 | `dead_session_reaper_kills_only_what_the_host_guard_admits_pg`(유지), `n3bc_public_bound_legacy_orphan_cleanup_removes_old_files_pg`(옛 last-release 시험을 바꿈) | host guard가 허용한 것만 kill하고, bound legacy orphan은 kill한 뒤 파일을 지운다 |
| orphan file GC | input_fence_tests 4건 | `orphan_sweep_tests::orphan_file_sweep_removes_aged_bound_and_unbound_groups`(git mv) | 오래된 bound·unbound 파일 묶음을 통째로 지운다 |
| queued boot recovery·옛 형식 decoding | c2_boot_restore_installs_nothing_for_an_input_protected_channel | `boot_restore_installs_a_bound_channels_queue_marker_override_and_placeholder`(git mv)와 queued_recovery_draft/s2a 시험 | bound legacy 채널의 queue·marker live id·override·stale placeholder 카드를 restore한다 |
| terminal UI 일반 reconcile | input_fence_tests 6건 | `sweep_tests::sweep_reconciles_every_obligation_shape_pg`(git mv) | obligation 모양마다 REST edit 횟수·본문과 성공 뒤 clear |
| placeholder stall pass | tick_leaves_input_protected_rows_before_and_after_probe | `tick_retries_5xx_without_mutating_retired_rows`(기존 pass-level). 뮤턴트 6이 RED | sweep pass에서 Stalled 가지가 실제로 실행된다 |
| respawn | absence_observers_leave_an_input_protected_channel_unarmed, c2b 4건 | `legacy_retry_rebinds_an_old_format_row_and_keeps_legacy_call_column`(gate 절반 제거)와 the_sweep_arms… 등 health 577건 | retry가 옛 형식 row를 rebind하고 legacy call column을 유지한다 |
| manual rebind | c2_* 2건, c2b_admitted_rebind… | `rebind_writes_its_row_and_claims_its_watcher_through_either_entry` | 두 진입점 모두 row를 쓰고 watcher를 claim한다 |
| relay reattach | c2b_admitted_relay_reattach… | `reserved_relay_reattach_completes_its_pinned_rebind` | 예약된 reattach가 pinned rebind를 완료한다 |
| pending start claim/reclaim | input_effect_tests 4건, pending_start_input_fence | `pending_start_worker_writes_its_claimed_row_and_consumes_the_record`(git mv)와 tests.rs의 claim/reclaim/abort 시험 | worker가 claim한 row를 쓰고 record를 소비한다 |
| inject ordering | a_channel_closed_to_legacy_input_takes_no_paste | a_busy_pane…, a_flat_paste…, a_vetoed_paste…(유지) | pane send와 veto handback 순서 |
| bridge child 수명 | c1_actual_bridge_children_retain_effect… | `actual_bridge_children_run_through_cleanup_and_retry_completion` | bridge child가 cleanup까지 가고, retry release는 dispatch 완료를 기다린다 |
| on_worker placeholder accessory | (main 13A가 c1 시험 삭제) | `shared_async_accessory_writes_through_card_and_pending_clear_files` | on_worker insert/remove가 카드 파일과 pending-clear 파일에 기록된다. 뮤턴트 11을 이 시험만 잡으므로 병합 때 HEAD 쪽을 남겼다 |
| RebindError 매핑 | c2_input_fenced_maps_to_conflict | 해당 없음(변형 자체 삭제) | |
| voice route, health snapshot | 바꾸지 않음 | 기존 시험 그대로 PASS | |
| orphan recovery dispatch(사양 4절) | 15B 범위 | 해당 없음 | [범위 밖 15B] |

## 4. 뮤턴트 표

환경변수 스위치 `U15A_MUT=n`으로 심고, 시험 바이너리 한 번에 뮤턴트별로 실행했다. 로그는 `mutants.log`·`mut-*.log`·`mut2-*.log`다.

- 원복 확인: `git diff`가 비고, `git grep -c U15A_MUT HEAD -- src`가 0건이다.
- 실행 바이너리는 병합 전 head이고, 병합 뒤 다시 돌리지 않았다. 대신 뮤턴트를 심은 생산 파일은 `git diff 7f4cb1152 HEAD`에서 바뀐 것이 없다. 바뀐 파일은 queued_placeholders_store.rs·queued_placeholders.rs 둘뿐이고, 둘 다 main 쪽 변경이다. 지키는 시험은 병합 뒤 바이너리(`agentdesk-66cf926f56c659fb`)에서 모두 PASS다.

| # | 뮤턴트 | 지키는 시험 | 결과 |
|---|---|---|---|
| ① 1 | restore가 watcher claim 생략 | n3bc_restore_off_registry_keeps_legacy_adoption_and_old_row_semantics_pg | RED |
| ② 2 | reaper가 host guard의 Kept 무시 | dead_session_reaper_kills_only_what_the_host_guard_admits_pg | RED |
| ③ 3 | orphan file unlink 생략 | orphan_file_sweep_removes_aged_bound_and_unbound_groups | RED |
| ④ 4 | terminal UI 성공 edit 뒤 clear 생략 | sweep_reconciles_every_obligation_shape_pg | RED |
| ⑤ 5 | pane send를 NotSent로 대체 | a_busy_pane…_pg, a_flat_paste…_pg | RED(2건) |
| 6 | Stalled → Active | tick_retries_5xx_without_mutating_retired_rows | RED |
| 7 | rebind가 Err 반환 | rebind_writes_its_row_and_claims_its_watcher_through_either_entry | RED |
| 8 | queue merge 루프 생략 | boot_restore_installs_… | **살아남음.** 등가 뮤턴트다. marker merge의 `absorb_disk_queue`가 디스크 queue도 같이 흡수한다. 옛 fence 시험도 같은 사각이었다. 후속 후보 |
| 9 | pending-start spawn 생략 | pending_start_worker_writes_its_claimed_row_and_consumes_the_record | RED |
| 10 | respawn retry가 false 반환 | legacy_retry_rebinds_an_old_format_row_and_keeps_legacy_call_column | RED |
| 11 | on_worker insert 생략 | shared_async_accessory_writes_through_card_and_pending_clear_files | RED |
| 12 | marker merge 생략 | boot_restore_installs_… (:95) | RED |
| 13 | placeholder install 생략 | boot_restore_installs_… (:102) | RED |
| 14 | override 루프 생략 | boot_restore_installs_… (:101) | RED |
| 대조 | 스위치 없음 | 12건 / 1건 | GREEN 12/12, 1/1 |

필수 ①~⑤는 5/5 RED다. 전체는 13/14 RED이고, 남은 1건(#8)은 등가 뮤턴트다.

## 5. running N 대조

병합 뒤 head(`c18f2b6a8`, 바이너리 `agentdesk-66cf926f56c659fb`)에서 `pg_throwaway.sh`로 돌렸다. 기대 목록은 `expected-tests2.txt`이고, `--list`를 `filters.txt` 접두사로 걸러 4425건이다. 로그는 `m-*.log`다.

| 묶음 | 필터 | running | pass | fail | ignored |
|---|---|---|---|---|---|
| A | tmux::, tmux_reaper, runtime_bootstrap, terminal_ui_obligation, placeholder_sweeper | 988 | 986 | 1 | 1 |
| B | health::, recovery_engine, voice_barge_in, shared_state, queued_placeholders_store | 964 | 963 | 1 | 0 |
| C | turn_bridge | 774 | 774 | 0 | 0 |
| D1 | tui_prompt_relay::tests | 295 | 295 | 0 | 0 |
| D2 | tui_prompt_relay(tests 제외) | 311 | 310 | 0 | 1 |
| E1 | router | 445 | 443 | 2 | 0 |
| E2 | input_runtime, relay_recovery, queue_io, placeholder_controller, tmux_watcher, live_bridge, queue_dispatch, tui_direct_pending_start | 648 | 647 | 0 | 1 |
| 합계 | | **4425 = 기대 4425** | 4418 | 4 | 3 |
| 병합으로 들어온 시험 중 묶음 밖 | `touched-extra.txt`(MERGE_TOUCHED_TESTS 중 199건) | 199 | 198 | 0 | 1 |
| source_scan_readers | 18모듈 | 352 | 343 + PG 9(따로 PASS) | 0 | |

실패 4건:
- **A `channel_homes_tests::th6_standby_reclaim_restart_restores_turns_pg`**: main에서 들어온 시험이다. 병렬 묶음에서만 실패했다. 단독 3/3 PASS(`m-th6*.log`), runtime_bootstrap 모듈 병렬 2/2 PASS(`m-rb-*.log`, 194 pass). 브랜치에서도 재현되지 않아 main 대조는 하지 않았다. 부하 flaky로 판정한다.
- **B `relay_auto_heal::retired_redrive_tests::redrive_tree_ignores_only_foreign_death_markers`**: 남의 `.tmp` 파일이 끼어든 것이다. 단독 PASS(`m-redrive.log`). relay_auto_heal은 이번 diff에 없다. 병합 전 묶음에서도 같은 양상이었다(`fail6.log`).
- **E1 `s1_herdr_first_create_and_retired_use_herdr_pg`, `s1_real_missing_namespace_reaches_selected_tmux_start_pg`**: 기준 `54ea7dd76` 바이너리에서도 같이 FAIL한다(`base-s1.log`). 호스트 환경 문제이고 이번 변경 기인이 아니다. main-baseline 목록 파일은 이 base에 없다.

병합 전(head `6de2ff32e`, 같은 트리)에도 기대 4389 = running 4389였다(chunkA~E2). 병합 뒤 늘어난 것은 main에서 들어온 시험이다.

## 6. 검사

| 검사 | 결과 |
|---|---|
| `cargo fmt --all -- --check` | rc=0 |
| `cargo check --all-targets` (병합 뒤) | rc=0. 바뀐 파일에 걸린 경고 13종은 모두 diff에서 그 심볼을 건드린 줄이 0이다. 호출자를 지운 줄도 없다. 병합 전 base 대조에서는 `provider` 미사용·`InputFenced` 미생성·health_reasons dead를 신규로 잡아 되돌리거나 삭제로 해소했다(`check2.log`) |
| clippy `--all-targets` (바뀐 hunk) | 2건: `queued_placeholders_store.rs:366`, `status_panel_tests.rs:218` `await_holding_lock`. 옛 fence 시험의 `let _lock = lock_test_env()` 줄을 그대로 옮긴 것이다. main 같은 파일에도 같은 줄이 있다(status_panel_tests.rs:225 등). 저장소 전체 108건과 같은 관용이다. 엄밀히 0은 아니다(`clippy2.log`) |
| writer-gate skip pin | `test_only_module_skip_pin_paths.py`에서 바뀐 이름 4건을 교체하고 삭제 2건을 지웠다. 806 skipped = 806 pinned, 두 writer gate rc=0 |
| `prepush_gates.sh <worktree> --static-only` | **PREPUSH_STATIC_OK c18f2b6a8** (`static4.log`). push 전 같은 SHA에서 돌렸고, 그 뒤 트리 변경은 없다 |

브리프 5단계의 인자 순서 `prepush_gates.sh --static-only <worktree>`는 스크립트가 받지 않는다. 첫 인자를 worktree로 `cd`하고 shift해서 `--static-only`가 버려진다. 그 결과 전체 prepush가 돌고, Windows 교차 검사가 `/target-win…` 읽기 전용 경로에서 FAIL했다(`static2.log`·`static3.log`). 나머지 단계는 모두 ok였다. 올바른 순서 `<worktree> --static-only`로 다시 돌려 위 OK를 얻었다.

## 7. cap·giant

`pr_cap_prod.py 2239d6c47 HEAD`: prod 27파일, 순 코드 −376(+134/−510, raw +136/−626) → **CAP PASS**. 시험·생성물 26항목 +619는 세지 않는다. 브리프 상한 30/+800 안이다.

| giant 파일 | main | HEAD | raw Δ |
|---|---|---|---|
| health/recovery.rs | 6991 | 6979 | −12 |
| health/watcher_respawn.rs | 2584 | 2535 | −49 |
| placeholder_sweeper.rs | 1252 | 1249 | −3 |
| tmux_reaper.rs | 1720 | 1641 | −79 |
| tmux_session_files.rs | 1734 | 1708 | −26 |
| tui_prompt_relay/synthetic_start.rs | 2902 | 2896 | −6 |
| turn_bridge/followup_requeue.rs | 1476 | 1473 | −3 |
| turn_bridge/retry_state.rs | 1125 | 1118 | −7 |
| watchers/lifecycle/restore.rs | 1091 | 983 | −108 |
| (시험) tmux_reaper/host_guard_tests.rs | 1124 | 464 | −660 |
| (시험) health/turn_deliver/inject_tests.rs | 1868 | 1844 | −24 |
| (시험) turn_bridge/status_panel_tests.rs | 1999 | 1966 | −33 |
| (시험) tui_direct_pending_start/tests.rs | 3612 | 3611 | −1 |
| (시험) tui_prompt_relay/tests.rs | 6653 | 6653 | 0 |

health/snapshot.rs(되돌림)와 inject.rs(−3, 1000줄 미만)는 표에 없다. 새 `#[allow]`는 0이고, baseline은 바꾸지 않았다.

생성물은 공식 생성기로만 썼다(`merge_main_regen.sh`, inventory → PG manifest → non-PG filter). 대상은 `lib_test_inventory_manifest.txt`, `pg_test_lane_manifest.txt`, `ci/non-pg-test-filter.sh`, `sql_execution_surface_inventory.json`, `.github/workflows/ci-pr.yml`의 경로 목록 1줄, ARCHITECTURE.md다. skip-pin은 게이트 지시대로 `_paths.py`만 고쳤다.

## 8. 16 넘김 (및 15C·13A 잔여)

| 항목 | 위치 | 넘기는 곳 | 사유 |
|---|---|---|---|
| `input_runtime::health_reasons`와 snapshot chain | health/snapshot.rs:1082, input_runtime/mod.rs | 16 | 유일한 생산 호출자다. 지우면 allow 밖 dead_code 경고가 새로 난다 |
| inject.rs `effect::admit`·`effect::scope`(:350, :763) | health/turn_deliver/inject.rs | 16(보충 지시) | |
| inject.rs `effect::synchronous`·`effect::scope`(:593, :650) | 같은 파일 | 13A 잔여·16 | queue_io의 s2b closing 시험이 permit 래퍼에 기댄다 |
| bridge `effect::detached(effect::current(), …)` | turn_bridge/mod.rs:271 | 15C | 금지 파일 headless_turn/entrypoints.rs의 c1b 시험 |
| voice `any_protected`/`channel_gate` | voice_barge_in/runtime_lifecycle.rs:568–594 | 15C/16 | 가지를 펴면 `provider` 인자가 미사용이 된다. 호출부는 금지 파일 `router/intake_gate*` |
| followup_requeue 시험 c1b_timeout_consumer… | followup_requeue.rs:431–489 | 13A | 보충 지시 |
| `boot_skip`/`BootTarget` 생산 호출 파일 11 → 2 | 남은 곳: router/intake_gate/ledger.rs(금지), runtime_bootstrap/orphan_recovery.rs(15B) | 15B·16 | 호출자 0은 아니다 |
| 줄어든 그 밖의 fence API 호출 파일 수(main → HEAD, 생산·input_runtime 밖) | lookup 11→6, effect::admit 11→8, scope 7→5, run 7→5, detached 5→2, synchronous 3→2, current 18→14, blocking 4→3, any_protected 3→3, channel_gate 7→7 | 16 | 호출자 0이 된 input_runtime 함수는 없다. 모두 다른 곳에서 쓰거나 fence 모듈 `#[allow(dead_code)]` 아래에 있다. 지우지 않았다 |

## 9. 리뷰 집중 항목 (막은 방법 / 남은 반례)

- **조기 return·에러 경로가 보호 없음 경로와 같은가.** 펴는 방식은 모든 가지를 운영 값으로 대입하는 것뿐이다(`boot_skip=false`, `lookup=None`, `admit=Ok(None)`). 새 조건이나 return은 더하지 않았다.
  - watcher_respawn: 옛 `admit` Err의 `record_failure` 가지는 운영에서 닿지 않아 지웠다. retry 실패의 budget 처리는 `retry_pending_watcher_respawn_admitted` 안에 그대로 있다.
  - manual_rebind: `try_recovery(...)?`의 Err 전파는 그대로다.
  - 반례: 없음.
- **동시성 경합.**
  - coordinate_adoption: `fence::blocking` thread-local 표시를 지웠다. 소비자 `require_worker`는 gate가 있을 때만 그 표시를 본다. 운영에서는 gate가 없으므로 같다.
  - tmux_session_files: unlink는 여전히 `with_session_temp_source_authority` 안에서 한다. 잠금 범위는 바뀌지 않았다.
  - shared_state on_worker: persist lock을 쥔 상태에서 `*_locked`를 부르는 계약 그대로다.
  - 반례: 없음.
- **옛 형식 복원.**
  - queued_recovery: retain이 사라져 모든 채널이 decoding 경로를 탄다. 옛 bound legacy queue·marker·override·placeholder는 `boot_restore_installs_…`가 지킨다(뮤턴트 12·13·14 RED).
  - restore: 옛 row adoption은 `n3bc_restore_off_registry…`가 지킨다.
  - respawn: 옛 형식 row rebind는 `legacy_retry_rebinds_an_old_format_row…`가 지킨다.
  - 남은 반례: queue merge 루프 생략(뮤턴트 8)은 시험으로 구별되지 않는다(등가). 후속 후보다.
- **잘못된 0/PASS.**
  - running N은 묶음마다 대조했다. 기대 4425 = 4425, 0-test 묶음은 없다.
  - PG 시험은 pg_throwaway로 실제 실행했고, source-scan의 PG 9건도 따로 실행해 PASS다.
  - 뮤턴트는 지키는 시험의 RED 줄(:95/:101/:102 등)을 확인했다.
  - 알려진 PASS 아님:
    - clippy 바뀐 hunk 2건(관용 줄).
    - 병렬 flaky 2건과 base에서도 실패하는 s1 2건.
    - 뮤턴트를 병합 전 바이너리에서 실행한 점(뮤턴트 파일은 병합 전후 동일).

## 미해결·다음 행동

- 독립 리뷰 배정: 코디네이터.
- 이 레그에서는 native 전체 prepush를 띄우지 않았다(POSTREVIEW_BG 없음). 브리프 끝 절차가 `--static-only`까지이기 때문이다.
- 브리프 5단계의 `--static-only` 인자 순서 오기 수정을 제안한다.
- 후속 후보: queue merge 루프 등가 뮤턴트(#8)를 구별하는 시험.
