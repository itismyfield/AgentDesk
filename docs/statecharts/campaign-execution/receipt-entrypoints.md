# S4-17 receipt·lock·결정 진입점 전수표

기준 `0051e898e4e43012c232c244a521c5a280765fb8`. 이 문서는 소스 읽기/rg만으로 작성했다. production 변경, cargo, Rust 시험, 운영 DB/설정/스토어 접근은 0이다.

모델 결과와 반례 대조는 [receipt.report.md](receipt.report.md), 기존 host 실행 범위는 [verification.report.md](verification.report.md)를 함께 읽는다. 아래 좌표는 모두 고정 source SHA 기준이다.

## 1. 현재 transaction/lock/receipt 순서 (모든 production 변경 진입점 공통)

`src/db/server_campaigns/mod.rs`:

|순서|좌표|계약|
|---|---|---|
|1|`:142`|pool.begin|
|2|`:156`|id/key identity validation; invalid이면 Err, tx drop rollback|
|3|`:159`|create = `create:` + blake3(serde(id,input)); command = `command:` + command_fingerprint(command); observed_at/key는 fingerprint에 없음|
|4|`:166`|server_campaign_keys INSERT ON CONFLICT DO NOTHING|
|5|`:172`|같은 campaign key row FOR UPDATE; create와 execute를 함께 직렬화|
|6|`:177`|현재 definition과 join한 existing run FOR UPDATE OF r. key row lock 다음|
|7|`:181`|동일 campaign/key receipt SELECT. 다른 fingerprint는 :190 IdempotencyConflict; 같으면 :193~205 저장 status/body를 그대로 반환|
|8|`:207`|read::get_in, 같은 tx에서 state 읽음. `read.rs:85`; projection SQL `read.rs:42`는 definition/run joined statement|
|9|`:210`|BeginReview + run 존재일 때 evidence_store::sealed(:213); evidence_store.rs:70,78의 evidence row FOR UPDATE. 저장 seal 없음/confirm_retry 실패는 StaleBinding(:216,219)|
|10|`:218`|create/command 결정 match. create None만 control::create_definition(:220), create Some는 AlreadyExists(:226), command Some는 control::decide_with_receipt(...,None)(:227), command None는 NotFound(:236)|
|11|`:240`|write::persist; events.empty이면 write.rs:57~59에서 no-op. run/attempt/action/definition/history/event의 모든 write는 같은 tx|
|12|`:242`|decision state에 run 있으면 모든 EvidenceWrite를 :244 write_evidence. Seal는 mod.rs:79~83 record, Admitted는 :85~92 record+admission decide|
|13|`:252`|render(outcome), 실패 시 모든 state/evidence write rollback|
|14|`:253`|server_campaign_receipts INSERT(status/body 포함); 실패 시 모든 앞선 write rollback|
|15|`:144`|transact가 commit; reply는 :145 뒤 노출. apply에서 ? 오류는 tx drop rollback|

receipt replay/충돌은 state load·seal load·stale checks·kernel보다 먼저다. replay도 기존 run lock까지 획득하고 commit한다. Err/NotFound/no-op도 성공한 DB transaction이라면 receipt를 1개 저장하지만 상태/event 효과는 0이다. 따라서 모델의 ‘같은 입력 효과 정확히 1’은 **성공한 변경 입력의 committed 효과 1**, 그 외는 **최대 1 durable receipt와 저장 outcome에 대응하는 상태 효과 0**으로 나눈다. ‘모든 요청이 state effect 1’은 현재 코드 계약이 아니다.

## 2. Production 외부 호출자 전수표

