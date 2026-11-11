## 진입점 전수표 (git grep 정본)

코드 SHA: `92ef5b00b205679965c24c5c017f7237e8b5d57f`. 원시 검색/명령은 lane-run `logs/s18-1/entrypoints-git-grep.txt`, `entrypoints-command.json`, 재생성은 `build_inventory.py`.
아래는 위 명령으로 고정한 경계 심볼의 선언·직접 호출·import/re-export·시험·주석 전체다. 동명의 `claim_turn` 등 인접 경로도 빠뜨리지 않고 유지한다. 함수별 경계 해석은 설계서 §0을 따른다.
`source`는 production token이 있는 행이며 crate 전체의 reachability/권한 증명은 아니다. `test/비실행`은 시험 파일 또는 production token이 없는 행이다.
한 줄에 범주가 여럿이면 가장 안쪽 권한 행동을 표시했다. 외부 port가 없다는 표시는 새 port가 raw 권한을 내보내지 않는다는 계약이며 기존 `pub(crate)` API 전체가 봉인됐다는 주장이 아니다.

| file:line | 구분 | 선언·호출·참조 | 모델 값/행동 | 제안 port/잔류 |
|---|---|---|---|---|
| `src/db/turns.rs:253` | source | `pub async fn record_turn_receipt(pool: &PgPool, receipt: &TurnReceipt<'_>) -> Result<()> {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:72` | test/비실행 | `record_turn_receipt(&pool, &first).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:73` | test/비실행 | `record_turn_receipt(&pool, &second).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:80` | test/비실행 | `record_turn_receipt(&pool, &first).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:81` | test/비실행 | `record_turn_receipt(&pool, &second).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:100` | test/비실행 | `record_turn_receipt(&left_pool, &r).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:105` | test/비실행 | `record_turn_receipt(&right_pool, &r).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:160` | test/비실행 | `record_turn_receipt(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:166` | test/비실행 | `record_turn_receipt(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:207` | test/비실행 | `record_turn_receipt(&pool, &foreign).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:240` | test/비실행 | `record_turn_receipt(&pool, &receipt(turn, &ids, TerminalOutcome::Failed, aged))` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/db/turns/receipt_tests.rs:312` | test/비실행 | `record_turn_receipt(&pool, &entry).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/server/routes/agents.rs:32` | source | `use crate::services::turn_lifecycle::{TurnLifecycleTarget, stop_turn_preserving_queue};` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/routes/agents.rs:838` | source | `crate::services::discord::health::start_headless_agent_turn_in_dm(` | AdmissionHold/Start | A (headless admission) |
| `src/server/routes/agents_input_result_tests.rs:204` | test/비실행 | `crate::db::turns::record_turn_receipt(&pool, &done)` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/server/routes/agents_input_result_tests.rs:309` | test/비실행 | `let admitted = handle.v2_admit_ticket_with_context(original, "t-owed".into(), context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/server/routes/agents_stop.rs:68` | source | `let lifecycle = stop_turn_preserving_queue(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/routes/agents_turn_deliver.rs:17` | source | `HealthRegistry, HumanInputRequest, deliver_human_input, deliver_with_reply,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_deliver.rs:18` | source | `preflight_human_input,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_deliver.rs:72` | source | `Some(reply) => deliver_with_reply(registry, request, reply).await,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_deliver.rs:73` | source | `None => deliver_human_input(registry, request).await,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_deliver.rs:260` | source | `preflight_human_input(` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_deliver/legacy_origin.rs:7` | source | `preflight_human_input,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_deliver/legacy_origin.rs:21` | source | `preflight_human_input(registry, channel, &request.provider, input.author_id).await` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_deliver/legacy_origin.rs:37` | source | `let prepared = match registry.prepare_external_human_input(request).await {` | Authorize/Admit/Deliver | H (원격 human) |
| `src/server/routes/agents_turn_target.rs:157` | source | `use crate::services::discord::health::external_turn_hold_for_start;` | AdmissionHold/Start | A (headless admission) |
| `src/server/routes/agents_turn_target.rs:158` | source | `let hold = external_turn_hold_for_start(registry, provider, channel_id).await?;` | AdmissionHold/Start | A (headless admission) |
| `src/server/routes/agents_turn_target.rs:187` | source | `crate::services::discord::health::start_headless_agent_turn(` | AdmissionHold/Start | A (headless admission) |
| `src/server/routes/dispatched_sessions_tests.rs:982` | test/비실행 | `let kill = crate::services::turn_lifecycle::force_kill_turn;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/routes/dispatched_sessions_tests.rs:1039` | test/비실행 | `let kill = crate::services::turn_lifecycle::force_kill_turn;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/routes/health_api.rs:777` | source | `health::stop_runtime_turn_preserving_watcher(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/routes/health_api.rs:787` | source | `health::stop_providerless_runtime_turn_preserving_watcher_strict_ownership(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/routes/kanban.rs:25` | source | `ForceKillVerdict, force_kill_row_verdict, force_kill_turn_with_verdict,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/routes/kanban.rs:153` | source | `let kill = force_kill_turn_with_verdict(registry, verdict, reason, "kanban_backlog_revert");` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/worker_registry.rs:427` | test/비실행 | `/// The tick's auto-queue cleanup replay reaches 'clear_provider_channel_runtime'` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/server/worker_registry.rs:606` | test/비실행 | `/// 'clear_provider_channel_runtime' is skipped, leaving the in-memory` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/agent_recovery/mod.rs:141` | test/비실행 | `pub fn claim_turn(&mut self, channel_id: &str, turn_id: &str) {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:79` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-preserve");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:239` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-1");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:262` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-2");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:314` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-generation");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:326` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-generation-2");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:430` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-4");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:456` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-5");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:474` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-6");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:500` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-7");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:611` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-10");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:667` | test/비실행 | `rate_limited.claim_turn(CHANNEL, "turn-10b");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/agent_recovery/tests.rs:715` | test/비실행 | `runtime.claim_turn(CHANNEL, "turn-9");` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/auto_queue/cleanup_tasks_pg_tests.rs:928` | test/비실행 | `/// present. Passing 'None' skips 'clear_provider_channel_runtime' entirely.` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/auto_queue/runtime.rs:9` | source | `use crate::services::discord::admin_host_guard::{ManagedResetVerdict, managed_reset_verdict};` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/auto_queue/runtime.rs:502` | source | `managed_reset_verdict(registry, &runtime.provider_name, channel, key).await` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/claude_tui/hook_server/o_restart_preservation_tests.rs:147` | test/비실행 | `let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/claude_tui/hook_server/rehydration_ingress_tests.rs:873` | test/비실행 | `let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/cluster/intake_router_hook/owner_record/output_tests.rs:347` | test/비실행 | `ownership::OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/cluster/intake_router_hook/owner_record/output_tests.rs:415` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/cluster/intake_router_hook/owner_record/output_tests.rs:539` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/cluster/intake_worker/o_route_tests.rs:260` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/cluster/intake_worker/o_route_tests.rs:312` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/admin_host_guard.rs:194` | source | `pub(crate) async fn managed_reset_verdict(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/admin_host_guard_verified_reset_tests.rs:366` | test/비실행 | `let verdict = managed_reset_verdict(&registry, "codex", fixture.channel, None)` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/admin_host_guard_verified_reset_tests.rs:386` | test/비실행 | `let verdict = managed_reset_verdict(&registry, "codex", fixture.channel, None).await.unwrap();` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/admin_host_guard_verified_reset_tests.rs:412` | test/비실행 | `let result = super::super::health::clear_provider_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/agent_handoff.rs:564` | source | `let result = health::start_headless_agent_turn(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/catch_up/frontier_sweep_tests.rs:245` | test/비실행 | `let start = discord::queue_io::mailbox_try_start_turn_behind_queue;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/catch_up/owner_pending_recovery_tests.rs:313` | test/비실행 | `let start = crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/commands/control/clear_teardown/codex_end_tests/reader_retirement_tests.rs:7` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/commands/control/clear_teardown/codex_end_tests/reader_retirement_tests.rs:139` | test/비실행 | `Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/commands/control/clear_teardown/codex_end_tests/reader_retirement_tests.rs:265` | test/비실행 | `let mut writer = ChannelWriter::new(channel, Arc::new(OwnershipGate::default()), Arc::new(NoDiscord), NoLease, alarms.clone());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/commands/control/herdr_recovery.rs:189` | source | `crate::services::discord::receipt_settle::settle_v2_drops(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/commands/control/herdr_recovery_tests.rs:24` | test/비실행 | `let admitted = mailbox.v2_admit_ticket_with_context(original, "t-h".into(), context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/commands/inspect/tests.rs:252` | test/비실행 | `use crate::db::turns::{TerminalOutcome, TurnReceipt, record_turn_receipt};` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/commands/inspect/tests.rs:295` | test/비실행 | `record_turn_receipt(&pool, &receipt).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/commands/inspect/tests.rs:383` | test/비실행 | `let admitted = handle.v2_admit_ticket_with_context(original, format!("t-{id}"), context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/commands/inspect/tests.rs:392` | test/비실행 | `crate::services::discord::settle_v2_drops(Some(&pool), &handle, "claude", channel).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/commands/stop.rs:98` | source | `pub(crate) async fn run_herdr_cancel(` | AuthorizedStop | N (nonforced Herdr만) |
| `src/services/discord/commands/text_commands.rs:35` | source | `let _ = super::super::health::schedule_pending_queue_drain_after_cancel(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:119` | source | `reserve_headless_agent_turn, reserve_headless_agent_turn_in_dm, start_headless_agent_turn,` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health.rs:120` | source | `start_headless_agent_turn_in_dm, start_reserved_headless_agent_turn_in_dm,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health.rs:121` | source | `start_reserved_headless_agent_turn_with_owner_channel,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health.rs:137` | source | `pub use turn_deliver::{HumanInputRequest, deliver_human_input, external_turn_hold_for_start};` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health.rs:139` | source | `pub(crate) use turn_deliver::{deliver_with_reply, preflight_human_input};` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health.rs:155` | test/비실행 | `pub(crate) use recovery::stop_provider_channel_runtime_with_policy;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:158` | source | `judge_provider_channel_stop, stop_judged_provider_channel,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:164` | source | `clear_idle_tmux_stale_turn, clear_provider_channel_runtime,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:165` | source | `finish_cancelled_provider_channel_mailbox, force_kill_provider_channel_runtime,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:167` | source | `resolve_tmux_session_for_cancel, schedule_pending_queue_drain_after_cancel,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:169` | source | `stop_providerless_runtime_turn_preserving_watcher_strict_ownership,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:170` | source | `stop_runtime_turn_preserving_watcher,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health.rs:175` | source | `resume_runtime_for_channel, retain_resume_runtime_owner_before_teardown, stop_channel_runtime,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/headless_turn.rs:16` | source | `pub async fn start_headless_agent_turn(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/headless_turn.rs:25` | source | `let reservation = reserve_headless_agent_turn(channel_id);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health/headless_turn.rs:26` | source | `start_reserved_headless_agent_turn(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/headless_turn.rs:52` | source | `pub fn reserve_headless_agent_turn(channel_id: ChannelId) -> HeadlessAgentTurnReservation {` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health/headless_turn.rs:62` | source | `pub async fn start_reserved_headless_agent_turn(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/headless_turn.rs:100` | source | `pub async fn start_reserved_headless_agent_turn_with_owner_channel(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/headless_turn.rs:143` | source | `pub async fn start_headless_agent_turn_in_dm(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/headless_turn.rs:175` | source | `let reservation = reserve_headless_agent_turn(dm_channel_id);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health/headless_turn.rs:193` | source | `pub async fn reserve_headless_agent_turn_in_dm(` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health/headless_turn.rs:222` | source | `Ok((dm_channel_id, reserve_headless_agent_turn(dm_channel_id)))` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health/headless_turn.rs:246` | source | `pub async fn start_reserved_headless_agent_turn_in_dm(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/headless_turn.rs:393` | test/비실행 | `reserve_headless_agent_turn, serenity, start_reserved_headless_agent_turn_in_dm,` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health/headless_turn.rs:459` | test/비실행 | `start_reserved_headless_agent_turn_in_dm(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/headless_turn.rs:469` | test/비실행 | `reserve_headless_agent_turn(dm_channel),` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/health/recovery.rs:56` | source | `judge_provider_channel_stop, stop_channel_runtime, stop_judged_provider_channel,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:57` | source | `stop_provider_channel_runtime_with_policy,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:73` | source | `resolve_tmux_session_for_cancel, schedule_pending_queue_drain_after_cancel,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:95` | source | `HardStopRuntimeResult, finish_cancelled_provider_channel_mailbox,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:97` | source | `stop_providerless_runtime_turn_preserving_watcher_strict_ownership,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:98` | source | `stop_runtime_turn_preserving_watcher,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:100` | source | `pub use runtime_stop::force_kill_provider_channel_runtime;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:103` | source | `use runtime_stop::stop_judged_channel_runtime;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:242` | source | `pub async fn clear_provider_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4122` | test/비실행 | `let result = super::stop_runtime_turn_preserving_watcher(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4185` | test/비실행 | `let result = super::stop_runtime_turn_preserving_watcher(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4275` | test/비실행 | `let result = super::finish_cancelled_provider_channel_mailbox(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4372` | test/비실행 | `let result = super::stop_providerless_runtime_turn_preserving_watcher_strict_ownership(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4489` | test/비실행 | `let result = super::stop_providerless_runtime_turn_preserving_watcher_strict_ownership(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4563` | test/비실행 | `super::stop_runtime_turn_preserving_watcher(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4824` | test/비실행 | `schedule_pending_queue_drain_after_cancel(&registry, "claude", channel, "test_refusal")` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4868` | test/비실행 | `schedule_pending_queue_drain_after_cancel(&registry, "claude", channel, "test_empty")` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4873` | test/비실행 | `schedule_pending_queue_drain_after_cancel(&registry, "claude", channel, "test_lost")` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery.rs:4906` | test/비실행 | `let result = schedule_pending_queue_drain_after_cancel(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/carried_turn_close.rs:454` | source | `&& discord::receipt_settle::record(Some(pool), row, stamp.outcome, stamp.confirmed_at).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/health/recovery/carried_turn_close_tests.rs:978` | test/비실행 | `discord::receipt_settle::settle_marked_row(&fx.shared, &PROVIDER, fx.channel.get())` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/health/recovery/channel_runtime.rs:186` | source | `pub async fn schedule_pending_queue_drain_after_cancel(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/live_agent_recovery.rs:188` | source | `let stopped = health::force_kill_provider_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/live_agent_recovery.rs:255` | source | `health::start_headless_agent_turn(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/recovery/runtime_lookup.rs:160` | source | `pub async fn stop_runtime_turn_preserving_watcher(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_lookup.rs:178` | source | `pub async fn stop_providerless_runtime_turn_preserving_watcher_strict_ownership(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_lookup.rs:188` | source | `pub async fn finish_cancelled_provider_channel_mailbox(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_lookup_tests.rs:6` | test/비실행 | `HardStopRuntimeResult, stop_providerless_runtime_turn_preserving_watcher_strict_ownership,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_lookup_tests.rs:7` | test/비실행 | `stop_runtime_turn_preserving_watcher,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_lookup_tests.rs:35` | test/비실행 | `stop_runtime_turn_preserving_watcher(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_lookup_tests.rs:50` | test/비실행 | `let stop = stop_providerless_runtime_turn_preserving_watcher_strict_ownership;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_stop.rs:65` | source | `pub(super) async fn stop_judged_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_stop.rs:124` | source | `discord::receipt_settle::stopped(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/health/recovery/runtime_stop.rs:163` | source | `discord::receipt_settle::stopped(&shared, &provider, channel_id, cleanup_policy, None)` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/health/recovery/runtime_stop.rs:244` | source | `discord::receipt_settle::stopped(&shared, &provider, channel_id, cleanup_policy, None).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/health/recovery/runtime_stop.rs:299` | source | `pub async fn force_kill_provider_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_stop.rs:306` | source | `stop_provider_channel_runtime_with_policy(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/runtime_stop/wake_tests.rs:107` | test/비실행 | `let kill = crate::services::turn_lifecycle::force_kill_turn;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:56` | source | `pub(crate) async fn judge_provider_channel_stop(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:78` | source | `pub(crate) async fn stop_judged_provider_channel(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:89` | source | `let stop = super::stop_judged_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:101` | source | `pub(crate) async fn stop_provider_channel_runtime_with_policy(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:108` | source | `let judged = judge_provider_channel_stop(registry, provider_name, channel_id).await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:109` | source | `stop_judged_provider_channel(judged, reason, cleanup_policy).await` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:114` | source | `pub(crate) async fn stop_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement.rs:124` | source | `let run = super::stop_judged_channel_runtime;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement/judged_finish_tests.rs:186` | test/비실행 | `let kill = crate::services::turn_lifecycle::force_kill_turn;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement/judged_finish_tests.rs:241` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement/judged_finish_tests.rs:272` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement/judged_finish_tests.rs:299` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement/judged_finish_tests.rs:373` | test/비실행 | `let result = crate::services::turn_lifecycle::stop_turn_preserving_queue(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/stop_judgement/judged_finish_tests.rs:433` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/health/recovery/watchdog_pass/terminal_receipt_tests.rs:257` | test/비실행 | `assert!(discord::receipt_settle::settle_marked_row(&recovered, &PROVIDER, channel).await);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/health/recovery/watchdog_pass/terminal_receipt_tests.rs:273` | test/비실행 | `assert!(!discord::receipt_settle::settle_marked_row(&recovered, &PROVIDER, channel).await);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/health/turn_deliver.rs:70` | source | `pub(crate) async fn preflight_human_input(` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver.rs:167` | source | `pub async fn external_turn_hold_for_start(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/health/turn_deliver.rs:491` | source | `pub async fn deliver_human_input(` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver.rs:499` | source | `pub(crate) async fn deliver_with_reply(` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver.rs:936` | test/비실행 | `match deliver_human_input(registry, request).await {` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject/herdr_tests.rs:14` | test/비실행 | `use super::super::super::{HumanInputDelivery, HumanInputRequest, deliver_human_input};` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject/herdr_tests.rs:190` | test/비실행 | `let delivered = deliver_human_input(&registry, request(herdr)).await;` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject/herdr_tests.rs:207` | test/비실행 | `let delivered = deliver_human_input(&registry, request(plain)).await;` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject/herdr_tests.rs:241` | test/비실행 | `let delivered = deliver_human_input(&registry, request(channel)).await;` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject/herdr_tests.rs:279` | test/비실행 | `let delivered = deliver_human_input(&registry, request(channel)).await;` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject/herdr_tests.rs:298` | test/비실행 | `let delivered = deliver_human_input(` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject/test_hook.rs:94` | source | `let claim = crate::services::discord::router::claim_reserved_headless_turn;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/health/turn_deliver/inject_tests.rs:13` | test/비실행 | `use super::{HumanInputDelivery, HumanInputRequest, deliver_human_input};` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject_tests.rs:414` | test/비실행 | `match deliver_human_input(registry, request).await {` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/inject_tests.rs:476` | test/비실행 | `let start = crate::services::discord::mailbox_try_start_turn_kinded;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/health/turn_deliver/inject_tests.rs:1851` | test/비실행 | `crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/health/turn_deliver/inject_tests.rs:1924` | test/비실행 | `let claim = crate::services::discord::router::claim_reserved_headless_turn;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/health/turn_deliver/inject_tests.rs:2024` | test/비실행 | `let delivered = deliver_human_input(registry, request).await;` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/health/turn_deliver/prepared.rs:11` | source | `pub(crate) async fn prepare_external_human_input(` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/herdr_terminate/http_force.rs:30` | source | `pub(in crate::services) async fn terminate_for_http<T, P>(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/herdr_terminate/http_force_tests.rs:16` | test/비실행 | `use super::terminate_for_http;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/herdr_terminate/http_force_tests.rs:789` | test/비실행 | `let (result, applied) = terminate_for_http(&node.registry, node.pool.clone(), (request, e08::identity::unbound(&rig, &node)), "test", move &#124;_&#124; {` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/herdr_terminate/missing_marker_turn_tests.rs:168` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/inflight/native_stop_pin/tests.rs:430` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/inflight/terminal_receipt_stamp_tests.rs:135` | test/비실행 | `let record_or_stamp = crate::services::discord::receipt_settle::record_or_stamp;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/inflight/terminal_receipt_stamp_tests.rs:137` | test/비실행 | `assert!(!record_or_stamp(Some(&blocked), &owed, failed).await);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/live_bridge/guard_tests.rs:444` | test/비실행 | `!discord::queue_io::mailbox_try_start_turn_behind_queue(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/mailbox_finish.rs:370` | source | `super::receipt_settle::settle_v2_drops(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/mod.rs:98` | source | `mod receipt_settle;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/mod.rs:99` | source | `pub(crate) use receipt_settle::settle_v2_drops;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/mod.rs:1568` | source | `mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/mod.rs:1583` | source | `async fn mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/mod.rs:1591` | source | `queue_io::mailbox_try_start_turn_kinded_with_feedback(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/mod.rs:2864` | test/비실행 | `mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/outbound/local_retry_io_tests.rs:4` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/local_retry_io_tests.rs:80` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/o_delivery_evidence_tests.rs:5` | test/비실행 | `ownership::OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/o_delivery_evidence_tests.rs:385` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/o_delivery_evidence_tests.rs:478` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/o_writer_repost_io_tests.rs:649` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/o_writer_repost_io_tests.rs:781` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/o_writer_repost_io_tests/owner_tests.rs:9` | test/비실행 | `ownership::OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/outbound/o_writer_repost_io_tests/owner_tests.rs:88` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/queue_io.rs:37` | source | `mailbox_recovery_kickoff, mailbox_try_start_turn_adopting, mailbox_try_start_turn_behind_queue,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io.rs:38` | source | `mailbox_try_start_turn_kinded_with_feedback, mailbox_try_start_turn_unless_released,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io.rs:111` | source | `super::receipt_settle::settle_v2_drops(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/queue_io.rs:247` | source | `super::receipt_settle::settle_v2_drops(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/queue_io/cancel_backstop_tests.rs:7` | test/비실행 | `use crate::services::discord::health::{HealthRegistry, schedule_pending_queue_drain_after_cancel};` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/queue_io/cancel_backstop_tests.rs:112` | test/비실행 | `let drain = schedule_pending_queue_drain_after_cancel(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/queue_io/idle_queue_tests.rs:847` | test/비실행 | `mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/admission.rs:52` | test/비실행 | `.v2_admit_ticket(original.clone(), ticket.0.clone())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/admission.rs:85` | source | `pub(crate) async fn admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/admission.rs:92` | source | `.v2_admit_ticket_with_context(original.clone(), ticket.0.clone(), context)` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/drive.rs:36` | source | `pub(crate) async fn claim_turn(` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive.rs:44` | source | `let claimed = owner.claim_turn(message, &front);` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive.rs:178` | source | `owner.grant_front(front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive_tests.rs:80` | test/비실행 | `let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive_tests.rs:389` | test/비실행 | `owner.grant_front(&front).map(&#124;()&#124; front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive_tests.rs:393` | test/비실행 | `let claim = claim_turn(&mailbox, front.1.message_id, &front).await;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive_tests.rs:395` | test/비실행 | `let again = claim_turn(&mailbox, front.1.message_id, &front).await;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive_tests.rs:543` | test/비실행 | `owner.grant_front(&front).map(&#124;()&#124; front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive_tests.rs:546` | test/비실행 | `claim_turn(&mailbox, front.1.message_id, &front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/drive_tests.rs:585` | test/비실행 | `handle.v2_admit_ticket_with_context(original(channel, id), format!("t{id}"), context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/effect/codex_herdr_tests.rs:444` | test/비실행 | `let claim = drive::claim_turn(&m, front.1.message_id, &front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/ingress.rs:312` | source | `.v2_resume_ticket(self.item.clone(), ticket, same_input)` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/notice.rs:56` | source | `crate::services::discord::settle_v2_drops(Some(pool), handle, provider, channel_id).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/notice_tests.rs:3` | test/비실행 | `use super::super::drive::{Drive, claim_turn, wake};` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/notice_tests.rs:25` | test/비실행 | `.v2_admit_ticket(original, ticket.into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/notice_tests.rs:222` | test/비실행 | `let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/owed_drop_tests.rs:6` | test/비실행 | `use crate::services::discord::settle_v2_drops;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/owed_drop_tests.rs:28` | test/비실행 | `handle.v2_admit_ticket_with_context(original(channel, id), format!("t-{id}"), context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/owed_drop_tests.rs:160` | test/비실행 | `settle_v2_drops(Some(&closed), &handle, "claude", ChannelId::new(channel)).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/owed_drop_tests.rs:174` | test/비실행 | `settle_v2_drops(Some(&pool), &handle, "claude", ChannelId::new(channel)).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/pump.rs:70` | source | `owner.reserve_claimed((claim.front(), claim.number()), attempt, payload)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/queue_io/input_owner/routine.rs:70` | source | `Ok(admission::admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/start.rs:6` | source | `use super::admission::{Admission, AdmissionTicket, admit_with_context};` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/start.rs:56` | source | `pub(crate) async fn admit_synthetic(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/start.rs:72` | source | `match admit_with_context(&drive.mailbox, original, ticket, provenance).await {` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/start_tests.rs:52` | test/비실행 | `admit_synthetic(drive, origin, (&original, ticket), provenance, preflight).await` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/input_owner/start_tests.rs:145` | test/비실행 | `let started = admit_synthetic(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/retained_notice_tests.rs:47` | test/비실행 | `.v2_admit_ticket(item(1, &[1], None), "opaque-1".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/retained_notice_tests.rs:65` | test/비실행 | `.v2_admit_ticket(item(2, &[1, 2], Some("quoted")), "opaque-2".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:7` | source | `pub(in crate::services::discord) async fn mailbox_try_start_turn_kinded_with_feedback(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:15` | source | `mailbox_try_start_turn_ordered(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:30` | source | `pub(in crate::services::discord) async fn mailbox_try_start_turn_behind_queue(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:44` | source | `mailbox_try_start_turn_ordered(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:59` | source | `pub(in crate::services::discord) async fn mailbox_try_start_turn_unless_released(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:67` | source | `mailbox_try_start_turn_adopting(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:81` | source | `pub(in crate::services::discord) async fn mailbox_try_start_turn_adopting(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:90` | source | `mailbox_try_start_turn_ordered(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission.rs:111` | source | `async fn mailbox_try_start_turn_ordered(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/queue_io/turn_admission_tests.rs:90` | test/비실행 | `mailbox_try_start_turn_adopting(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/receipt_settle.rs:16` | source | `use crate::db::turns::{TerminalOutcome, TurnReceipt, record_turn_receipt};` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:84` | source | `match record_turn_receipt(pool, &receipt).await {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:94` | source | `pub(in crate::services::discord) async fn record_or_stamp(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:166` | source | `pub(in crate::services::discord) async fn settle_marked_row(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:186` | source | `pub(in crate::services::discord) async fn settle_boot(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:214` | source | `pub(in crate::services::discord) async fn settle_cleared(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:243` | source | `pub(in crate::services::discord) struct FinalizerClear {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:248` | source | `impl FinalizerClear {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:276` | source | `pub(in crate::services::discord) async fn record_before_clear(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:287` | source | `record_or_stamp(shared.pg_pool.as_ref(), row, outcome).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:293` | source | `fn split(self) -> (bool, FinalizerClear);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:297` | source | `fn split(self) -> (bool, FinalizerClear) {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:300` | source | `FinalizerClear {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:308` | source | `impl ClaimClear for FinalizerClear {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:309` | source | `fn split(self) -> (bool, FinalizerClear) {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:367` | source | `record_or_stamp(shared.pg_pool.as_ref(), &row, outcome).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:387` | source | `record_or_stamp(shared.pg_pool.as_ref(), &row, TerminalOutcome::Failed).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle.rs:392` | source | `pub(crate) use crate::services::queue::settle_v2_drops;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle_tests.rs:61` | test/비실행 | `let clear = FinalizerClear::new(event, ctx);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle_tests.rs:121` | test/비실행 | `let may_clear = record_or_stamp(Some(&pool), &owed, TerminalOutcome::Cancelled).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle_tests.rs:126` | test/비실행 | `assert!(record_or_stamp(Some(&pool), &plain, TerminalOutcome::Failed).await);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle_tests.rs:135` | test/비실행 | `assert!(!record_or_stamp(Some(&pool), &marked, TerminalOutcome::Failed).await);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/receipt_settle_tests.rs:142` | test/비실행 | `assert!(record_or_stamp(None, &plain, TerminalOutcome::Failed).await);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/recovery_engine/analytics_transcript.rs:75` | source | `let ended = super::receipt_settle::body_outcome(relayed_answer);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/recovery_engine/analytics_transcript.rs:76` | source | `super::receipt_settle::record(pg_pool, state, ended, chrono::Utc::now()).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/recovery_engine/herdr_admitted_restart.rs:168` | source | `super::receipt_settle::body_outcome(&row.full_response)` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/recovery_engine/herdr_admitted_restart.rs:172` | source | `if !super::receipt_settle::record_or_stamp(shared.pg_pool.as_ref(), &row, ended).await {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/recovery_engine/herdr_row_end_settle_tests.rs:376` | test/비실행 | `let hold = crate::services::discord::health::external_turn_hold_for_start(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/recovery_engine/restore_inflight.rs:100` | source | `let (states, boot) = super::receipt_settle::settle_boot(shared, states).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/recovery_engine/restore_inflight.rs:112` | source | `boot.settle_cleared(shared, provider).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/recovery_engine/runtime.rs:325` | source | `let claim = super::queue_io::mailbox_try_start_turn_unless_released(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/relay_recovery/apply.rs:545` | source | `let outcome = super::health::schedule_pending_queue_drain_after_cancel(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/relay_recovery/tests/incarnation_follow_up.rs:237` | test/비실행 | `let skipped = crate::services::discord::health::finish_cancelled_provider_channel_mailbox(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/relay_recovery/tests/incarnation_follow_up.rs:248` | test/비실행 | `let finished = crate::services::discord::health::finish_cancelled_provider_channel_mailbox(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/router/intake_dispatch/initial_fresh_tests.rs:13` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/intake_dispatch/initial_fresh_tests.rs:385` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/intake_dispatch/initial_fresh_tests.rs:388` | test/비실행 | `let mut hosts = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/intake_dispatch/tests/owner_ingress_tests.rs:791` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler.rs:94` | source | `pub(in crate::services::discord) use self::turn_lifecycle::mailbox_try_start_turn_with_terminal_marker_cleanup;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler.rs:118` | source | `let started = mailbox_try_start_turn_with_terminal_marker_cleanup(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler.rs:140` | source | `mailbox_try_start_turn_with_terminal_marker_cleanup(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/foreign_row.rs:11` | source | `use crate::services::discord::{SharedData, receipt_settle};` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/router/message_handler/foreign_row.rs:45` | source | `&& receipt_settle::settle_marked_row(shared, provider, state.channel_id).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/router/message_handler/foreign_row.rs:72` | source | `&& receipt_settle::settle_marked_row(shared, provider, state.channel_id).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/router/message_handler/headless_turn.rs:223` | source | `let claim = super::super::turn_start::claim_reserved_headless_turn;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/headless_turn/replay_start_origin_tests.rs:116` | test/비실행 | `let delivered = health::deliver_human_input(&host.registry, request).await;` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_origin_tests.rs:146` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:200` | test/비실행 | `health::start_reserved_headless_agent_turn_with_owner_channel(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:329` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:366` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:441` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:512` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:556` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:586` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:615` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:718` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:757` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:801` | test/비실행 | `let reservation=health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:824` | test/비실행 | `let reservation=health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:846` | test/비실행 | `let reservation=health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:913` | test/비실행 | `let reservation=health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1079` | test/비실행 | `let control=health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1104` | test/비실행 | `let subject=health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1174` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1185` | test/비실행 | `let outcome = health::start_reserved_headless_agent_turn_with_owner_channel(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1248` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1268` | test/비실행 | `let outcome = health::start_reserved_headless_agent_turn_with_owner_channel(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1338` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:1363` | test/비실행 | `let reservation = health::reserve_headless_agent_turn(channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/intake_turn/first_key_gate_paths_tests.rs:50` | test/비실행 | `let claim = crate::services::discord::router::claim_reserved_headless_turn;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/intake_turn/route_claim_tests.rs:63` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/intake_turn/route_queue_tests.rs:149` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:10` | source | `Drive, Front, FrontClaim, Handoff, Settled as Owed, claim_turn,` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:209` | source | `crate::services::discord::health::external_turn_hold_for_start(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:362` | source | `let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:652` | source | `start::admit_synthetic(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:875` | source | `let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_herdr_tests.rs:397` | test/비실행 | `let resumed = owner.runtime.block_on(owner.mailbox.v2_resume_ticket(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_idle_tests.rs:914` | test/비실행 | `assert!(owner.claim_turn(stolen.1.message_id, &stolen).is_some());` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_idle_tests.rs:2638` | test/비실행 | `use crate::services::discord::queue_io::input_owner::admission::admit_with_context;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_idle_tests.rs:2662` | test/비실행 | `admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_local_control_tests.rs:140` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_person_origin_tests.rs:62` | test/비실행 | `admission::admit_with_context(&rig.mailbox, &original, ticket, context).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_retained_marker_tests.rs:35` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_rollback_tests.rs:322` | test/비실행 | `admission::admit_with_context(&mailbox, &original, ticket, context).await` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_rollback_tests.rs:1239` | test/비실행 | `discord::settle_v2_drops(Some(&pool), &rig.mailbox, "claude", rig.channel).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_settled_front_tests.rs:3` | test/비실행 | `use crate::db::turns::{TerminalOutcome, TurnReceipt, record_turn_receipt};` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_settled_front_tests.rs:154` | test/비실행 | `record_turn_receipt(pool, &receipt).await.unwrap();` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_settled_front_tests.rs:198` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:10` | test/비실행 | `use crate::services::discord::queue_io::input_owner::drive::{Drive, Front, Handoff, claim_turn};` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:228` | test/비실행 | `let claimed = claim_turn(&self.mailbox, front.1.message_id, front);` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:277` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:1714` | test/비실행 | `use crate::services::discord::queue_io::input_owner::admission::admit_with_context;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:1731` | test/비실행 | `true => admit_with_context(&mailbox, &original, ticket, json!({"routine":1})).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:1753` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:1807` | test/비실행 | `owner.grant_front(&front).map(&#124;()&#124; front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs:1810` | test/비실행 | `let claim = claim_turn(&mailbox, front.1.message_id, &front).await;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_turn_mode_tests.rs:12` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_turn_mode_tests.rs:166` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_turn_mode_tests.rs:173` | test/비실행 | `let hosts = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_turn_mode_tests.rs:632` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input_turn_mode_tests.rs:639` | test/비실행 | `let hosts = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:127` | test/비실행 | `reserve_headless_agent_turn, start_reserved_headless_agent_turn_in_dm,` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:128` | test/비실행 | `start_reserved_headless_agent_turn_with_owner_channel,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:135` | test/비실행 | `let (metadata, reservation) = (Some(metadata), reserve_headless_agent_turn(LEGACY));` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:138` | test/비실행 | `start_reserved_headless_agent_turn_in_dm(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:154` | test/비실행 | `start_reserved_headless_agent_turn_with_owner_channel(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:246` | test/비실행 | `let admitted = admission::admit_with_context(&kept_by, &original, ticket, context).await;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:258` | test/비실행 | `owner.grant_front(&front).map(&#124;()&#124; front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_start_failure_tests.rs:230` | test/비실행 | `let admitted = crate::services::discord::queue_io::input_owner::admission::admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:4` | test/비실행 | `use crate::services::discord::queue_io::input_owner::drive::claim_turn;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:36` | test/비실행 | `.v2_admit_ticket(item, "start-facts".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:43` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:48` | test/비실행 | `let claim = claim_turn(&mailbox, message, &front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:285` | test/비실행 | `.v2_admit_ticket(item, "settled-before-completion".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:292` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:297` | test/비실행 | `let claim = claim_turn(&mailbox, 7787003, &front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure_state_tests.rs:328` | test/비실행 | `slot.as_mut().ok_or("owner")?.reserve_claimed((claim.front(), claim.number()), attempt, payload).map(drop)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/provider_dispatch_codex_t14_actual_tests.rs:13` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/provider_dispatch_codex_t14_actual_tests.rs:663` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/provider_dispatch_codex_t14_actual_tests.rs:667` | test/비실행 | `let handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/router/message_handler/root_replaced_recovery_tests/preclaim_tests.rs:4` | test/비실행 | `Admission, AdmissionTicket, TurnBind, admit_with_context, exact_turn,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/root_replaced_recovery_tests/preclaim_tests.rs:359` | test/비실행 | `admit_with_context(&mailbox, &expected, ticket.clone(), context).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/root_replaced_recovery_tests/preclaim_tests.rs:367` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/root_replaced_recovery_tests/preclaim_tests.rs:790` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/router/message_handler/s1_entry_tests.rs:252` | test/비실행 | `let result = turn_deliver::deliver_human_input(&registry, request(channel))` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/router/message_handler/s1_entry_tests.rs:600` | test/비실행 | `let delivery = turn_deliver::deliver_human_input(&registry, request(channel))` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/router/message_handler/s1_entry_tests.rs:670` | test/비실행 | `let held = turn_deliver::deliver_human_input(&registry, request(channel))` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/router/message_handler/s1_entry_tests.rs:814` | test/비실행 | `let delivered = turn_deliver::deliver_human_input(&registry, request(channel))` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/router/message_handler/s1_entry_tests.rs:918` | test/비실행 | `let delivery = turn_deliver::deliver_human_input(&registry, request(channel))` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/router/message_handler/s1_entry_tests.rs:1610` | test/비실행 | `turn_deliver::deliver_human_input(&registry, request_for(channel, provider.clone()))` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/router/message_handler/turn_lifecycle.rs:14` | source | `pub(in crate::services::discord) async fn mailbox_try_start_turn_with_terminal_marker_cleanup(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/turn_lifecycle.rs:31` | source | `crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/message_handler/turn_lifecycle.rs:242` | test/비실행 | `let started = mailbox_try_start_turn_with_terminal_marker_cleanup(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/mod.rs:46` | test/비실행 | `pub(super) use turn_start::claim_reserved_headless_turn;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/turn_start.rs:230` | source | `pub(in crate::services::discord) async fn claim_reserved_headless_turn(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/router/turn_start.rs:256` | source | `let start = super::super::queue_io::mailbox_try_start_turn_behind_queue;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:58` | test/비실행 | `.v2_admit_ticket_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:74` | test/비실행 | `.v2_admit_ticket_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:318` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:319` | test/비실행 | `assert!(owner.claim_turn(A, &front).is_some());` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:849` | test/비실행 | `o.grant_front(&f)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:854` | test/비실행 | `let claim = input_owner::drive::claim_turn(&handle, A, &front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:869` | test/비실행 | `.v2_request(move &#124;s&#124; s.as_mut().unwrap().reserve_claimed(` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs:931` | test/비실행 | `o.grant_front(&f)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/discord/runtime_bootstrap/home_lifecycle_tests.rs:4` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/home_lifecycle_tests.rs:31` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/home_lifecycle_tests.rs:44` | test/비실행 | `let handles = host::start_managed(ShadowProvider::Claude, true, owned, parts);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host.rs:813` | source | `for handle in host::start_managed(shadow, false, owned, &#124;&#124; parts(shared, provider, config)) {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host.rs:879` | source | `for handle in host::start_managed(shadow, true, owned, &#124;&#124; parts(shared, provider, config)) {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_fresh_launch_tests.rs:393` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_fresh_launch_tests.rs:430` | test/비실행 | `handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_fresh_launch_tests/ordinary_restart_tests.rs:253` | test/비실행 | `let mut hosts = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_fresh_launch_tests/ordinary_restart_tests/fresh_scope_restart_tests.rs:143` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_restart_phase_tests.rs:133` | test/비실행 | `*handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution.rs:220` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution.rs:273` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution.rs:295` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution.rs:349` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(rebound)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution.rs:383` | test/비실행 | `snapshot.install_execution(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution.rs:448` | test/비실행 | `snapshot.install_execution(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_more_tests.rs:3` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_more_tests.rs:10` | test/비실행 | `global: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_more_tests.rs:11` | test/비실행 | `local: Option<Arc<OwnershipGate>>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_more_tests.rs:124` | test/비실행 | `Ok(Some(permit)) => snapshot.install_execution(channel, Some(Ok(permit))),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_more_tests.rs:125` | test/비실행 | `Err(reason) => snapshot.install_execution(channel, Some(Err(reason))),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_more_tests.rs:167` | test/비실행 | `let handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_race_tests.rs:183` | test/비실행 | `.install_execution(f.channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_race_tests.rs:193` | test/비실행 | `f.handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_tests.rs:130` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/boot_input_tests.rs:171` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/missing_halt_tests.rs:276` | test/비실행 | `.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/missing_halt_tests.rs:414` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/missing_halt_tests.rs:453` | test/비실행 | `let mut successor = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/park_missing_tests.rs:162` | test/비실행 | `.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/park_reply_tests.rs:80` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/park_reply_tests.rs:254` | test/비실행 | `let mut restarted = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/refresh_tests.rs:262` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/refresh_tests.rs:276` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/refresh_tests.rs:292` | test/비실행 | `snapshot.install_execution(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/refresh_tests.rs:421` | test/비실행 | `let _handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/refresh_tests.rs:556` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/refresh_tests.rs:695` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/refresh_tests.rs:783` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host_takeover_tests.rs:16` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host_takeover_tests.rs:283` | test/비실행 | `let gate = OwnershipGate::default();` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/o_writer_host_takeover_tests.rs:538` | test/비실행 | `let mut handles = host::start_managed(Claude, true, owned, &#124;&#124; parts);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/owner_execution.rs:113` | source | `snapshot.install_execution(channel, view);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/owner_execution.rs:192` | source | `snapshot.suspend_execution(channel, error.reason, error.temporary);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/owner_execution.rs:197` | source | `if snapshot.resume_execution(channel, permit.as_ref()) {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/owner_execution.rs:213` | source | `snapshot.suspend_execution(channel, CHANGED.into(), false);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/owner_execution.rs:233` | source | `snapshot.install_execution(channel, permit.map(Ok));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/runtime_bootstrap/queued_recovery_s2a_tests.rs:183` | test/비실행 | `.v2_admit_ticket(queued(drained, 1), ticket)` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/runtime_bootstrap/queued_recovery_s2a_tests.rs:212` | test/비실행 | `let admitted = handle.v2_admit_ticket(queued(cleared, 1), ticket).await;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/runtime_bootstrap/queued_recovery_s2a_tests.rs:340` | test/비실행 | `.v2_admit_ticket(queued(cleared, 1), again)` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/runtime_bootstrap/queued_recovery_s2a_tests.rs:349` | test/비실행 | `.v2_admit_ticket(queued(cleared, 2), next)` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/session_relay_sink/fixtures/o_writer.rs:10` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/session_relay_sink/fixtures/o_writer.rs:105` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/session_relay_sink/fixtures/o_writer.rs:166` | source | `gate: Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/session_relay_sink/o_adoption_e2e_tests.rs:8` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/session_relay_sink/o_adoption_e2e_tests.rs:25` | test/비실행 | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/session_relay_sink/o_adoption_e2e_tests.rs:40` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/session_relay_sink/o_delivery_e2e_tests.rs:428` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/session_relay_sink/o_delivery_e2e_tests.rs:448` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tmux.rs:650` | source | `let started = super::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tmux.rs:1011` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tmux.rs:1052` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tmux_watcher.rs:1554` | source | `crate::services::discord::receipt_settle::watcher_outcome(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/tmux_watcher/codex_replacement_tests.rs:448` | test/비실행 | `health::external_turn_hold_for_start(Some(&registry), &ProviderKind::Codex, CHANNEL).await;` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/tmux_watcher/judged_stop_harness_tests.rs:44` | test/비실행 | `let stop_turn = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/tmux_watcher/no_result_exits.rs:429` | source | `!crate::services::discord::receipt_settle::record_or_stamp(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/tmux_watcher/no_result_exits.rs:432` | source | `crate::services::discord::receipt_settle::body_outcome(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/tmux_watcher/o_delegated_watcher_tests.rs:300` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tmux_watcher/o_delegated_watcher_tests.rs:370` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tmux_watcher/path_b_record.rs:187` | source | `!crate::services::discord::receipt_settle::record_or_stamp(pool, state, outcome).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/tmux_watcher/post_stream_exit.rs:167` | source | `&& !crate::services::discord::receipt_settle::pane_dead(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/tmux_watcher/receipt_watcher_harness_tests.rs:178` | test/비실행 | `let kill = crate::services::turn_lifecycle::force_kill_turn;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/tmux_watcher/receipt_watcher_harness_tests.rs:439` | test/비실행 | `let stop = crate::services::discord::health::stop_channel_runtime;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/tmux_watcher/stall_exit.rs:47` | source | `crate::services::discord::receipt_settle::record_or_stamp(pool, pinned_inflight, failed)` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/tmux_watcher/stall_exit.rs:196` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tmux_watcher/stall_exit.rs:269` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_direct_pending_start.rs:424` | test/비실행 | `// through 'mailbox_try_start_turn_kinded', which (a) starts a` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_direct_pending_start.rs:556` | test/비실행 | `/// kickoff routes through 'mailbox_try_start_turn_kinded', which either starts a` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_direct_pending_start/tests.rs:2387` | test/비실행 | `/// production is a normal merge inside 'mailbox_try_start_turn_kinded').` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/claude_source_idle_tests.rs:5` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/claude_source_idle_tests.rs:384` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/herdr_source_tests.rs:1021` | test/비실행 | `let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/busy_inject_e2e_tests.rs:321` | test/비실행 | `let start = crate::services::discord::mailbox_try_start_turn_kinded;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/relay_e2e/claude_idle_o_host_fixture_tests.rs:8` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/claude_idle_o_host_fixture_tests.rs:195` | test/비실행 | `pub gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/claude_idle_o_host_fixture_tests.rs:277` | test/비실행 | `gate: Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/codex_o_handoff_replay_tests.rs:7` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/codex_o_handoff_replay_tests.rs:134` | test/비실행 | `let gate = Arc::new(OwnershipGate::default()); gate.acquired();` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_e2e_tests.rs:40` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_e2e_tests.rs:389` | test/비실행 | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_e2e_tests.rs:396` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_e2e_tests.rs:423` | test/비실행 | `self.running = host::start_managed(ShadowProvider::Claude, true, owned, parts);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_e2e_tests.rs:1001` | test/비실행 | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_e2e_tests.rs:1008` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/mod.rs:808` | test/비실행 | `health::start_headless_agent_turn(` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests/herdr_direct_actual_client_tests.rs:352` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests/herdr_direct_actual_client_tests.rs:357` | test/비실행 | `let handles = host::start_managed(ShadowProvider::Claude, true, owned, &#124;&#124; HostParts {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests/herdr_direct_roundtrip_tests.rs:22` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests/herdr_direct_roundtrip_tests.rs:267` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests/herdr_direct_roundtrip_tests.rs:272` | test/비실행 | `let handles = host::start_managed(ShadowProvider::Claude, true, owned, &#124;&#124; HostParts {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:42` | test/비실행 | `use crate::services::discord::mailbox_try_start_turn_kinded;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:336` | test/비실행 | `mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:379` | test/비실행 | `let next_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:559` | test/비실행 | `let next_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:776` | test/비실행 | `mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:843` | test/비실행 | `let next_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:922` | test/비실행 | `let next_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:1154` | test/비실행 | `mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:1209` | test/비실행 | `let synth_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:1267` | test/비실행 | `let synth_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:1475` | test/비실행 | `let synth_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:1804` | test/비실행 | `let synth_claimed = mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:196` | source | `return super::super::super::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:201` | source | `let admission = super::super::super::queue_io::mailbox_try_start_turn_adopting(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/tui_prompt_relay/tests/fenced_admission_tests.rs:265` | test/비실행 | `crate::services::discord::queue_io::mailbox_try_start_turn_unless_released;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_clear_order_tests.rs:59` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_policy_tests.rs:100` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_policy_tests.rs:582` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_reconcile_tests.rs:46` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_reconcile_tests.rs:179` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_retry_tests.rs:354` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tail_tests.rs:80` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tail_tests.rs:381` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tests.rs:158` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tests.rs:472` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tests.rs:570` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tests.rs:623` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tests.rs:644` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/before_start_stop_tests.rs:742` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/bridge_entry_persist.rs:510` | source | `crate::services::discord::receipt_settle::entry_aborted(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_bridge/completion_postlude/channel_episode_scope.rs:345` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/completion_postlude/channel_episode_scope.rs:388` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/completion_postlude/legacy_summary_tests.rs:46` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/completion_postlude/legacy_summary_tests.rs:498` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/entry_abort_mailbox_tests.rs:97` | test/비실행 | `discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/entry_abort_mailbox_tests.rs:208` | test/비실행 | `let readopted = discord::queue_io::mailbox_try_start_turn_adopting(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/status_panel_tests.rs:709` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/status_panel_tests.rs:817` | test/비실행 | `crate::services::discord::mailbox_try_start_turn_kinded(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/codex_empty_tests.rs:498` | test/비실행 | `ownership::OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/codex_empty_tests.rs:568` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/discord/turn_bridge/terminal_receipt_tests.rs:400` | test/비실행 | `let settled = crate::services::discord::receipt_settle::settle_marked_row(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery_tests.rs:523` | test/비실행 | `let result = crate::services::turn_lifecycle::stop_turn_preserving_queue(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:137` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:287` | test/비실행 | `let judge = crate::services::discord::health::judge_provider_channel_stop;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:293` | test/비실행 | `let stop_judged = crate::services::discord::health::stop_judged_provider_channel;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:335` | test/비실행 | `let judge = crate::services::discord::health::judge_provider_channel_stop;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:336` | test/비실행 | `let stop_judged = crate::services::discord::health::stop_judged_provider_channel;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:381` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:414` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:494` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:501` | test/비실행 | `let drain = crate::services::discord::health::schedule_pending_queue_drain_after_cancel;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:692` | test/비실행 | `let stop = crate::services::discord::health::stop_channel_runtime;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:703` | test/비실행 | `let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs:728` | test/비실행 | `let stop = crate::services::discord::health::stop_channel_runtime;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/native_stop_contract_tests.rs:752` | test/비실행 | `let result = crate::services::turn_lifecycle::stop_turn_preserving_queue(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/native_stop_contract_tests.rs:888` | test/비실행 | `let result = crate::services::turn_lifecycle::stop_turn_preserving_queue(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/process_force_kill_tests.rs:137` | test/비실행 | `let kill = crate::services::turn_lifecycle::force_kill_turn;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/tmux_runtime/stop_host_tests.rs:559` | test/비실행 | `let stop = crate::services::discord::health::stop_provider_channel_runtime_with_policy;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_bridge/turn_analytics.rs:2` | source | `use crate::db::turns::{PersistTurnOwned, TerminalOutcome, TurnReceipt, record_turn_receipt};` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_bridge/turn_analytics.rs:54` | source | `match record_turn_receipt(pool, &receipt).await {` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_bridge/turn_analytics.rs:66` | source | `!crate::services::discord::receipt_settle::keep_marked(inflight_state, stamp)` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_finalizer/episode.rs:61` | source | `clear_inflight: impl crate::services::discord::receipt_settle::ClaimClear,` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_finalizer/episode.rs:131` | source | `&& receipt.record_before_clear(shared, row).await` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_finalizer/finalize.rs:90` | source | `crate::services::discord::receipt_settle::FinalizerClear::new(event, ctx),` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/discord/turn_presence/activity/unbound/dormancy_tests.rs:123` | test/비실행 | `let delivered = turn_deliver::deliver_human_input(` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity/unbound/dormancy_tests.rs:141` | test/비실행 | `turn_deliver::external_turn_hold_for_start(Some(&registry), &provider, CHANNEL).await,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/legacy_divergence_entry_tests.rs:3` | test/비실행 | `HealthRegistry, HumanInputDelivery, HumanInputRequest, deliver_human_input,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity/unbound/legacy_divergence_entry_tests.rs:4` | test/비실행 | `external_turn_hold_for_start, start_without_gateway,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/legacy_divergence_entry_tests.rs:37` | test/비실행 | `external_turn_hold_for_start(Some(&registry), &ProviderKind::Claude, channel).await,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/legacy_divergence_entry_tests.rs:49` | test/비실행 | `let first = deliver_human_input(&registry, request()).await.unwrap();` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity/unbound/legacy_divergence_entry_tests.rs:60` | test/비실행 | `let again = deliver_human_input(&registry, request()).await.unwrap();` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:6` | test/비실행 | `HealthRegistry, HumanInputDelivery, HumanInputRequest, deliver_human_input,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:7` | test/비실행 | `external_turn_hold_for_start, start_without_gateway,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:276` | test/비실행 | `let hold = external_turn_hold_for_start(Some(&registry), &provider, channel).await;` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:287` | test/비실행 | `let first = deliver_human_input(&registry, request()).await.unwrap();` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:297` | test/비실행 | `let second = deliver_human_input(&registry, request()).await.unwrap();` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:406` | test/비실행 | `let hold = &#124;&#124; external_turn_hold_for_start(Some(&registry), &provider, rig.channel.get());` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:540` | test/비실행 | `external_turn_hold_for_start(Some(&registry), &ProviderKind::Claude, rig.channel.get())` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:562` | test/비실행 | `external_turn_hold_for_start(Some(&registry), &ProviderKind::Claude, rig.channel.get())` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/live_owner_tests.rs:622` | test/비실행 | `let hold = &#124;&#124; external_turn_hold_for_start(Some(&registry), &provider, other.get());` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity/unbound/receipt_tests.rs:11` | test/비실행 | `use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_presence/activity/unbound/receipt_tests.rs:471` | test/비실행 | `let stopped = force_kill_turn(Some(&registry), &target, "receipt reset", "receipt_test").await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/discord/turn_presence/activity_tests.rs:1523` | test/비실행 | `HealthRegistry, HumanInputDelivery, HumanInputRequest, deliver_human_input,` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity_tests.rs:1524` | test/비실행 | `external_turn_hold_for_start, register_inject_runtime, start_without_gateway,` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/turn_presence/activity_tests.rs:1552` | test/비실행 | `let delivered = deliver_human_input(&registry, request).await;` | Authorize/Admit/Deliver | H (원격 human) |
| `src/services/discord/turn_presence/activity_tests.rs:1557` | test/비실행 | `let held = external_turn_hold_for_start(Some(&registry), &provider, channel).await;` | AdmissionHold/Start | A (headless admission) |
| `src/services/discord/zombie_foreground_release.rs:44` | source | `pub(crate) use super::commands::stop::run_herdr_cancel;` | AuthorizedStop | N (nonforced Herdr만) |
| `src/services/dispatched_sessions/herdr_kill.rs:13` | source | `use crate::services::discord::herdr_terminate::http_force::{Expected, terminate_for_http};` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/dispatched_sessions/herdr_kill.rs:60` | source | `let (result, effects) = terminate_for_http(registry, pool, bound, reason, {` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue.rs:13` | source | `ForceKillRow, TurnLifecycleTarget, force_kill_turn_without_cancel_event,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue.rs:14` | source | `stop_turn_preserving_queue_without_cancel_event,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue.rs:31` | source | `pub(crate) use cancel_effects::settle_v2_drops;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/queue.rs:144` | source | `let lifecycle = stop_turn_preserving_queue_without_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue.rs:485` | source | `force_kill_turn_without_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue.rs:494` | source | `stop_turn_preserving_queue_without_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue/cancel_effects.rs:86` | source | `let outcome = crate::services::discord::health::schedule_pending_queue_drain_after_cancel(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue/cancel_effects.rs:133` | source | `let finish = crate::services::discord::health::finish_cancelled_provider_channel_mailbox(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue/cancel_effects.rs:168` | source | `crate::services::queue::settle_v2_drops(pool, &handle, provider.as_str(), channel).await;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/queue/cancel_effects.rs:430` | source | `crate::services::queue::settle_v2_drops(Some(pool), actor, provider.as_str(), channel)` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/queue/cancel_effects.rs:450` | source | `pub(crate) async fn settle_v2_drops(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/queue/cancel_effects_v2_tests.rs:62` | test/비실행 | `handle.v2_admit_ticket_with_context(item(id, channel.get()), "t-forced".into(), context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/queue/cancel_effects_v2_tests.rs:141` | test/비실행 | `handle.v2_admit_ticket_with_context(item(id, channel.get()), ticket, context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/queue/cancel_effects_v2_tests.rs:323` | test/비실행 | `let admitted = handle.v2_admit_ticket(input.clone(), ticket()).await;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/queue/cancel_effects_v2_tests.rs:363` | test/비실행 | `let recovered = handle.v2_admit_ticket(input.clone(), ticket()).await;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/queue/cancel_effects_v2_tests.rs:395` | test/비실행 | `let admitted = admitter.v2_admit_ticket(pending, ticket()).await;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/queue/cancel_effects_v2_tests.rs:410` | test/비실행 | `let recovered = handle.v2_admit_ticket(input.clone(), ticket()).await;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/queue/herdr_stop.rs:24` | source | `let stop = crate::services::discord::zombie_foreground_release::run_herdr_cancel;` | AuthorizedStop | N (nonforced Herdr만) |
| `src/services/queue/http_force.rs:13` | source | `use crate::services::discord::herdr_terminate::http_force::{Expected, terminate_for_http};` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/queue/http_force.rs:86` | source | `let (result, effects) = terminate_for_http(registry, pool.clone(), bound, REASON, {` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/routines/agent_executor.rs:18` | source | `HealthRegistry, reserve_headless_agent_turn, reserve_headless_agent_turn_in_dm,` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/routines/agent_executor.rs:19` | source | `resolve_bot_http, start_reserved_headless_agent_turn_in_dm,` | AdmissionHold/Start | A (headless admission) |
| `src/services/routines/agent_executor.rs:20` | source | `start_reserved_headless_agent_turn_with_owner_channel,` | AdmissionHold/Start | A (headless admission) |
| `src/services/routines/agent_executor/launch.rs:60` | source | `let (dm_channel_id, reservation) = reserve_headless_agent_turn_in_dm(` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/routines/agent_executor/launch.rs:106` | source | `reserve_headless_agent_turn(turn_channel_id),` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/routines/agent_executor/launch.rs:181` | source | `start_reserved_headless_agent_turn_in_dm(` | AdmissionHold/Start | A (headless admission) |
| `src/services/routines/agent_executor/launch.rs:198` | source | `start_reserved_headless_agent_turn_with_owner_channel(` | AdmissionHold/Start | A (headless admission) |
| `src/services/routines/agent_executor/owner_input_tests.rs:30` | test/비실행 | `let reservation = reserve_headless_agent_turn(poise::serenity_prelude::ChannelId::new(7272));` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/routines/agent_executor/owner_review_tests.rs:442` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/routines/agent_executor/owner_review_tests.rs:444` | test/비실행 | `.claim_turn(front.1.message_id, &front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/routines/session_control.rs:9` | source | `use crate::services::discord::health::{HealthRegistry, clear_provider_channel_runtime};` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/routines/session_control.rs:12` | source | `ForceKillRow, TurnLifecycleStopResult, TurnLifecycleTarget, force_kill_turn,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/routines/session_control.rs:105` | source | `let reset = clear_provider_channel_runtime(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/routines/session_control.rs:205` | test/비실행 | `/// diverge from the name 'start_reserved_headless_agent_turn' actually used` | AdmissionHold/Start | A (headless admission) |
| `src/services/routines/session_control.rs:235` | test/비실행 | `/// bindings (needed so 'force_kill_turn' clears the right runtime mailbox).` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/routines/session_control.rs:690` | source | `return force_kill_turn(registry, &lifecycle_target, reason, code).await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/scheduled_messages.rs:42` | source | `HealthRegistry, reserve_headless_agent_turn,` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/scheduled_messages.rs:43` | source | `start_reserved_headless_agent_turn_with_owner_channel,` | AdmissionHold/Start | A (headless admission) |
| `src/services/scheduled_messages.rs:358` | source | `let reservation = reserve_headless_agent_turn(turn_channel);` | ReservationValue (grant 아님) | A (headless admission 준비) |
| `src/services/scheduled_messages.rs:429` | source | `let outcome = start_reserved_headless_agent_turn_with_owner_channel(` | AdmissionHold/Start | A (headless admission) |
| `src/services/scheduled_messages/owner_input_tests.rs:177` | test/비실행 | `owner.grant_front(&front)?;owner.claim_turn(front.1.message_id,&front).ok_or("claim")?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/session_resume.rs:26` | source | `ForceKillRow, TurnLifecycleTarget, force_kill_turn_with_verdict, force_kill_verdict,` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/session_resume.rs:432` | source | `let verdict = force_kill_verdict(Some(registry), &target, Some(row)).await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/session_resume.rs:563` | source | `let kill = force_kill_turn_with_verdict(Some(registry), verdict, reason, "force_kill");` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/tmux_diagnostics.rs:174` | test/비실행 | `// completed/skipped/paused the run). 'force_kill_turn' records the` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/tui_o/alarm.rs:830` | test/비실행 | `use super::super::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/alarm.rs:870` | test/비실행 | `Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/channel_policy.rs:427` | source | `pub(crate) fn install_execution(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/channel_policy.rs:496` | source | `pub(crate) fn suspend_execution(&self, channel: u64, reason: String, temporary: bool) {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/channel_policy.rs:527` | source | `pub(crate) fn resume_execution(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/gates_m1_support.rs:5` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/gates_m1_support.rs:82` | source | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:41` | source | `pub struct OwnershipGate {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:46` | source | `impl Default for OwnershipGate {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:62` | source | `impl OwnershipGate {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:184` | source | `static GATES: LazyLock<Mutex<HashMap<String, Arc<OwnershipGate>>>> = LazyLock::new(Mutex::default);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:187` | source | `pub fn gate(provider: &str) -> Arc<OwnershipGate> {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:193` | source | `pub async fn release_gateway<F: Future>(gate: &OwnershipGate, release: F) -> F::Output {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:204` | test/비실행 | `let gate = OwnershipGate::default();` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership.rs:226` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution.rs:2` | source | `use super::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution.rs:21` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution.rs:36` | source | `pub(crate) fn gate(&self) -> Arc<OwnershipGate> {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution.rs:176` | source | `gate: Arc::new(OwnershipGate::for_execution(generation)),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:74` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:113` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:119` | test/비실행 | `snapshot.install_execution(channel, Some(Err(foreign)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:135` | test/비실행 | `snapshot.install_execution(channel, Some(Err(unknown)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:274` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(permit)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:279` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(again)));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:298` | test/비실행 | `changing.install_execution(channel, Some(Err("revoked".into())))` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:354` | test/비실행 | `gate: Arc::new(super::super::OwnershipGate::for_execution(generation)),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:361` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(Arc::clone(&permit))));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:371` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(read_now().await.unwrap().unwrap())));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:372` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(Arc::new(bound))));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/ownership/execution/tests/mod.rs:382` | test/비실행 | `snapshot.install_execution(channel, Some(Ok(Arc::new(swapped))));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/allowlist_host_tests.rs:5` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/allowlist_host_tests.rs:118` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/allowlist_host_tests.rs:120` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/dispatch.rs:21` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/dispatch.rs:41` | source | `fn gate(&self) -> Arc<OwnershipGate>;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/projection_tests.rs:20` | test/비실행 | `use crate::services::tui_o::writer::host::{FencedFacts, start_managed};` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/projection_tests.rs:747` | test/비실행 | `let mut writers = start_managed(ShadowProvider::Claude, true, owned, parts);` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_activation_tests.rs:8` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_activation_tests.rs:27` | test/비실행 | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_activation_tests.rs:52` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_activation_tests.rs:100` | test/비실행 | `fn gate(&self) -> Arc<OwnershipGate> {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_tests.rs:26` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_tests.rs:41` | test/비실행 | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_tests.rs:53` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_tests.rs:95` | test/비실행 | `fn gate(&self) -> Arc<OwnershipGate> {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runner_tests.rs:608` | test/비실행 | `3 => writer.gate = Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:268` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:333` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:337` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:507` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:591` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:594` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:869` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:1018` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/repost/runtime_host_activation_tests.rs:1021` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/notice_activation_tests.rs:62` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/notice_work_tests.rs:3` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/notice_work_tests.rs:152` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/scoped_submission_tests.rs:4` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/scoped_submission_tests.rs:41` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/scoped_transition_tests.rs:16` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/scoped_transition_tests.rs:107` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/scoped_transition_tests.rs:315` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/transition.rs:13` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/transition.rs:91` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/transition_snapshot.rs:11` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/transition_snapshot.rs:154` | source | `gate: &OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/transition_snapshot.rs:163` | source | `gate: &OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/actor/transition_snapshot.rs:294` | source | `fn home_now(gate: &OwnershipGate) -> Home {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deferred.rs:16` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deferred.rs:37` | source | `pub(super) gate: &'a OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deliver.rs:18` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deliver.rs:210` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deliver.rs:241` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deliver.rs:252` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deliver.rs:304` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/deliver.rs:932` | source | `fn gate(&self) -> Arc<OwnershipGate> {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/executed_resume_tests.rs:510` | test/비실행 | `let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:23` | source | `use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:162` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:223` | source | `pub(crate) fn hosted_under(&self, channel: u64, gate: &Arc<OwnershipGate>) -> bool {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:232` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:338` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:406` | source | `pub gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:411` | source | `pub(super) fn owned_now(gate: &OwnershipGate) -> bool {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:423` | source | `detached(start_managed(provider, pg_gateway, owned, prepare))` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:530` | source | `pub fn start_managed<I: HostIo>(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:690` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:777` | source | `gate: &'a Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host.rs:980` | source | `pub(super) async fn until_owned(gate: &OwnershipGate) {` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host/first_init.rs:12` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host/first_init.rs:43` | source | `(gate, stopping): (&Arc<OwnershipGate>, &watch::Receiver<bool>),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host/first_init.rs:201` | source | `gate: &OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host/first_init.rs:288` | source | `(gate, stopping): (&Arc<OwnershipGate>, &watch::Receiver<bool>),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host/fresh_input_tests.rs:27` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host/fresh_input_tests.rs:238` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host/transition.rs:145` | test/비실행 | `gate: &Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host_managed_tests.rs:5` | test/비실행 | `use crate::services::tui_o::writer::host::{ManagedWriterHandle, WriterStopped, start_managed};` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host_managed_tests.rs:124` | test/비실행 | `start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/host_managed_tests.rs:276` | test/비실행 | `let failed = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/input_notices/migration_tests.rs:4` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/input_notices/migration_tests.rs:127` | test/비실행 | `Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt.rs:10` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt.rs:115` | source | `gate: &OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt/claude.rs:7` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt/claude.rs:40` | source | `pub(super) gate: &'a OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt/codex.rs:8` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt/codex.rs:78` | source | `pub(super) gate: &'a OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt/codex.rs:92` | source | `gate: &OwnershipGate,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_binding_tests.rs:10` | test/비실행 | `use crate::services::tui_o::writer::host::{FencedFacts, ManagedWriterHandle, start_managed};` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_binding_tests.rs:212` | test/비실행 | `let mut handles = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_binding_tests.rs:580` | test/비실행 | `let handles = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_binding_tests.rs:697` | test/비실행 | `let handles = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_cleanup_tests.rs:6` | test/비실행 | `use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_cleanup_tests.rs:43` | test/비실행 | `start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_host_tests.rs:7` | test/비실행 | `use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/late_adopt_host_tests.rs:138` | test/비실행 | `let mut handles = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/local_retry_host_tests.rs:48` | test/비실행 | `let gate = Arc::new(OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/local_retry_host_tests.rs:52` | test/비실행 | `super::super::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/operator_resume_source_tests.rs:89` | test/비실행 | `let mut hosts = host::start_managed(ShadowProvider::Codex, true,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/owed_tests.rs:12` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/owed_tests.rs:194` | test/비실행 | `Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/p5a2_actor_tests.rs:9` | test/비실행 | `Custody, HostIo, HostParts, ManagedWriterHandle, Readiness, start_managed,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/p5a2_actor_tests.rs:196` | test/비실행 | `let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/p5a2_actor_tests.rs:203` | test/비실행 | `let handles = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/park_alias_tests.rs:179` | test/비실행 | `let mut handles = host::start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/rotation.rs:503` | source | `self.settle_cleared(writer)?;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/tui_o/writer/rotation/clear_end.rs:26` | source | `pub(super) fn settle_cleared<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/tui_o/writer/rotation/readers.rs:260` | source | `self.settle_cleared(writer)?;` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/tui_o/writer/rotation/scoped_tests.rs:210` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/rotation/scoped_tests.rs:305` | test/비실행 | `Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/rotation/scoped_tests.rs:361` | test/비실행 | `Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/s2a_actor_tests.rs:9` | test/비실행 | `use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/s2a_actor_tests.rs:35` | test/비실행 | `let mut handles = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/s2a_actor_tests.rs:176` | test/비실행 | `let handles = start_managed(` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/transition_port.rs:15` | source | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/transition_port.rs:226` | source | `pub(super) gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/transition_port.rs:256` | source | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/transition_route_tests.rs:11` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/transition_route_tests.rs:29` | test/비실행 | `gate: &Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/transition_route_tests.rs:322` | test/비실행 | `let (owned, unowned) = (&chain.harness.gate, &Arc::new(OwnershipGate::default()));` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/writer_tests.rs:15` | test/비실행 | `use crate::services::tui_o::ownership::OwnershipGate;` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/writer_tests.rs:288` | test/비실행 | `gate: Arc<OwnershipGate>,` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/writer_tests.rs:332` | test/비실행 | `Arc::new(OwnershipGate::default()),` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/tui_o/writer/writer_tests.rs:877` | test/비실행 | `let closes: [fn(&OwnershipGate); 2] = [OwnershipGate::uncertain, OwnershipGate::lost];` | ExecutionAuthority Prepare/Install/Hold | 기존 authority 소유자; R은 관측만 |
| `src/services/turn_lifecycle.rs:96` | source | `pub(crate) async fn stop_turn_preserving_queue(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:101` | source | `stop_turn_preserving_queue_with_cancel_event(health_registry, target, reason, true).await` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:104` | source | `pub(crate) async fn stop_turn_preserving_queue_without_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:109` | source | `stop_turn_preserving_queue_with_cancel_event(health_registry, target, reason, false).await` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:112` | source | `async fn stop_turn_preserving_queue_with_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:131` | source | `pub(crate) async fn force_kill_turn(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:137` | source | `force_kill_turn_with_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:156` | test/비실행 | `/// ['force_kill_turn'] for a caller holding its target's row: with no runtime to key the` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:166` | source | `force_kill_turn_with_cancel_event(health_registry, target, Some(row), reason, code, true).await` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:169` | source | `pub(crate) async fn force_kill_turn_without_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:176` | source | `force_kill_turn_with_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:187` | source | `async fn force_kill_turn_with_cancel_event(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:195` | source | `let verdict = force_kill_verdict(health_registry, target, row).await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:219` | source | `pub(crate) async fn force_kill_verdict(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:253` | test/비실행 | `/// ['force_kill_verdict'] for a sessions row read as stored: its raw provider and thread.` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:273` | source | `force_kill_verdict(registry, &target, Some(row)).await` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:276` | test/비실행 | `/// ['force_kill_turn'] of the target 'verdict' judged, run on that verdict.` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:277` | source | `pub(crate) async fn force_kill_turn_with_verdict(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:331` | source | `let judge = crate::services::discord::health::judge_provider_channel_stop;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:409` | source | `let stop = crate::services::discord::health::stop_channel_runtime;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:417` | source | `let stop = crate::services::discord::health::stop_judged_provider_channel;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:445` | source | `let hard_stop = crate::services::discord::health::stop_runtime_turn_preserving_watcher(` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:688` | test/비실행 | `super::stop_turn_preserving_queue(None, &target, "queue-api cancel_turn (preserve)")` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:1113` | test/비실행 | `let result = force_kill_turn(Some(&registry), &target, "p4a host guard", "p4a").await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:1149` | test/비실행 | `let verdict = force_kill_verdict(Some(&registry), &target, None).await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:1154` | test/비실행 | `let kill = force_kill_turn_with_verdict(Some(&registry), verdict, "moved on", "p4r");` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:1190` | test/비실행 | `let verdict = force_kill_verdict(Some(&registry), &target, None).await;` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_lifecycle.rs:1196` | test/비실행 | `let kill = force_kill_turn_with_verdict(Some(&registry), verdict, "marker", "p4r");` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_orchestrator.rs:1451` | test/비실행 | `/// between 'force_kill_turn_without_cancel_event' and this purge.` | Stop/Reset/Drain | 제외 L (#7651/기존 exact-host guard) |
| `src/services/turn_orchestrator/queued_head/v2.rs:398` | source | `pub(crate) fn grant_front(&mut self, front: &Front) -> Result<(), String> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2.rs:411` | source | `pub(crate) fn claim_turn(&mut self, message: u64, front: &Front) -> Option<Front> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2.rs:443` | source | `pub(crate) fn reserve_claimed(` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2.rs:506` | test/비실행 | `/// starts, and another claim keeps V1's order; its own front starts only through 'claim_turn'.` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:20` | source | `pub(crate) struct WriterLease(Vec<Arc<AtomicBool>>);` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:21` | source | `impl Drop for WriterLease {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:30` | source | `pub(crate) struct EffectPermit {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:32` | source | `lease: WriterLease,` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:35` | source | `impl EffectPermit {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:36` | source | `pub(crate) fn consume(self, key: &AttemptRef) -> Result<WriterLease, String> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:44` | source | `fn writer(target: &TargetIdentity) -> Result<WriterLease, String> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:50` | source | `fn writer_named(name: String) -> Result<WriterLease, String> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:66` | source | `Ok(WriterLease(vec![slot]))` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:82` | test/비실행 | `pub(crate) fn arm(&mut self, key: &AttemptRef, now: u64) -> Result<EffectPermit, String> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:89` | source | `) -> Result<EffectPermit, String> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:97` | source | `) -> Result<EffectPermit, String> {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:116` | source | `Ok(EffectPermit {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/attempt.rs:201` | source | `impl WriterLease {` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/migration_tests.rs:120` | test/비실행 | `handle.v2_admit_ticket(fresh, "after-upgrade".into()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/origin_tests.rs:45` | test/비실행 | `handle.v2_admit_ticket_with_context(original(channel, id), ticket, provenance(origin));` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/origin_tests.rs:335` | test/비실행 | `let settle = &#124;&#124; crate::services::discord::settle_v2_drops(Some(&pool), &handle, "claude", id);` | Terminal/Receipt/BootSettle | 기존 receipt 소유자; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/projection_tests.rs:394` | test/비실행 | `.v2_admit_ticket(item(100), "front-100".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/projection_tests.rs:418` | test/비실행 | `assert!(owner.grant_front(&other).is_err());` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/projection_tests.rs:419` | test/비실행 | `owner.grant_front(&front)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/projection_tests.rs:423` | test/비실행 | `owner.claim_turn(100, &front).is_none(),` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/projection_tests.rs:424` | test/비실행 | `owner.claim_turn(100, &front).is_none(),` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/projection_tests.rs:827` | test/비실행 | `owner.grant_front(&front)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/projection_tests.rs:838` | test/비실행 | `Ok(owner.claim_turn(front.1.message_id, &front).is_some())` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/requeue_tests.rs:44` | test/비실행 | `handle.v2_admit_ticket_with_context(item(channel, *id), ticket(*id), context);` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:775` | test/비실행 | `pub(crate) async fn v2_admit_ticket(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:794` | source | `pub(crate) async fn v2_resume_ticket(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:916` | source | `fn admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:974` | source | `pub(crate) async fn v2_admit_ticket_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:988` | source | `Some(owner) => owner.admit_with_context(&original, ticket, context),` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:49` | test/비실행 | `.v2_admit_ticket_with_context(original(), "ticket-run".into(), context.clone())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:54` | test/비실행 | `.v2_admit_ticket_with_context(original(), "ticket-run".into(), context.clone())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:68` | test/비실행 | `.v2_admit_ticket_with_context(original(), "ticket-run".into(), context)` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:88` | test/비실행 | `.v2_admit_ticket_with_context(original(), "ticket-run".into(), context.clone())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:97` | test/비실행 | `.v2_admit_ticket_with_context(input, "ticket-run".into(), value)` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:105` | test/비실행 | `.v2_admit_ticket(original(), "legacy".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:117` | test/비실행 | `.v2_admit_ticket_with_context(original(), "legacy".into(), json!({"run_id":"new"}))` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:130` | test/비실행 | `.v2_admit_ticket_with_context(original(), "settled".into(), context.clone())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:189` | test/비실행 | `handle.v2_admit_ticket_with_context(original.clone(), ticket.into(), context.clone());` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:272` | test/비실행 | `.admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:280` | test/비실행 | `.admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:291` | test/비실행 | `owner.grant_front(&front).unwrap();` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/context_tests.rs:333` | test/비실행 | `.admit_with_context(` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/front_tests.rs:30` | test/비실행 | `owner.grant_front(grant)?;` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/front_tests.rs:34` | test/비실행 | `Some(front) => owner.claim_turn(id, front).is_none(),` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/front_tests.rs:53` | test/비실행 | `let admitted = owned.1.v2_admit_ticket(original(id, body), run.into());` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/front_tests.rs:88` | test/비실행 | `let known = handle.v2_admit_ticket(original(a, "known A"), "run-a".into());` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/front_tests.rs:97` | test/비실행 | `let partial = handle.v2_admit_ticket(whole.clone(), "run-ab".into());` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/front_tests.rs:119` | test/비실행 | `let admitted = handle.v2_admit_ticket(original(id, body), run.into());` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/front_tests.rs:204` | test/비실행 | `let admitted = handle.v2_admit_ticket(original(id, "twin"), "run-t".into());` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:41` | test/비실행 | `owner.grant_front(&front).unwrap();` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:46` | test/비실행 | `assert!(owner.claim_turn(original.message_id, &front).is_some());` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:353` | test/비실행 | `handle.v2_admit_ticket(input.clone(), ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:357` | test/비실행 | `handle.v2_admit_ticket(input, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:387` | test/비실행 | `handle.v2_admit_ticket(input.clone(), ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:396` | test/비실행 | `handle.v2_admit_ticket(input.clone(), ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:419` | test/비실행 | `restarted.v2_admit_ticket(input, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:442` | test/비실행 | `handle.v2_admit_ticket(input, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:551` | test/비실행 | `handle.v2_admit_ticket(legacy, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:560` | test/비실행 | `handle.v2_admit_ticket(fresh, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:569` | test/비실행 | `handle.v2_admit_ticket(other, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:587` | test/비실행 | `handle.v2_admit_ticket(input.clone(), ticket.into()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:623` | test/비실행 | `handle.v2_admit_ticket(first, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:631` | test/비실행 | `handle.v2_admit_ticket(other.clone(), ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:660` | test/비실행 | `.v2_admit_ticket(with(source.clone(), json!({})), ticket())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:667` | test/비실행 | `.v2_admit_ticket(overridden, "run-7272-o".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:710` | test/비실행 | `.v2_admit_ticket(variant.clone(), ticket.clone())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:726` | test/비실행 | `handle.v2_admit_ticket(retried, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:730` | test/비실행 | `let refused = handle.v2_admit_ticket(unsettled, "run-7272-g".into()).await;` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:756` | test/비실행 | `handle.v2_admit_ticket(input.clone(), ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:791` | test/비실행 | `restarted.v2_admit_ticket(input, ticket()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:890` | test/비실행 | `.v2_admit_ticket(input.clone(), "run-t1".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:902` | test/비실행 | `handle.v2_admit_ticket(input.clone(), "run-t2".into()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:931` | test/비실행 | `handle.v2_admit_ticket(input.clone(), ticket).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:962` | test/비실행 | `.v2_admit_ticket(original(a, "known A"), "run-a".into())` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:974` | test/비실행 | `handle.v2_admit_ticket(whole.clone(), "run-ab".into()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission/tests.rs:1005` | test/비실행 | `owned.1.v2_admit_ticket(input.clone(), ticket.into()).await,` | Admit/Claim/Wake | A/H 뒤 기존 actor; 직접 노출 없음 |
| `src/services/turn_orchestrator/queued_head/v2/start_failure_tests.rs:9` | test/비실행 | `owner.grant_front(&front).unwrap();` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/start_failure_tests.rs:10` | test/비실행 | `owner.claim_turn(front.1.message_id, &front).unwrap();` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
| `src/services/turn_orchestrator/queued_head/v2/start_failure_tests.rs:113` | test/비실행 | `.reserve_claimed((&front, n), attempt, payload)` | Grant/Claim/Effect | 내부 actor seal; 외부 port 없음 |
