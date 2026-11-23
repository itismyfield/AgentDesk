# KE-T 조각별 대상 목록 (base f352ad6f96 = bdf9ad2227 + 56438ccde0, scan_a.py/scan_b.py/plan_b.py/slices_ke.py 산출)

줄 번호는 모두 base 기준이다. 조각 착수 때 그 조각 base에서 스캐너를 다시 돌려 재판정한다.

## (A) feature 전환 대상(정의 위치 → 조각)

정의 위치는 위쪽 시험이 직접 닿는 root gate 줄이다. 시험 전용 파일이면 그 파일을 선언하는 `mod` 줄의 gate다. 조각이 실제로 바꿀 gate 전부(root + 닫힘)는 아래 `(A) gate 전수표`가 정본이다(수리 1).

| 조각 | 정의 위치 | 사용 행 | 사용 파일 | 항목 | 비고 |
|---|---|---:|---:|---|---|
| KE-T1 | `src/dispatch/mod.rs:10` | 140 | 48 | `test_support` | 섞인 시험 모듈(#[test] 1); gate 안 위쪽 참조 {"TS": 3} |
| KE-T1 | `src/engine/mod.rs:1049` | 7 | 3 | `eval_js` |  |
| KE-T1 | `src/kanban/transition_core.rs:29` | 1 | 1 | `with_preflight_review_enter_outbox_suppressed` |  |
| KE-T1 | `src/reconcile.rs:238` | 1 | 1 | `reset_dispatch_delivery_event_mismatch_metrics_for_tests` |  |
| KE-T1 | `src/reconcile.rs:254` | 1 | 1 | `lock_dispatch_delivery_metric_tests` |  |
| KE-C/M | `src/services/auto_queue/runtime.rs:616` | 1 | 1 | `slot_reset_host_pg_tests` | 수리 1 P1-3: `runtime_clears` 묶음(helper·기록 경로 `runtime.rs:274-294`·static `:600,609`·의존 시험 2파일)째 KE-C/M으로 넘김. KE-T 범위 밖; 섞인 시험 모듈(#[test] 5); gate 안 위쪽 참조 {"PROD": 5, "FIX": 13, "ALIAS": 1, "AMB": 1} |
| KE-T1 | `src/services/campaign_execution/control.rs:971` | 1 | 1 | `tests` | 섞인 시험 모듈(#[test] 16) |
| KE-T1 | `src/services/discord/busy_followup_retry_store.rs:66` | 4 | 1 | `set_clock_for_tests` | B와 다른 hunk(KE-T5 인라인 RELOC 파일) |
| KE-T1 | `src/services/discord/busy_followup_retry_store.rs:73` | 1 | 1 | `fail_release_steps_for_tests` | B와 다른 hunk(KE-T5 인라인 RELOC 파일) |
| KE-T1 | `src/services/discord/busy_followup_retry_store.rs:79` | 2 | 1 | `fail_saves_for_tests` | B와 다른 hunk(KE-T5 인라인 RELOC 파일) |
| KE-T1 | `src/services/discord/busy_followup_retry_store.rs:387` | 1 | 1 | `state_is_capped` | B와 다른 hunk(KE-T5 인라인 RELOC 파일) |
| KE-T1 | `src/services/discord/busy_followup_retry_store.rs:392` | 6 | 4 | `is_capped` | B와 다른 hunk(KE-T5 인라인 RELOC 파일) |
| KE-T1 | `src/services/discord/busy_followup_retry_store.rs:457` | 12 | 11 | `record_busy_retry` | B와 다른 hunk(KE-T5 인라인 RELOC 파일) |
| KE-T1 | `src/services/discord/inject_disposition.rs:469` | 3 | 2 | `test_support` |  |
| KE-T1 | `src/services/discord/inject_disposition/dispatched.rs:131` | 12 | 4 | `fail_writes_for_tests` |  |
| KE-T1 | `src/services/discord/recovery_known_ids.rs:176` | 1 | 1 | `recovery_known_message_ids` |  |
| KE-T1 | `src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:38` | 1 | 1 | `gc_stale_thread_sessions_with_probe_pg` |  |
| KE-T1 | `src/services/dispatches/discord_delivery/mod.rs:9` | 2 | 1 | `send_review_result_to_primary_for_preflight_harness_with_transport` |  |
| KE-T1 | `src/services/engine_runtime/mod.rs:72` | 4 | 1 | `record_allocations_for_tests` |  |
| KE-T1 | `src/services/engine_runtime/mod.rs:100` | 1 | 1 | `AllocationRecorder` |  |
| KE-T1 | `src/services/engine_runtime/mod.rs:241` | 4 | 3 | `is_degenerate_legacy` |  |
| KE-T1 | `src/services/engine_runtime/mod.rs:738` | 8 | 1 | `payload_lock_entries_for_test` |  |
| KE-T1 | `src/services/engine_runtime/mod.rs:918` | 7 | 1 | `key_for_tests`, `release_owned_state`, `token_for_tests` |  |
| KE-T1 | `src/services/engine_runtime/mod.rs:953` | 4 | 1 | `commit_exact`, `reclaim_exact_if_expired`, `release_exact_for_tests`, `renew_exact` |  |
| KE-T1 | `src/services/engine_runtime/relay_state.rs:105` | 10 | 2 | `paired_reservation_matches_for_test` |  |
| KE-T1 | `src/services/engine_runtime/relay_state.rs:579` | 5 | 2 | `has_restored_owner_for_tests`, `seed_binding_for_tests` |  |
| KE-T1 | `src/services/engine_runtime/relay_state.rs:806` | 1 | 1 | `FrontierPublication` |  |
| KE-T1 | `src/services/engine_runtime/relay_state.rs:964` | 1 | 1 | `certificate_for_tests` |  |
| KE-T1 | `src/services/hang_forensics.rs:525` | 3 | 2 | `counter_test_lock` |  |
| KE-T1 | `src/services/health_diagnostics.rs:42` | 2 | 1 | `backlog_probe` |  |
| KE-T1 | `src/services/health_diagnostics.rs:162` | 8 | 7 | `pg_wire_proxy` |  |
| KE-T8 | `src/services/imsg_reply/agy_input.rs:30` | 3 | 2 | `unready` | #8339 agy 계열 확인 전 보류 [U] |
| KE-T8 | `src/services/imsg_reply/agy_input.rs:47` | 2 | 2 | `SELECTED_WRITER` | #8339 agy 계열 확인 전 보류 [U] |
| KE-T1 | `src/services/imsg_reply/mod.rs:118` | 2 | 1 | `parse_enabled` |  |
| KE-T8 | `src/services/mod.rs:113` | 2 | 1 | `recovery_defer` | services/mod.rs는 KRX-T5·T5b host 줄과 같은 파일. 두 조각 착지 뒤; 섞인 시험 모듈(#[test] 9) |
| KE-T8 | `src/services/mod.rs:186` | 4 | 2 | `claude_herdr_first_input` | KRX-T8 KE host `krx_claude_tests`(시험 50·16파일)를 DL 시험이 fixture로 씀. 섞인 host라 gate 전환 대신 KRX-T8b 처방과 함께 판정 [U]; 섞인 시험 모듈(#[test] 50); gate 안 위쪽 참조 {"TS": 5} |
| KE-T2 | `src/services/queue.rs:24` | 1 | 1 | `force_purge_channel_mailbox` | 수리 1 P1-1: 큐 hook 묶음(`queue/cancel_effects.rs`, `replace_queue` 의존)째 T2 |
| KE-T2 | `src/services/queue.rs:27` | 1 | 1 | `set_pre_purge_hook_for_test` | 수리 1 P1-1: 큐 hook 묶음째 T2 |
| KE-T2 | `src/services/queue.rs:29` | 6 | 4 | `set_pre_purge_probe_for_test` | 수리 1 P1-1: 큐 hook 묶음째 T2 |
| KE-T1 | `src/services/routines/session_control.rs:3` | 3 | 2 | `seed_started_control_session` |  |
| KE-T1 | `src/services/routines/session_control.rs:5` | 2 | 1 | `CONTROL_TEST_PAUSE`, `ControlPause` |  |
| KE-T1 | `src/services/routines/store.rs:11` | 1 | 1 | `incident_content` |  |
| KE-T1 | `src/services/routines/store.rs:2102` | 2 | 2 | `attach_routine_with_execution` |  |
| KE-T2 | `src/services/scheduled_messages/turn_completion.rs:156` | 28 | 12 | `tests` | 섞인 시험 모듈(#[test] 3) |
| KE-T2 | `src/services/session_resume/catalog.rs:34` | 3 | 2 | `scope_claude_home_for_tests` |  |
| KE-T2 | `src/services/session_resume/catalog.rs:60` | 1 | 1 | `delay_previews_for_tests` |  |
| KE-T2 | `src/services/turn_cancel_queue_guard.rs:238` | 1 | 1 | `set_pre_record_probe_for_test` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:63` | 1 | 1 | `RECENT_CLAIMS_CAP` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:97` | 10 | 9 | `save_channel_queue` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:121` | 3 | 2 | `enqueue_intervention` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:123` | 1 | 1 | `BindingIntent` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:141` | 2 | 2 | `TakeHeadCondition::Unchecked` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:307` | 29 | 20 | `load_channel_pending_queue_for_tests` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:873` | 73 | 31 | `take_next_soft` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:942` | 7 | 5 | `hard_stop` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:1000` | 105 | 53 | `replace_queue` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:1037` | 1 | 1 | `merge_restored_dispatch_marker` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:1061` | 12 | 11 | `age_active_turn_for_test` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:1071` | 3 | 3 | `age_inbound_waits_for_test` |  |
| KE-T2 | `src/services/turn_orchestrator.rs:2659` | 100 | 36 | `test_support` |  |
| KE-T2 | `src/services/turn_orchestrator/checked_purge.rs:122` | 1 | 1 | `fail_next_publication_for_test` |  |
| KE-T2 | `src/services/turn_orchestrator/clear_channel.rs:381` | 4 | 3 | `fail_clear_removals` |  |
| KE-T2 | `src/services/turn_orchestrator/dispatch_reservation.rs:201` | 1 | 1 | `fail_next_marker_settle` |  |
| KE-T2 | `src/services/turn_orchestrator/intervention.rs:54` | 2 | 2 | `source_clock_sample_hook_for_tests` |  |
| KE-T2 | `src/services/turn_orchestrator/mailbox_observation.rs:33` | 3 | 1 | `enumerations_for_tests` |  |
| KE-T2 | `src/services/turn_orchestrator/pending_queue_persistence.rs:436` | 1 | 1 | `save_channel_queue` |  |
| KE-T8 | `src/services/turn_orchestrator/policy_close.rs:486` | 25 | 14 | `test_hooks` | #8343 입력 경로 소유(policy_close.rs). 착지 뒤 |
| KE-T2 | `src/services/turn_orchestrator/queued_head.rs:775` | 6 | 4 | `claim_queued_head` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2.rs:260` | 28 | 17 | `enqueue_fixture` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2.rs:336` | 20 | 15 | `reserve_unit` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2/actor.rs:17` | 132 | 68 | `v2_adopt` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2/control.rs:278` | 5 | 2 | `cancel_fault_for_tests`, `cancel_sequence_for_tests` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2/retained.rs:7` | 1 | 1 | `successor_not_sent` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:781` | 35 | 17 | `v2_admit_ticket` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:844` | 3 | 2 | `v2_arm_head_for_tests` |  |
| KE-T2 | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:850` | 2 | 2 | `v2_reserve_head_for_tests` |  |
| KE-T2 | `src/services/turn_orchestrator/registry_purge.rs:343` | 5 | 4 | `try_hydrate_pending_queue_from_disk` |  |
| KE-T1 | `src/supervisor/orphan_recovery/mod.rs:9` | 1 | 1 | `recover_once` |  |

## (A) gate 전수표(수리 1, `gate_graph.py`) — KE-T1·T2·T8의 정본

첫 설계의 `(A) 같은 파일 전이 닫힘` 표를 대신한다. 필드·변형·필드 초기화·hook을 실행하는 문장·match 팔·재노출 대상 파일·`cfg(not(test))` 짝까지 이름 참조 그래프로 묶었다(규칙은 `gate_graph.py` 머리말). 줄은 base `f352ad6f96`의 gate 속성 줄이고, `안쪽`은 같은 범위 안에 있어 함께 바뀌는 gate 줄이다. `cfg(all(test, unix))`는 `cfg(all(any(test, feature = "test-support"), unix))`로 바꾼다.

### KE-T1: gate 91곳 / 파일 22개 / 닫힘 단위 29개, `cfg(not(test))` 짝 2곳, 쪼갤 match 팔 0곳

| 파일 | gate 줄(종류) | 안쪽 gate |
|---|---|---|
| `src/dispatch/mod.rs` | `:10`(mod) |  |
| `src/engine/mod.rs` | `:1049`(fn) |  |
| `src/kanban/transition_core.rs` | `:24`(문장·필드·팔), `:29`(fn), `:39`(fn) |  |
| `src/reconcile.rs` | `:238`(fn), `:254`(fn) |  |
| `src/services/campaign_execution/control.rs` | `:971`(mod) |  |
| `src/services/discord/busy_followup_retry_store.rs` | `:57`(문장·필드·팔), `:66`(fn), `:73`(fn), `:79`(fn), `:84`(fn), `:95`(문장·필드·팔), `:228`(문장·필드·팔), `:387`(fn), `:392`(fn), `:457`(fn) |  |
| `src/services/discord/inject_disposition.rs` | `:469`(mod) |  |
| `src/services/discord/inject_disposition/dispatched.rs` | `:65`(문장·필드·팔), `:118`(static), `:120`(struct), `:122`(impl), `:131`(fn) |  |
| `src/services/discord/recovery_known_ids.rs` | `:176`(fn) |  |
| `src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs` | `:38`(fn) |  |
| `src/services/dispatches/discord_delivery/mod.rs` | `:9`(use) |  |
| `src/services/dispatches/discord_delivery/orchestration.rs` | `:1204`(fn) |  |
| `src/services/engine_runtime/mod.rs` | `:63`(문장·필드·팔), `:72`(fn), `:92`(type), `:95`(문장·필드·팔), `:100`(struct), `:105`(impl), `:112`(impl), `:241`(fn), `:283`(문장·필드·팔), `:293`(문장·필드·팔), `:382`(문장·필드·팔), `:569`(문장·필드·팔), `:595`(문장·필드·팔), `:609`(문장·필드·팔), `:639`(문장·필드·팔), `:648`(문장·필드·팔), `:653`(문장·필드·팔), `:686`(문장·필드·팔), `:708`(문장·필드·팔), `:738`(fn), `:745`(static), `:749`(use), `:752`(struct), `:756`(impl), `:769`(use), `:771`(use), `:775`(struct), `:778`(impl), `:789`(struct), `:796`(trait), `:800`(impl), `:804`(impl), `:808`(impl), `:813`(struct), `:823`(impl), `:906`(impl), `:918`(impl), `:944`(impl), `:953`(impl), `:1127`(fn), `:791`(struct, 손 추가), `:793`(struct, 손 추가) |  |
| `src/services/engine_runtime/relay_state.rs` | `:105`(fn), `:579`(impl), `:806`(enum, unix), `:964`(fn, unix) |  |
| `src/services/hang_forensics.rs` | `:525`(fn) |  |
| `src/services/health_diagnostics.rs` | `:42`(mod), `:80`(문장·필드·팔), `:162`(use) |  |
| `src/services/imsg_reply/mod.rs` | `:118`(fn) |  |
| `src/services/routines/session_control.rs` | `:3`(use), `:5`(use, unix) |  |
| `src/services/routines/session_control/persistent_target.rs` | `:7`(enum), `:14`(문장·필드·팔), `:19`(fn), `:72`(문장·필드·팔), `:155`(문장·필드·팔), `:373`(fn) |  |
| `src/services/routines/store.rs` | `:11`(use), `:2102`(fn) |  |
| `src/supervisor/orphan_recovery/mod.rs` | `:9`(use) |  |
| `src/utils/mod.rs` | `:10`(fn(KRX), 손 추가) |  |

`cfg(not(test))` 짝 → `cfg(not(any(test, feature = "test-support")))`:
- `src/kanban/transition_core.rs:46` `suppress_review_enter_outbox_for_preflight_harness`
- `src/services/discord/busy_followup_retry_store.rs:89` `injected_release_fault`

손 추가 근거:
- `src/services/engine_runtime/mod.rs:791` — 표지 타입 `Committed`(다른 파일 prod 이름과 같아 그래프가 못 이음). 파일 안 시험 gate 42곳 전부 전환
- `src/services/engine_runtime/mod.rs:793` — 표지 타입 `Released`(같은 이유)
- `src/utils/mod.rs:10` — KRX `parse_env_flag_for_test`. `imsg_reply/mod.rs:118` `parse_enabled`가 부른다. KRX-T1·T2 전환 목록 밖이라 KE-T1이 KRX 줄 1개를 함께 바꾼다

### KE-T2: gate 100곳 / 파일 20개 / 닫힘 단위 28개, `cfg(not(test))` 짝 0곳, 쪼갤 match 팔 6곳

| 파일 | gate 줄(종류) | 안쪽 gate |
|---|---|---|
| `src/services/queue.rs` | `:24`(use), `:27`(use), `:29`(use, unix) |  |
| `src/services/queue/cancel_effects.rs` | `:17`(use), `:19`(use), `:25`(type), `:28`(static), `:31`(fn), `:43`(type), `:46`(static), `:51`(fn, unix), `:59`(fn), `:198`(문장·필드·팔), `:412`(문장·필드·팔) |  |
| `src/services/scheduled_messages/turn_completion.rs` | `:156`(mod) |  |
| `src/services/session_resume/catalog.rs` | `:31`(문장·필드·팔), `:34`(fn), `:42`(static), `:45`(struct), `:48`(impl), `:60`(fn), `:71`(문장·필드·팔), `:656`(문장·필드·팔) |  |
| `src/services/turn_cancel_queue_guard.rs` | `:208`(문장·필드·팔), `:229`(type, unix), `:232`(static, unix), `:238`(fn, unix), `:246`(fn, unix) |  |
| `src/services/turn_orchestrator.rs` | `:63`(use), `:95`(use), `:97`(use), `:121`(use), `:123`(use, unix), `:141`(문장·필드·팔), `:307`(fn), `:873`(fn), `:881`(fn), `:942`(fn), `:1000`(fn), `:1037`(fn), `:1061`(fn), `:1071`(fn), `:1422`(문장·필드·팔), `:1464`(문장·필드·팔), `:1496`(문장·필드·팔), `:1501`(문장·필드·팔), `:1901`(문장·필드·팔), `:2217`(문장·필드·팔), `:2467`(문장·필드·팔), `:2505`(문장·필드·팔), `:2615`(문장·필드·팔), `:2625`(문장·필드·팔), `:2635`(문장·필드·팔), `:2659`(mod) |  |
| `src/services/turn_orchestrator/checked_purge.rs` | `:122`(fn, unix) |  |
| `src/services/turn_orchestrator/clear_channel.rs` | `:303`(문장·필드·팔), `:313`(문장·필드·팔), `:326`(문장·필드·팔), `:362`(enum), `:369`(static), `:372`(fn), `:381`(impl) |  |
| `src/services/turn_orchestrator/dispatch_reservation.rs` | `:165`(문장·필드·팔), `:189`(static), `:192`(fn), `:201`(impl) |  |
| `src/services/turn_orchestrator/intervention.rs` | `:48`(문장·필드·팔), `:54`(fn), `:100`(문장·필드·팔) |  |
| `src/services/turn_orchestrator/mailbox_observation.rs` | `:20`(문장·필드·팔, unix), `:27`(문장·필드·팔, unix), `:33`(fn, unix) |  |
| `src/services/turn_orchestrator/pending_queue_persistence.rs` | `:421`(fn), `:436`(fn), `:477`(mod), `:824`(fn), `:867`(mod) |  |
| `src/services/turn_orchestrator/pending_queue_persistence/snapshot_io.rs` | `:21`(문장·필드·팔), `:67`(문장·필드·팔) |  |
| `src/services/turn_orchestrator/queued_head.rs` | `:775`(fn) |  |
| `src/services/turn_orchestrator/queued_head/v2.rs` | `:30`(use), `:260`(fn), `:336`(fn) |  |
| `src/services/turn_orchestrator/queued_head/v2/actor.rs` | `:17`(fn) |  |
| `src/services/turn_orchestrator/queued_head/v2/control.rs` | `:61`(문장·필드·팔), `:64`(문장·필드·팔), `:129`(문장·필드·팔), `:278`(impl, unix) |  |
| `src/services/turn_orchestrator/queued_head/v2/retained.rs` | `:7`(fn) |  |
| `src/services/turn_orchestrator/queued_head/v2/source_admission.rs` | `:3`(use), `:763`(fn), `:781`(fn), `:844`(fn), `:850`(fn) |  |
| `src/services/turn_orchestrator/registry_purge.rs` | `:48`(use), `:123`(문장·필드·팔), `:167`(문장·필드·팔), `:176`(문장·필드·팔), `:186`(문장·필드·팔), `:343`(fn) |  |

쪼갤 match 팔(닫힘 변형과 닫힘 밖 시험 전용 변형이 한 팔에 묶임 → 닫힘 변형만 새 팔로 떼어 `any(test, feature)`, 나머지는 `cfg(test)` 그대로):
- `src/services/turn_orchestrator.rs:1901` — 남길 변형: `CancelActiveTurnIfUserMessageWithReason` (`src/services/turn_orchestrator.rs:1299`)
- `src/services/turn_orchestrator.rs:2635` — 남길 변형: `AgeValveClearedDispatchForTest` (`src/services/turn_orchestrator.rs:1506`)
- `src/services/turn_orchestrator/queued_head/v2/control.rs:61` — 남길 변형: `CancelActiveTurnIfUserMessageWithReason` (`src/services/turn_orchestrator.rs:1299`)
- `src/services/turn_orchestrator/queued_head/v2/control.rs:64` — 남길 변형: `AgeValveClearedDispatchForTest` (`src/services/turn_orchestrator.rs:1506`)
- `src/services/turn_orchestrator/registry_purge.rs:123` — 남길 변형: `AgeValveClearedDispatchForTest` (`src/services/turn_orchestrator.rs:1506`)
- `src/services/turn_orchestrator/registry_purge.rs:167` — 남길 변형: `CancelActiveTurnIfUserMessageWithReason` (`src/services/turn_orchestrator.rs:1299`)

### KE-T8: gate 10곳 / 파일 3개 / 닫힘 단위 5개, `cfg(not(test))` 짝 0곳, 쪼갤 match 팔 0곳

| 파일 | gate 줄(종류) | 안쪽 gate |
|---|---|---|
| `src/services/imsg_reply/agy_input.rs` | `:30`(impl), `:47`(문장·필드·팔), `:56`(문장·필드·팔) |  |
| `src/services/mod.rs` | `:113`(mod), `:186`(mod) |  |
| `src/services/turn_orchestrator/policy_close.rs` | `:20`(문장·필드·팔), `:175`(문장·필드·팔), `:184`(문장·필드·팔), `:277`(문장·필드·팔), `:486`(mod) |  |



## (B) 파일별 처리

| 조각 | 파일 | 종류 | 방식 | 옮길 crate | 사이트 | 시험 수(파일 전체) | 선언 부모 | 버킷 |
|---|---|---|---|---|---:|---|---|---|
| KE-T3 | `src/dispatch/test_support.rs` | dedicated | PATH | KE | 3 | 1 | `src/dispatch/mod.rs` | {'TS': 3} |
| KE-T3 | `src/engine/loader.rs` | inline | PATH | KE | 6 | 17 | `src/engine/loader.rs` | {'TS': 6} |
| KE-T3 | `src/engine/ops/http_ops.rs` | inline | PATH | KE | 4 | 21 | `src/engine/ops/http_ops.rs` | {'TS': 4} |
| KE-T3 | `src/engine/ops/kanban_ops.rs` | inline | PATH | KE | 2 | 16 | `src/engine/ops/kanban_ops.rs` | {'TS': 2} |
| KE-T3 | `src/engine/ops/read_bridge/contract_tests.rs` | dedicated | PATH | KE | 1 | 4 | `src/engine/ops/read_bridge.rs` | {'TS': 1} |
| KE-T3 | `src/engine/ops/read_bridge/epoch_tests.rs` | dedicated | PATH | KE | 1 | 1 | `src/engine/ops/read_bridge.rs` | {'TS': 1} |
| KE-T3 | `src/github/sync/campaign_nodes_tests.rs` | dedicated | PATH | KE | 1 | 10 | `src/github/sync/campaign_nodes.rs` | {'TS': 1} |
| KE-T3 | `src/services/auto_queue/activate_command.rs` | inline | PATH | KE | 1 | 19 | `src/services/auto_queue/activate_command.rs` | {'TS': 1} |
| KE-T3 | `src/services/auto_queue/advisory_return_pg_tests.rs` | dedicated | PATH | KE | 2 | 6 | `src/services/auto_queue/route.rs` | {'TS': 2} |
| KE-T3 | `src/services/auto_queue/campaign_handoff_tests.rs` | dedicated | PATH | KE | 1 | 10 | `src/services/auto_queue/campaign_handoff.rs` | {'TS': 1} |
| KE-T3 | `src/services/auto_queue/command.rs` | inline | PATH | KE | 1 | 8 | `src/services/auto_queue/command.rs` | {'TS': 1} |
| KE-T3 | `src/services/auto_queue/fsm.rs` | inline | PATH | KE | 1 | 3 | `src/services/auto_queue/fsm.rs` | {'TS': 1} |
| KE-T3 | `src/services/auto_queue/phase_snapshot_tests.rs` | dedicated | PATH | KE | 4 | 2 | `src/services/auto_queue/phase_gate.rs` | {'TS': 4} |
| KE-T3 | `src/services/auto_queue/transactions/completion_tests.rs` | dedicated | PATH | KE | 2 | 8 | `src/services/auto_queue/transactions/completion.rs` | {'TS': 2} |
| KE-T3 | `src/services/claude/herdr_turn/first_input/tests.rs` | dedicated | PATH | KE | 1 | 5 | `src/services/krx_claude_tests.rs` | {'TS': 1} |
| KE-T3 | `src/services/codex/tui_session_launch/first_input/native_tests.rs` | dedicated | PATH | KE | 4 | 2 | `src/services/codex/tui_session_launch/first_input/tests.rs` | {'TS': 4} |
| KE-T3 | `src/services/decision_gate/runtime/tests.rs` | dedicated | PATH | KE | 2 | 9 | `src/services/decision_gate/runtime.rs` | {'TS': 2} |
| KE-T3 | `src/services/decision_gate/tests.rs` | dedicated | PATH | KE | 2 | 6 | `src/services/decision_gate/mod.rs` | {'TS': 2} |
| KE-T3 | `src/services/dispatched_sessions/resume_fence_api_tests.rs` | dedicated | PATH | KE | 2 | 1 | `src/services/dispatched_sessions.rs` | {'ALIAS': 2} |
| KE-T3 | `src/services/health_diagnostics.rs` | inline | PATH | KE | 3 | 6 | `src/services/health_diagnostics.rs` | {'TS': 3} |
| KE-T3 | `src/services/maintenance/jobs/worktree_orphan_sweep.rs` | inline | PATH | KE | 2 | 45 | `src/services/maintenance/jobs/worktree_orphan_sweep.rs` | {'PP': 2} |
| KE-T3 | `src/services/operator_connectors.rs` | inline | PATH | KE | 5 | 12 | `src/services/operator_connectors.rs` | {'PP': 5} |
| KE-T3 | `src/services/routines/loader/tests/mod.rs` | dedicated | PATH | KE | 2 | 46 | `src/services/routines/loader.rs` | {'TS': 2} |
| KE-T3 | `src/services/routines/migrated.rs` | inline | PATH | KE | 4 | 34 | `src/services/routines/migrated.rs` | {'PP': 4} |
| KE-T3 | `src/services/stale_turn_reconciler/owner_boundary_tests.rs` | dedicated | PATH | KE | 1 | 4 | `src/services/stale_turn_reconciler.rs` | {'ALIAS': 1} |
| KE-T3 | `src/services/tmux_wrapper.rs` | inline | PATH | KE | 1 | 6 | `src/services/tmux_wrapper.rs` | {'ALIAS': 1} |
| KE-T3 | `src/services/turn_orchestrator.rs` | inline | PATH | KE | 2 | 98 | `src/services/turn_orchestrator.rs` | {'PP': 2} |
| KE-T3 | `src/services/auto_queue/runtime.rs` | inline | SEAM | ROOT | 2 | 0 | `src/services/auto_queue/runtime.rs` | {'FIX': 1, 'PROD': 1} |
| KE-T3 | `src/services/dispatched_sessions/herdr_kill.rs` | inline | SEAM | DL | 1 | 0 | `src/services/dispatched_sessions/herdr_kill.rs` | {'FIX': 1} |
| KE-T3 | `src/services/dispatched_sessions/herdr_kill/effects.rs` | inline | SEAM | DL | 1 | 0 | `src/services/dispatched_sessions/herdr_kill/effects.rs` | {'FIX': 1} |
| KE-T3 | `src/services/queue/http_force.rs` | inline | SEAM | DL | 1 | 0 | `src/services/queue/http_force.rs` | {'FIX': 1} |
| KE-T3 | `src/services/dispatches/outbox_route.rs` | inline | SINK | KRX | 1 | 6 | `src/services/dispatches/outbox_route.rs` | {'FIX': 1} |
| KE-T3 | `src/services/onboarding/mod.rs` | inline | SINK | KRX | 1 | 7 | `src/services/onboarding/mod.rs` | {'FIX': 1} |
| KE-T4 | `src/services/agent_channels.rs` | inline | RELOC | DL | 1 | 2(옮길 함수 1~2, 함수 밖 0) | `src/services/agent_channels.rs` | {'FIX': 1} |
| KE-T4 | `src/services/auto_queue/cleanup_tasks_pg_tests.rs` | dedicated | RELOC | ROOT | 3 | 23 | `src/services/auto_queue/cleanup_tasks.rs` | {'PROD': 2, 'FIX': 1} |
| KE-T4 | `src/services/dispatched_sessions/listing_tests.rs` | dedicated | RELOC | ROOT | 5 | 3 | `src/services/dispatched_sessions.rs` | {'FIX': 1, 'ALIAS': 2, 'AMB': 1, 'PROD': 1} |
| KE-T4 | `src/services/dispatched_sessions/raw_alias_pg_tests.rs` | dedicated | RELOC | ROOT | 2 | 4 | `src/services/dispatched_sessions.rs` | {'FIX': 1, 'PROD': 1} |
| KE-T4 | `src/services/dispatches/discord_delivery/transport.rs` | inline | RELOC | DL | 2 | 12(옮길 함수 1~12, 함수 밖 0) | `src/services/dispatches/discord_delivery/transport.rs` | {'ALIAS': 1, 'PROD': 1} |
| KE-T4 | `src/services/hang_forensics/tests/mod.rs` | dedicated | RELOC | ROOT | 1 | 31 | `src/services/hang_forensics.rs` | {'PROD': 1} |
| KE-T4 | `src/services/maintenance/jobs/db_retention/record_caps.rs` | inline | RELOC | ROOT | 1 | 4(옮길 함수 1~4, 함수 밖 0) | `src/services/maintenance/jobs/db_retention/record_caps.rs` | {'PROD': 1} |
| KE-T4 | `src/services/queue.rs` | inline | RELOC | ROOT | 1 | 2(옮길 함수 0~2, 함수 밖 1) | `src/services/queue.rs` | {'PROD': 1} |
| KE-T4 | `src/services/queue/cancel_effects_v2_tests.rs` | dedicated | RELOC | ROOT | 2 | 13 | `src/services/queue/cancel_effects.rs` | {'PROD': 1, 'FIX': 1} |
| KE-T4 | `src/services/session_activity.rs` | inline | RELOC | DL | 1 | 5(옮길 함수 1~5, 함수 밖 0) | `src/services/session_activity.rs` | {'FIX': 1} |
| KE-T5 | `src/services/discord/busy_followup_retry_store.rs` | inline | RELOC | DL | 1 | 5(옮길 함수 0~5, 함수 밖 1) | `src/services/discord/busy_followup_retry_store.rs` | {'PROD': 1} |
| KE-T5 | `src/services/discord/busy_followup_retry_store/policy_tests.rs` | dedicated | RELOC | DL | 4 | 13 | `src/services/discord/busy_followup_retry_store/policy.rs` | {'PROD': 3, 'FIX': 1} |
| KE-T5 | `src/services/session_resume.rs` | inline | RELOC | ROOT | 19 | 17(옮길 함수 4~17, 함수 밖 2) | `src/services/session_resume.rs` | {'FIX': 15, 'PROD': 3, 'AMB': 1} |
| KE-T5 | `src/services/stale_turn_reconciler.rs` | inline | RELOC | ROOT | 7 | 20(옮길 함수 3~20, 함수 밖 0) | `src/services/stale_turn_reconciler.rs` | {'PROD': 5, 'FIX': 1, 'ALIAS': 1} |
| KE-T5 | `src/services/turn_lifecycle.rs` | inline | RELOC | ROOT | 19 | 9(옮길 함수 5~9, 함수 밖 3) | `src/services/turn_lifecycle.rs` | {'AMB': 2, 'ALIAS': 1, 'PROD': 5, 'FIX': 11} |
| KE-T5 | `src/services/turn_orchestrator/queued_head/mend_tests.rs` | dedicated | RELOC | ROOT | 2 | 1 | `src/services/turn_orchestrator/queued_head/mend.rs` | {'AMB': 2} |
| KE-T5 | `src/services/turn_orchestrator/queued_head/v2/origin_tests.rs` | dedicated | RELOC | ROOT | 2 | 10 | `src/services/turn_orchestrator/queued_head/v2/origin.rs` | {'FIX': 1, 'ALIAS': 1} |
| KE-T5 | `src/services/turn_orchestrator/queued_head_s2a_tests.rs` | dedicated | RELOC | ROOT | 2 | 17 | `src/services/turn_orchestrator/queued_head.rs` | {'PROD': 2} |
| KE-T5 | `src/services/turn_orchestrator/queued_head_tests.rs` | dedicated | RELOC | ROOT | 1 | 20 | `src/services/turn_orchestrator/queued_head.rs` | {'PROD': 1} |
| KE-T6 | `src/services/routines/agent_executor/execution.rs` | inline | RELOC | ROOT | 1 | 7(옮길 함수 0~7, 함수 밖 1) | `src/services/routines/agent_executor/execution.rs` | {'PROD': 1} |
| KE-T6 | `src/services/routines/agent_executor/owner_input_tests.rs` | dedicated | RELOC | ROOT | 1 | 5 | `src/services/routines/agent_executor.rs` | {'AMB': 1} |
| KE-T6 | `src/services/routines/agent_executor/owner_launch_tests.rs` | dedicated | RELOC | ROOT | 2 | 1 | `src/services/routines/agent_executor/owner_input_tests.rs` | {'PROD': 1, 'FIX': 1} |
| KE-T6 | `src/services/routines/agent_executor/provider_choice_tests.rs` | dedicated | RELOC | ROOT | 2 | 5 | `src/services/routines/agent_executor.rs` | {'PROD': 1, 'FIX': 1} |
| KE-T6 | `src/services/routines/agent_executor/reliability.rs` | inline | RELOC | ROOT | 3 | 9(옮길 함수 1~9, 함수 밖 0) | `src/services/routines/agent_executor/reliability.rs` | {'PROD': 1, 'FIX': 2} |
| KE-T6 | `src/services/routines/execution_mode_pg_tests.rs` | dedicated | RELOC | ROOT | 1 | 4 | `src/services/routines/mod.rs` | {'PROD': 1} |
| KE-T6 | `src/services/routines/reliability_pg_tests.rs` | dedicated | RELOC | ROOT | 2 | 10 | `src/services/routines/mod.rs` | {'PROD': 1, 'FIX': 1} |
| KE-T6 | `src/services/routines/reliability_pg_tests/run_diagnostics_tests.rs` | dedicated | RELOC | ROOT | 1 | 5 | `src/services/routines/reliability_pg_tests.rs` | {'PROD': 1} |
| KE-T6 | `src/services/scheduled_messages/owner_input_tests.rs` | dedicated | RELOC | ROOT | 1 | 2 | `src/services/scheduled_messages/postgres_tests.rs` | {'PROD': 1} |
| KE-T6 | `src/services/scheduled_messages/postgres_tests.rs` | dedicated | RELOC | ROOT | 1 | 21 | `src/services/scheduled_messages.rs` | {'PROD': 1} |
| KE-T7 | `src/engine/ops/exec_ops.rs` | inline | RELOC | DL | 9 | 8(옮길 함수 1~8, 함수 밖 0) | `src/engine/ops/exec_ops.rs` | {'PROD': 1, 'AMB': 4, 'TS': 4} |
| KE-T7 | `src/engine/ops/exec_ops/circuit_evidence_tests.rs` | dedicated | RELOC | DL | 5 | 3 | `src/engine/ops/exec_ops.rs` | {'AMB': 5} |
| KE-T7 | `src/engine/ops/timeouts_ops/host_repair_tests.rs` | dedicated | RELOC | DL | 1 | 10 | `src/engine/ops/timeouts_ops/host_repair.rs` | {'FIX': 1} |
| KE-T7 | `src/engine/ops/unsent_notify_tests.rs` | dedicated | RELOC | ROOT | 4 | 5 | `src/engine/ops.rs` | {'PROD': 3, 'TS': 1} |
| KE-T7 | `src/github/triage/metadata_pg_tests.rs` | dedicated | RELOC | ROOT | 1 | 5 | `src/github/triage.rs` | {'AMB': 1} |
| KE-T7 | `src/reconcile.rs` | inline | RELOC | ROOT | 1 | 12(옮길 함수 1~12, 함수 밖 0) | `src/reconcile.rs` | {'PROD': 1} |
| KE-T8 | `src/high_risk_recovery.rs` | dedicated | ROOTED | ROOT | 6 | 7 | `src/lib.rs` | {'FIX': 2, 'PROD': 4} |
| KE-T8 | `src/services/auto_queue/dispatched_entry_recovery_pg_tests.rs` | dedicated | ROOTED | ROOT | 6 | 6 | `src/high_risk_recovery.rs` | {'PROD': 2, 'FIX': 4} |
| KE-T8 | `src/services/dispatches/http_outbound.rs` | inline | SEAM(port) | DL | 1 | 0 | `src/services/dispatches/http_outbound.rs` | {'PROD': 1} |
| KE-T8 | `src/services/dispatches/thread_archive_defer.rs` | inline | SEAM(port) | DL | 1 | 0 | `src/services/dispatches/thread_archive_defer.rs` | {'PROD': 1} |
| KE-C/M | `src/services/auto_queue/runtime/slot_reset_host_pg_tests.rs` | dedicated | RELOC | ROOT | 20 | 5 | `src/services/auto_queue/runtime.rs` | {'PROD': 5, 'FIX': 13, 'ALIAS': 1, 'AMB': 1} |
| KE-C/M | `src/services/auto_queue/runtime/verified_reset_pg_tests.rs` | dedicated | RELOC | ROOT | 5 | 1 | `src/services/auto_queue/runtime.rs` | {'PROD': 4, 'FIX': 1} |

## (B) 스캐너 밖에서 조각이 함께 져야 하는 행(공유 fixture 연쇄·seam)

| 조각 | 파일 | 종류 | 방식 | 대상 | 근거 |
|---|---|---|---|---|---|
| KE-C/M | `src/services/routines/session_control.rs` | inline | RELOC(연쇄) | ROOT | `:757` `verified_reset_pg_tests::Fixture` 사용. 수리 1 P1-3으로 verified_reset이 KE-C/M 묶음에 들어가 이 시험 함수도 같이 간다 |
| KE-C/M | `src/services/discord/turn_bridge/tmux_runtime/stop_host_tests.rs` | dedicated | PATH(위쪽 사용처) | DL | `:1112` `runtime_clears_after_done`(DL은 `Applied` 단언). helper가 `RUNTIME_CLEARS`도 읽고 ROOT `ManagedReset`을 돌려주므로 KE에 남길 수 없다(수리 1 P1-3). 묶음째 KE-C/M |
| KE-T6 | `src/services/routines/agent_executor.rs` | inline | SEAM(시험 모듈) | KE | `:868-872` KE prod cfg(test)가 `owner_input_tests::LEGACY_TEARDOWNS`를 센다 → task_local 선언만 KE 시험 hook 파일로 떼어 `any(test, feature)`, 실행 문장도 같은 gate |
| KE-T6 | `src/services/routines/agent_executor/launch.rs` | inline | SEAM(시험 모듈) | KE | `:11-14` `owner_input_tests::inject` 호출. `inject`(`owner_input_tests.rs:14-`)는 ROOT `discord::health::reserve_headless_agent_turn`(`:4,31`)을 부르므로 KE에 남길 수 없다(수리 1 P1-4). KE에는 주입 슬롯(`STARTS` 대체 task_local)과 좁은 결과 계약(예약 결과 `turn_id`·채널만 담는 값)만 두고, 예약은 ROOT fixture가 해서 그 값을 슬롯에 넣는다 |
| KE-T6 | `src/services/routines/agent_executor/owner_input.rs` | inline | SEAM(시험 모듈) | KE | `:150-153` `owner_input_tests::OWNER` task_local → 선언만 KE 시험 hook 파일로 떼어 `any(test, feature)` |
| KE-T6 | `src/services/scheduled_messages.rs` | inline | SEAM(시험 모듈) | KE | `:412-415` `postgres_tests::START_TARGET` task_local(`postgres_tests.rs:10`) → 선언만 KE에 남기는 분리 |
| KE-T6 | `src/services/scheduled_messages/owner_input.rs` | inline | (A) 새 행(옮긴 시험이 쓰는 KE hook) | KE | scheduled owner 시험(`owner_input_tests.rs:43,188`, ROOT로 감)이 쓰는 `OWNER`(선언 `:177`, 실행 `:165`)·`REJECTION_TEST`(선언 `:390`, 실행 `:120,122,133`)와 짝 `cfg(not(test))` `:127`을 같은 조각에서 `any(test, feature)`/`not(any(…))`로 바꾼다(수리 1 P1-4) |
| KE-T6 | `src/services/routines/agent_executor/owner_custody_tests.rs` | dedicated | RELOC(자식) | ROOT | `owner_input_tests.rs:307-308` `#[path] mod custody_tests` 2시험. 부모와 함께 옮긴다(수리 1 P2) |
| KE-T6 | `src/services/routines/agent_executor/owner_review_tests.rs` | dedicated | RELOC(자식) | ROOT | `owner_input_tests.rs:312-313` `#[path] mod review_tests` 13시험. 부모와 함께 옮긴다(수리 1 P2) |
| KE-T7 | `src/services/stale_turn_reconciler.rs` | inline | (A) 새 행 | KE | host_repair_tests(→DL)가 `stale_turn_reconciler::owner_boundary_tests`를 씀 → 옮기면 새 (A) 행. gate 전환은 KE-T5 파일이라 T5 뒤 |
| KE-C/M | `src/services/auto_queue/runtime.rs` | inline | SEAM(static 타입) | ROOT | `:600,609` `RUNTIME_CLEARS`·`_DONE`과 기록 경로 `:274-294`. KE prod `verdict.apply()`(`:281`)가 이미 ROOT `ManagedReset`을 받는 prod 간선이라 KE-T 범위 밖([U]4 확정). `Applied`/`Refused` 구분과 완료 신호 `RUNTIME_CLEARS_DONE`을 유지하는 계약으로 KE-C/M이 한 번에 옮긴다 |
| KE-T8 | `src/engine/ops/exec_ops.rs` | inline | SEAM(port) | DL | `:36` `install_engine_probes()` 즉석 설치(구현은 DL inflight) |

## 조각별 합계

| 조각 | (A) root 정의 / 사용 행 / 바꿀 gate·파일(전수표) | (B) 파일(전용+인라인) / 사이트 / 시험 | 방식 | 덧붙인 행 |
|---|---|---|---|---:|
| KE-T1 | 36 / 265 / 91곳·22파일 | 0(0+0) / 0 / 0 | {} | 0 |
| KE-T2 | 36 / 637 / 100곳·20파일 | 0(0+0) / 0 / 0 | {} | 0 |
| KE-T3 | 0 / 0 / — | 33(15+18) / 68 / 413 | {'PATH': 27, 'SEAM': 4, 'SINK': 2} | 0 |
| KE-T4 | 0 / 0 / — | 10(5+5) / 19 / 99 | {'RELOC': 10} | 0 |
| KE-T5 | 0 / 0 / — | 9(5+4) / 57 / 112 | {'RELOC': 9} | 0 |
| KE-T6 | 0 / 0 / — | 10(8+2) / 15 / 69 | {'RELOC': 10} | 7 |
| KE-T7 | 0 / 0 / — | 6(4+2) / 21 / 43 | {'RELOC': 6} | 1 |
| KE-T8 | 5 / 36 / 10곳·3파일 | 4(2+2) / 14 / 13 | {'ROOTED': 2, 'SEAM(port)': 2} | 1 |
| KE-C/M | 1 / 1 / — | 2(2+0) / 25 / 6 | {'RELOC': 2} | 3 |