|진입점|실제 호출 좌표|request/관측|transaction·lock|
|---|---|---|---|
|POST /server-campaigns|`src/server/routes/domains/admin.rs:36` → `src/server/routes/server_campaigns/mod.rs:114,122`|DefinitionInput/id/key; observed_at=Utc::now(:127); protocol::created renderer|DB create(:105)→transact(:134), 위 공통 순서|
|PUT /server-campaigns/{id}/definition|`src/server/routes/domains/admin.rs:41` → `src/server/routes/server_campaigns/mod.rs:135,154`|EditDefinition(:147); observed_at=Utc::now(:146); protocol::edited|DB execute(:117)→동일 순서|
|POST /server-campaigns/{id}/commands|`src/server/routes/domains/admin.rs:45` → `src/server/routes/server_campaigns/mod.rs:160,174`|PublicCommand.into_domain(:171); observed_at=Utc::now(:170); protocol::commanded|DB execute→동일 순서|
|GET list|`src/server/routes/server_campaigns/mod.rs:87`→store::list; `read.rs:97`|coherent joined projection; view rendering|no explicit tx/row lock, one SQL projection|
|GET detail|`src/server/routes/server_campaigns/mod.rs:107`→store::get; `read.rs:76`|same coherent projection|no explicit tx/row lock|
|GET events|`src/server/routes/server_campaigns/mod.rs:197,202`→store::get/events; `read.rs:124`|events paginated|separate reads, no mutating decision|

production DB create/execute 호출자는 위 3곳뿐이다. 전체 src의 server_campaigns/control 심벌 검색에서도 다른 production alias 호출자는 없었다. HTTP fixture 직접 호출은 시험 절에 열거했다.

## 3. Kernel 내부 command/transition 진입점 전수표

모든 항목은 `control::decide` (`src/services/campaign_execution/control.rs:286`)가 state를 clone(:290)한 뒤 순수하게 결정한다. 직접 tx/lock/DB/now를 소유하지 않으며 DB를 거치면 위 같은 transaction lock 안에서 실행된다. 내부 domain command는 HTTP DTO로 공개하지 않고 등록된 production runtime 호출자도 없다.

|Command군|dispatch/callee 좌표|관측·효과|
|---|---|---|
|create_definition|`control.rs:252` (DB 호출 :220)|definition validate, Prepared/run=None, event_sequence=1 (:269~282)|
|EditDefinition|`control.rs:292`|revision/edit_guard(:297~299), superseded evidence·definition revision 갱신|
|Start|`control.rs:335`|revision 검증 후 ExecutionUnavailable(:337)|
|Resume / PrepareRetry|`control.rs:339,343`|revision/run target 검증 후 ExecutionUnavailable(:348~349). failure budget과 옛 work 유지|
|RequestStop|`control.rs:351`|stop guard, FinishStage→Immediate만 escalate(:362~369), finish_grants(:384), provisional_finish_actions(:412), unstarted cancel(:415), attempt interrupt(:428), Stopping(:441). 같은/약한 stop은 unchanged(:367)|
|PlanAction|`control.rs:448`|validate_new_start(:449), general action만 추가|
|AdvanceAction|`control.rs:472`|validate_new_start(:477), Planned→Claimed→StartPending만 허용(:482~489). verification phase 불허|
|ObserveAction|`control.rs:494`→observe_action(:767)|Started/NotStarted/Reconciled/Unknown, late facts. late launch는 grant_late_start(:878), unsent는 revoke_unlaunched(:883)|
|ObserveSession|`control.rs:510`|exact identity의 Alive/Gone/Unknown 기록; unseen이면 receipt가 있는 action binding 필요(:526)|
|AcknowledgeFence|`control.rs:545`|run/current version/target/startpending 검사; ack 추가(:576)|
|ConfirmStopFence|`control.rs:585`|모든 target ACK 및 unreconciled 없음; once(:607~610)|
|ConfirmStopped|`control.rs:613`|confirmed fence + quiescent; Stopped(:623)|
|AssignWork|`control.rs:626,630`→`stage_fsm.rs:95`|Work attempt/pin/모든 verification slot/launch intent를 한 decision에 등록|
|BeginReview|`control.rs:640,646`→`stage_fsm.rs:274`|current epoch/seal/prerequisite의 stage handover. DB가 stored seal 일치를 먼저 확인|
|AdvanceVerificationPhase|`control.rs:657,662`→`stage_fsm.rs:492`|current holder 검사(`validate_verification_start`:395, 호출 :504)|
|InterruptNode|`control.rs:672,678`→`stage_fsm.rs:578`|Running/Stopping, exact run/epoch/session. 같은 interruption이면 no-op(:600~604), unstarted revoke|
|AcceptStageResult|`control.rs:690,695`→`stage_fsm.rs:652`|stored V 및 review gate로 평가. Wait/Reject는 Verifying(:799~800), dead finishing은 UserInterrupted(:786~797), Pass/QualityFail만 finalize(:802)|
|SealCandidate|`control.rs:706,715`→`verification_commands.rs:47,54,103`|request의 workspace observation 및 evidence로 seal 결정. EvidenceWrite::Admitted+Seal(:156~162)|
|BindVerificationSubject|`control.rs:707`→`verification_commands.rs:59,169`|current holder(:180), same slot value는 no-op(:183~184)|
|BindVerificationInput|`control.rs:708`→`verification_commands.rs:69,169`|위와 동일, fill-once phase input|
|BindVerificationPlan|`control.rs:709`→`verification_commands.rs:77,169`|위와 동일, attempt execution spec의 plan|
|RecordVerificationPhase|`control.rs:710`→`verification_commands.rs:89,209`|gate가 이미 실행한 Started/NotStarted/Indeterminate/Resolved/Terminated/Finished 사실. current holder 불필요, stop 후 facts도 유지|
|SettleVerification|`control.rs:711`→`verification_commands.rs:94,377`|collection/cancellation이 있으면 no-op(:392), collect 또는 cancel, EvidenceWrite::Admitted(:451)|

추가 내부 호출 폐쇄:

- `stage_fsm::finish_grants`: control.rs:384 → stage_fsm.rs:856.
- `provisional_finish_actions`: control.rs:412 → stage_fsm.rs:861.
- `grant_late_start`: control.rs:878 → stage_fsm.rs:871.
- `revoke_unlaunched`: control.rs:883 → stage_fsm.rs:889 → revoke_unstarted(:901).
- `revoke_unstarted`: stage_fsm.rs:538, interrupt_node(:628), accept_stage_result(:792,822), revoke_unlaunched(:901). 새 authority/cleanup은 추가하지 않는다.
- `validate_verification_start`: stage_fsm.rs:395; stage_fsm.rs:504, verification_commands.rs:180,200, effect_envelope.rs:68이 모든 production 호출자다.
- failure budget의 유일한 production 호출은 `stage_fsm.rs:810` → `failure_budget.rs:13` record_quality_failure다. 확인된 QualityFail만 통과하며 (series,failed_cycle) 중복은 AlreadyCounted(:19~21), quality_failures+1(:24), quality_repairs 상한이면 Failed/Exhausted(:25~27), 나머지는 Pending/repair_cycles+1(:29~31).

## 4. 인접 상태 소비자와 재시작의 의미

|경로|좌표|이번 변경과의 관계|
|---|---|---|
|일시정지/취소에 대응하는 기존 경로|`src/server/routes/server_campaigns/dto.rs:34~52`는 Start/Stop/Resume/Retry뿐; Stop 매핑 `src/server/routes/server_campaigns/dto.rs:80~88`; StopMode=`Immediate,FinishStage` `domain/campaign/model.rs:38`|Pause/Cancel이라는 campaign command는 없다. Immediate stop은 미시작 phase cancel과 UserInterrupted, FinishStage는 frozen check만 계속 진행한다. 새 pause/cancel 상태·권한을 추가하지 않는다|
|재개/재시도|`src/server/routes/server_campaigns/dto.rs:89,96`→control.rs:339~349|현재 ExecutionUnavailable. 상태나 budget을 리셋하지 않는 계약을 유지|
|view guard|`view.rs:188~208`→control::edit_guard(:199)/stop_guard(:201); readiness `view.rs:268`→control.rs:41|list/detail에서 같은 state를 표시하는 read-only pure 소비. lock 밖 표시 결과를 mutation 허가증으로 쓰지 않음|
|general effect envelope|`effect_envelope.rs:117,123`→control::validate_new_start; state/action lookup(:124~130)|기존 dormant host adapter용. DB mutation을 호출하지 않는다. command kernel의 validate_new_start 본체는 service 소유|
|verification envelope|`effect_envelope.rs:46,57,68`→stage_fsm::validate_verification_start|같은 state를 소비하는 host 승인값. 최종 host gate의 fence check는 별도 책임|
|host fence projection|`effect_envelope.rs:156`|run control_version/node_epoch/stop frozen grant를 host로 전달|
|host journal 복원|`host_gate.rs:342` EffectGate::recover_journal, :377~385 Initiating→Indeterminate, :387~395 recovered unfinished/undelivered 집합|DB receipt와 다른 journal. campaign runtime은 dormant (`lib.rs:286`). 전체 src 비시험 검색에서 recover_journal/recovery_snapshot/termination_observation의 production 호출자는 정의 외에 0. 이번 변경에서 자동 recovery 실행을 추가하지 않음|
|host 재전달|`host_gate.rs:761` recovery_snapshot; :780~785 Finished 중 미delivered만 재offer; mark_delivered :745|외부 효과 exactly once는 기존 CampaignVerification 담당. DB receipt/commit 모델의 exactly once와 구분|
|DB 재연결/서버 재시작 대표|`read.rs:76,85` persisted projection + `mod.rs:181` persisted receipt; fixture reconnect `storage_tests.rs:94~96`|in-memory 결정 상태를 유지하지 않는다. commit 전 crash는 PG transaction rollback, commit 후 응답 loss는 receipt replay. 실제 process restart 실험은 이번에 실행하지 않음|

`src/services/campaign_execution/recovery.rs` (없음). server startup/worker registry에도 server campaign 복구 loop가 없다. root 공개는 dormant adapter뿐 (`lib.rs:286~293`). 옛 `src/db/campaigns.rs`의 legacy campaign CRUD는 별도 경로이며 이번에 바꾸지 않는다.

### legacy ledger의 pause/resume/cancel은 별도 상태·테이블

|구분|진입점/잠금/저장 좌표|S4-17 관계|
|---|---|---|
|legacy ledger paused/active/cancelled|`src/db/campaigns.rs:14~20` CampaignStatus = Planned/Active/Paused/Completed/Cancelled; HTTP `src/server/routes/campaigns.rs:210` replace → :273 campaigns::replace; DB `src/db/campaigns.rs:545` replace → :557 begin → :559 campaigns row FOR UPDATE → :566 revision CAS → :569 checkpoint → :570 store_revision → :571 commit|pause/resume/cancel은 status를 Paused/Active/Cancelled로 PUT하는 기존 ledger CRUD. `campaigns`/`campaign_revisions` 테이블(:583/:590)에 저장하며 server_campaign control/receipt를 사용하지 않음|
|legacy issue sync|`src/db/campaigns/issue_sync.rs:163` apply_issue_facts → :199 campaigns FOR UPDATE → :207 Active 검사 → :221 commit|legacy ledger만 갱신. server_campaigns kernel/receipt와 독립|
|legacy autoqueue handoff|`src/server/routes/campaigns.rs:280~294` saved_with_handoff, :281 Active+auto_queue일 때만 :284 hand_off_ready_nodes_pg|legacy 저장 commit 뒤 handoff. server_campaigns Start/Resume/Retry의 ExecutionUnavailable을 풀어주는 경로가 아님|

따라서 legacy CampaignStatus와 server CampaignControl을 한 변수의 값으로 합치지 않는다. 이번 모델에서 legacy 경로를 범위 밖으로 둬도 server_campaign_keys lock/receipt 원자성의 원인이 될 공유 table/state는 없다. 기존 동등성 `src/db/server_campaigns/storage_tests.rs:767`이 legacy CRUD/CAS/issue_sync 불변을 확인한다.

### 옛 저장 행과 receipt 복원

- `src/db/server_campaigns/read.rs:61~73`은 저장 definition/control/run/nodes/sequence를 그대로 복원한다. 별도 restore setter나 in-memory NodeRuntime::default로 덮지 않는다. `NodeRuntime::default` (`src/domain/campaign/model.rs:332`)는 create의 새 노드(:267) 및 edit의 새 노드(:324)에만 쓰인다.
- 옛 Run JSON에 없는 `verifications`, `prerequisites`, `fences`, `seals`, `environments`는 `src/domain/campaign/model.rs:284,287,289,292,295`의 serde default로 빈 map이다. 옛 Attempt.execution은 :206 default로 None, StopIntent.finish_grants는 :259 default로 빈 Vec다. 그 외 quality_failures/repair_cycles/series/current_attempt 등 저장 필드는 바꾸지 않는다. 모든 숫자 필드의 임의 default 수용을 새로 만들지 않는다.
- NodeFence의 epoch/interruption/counted_failure는 :315~321의 기존 구조와 default를 유지한다. 빈 fences map이면 `src/services/campaign_execution/stage_fsm.rs:48`의 기존 map_or/default fence를 계속 쓴다. 옛 map이 빈다는 이유로 저장 failure budget을 reset하지 않는다.
- 옛 Rejection JSON의 definition_error 부재는 `src/domain/campaign/model.rs:126` default로 None. 기존 `src/services/campaign_execution/model_tests.rs:215~249`가 옛 receipt outcome JSON을 왕복한다. `CommandOutcome` enum 표현과 response_status/body는 같은 DB 정의(:40~58)를 유지한다.
- `src/services/campaign_execution/model_tests.rs:253`은 command bytes/fingerprint, :268은 WorkspaceObservation bytes를 고정한다. receipt replay는 [base의 mod.rs 193~205행](https://github.com/itismyfield/AgentDesk/blob/0051e898e4e43012c232c244a521c5a280765fb8/src/db/server_campaigns/mod.rs#L193-L205)의 saved status/body를 사용해 새 renderer/현재 state/default로 다시 만들지 않는다. 기존 receipt 테이블 스키마 `migrations/postgres/0161_server_campaigns.sql:99~105`에 대한 schema/migration 변경은 없다.
- 다음 구현 레그에서 추가할 옛 Run fixture는 새 default map/optional 필드만 JSON에서 뺀 행을 읽고 budget/attempt/fence·기존 receipt 재생을 확인하면 된다. 이는 현재 main도 통과해야 하는 호환 동등성 시험이며 main 버그 RED로 세지 않는다.

## 5. 모델 값 범위와 코드의 연결

|추상값/행동|코드 근거|
|---|---|
|Caller={1,2}|같은 campaign key의 두 동시 create/execute는 storage_tests.rs:153~155,239~241. 개수 2는 최소 경쟁 대표이며 실제 caller 수 상한이 아님|
|phase=begin/locked/receipt/read/decided/written/committed/rolledback/replay/conflict|mod.rs:142,172,181,207,218,240,253,144 및 ? rollback|
|receipt=None / same fingerprint / other fingerprint|mod.rs:181~205. key row lock 안에서 기존 receipt 읽음|
|outcome=Changed / Noop / Rejected / NotFound|mod.rs:238~250; control.rs:732~744; write.rs:57~59|
|campaign=absent / present (Prepared/Running/Stopping/Stopped/Succeeded)|read.rs:85 optional + src/domain/campaign/model.rs:31~37. 현재 production create는 초기 Prepared, active run은 옛 저장 행/fixture로도 존재 가능|
|request=Create / Edit / Stop / Resume / Retry / Internal|mod.rs:98~102, dto.rs:34~52, domain model.rs:364~474. 내부 kind는 위 모든 command 표에서 opaque 대표 가능|
|snapshot revision/control_version/event_sequence|model.rs:351~359; Run :276~279. decision은 lock 후 read한 고정 state 사용|
|seal=present-match / missing-or-mismatch|mod.rs:210~216; evidence_store.rs:88~101|
|rollback/error injection|storage_tests.rs:398~413 explicit rollback; failed renderer :415~423; receipt trigger failure :425~460|
|restart|storage_tests.rs:94~96 new pool; receipt/state durable read. host restart는 기존 verification model 담당이며 이번 DB 모델은 그 결과를 외부 사실로 취급|

## 6. 대상 시험 (본 레그 실행 0)

### 기존 main에서 통과할 동등성 시험

- DB receipt/concurrency: `storage_tests.rs:150` concurrent_create_same_key_and_lost_response_replay_before_stale_pg; :201 concurrent_create_different_body_same_key_conflicts_pg; :235 concurrent_commands_share_receipt_and_cas_loser_is_durable_pg; :307 failed_create_and_missing_target_receipts_survive_new_pool_pg. 
- DB atomicity/lock: :388 rollback_and_receipt_insert_failure_leave_no_partial_action_or_event_pg; :470 run_lock_holds_command_until_commit_and_replays_response_loss_pg. 
- 旧state/immutability/budget: :513 prune_preserves_execution_failure_evidence_pins_actions_and_receipts_pg; :645 immutable_attempt_binding_and_action_target_mismatch_roll_back_pg; :717 resume_and_retry_preserve_failure_budget_and_create_no_legacy_work_pg; :767 execution_stays_unavailable_and_legacy_crud_cas_issue_sync_unchanged_pg. 
- Stage DB: `stage_store_tests.rs:21` begin_review_commits_once_survives_restart_and_cites_only_the_stored_seal_pg; :65 stored_v_origin_filled_slots_prerequisites_and_pins_cannot_be_rewritten_pg. 
- Verification DB: `verification_store_tests.rs:366` work_result_reaches_review_through_store_commands_only_pg; :450 environment_is_stored_once_and_survives_restart_pg; :720 seal_and_settlement_commit_evidence_and_state_together_pg. 추가 관련 :488 generic_observation_is_refused_for_check_phases_pg; :551 gate_started_phase_is_recorded_after_fence_ack_pg; :586 finish_stage_closes_attempt_whose_check_was_interrupted_pg; :632 settle_closes_partly_cancelled_check_after_immediate_stop_pg; :664 late_launch_receipt_keeps_finish_scope_pg. 
- Recovery DB: `recovery_tests.rs:35,109,213,324,357,528,592,642,702`. 특히 :592 fake_gate_start_first_recovers_receipt_and_unknown_session_after_pool_restart_pg, :642 fake_gate_crash_after_start_before_receipt_keeps_unknown_and_never_repeats_pg. 
- HTTP bytes: `src/server/routes/server_campaigns/http_tests.rs:310` create_edit_command_receipts_replay_exact_status_body_after_new_pool_pg; :632 invalid_definition_returns_400_and_durable_failure_receipt_pg; :987 http_stop_targets_run_definition_after_events_and_replays_original_receipt_pg; :1067 public_execution_is_unavailable_without_legacy_queue_or_card_effects_pg. 
- Pure kernel: `control_tests.rs:850` receipt_replay_and_body_conflict_precede_stale_checks_and_emit_no_events; :972 execution_is_unavailable_without_a_production_runtime_caller; all control/stage_fsm tests. `failure_budget_tests.rs:12` three_repairs_are_allowed_and_each_cycle_counts_once. 

이들은 이동 전후 행동을 지키는 회귀 시험이며, 수정 전 main을 RED로 만드는 버그 재현이라고 부르지 않는다.

### 수정 전 RED 가능한 항목

1. 계층 책임 시험/공식 module_layers 재측정: DB→service control import/path가 있는 main은 잔존 의존이 있다. 이동 후 0을 검증하는 구조 수용 검사는 RED→GREEN으로 구분할 수 있다. 행동 버그 RED와 명확히 구분한다.
2. opaque prepared/apply 새 API misuse의 compile-fail/API 표면 검사: 새 API가 없는 main에서는 당연히 컴파일할 수 있으나 버그 RED로 세지 않는다. lock 밖 snapshot/다른 tx apply/같은 token의 이중 apply를 허용하지 않는 설계를 타입과 전용 mutation 시험으로 확인한다.
3. 구현 후 mutant로 lock/read 순서를 뒤바꾸거나, receipt read를 stale check 뒤로 옮기거나, receipt INSERT를 다른 tx로 분리했을 때 위 기존 PG 시험이 RED가 되는지 확인한다. 지금 main의 버그라고 주장하지 않는다. 상태 차트에도 같은 3 mutant를 주입할 수 있다.

## 7. 다음 구현 레그 시험 연결

- `storage_tests.rs:27~43`의 create/execute helper를 새 service facade로 연결하면 stage/recovery/verification store 시험 대부분이 새 경로를 거친다.
- HTTP fixture의 직접 DB 호출은 `src/server/routes/server_campaigns/http_tests.rs:326,1002`, routes 본체 호출은  :122/:154/:174. fixture도 새 service 경유로 적응해야 한다.
- private apply 직접 시험은  `storage_tests.rs:400,476` 의 두 곳이다. 새 prepare/apply를 **같은 열린 tx에서** 통과시키고 기존 명시 rollback/lock wait timeout/commit의 의미를 지킨다. DB 내 cfg(test) service 재수출은 대안이며 production 역의존 0과 구분한다.
- serde/wire는  `src/domain/campaign/model.rs:126` Rejection.definition_error default, :206 Attempt.execution default, :259 StopIntent.finish_grants default, :284~296 Run maps defaults를 바꾸지 않는다. receipt schema  `migrations/postgres/0161_server_campaigns.sql:99~105`는 저장 response 필수이며 renderer 재실행이나 행 재작성을 하지 않는다.
