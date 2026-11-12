# Outbox 진입점 전수표

좌표 기준 `92c480f00afc91d795607e4d15bf992d99a4f8c4` (S13 완료, 2026-11-12). 검색 모집단·원시 로그·정적 일치 행 수는 설계 기준 `5c3ed37cddc80036fa8a274419b0219f49926704`의 집계를 유지한다. 운영 설정·운영 DB는 열지 않았다. 함수·cfg(test)·입력/tx는 해당 소스와 대조했으며 숫자는 호출/실행 횟수가 아니다.

## 검색 모집단

|검색|원시 일치 행|선언/주석 제외 production|시험 fixture|
|---|---:|---:|---:|
|enqueue/stage/lifecycle/session/validate 함수 정규식|184|66|91|
|stage_held / replay enqueue_on_tx / validate_send_source_for / enqueue_plain_on_tx 정규식|37|8|26|
|`enqueue` 별칭(custody_notice.rs:10→:105)|1|1|0|
|`INSERT INTO message_outbox`(대소문자 무시, Rust 파일)|12|8|4|
|`OutboxMessage\s*\{`|79|30|49|

주 검색 184행에는 선언 27행이 포함된다. 추가 검색 37행에는 선언 3행이 포함된다. source validation wrapper의 `src/services/message_outbox.rs:56`는 추가 검색에만 든다. `message_outbox_redrive_audit`·`message_outbox_circuit_authority` INSERT는 message_outbox 행이 아니므로 위 INSERT 집계에 포함하지 않는다. source 목록/코드 주석은 호출로 세지 않는다. 별칭 전수 검색에서 발견한 production outbox enqueue alias는 custody :10 하나이며 실제 호출 `src/services/discord/inflight/removal/custody_notice.rs:105`를 추가했다.

원시 로그: `/home/adk/adk/lane-runs/i7353/logs/design-s41314-1/`의 `i7353-d1314-calls.txt`, `extra-calls.git-grep.txt`, `i7353-d1314-inserts.txt`, `dto.git-grep.txt`, `inventory-annotated.json`. 정규식/범위는 `commands.md`에 고정한다.

## INSERT/검증 경로의 관측 순서

V=source 검증(일반 경로의 live known-agent 또는 replay hold의 정적 허용 검사); D=eligible display의 provider SQL와 행별 live default; C=새 행 INSERT 뒤 config membership/producer/task-local census. D와 C는 서로 다른 시점이다. 해당 eligibility/attachment 분기에서 생략되는 관측은 억지로 수행하지 않는다.

|INSERT 좌표 / 경로|tx 시작/소유|V / D / C 시점과 결과|입력 출처|
|---|---|---|---|
|`src/services/message_outbox.rs:280` persistent/origin-on-tx|외부 tx 또는 pool wrapper :235|V :267(tx 안); pool은 :234에서도 V. D :278(tx 안); C :305, 새 행일 때만. 충돌은 기존 id/메타데이터 유지|상위 producer DTO/artifact|
|`src/services/message_outbox.rs:378` exact/origin|:376|V :363→cancel :365→D :369(pool acquire, tx 전)→begin :376→INSERT→새 행 C :405→commit :408|headless 및 generic exact DTO, exact key·원문 session/nonce|
|`src/services/message_outbox.rs:561` TTL-on-tx|외부 tx 또는 :472|pool V :430→duplicate :434→cancel :460→begin; core V :543→expired-key UPDATE :557→D :559→INSERT→새 행 C :592→commit :482|generic DTO/TTL/attachment/origin|
|`src/services/message_outbox.rs:660` held stage|:655|V :640(tx 전); key release :656→D :658(tx 안)→INSERT held→새 행 C :685→충돌 기존 id 조회 :691→commit :701|legacy circuit 또는 generic stage|
|`src/services/message_outbox.rs:852` plain/completion|외부 tx; 아래 완료 폐쇄 포함|V :840→D :844→INSERT→C :869→savepoint store :870; 모두 같은 tx. S14a 뒤 조립: `enqueue_session_notice_pg_on_tx`(`src/services/message_outbox/session_notice.rs:174`, explicit_completion=true) → `message_outbox::enqueue_plain_on_tx`(`src/services/message_outbox.rs:836`, V·D) → `plain::insert_on_tx`(`src/services/message_outbox/plain.rs:10`, 정규화·INSERT·C)|일반 plain producer / 자동큐 system 완료|
|`src/services/message_outbox.rs:922` lifecycle|:905|V :881→중복조회 :885(tx 전); key release :906→D :907(정규화한 reason/session DTO)→INSERT→새 행 C :943→commit :946|lifecycle 고정 source|
|`src/db/replay_disposition/hold_notice/outbox.rs:25` replay hold|`src/db/replay_disposition/write/hold_notice.rs:21` 또는 `:128`|V `src/db/replay_disposition/hold_notice/outbox.rs:17` tx 안(`src/domain/outbound/outbox/source.rs:141`); session 정규화 호출 `src/db/replay_disposition/hold_notice/outbox.rs:22`→`src/domain/outbound/outbox/identity.rs:45`; 동적 known-agent/D/C 없음. 영구 dedupe와 payload 동일성 조건 `src/db/replay_disposition/hold_notice/outbox.rs:31` 유지|replay lifecycle 상수 DTO, routing bot/session|
|`src/services/message_outbox_circuit_authority.rs:317` stamped held|:299|V :281(tx 전)→channel/owner/authority fence :300~307→expired-held delete :309→INSERT→commit :346. D/C 없음|circuit producer stall_watchdog DTO|

직접 source 검증 호출: `src/services/message_outbox.rs` **:234, :267, :363, :430, :543, :640, :804, :840, :881**, replay `src/db/replay_disposition/hold_notice/outbox.rs:17`(정적 허용 검사), `src/services/message_outbox_circuit_authority.rs:281`, redrive `src/services/message_outbox_recovery.rs:67`. 마지막은 새 INSERT 없이 row 상태를 바꾸는 운영자 명령이다. validate_outbox_source 본체는 `src/services/message_outbox.rs:55`→`src/services/discord/outbound/source_registry.rs:65`/`:71`이며 direct-send gate `send_gate.rs:447`도 아래에 분리했다. 정적 표·허용 판단의 정본은 `src/domain/outbound/outbox/source.rs:57`/`:135`이다.

## Production 호출 행 전체

모든 행의 입력 출처는 source label의 작성자와 content의 작성자를 구별한다. 사용자/에이전트 본문이 들어간다고 source label까지 신뢰하는 것으로 해석하지 않는다. pool API는 그 함수가 소유한 tx에 들어가며 호출자가 이미 다른 tx를 열었더라도 그 tx로 자동 합류하지 않는다.

|호출 좌표 / caller|호출 transaction (시작 좌표 또는 상속 인수)|관측 시점|입력 출처|
|---|---|---|---|
|`src/services/auto_queue/transactions/completion.rs:65` (`enqueue_completion_notices`)|상속 tx(:49); §완료 호출 폐쇄 표의 root/인수|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|자동큐 lifecycle 완료; run/agent/entry SQL에서 target·count, 서비스 고정 system/notify|
|`src/db/replay_disposition/hold_notice/outbox.rs:17` (`enqueue_on_tx`)|상속 tx: write/hold_notice.rs:21 또는 :128|source 검증 tx 안; designation/census 없음|replay 보류·복구 두 DB 호출만; source는 lifecycle_notifier 상수|
|`src/db/replay_disposition/write/hold_notice.rs:78` (`withhold_and_notify`)|`src/db/replay_disposition/write/hold_notice.rs:21`|tx 안(잠긴 receipt/CAS와 함께); source만|replay lifecycle; 고정 안내문, routing bot/session은 상위 복구/attempt 서비스|
|`src/db/replay_disposition/write/hold_notice.rs:231` (`recover_hold_notice`)|`src/db/replay_disposition/write/hold_notice.rs:128`|tx 안(잠긴 receipt/CAS와 함께); source만|replay lifecycle; 고정 안내문, routing bot/session은 상위 복구/attempt 서비스|
|`src/engine/ops/message_ops.rs:62` (`queue_message`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|PolicyEngine JS message.queue 명령; target/content/bot/source 인수(에이전트/업무 정책 입력)|
|`src/github/sync.rs:1456` (`enqueue_terminal_open_alert_pg`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|GitHub 동기화 시스템; 외부 issue/card 정보를 안내문으로 포맷; github_sync|
|`src/server/routes/imessage_input_notice.rs:98` (`echo`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|사용자 iMessage 입력의 echo; 서비스 고정 imessage_input_notice|
|`src/server/routes/message_outbox.rs:171` (`enqueue_monitor_alert`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|monitor-alert API 명령(에이전트/운영 도구); 서비스 고정 auto-queue-monitor|
|`src/services/claude/fresh_notice.rs:45` (`announce`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|Claude 새 세션 lifecycle; lifecycle_notifier|
|`src/services/cluster/intake_router_hook/owner_record/event.rs:167` (`reserve_on_tx`)|상속 tx(:81) ← custody.rs:153 → :230/:270|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|cluster owner 변경; 서비스 SOURCE=routing_transition|
|`src/services/cluster/intake_router_hook/owner_record/output.rs:133` (`enqueue_on_tx`)|상속 tx(:129); Scope acquire가 output.rs:62에서 시작|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 producer의 OutboxMessage·artifact 전달; 내용/동적 source를 새로 만들지 않음|
|`src/services/cluster/intake_router_hook/owner_record/plan.rs:204` (`release_held`)|`src/services/cluster/intake_router_hook/owner_record/plan.rs:192`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|cluster 보류 plan 해제; lifecycle_notifier|
|`src/services/codex_tui/input/submission.rs:67` (`enqueue_draft_notice`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|Codex 제출/사용자 초안 거절 lifecycle; lifecycle_notifier|
|`src/services/discord/health/recovery/stall_alert.rs:181` (`enqueue_stall_page`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|stall watchdog 시스템; stall_watchdog|
|`src/services/discord/inflight/removal/custody_notice.rs:105` (`enqueue_custody_notices`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|boot custody 복구 안내; boot_custody_notice|
|`src/services/discord/mcp_credential_watcher.rs:470` (`broadcast_credential_change`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|credential 변경 lifecycle; lifecycle_notifier|
|`src/services/discord/outbound/send_gate.rs:447` (`is_allowed_send_source_for`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|송신 시점; SQL tx 밖|공유 direct-send 승인 gate; outbox INSERT caller 아님; 요청 source/caller 입력|
|`src/services/discord/queue_io/retained_notice.rs:40` (`enqueue`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|retained 사용자 입력·초안 복구 안내; retained_input_notice|
|`src/services/discord/queue_io/retained_notice.rs:67` (`draft_unproven`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|retained 사용자 입력·초안 복구 안내; retained_input_notice|
|`src/services/discord/queue_io/retained_notice.rs:96` (`carried_unverified_closed`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|retained 사용자 입력·초안 복구 안내; retained_input_notice|
|`src/services/discord/queue_overflow_dlq.rs:110` (`maybe_notify_orphan_queue_overflow`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|queue overflow 시스템; queue_overflow_notice|
|`src/services/discord/recovery_engine/manual_rebind/adoption.rs:453` (`announce_adopt_fence_forward`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|운영자 rebind 명령 결과; adopt_fence_forward_notice|
|`src/services/discord/relay_recovery_circuit_alert_producer.rs:55` (`legacy_enqueue`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|circuit 시스템 안내; stall_watchdog; legacy/stamped 분기|
|`src/services/discord/relay_recovery_circuit_alert_producer.rs:131` (`enqueue`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|circuit 시스템 안내; stall_watchdog; legacy/stamped 분기|
|`src/services/discord/router/message_handler/goal_lifecycle.rs:149` (`send_codex_goal_lifecycle_notice`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|사용자 goal 제어 명령 lifecycle; lifecycle_notifier|
|`src/services/discord/router/message_handler/pre_admission_control.rs:171` (`send_pre_admission_notice`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|사용자 제어 명령의 admission 전 거절 안내; lifecycle_notifier|
|`src/services/discord/task_notification_delivery/footer_only_marker.rs:66` (`enqueue_footer_only_marker`)|`src/services/discord/task_notification_delivery/footer_only_marker.rs:65`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|도구/background 작업 완료 시스템; lifecycle_notifier|
|`src/services/discord/tmux.rs:581` (`emit_context_compacted_lifecycle_from_watcher`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|context compact lifecycle fallback; lifecycle_notifier|
|`src/services/discord/tmux/turn_lifecycle.rs:304` (`enqueue_monitor_auto_turn_deferred_notification`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|monitor auto-turn deferred lifecycle; lifecycle_notifier|
|`src/services/discord/tmux_watcher/discrete_trigger_marker.rs:93` (`enqueue_suppressed_task_notification`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|background/task 완료 lifecycle; lifecycle_notifier|
|`src/services/discord/tmux_watcher/terminal_token_update.rs:143` (`run_watcher_terminal_token_update`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|context 자동 압축 시스템; system|
|`src/services/discord/turn_bridge/followup_requeue/preparation_notice.rs:33` (`enqueue`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|follow-up 준비 시스템; system|
|`src/services/discord/turn_bridge/headless_delivery/durable_outbox.rs:221` (`enqueue_headless_outbox_with_origin`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|에이전트 terminal 본문; 상위 headless_turn DTO·artifact/nonce|
|`src/services/discord/turn_bridge/headless_delivery/durable_outbox.rs:232` (`enqueue_headless_outbox_with_origin`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|에이전트 terminal 본문; 상위 headless_turn DTO·artifact/nonce|
|`src/services/dispatch_watchdog.rs:180` (`enqueue_stuck_alert`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|dispatch watchdog 시스템; dispatch_watchdog|
|`src/services/message_outbox.rs:56` (`validate_outbox_source`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:87` (`enqueue_lifecycle_notification_best_effort`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:193` (`enqueue_outbox_pg_returning_id`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:201` (`enqueue_outbox_pg_returning_id_with_cancel`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:219` (`enqueue_outbox_pg_returning_id_with_ttl`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:234` (`enqueue_outbox_pg_returning_id_with_persistent_dedupe`)|pool tx 시작 message_outbox.rs:235|V tx 전; 경로에 따라 tx 안 V 재검증; D/C는 INSERT 표|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:236` (`enqueue_outbox_pg_returning_id_with_persistent_dedupe`)|`src/services/message_outbox.rs:235`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:250` (`enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx`)|`src/services/message_outbox.rs:247`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:258` (`enqueue_outbox_pg_with_attachment_on_tx`)|`src/services/message_outbox.rs:254`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:267` (`enqueue_outbox_with_origin_on_tx`)|`src/services/message_outbox.rs:262`|V·D tx 안; C 새 INSERT 뒤 같은 tx|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:324` (`enqueue_outbox_pg_returning_outcome_with_exact_dedupe_and_cancel`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:341` (`enqueue_outbox_pg_with_exact_dedupe_and_attachment`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:363` (`enqueue_outbox_pg_with_exact_origin`)|관측 후 tx 시작 message_outbox.rs:376|V·D tx 전; C 새 INSERT 뒤 tx 안|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:418` (`enqueue_outbox_pg_returning_outcome_with_ttl_and_cancel`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:430` (`enqueue_outbox_pg_with_ttl_origin`)|pool tx 시작 message_outbox.rs:472|V tx 전; 경로에 따라 tx 안 V 재검증; D/C는 INSERT 표|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:473` (`enqueue_outbox_pg_with_ttl_origin`)|`src/services/message_outbox.rs:472`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:505` (`enqueue_outbox_pg_returning_id_with_ttl_and_cancel`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:522` (`enqueue_outbox_pg_on_tx_with_ttl`)|`src/services/message_outbox.rs:518`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:531` (`enqueue_outbox_pg_on_tx_with_ttl_and_attachment`)|`src/services/message_outbox.rs:526`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:543` (`enqueue_outbox_pg_on_tx_with_ttl_origin`)|`src/services/message_outbox.rs:536`|V·D tx 안; C 새 INSERT 뒤 같은 tx|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:618` (`enqueue_outbox_pg_with_ttl`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:631` (`stage_outbox_pg_with_ttl`)|pool tx 시작 message_outbox.rs:655|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:640` (`stage_outbox_pg_with_ttl_and_attachment`)|pool tx 시작 message_outbox.rs:655|V tx 전; 경로에 따라 tx 안 V 재검증; D/C는 INSERT 표|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:792` (`enqueue_outbox_pg`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:804` (`enqueue_outbox_best_effort`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V tx 전; 경로에 따라 tx 안 V 재검증; D/C는 INSERT 표|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:806` (`enqueue_outbox_best_effort`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:823` (`enqueue_outbox_pg_on_tx`)|`src/services/message_outbox.rs:820`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:831` (`enqueue_outbox_pg_on_tx_with_attachment`)|`src/services/message_outbox.rs:827`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:840` (`enqueue_plain_on_tx`)|`src/services/message_outbox.rs:835`|V·D tx 안; C 새 INSERT 뒤 같은 tx|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox.rs:881` (`enqueue_lifecycle_notification_pg`)|pool tx 시작 message_outbox.rs:905|V tx 전; 경로에 따라 tx 안 V 재검증; D/C는 INSERT 표|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/message_outbox/session_notice.rs:174` (`enqueue_session_notice_pg_on_tx`)|`src/services/message_outbox.rs:836` (`enqueue_plain_on_tx`, V·D) → `src/services/message_outbox/plain.rs:10` (`insert_on_tx`, 정규화·INSERT·C)|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|자동큐 explicit-completion opt-in; 상위 raw DTO를 같은 tx plain helper로 전달|
|`src/services/message_outbox_circuit_authority.rs:281` (`stage_held`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 circuit 경보 producer DTO; 소유자/authority SQL fence 유지|
|`src/services/message_outbox_recovery.rs:67` (`redrive_failed_rows`)|`src/services/message_outbox_recovery.rs:61`|tx 안 :61→:67; INSERT 없음|운영자 redrive 명령; 기존 DB row source를 tx 안에서 다시 검증|
|`src/services/observability/turn_lifecycle.rs:410` (`emit_turn_lifecycle`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|TurnEvent lifecycle 시스템; lifecycle_notifier|
|`src/services/routines/discord_log.rs:731` (`log_to_target`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|routine 실행 결과 시스템; routine-runtime|
|`src/services/routines/discord_log.rs:764` (`log_to_target_with_ttl`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|routine 실행 결과 시스템; routine-runtime|
|`src/services/scheduled_messages.rs:550` (`enqueue_raw_fallback_on_tx`)|상속 tx(:535) ← 같은 파일 :596 또는 :761|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|사용자/운영자가 저장한 예약 content의 fallback; OUTBOX_SOURCE=scheduled_message|
|`src/services/scheduled_messages/push_handoff.rs:127` (`commit`)|`src/services/scheduled_messages/push_handoff.rs:108`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|예약 push producer; scheduled_message, 첨부는 저장 입력|
|`src/services/slo/mod.rs:392` (`enqueue_alert_pg`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|SLO 시스템 경보; slo_alerter|
|`src/services/tui_o/alarm.rs:359` (`notify`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|TUI O alarm 시스템; tui_o_alarm|

## DTO 생성 30행

raw `Option<&str>`와 원문 source/reason/session을 보유하는 값 생성이다. 이 단계에서 정규화한 row/복원 DTO로 합치지 않는다.

|좌표 / 생성 함수|호출 transaction|관측 시점|입력 출처|
|---|---|---|---|
|`src/services/auto_queue/transactions/completion.rs:67` (`enqueue_completion_notices`)|상속 tx(:49); §완료 호출 폐쇄 표의 root/인수|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|자동큐 lifecycle 완료; run/agent/entry SQL에서 target·count, 서비스 고정 system/notify|
|`src/db/replay_disposition/write/hold_notice.rs:80` (`withhold_and_notify`)|`src/db/replay_disposition/write/hold_notice.rs:21`|tx 안(잠긴 receipt/CAS와 함께); source만|replay lifecycle; 고정 안내문, routing bot/session은 상위 복구/attempt 서비스|
|`src/db/replay_disposition/write/hold_notice.rs:233` (`recover_hold_notice`)|`src/db/replay_disposition/write/hold_notice.rs:128`|tx 안(잠긴 receipt/CAS와 함께); source만|replay lifecycle; 고정 안내문, routing bot/session은 상위 복구/attempt 서비스|
|`src/engine/ops/message_ops.rs:64` (`queue_message`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|PolicyEngine JS message.queue 명령; target/content/bot/source 인수(에이전트/업무 정책 입력)|
|`src/github/sync.rs:1458` (`enqueue_terminal_open_alert_pg`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|GitHub 동기화 시스템; 외부 issue/card 정보를 안내문으로 포맷; github_sync|
|`src/server/routes/imessage_input_notice.rs:89` (`echo`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|사용자 iMessage 입력의 echo; 서비스 고정 imessage_input_notice|
|`src/server/routes/message_outbox.rs:173` (`enqueue_monitor_alert`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|monitor-alert API 명령(에이전트/운영 도구); 서비스 고정 auto-queue-monitor|
|`src/services/cluster/intake_router_hook/owner_record/event.rs:169` (`reserve_on_tx`)|상속 tx(:81) ← custody.rs:153 → :230/:270|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|cluster owner 변경; 서비스 SOURCE=routing_transition|
|`src/services/cluster/intake_router_hook/owner_record/plan.rs:206` (`release_held`)|`src/services/cluster/intake_router_hook/owner_record/plan.rs:192`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|cluster 보류 plan 해제; lifecycle_notifier|
|`src/services/codex_tui/input/submission.rs:69` (`enqueue_draft_notice`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|Codex 제출/사용자 초안 거절 lifecycle; lifecycle_notifier|
|`src/services/discord/health/recovery/stall_alert.rs:183` (`enqueue_stall_page`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|stall watchdog 시스템; stall_watchdog|
|`src/services/discord/inflight/removal/custody_notice.rs:96` (`enqueue_custody_notices`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|boot custody 복구 안내; boot_custody_notice|
|`src/services/discord/queue_io/retained_notice.rs:42` (`enqueue`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|retained 사용자 입력·초안 복구 안내; retained_input_notice|
|`src/services/discord/queue_io/retained_notice.rs:69` (`draft_unproven`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|retained 사용자 입력·초안 복구 안내; retained_input_notice|
|`src/services/discord/queue_io/retained_notice.rs:98` (`carried_unverified_closed`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|retained 사용자 입력·초안 복구 안내; retained_input_notice|
|`src/services/discord/queue_overflow_dlq.rs:112` (`maybe_notify_orphan_queue_overflow`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|queue overflow 시스템; queue_overflow_notice|
|`src/services/discord/recovery_engine/manual_rebind/adoption.rs:445` (`announce_adopt_fence_forward`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|운영자 rebind 명령 결과; adopt_fence_forward_notice|
|`src/services/discord/relay_recovery_circuit_alert_producer.rs:26` (`legacy_message`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|circuit 시스템 안내; stall_watchdog; legacy/stamped 분기|
|`src/services/discord/task_notification_delivery/footer_only_marker.rs:68` (`enqueue_footer_only_marker`)|`src/services/discord/task_notification_delivery/footer_only_marker.rs:65`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|도구/background 작업 완료 시스템; lifecycle_notifier|
|`src/services/discord/tmux_watcher/terminal_token_update.rs:145` (`run_watcher_terminal_token_update`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|context 자동 압축 시스템; system|
|`src/services/discord/turn_bridge/followup_requeue/preparation_notice.rs:35` (`enqueue`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|follow-up 준비 시스템; system|
|`src/services/discord/turn_bridge/headless_delivery.rs:535` (`enqueue_headless_turn_row`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|에이전트 terminal 본문; source=headless_turn, producer bot/session|
|`src/services/dispatch_watchdog.rs:182` (`enqueue_stuck_alert`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|dispatch watchdog 시스템; dispatch_watchdog|
|`src/services/message_outbox.rs:909` (`enqueue_lifecycle_notification_pg`)|`src/services/message_outbox.rs:905`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|상위 원문 DTO 전달; lifecycle wrapper만 lifecycle_notifier 고정|
|`src/services/routines/discord_log.rs:733` (`log_to_target`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|routine 실행 결과 시스템; routine-runtime|
|`src/services/routines/discord_log.rs:766` (`log_to_target_with_ttl`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|routine 실행 결과 시스템; routine-runtime|
|`src/services/scheduled_messages.rs:552` (`enqueue_raw_fallback_on_tx`)|상속 tx(:535) ← 같은 파일 :596 또는 :761|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|사용자/운영자가 저장한 예약 content의 fallback; OUTBOX_SOURCE=scheduled_message|
|`src/services/scheduled_messages/push_handoff.rs:129` (`commit`)|`src/services/scheduled_messages/push_handoff.rs:108`|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|예약 push producer; scheduled_message, 첨부는 저장 입력|
|`src/services/slo/mod.rs:394` (`enqueue_alert_pg`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|SLO 시스템 경보; slo_alerter|
|`src/services/tui_o/alarm.rs:361` (`notify`)|caller tx 없음; pool API의 tx 시작은 아래 INSERT 경로표|V/D/C는 선택 API의 아래 경로표를 따름(자체 선관측 없음)|TUI O alarm 시스템; tui_o_alarm|

## 완료 호출 폐쇄 (DB caller를 서비스로 옮길 때 필요한 범위)

`queue_run_completion_notify_on_pg`에서 시작해 각 이름을 `git grep -n -E`로 재검색하고, DB caller를 재귀 추적했다. `completion-closure.git-grep.txt`가 원시 결과다. production 호출은 28개 파일(DB 8, 상위 20)에 걸친다. 아래는 DB 함수 이름의 동명 일치를 소스로 확인한 호출 행이다. dispatch failure의 `record_on_pg_tx`도 해당 impl의 수신 타입을 확인했다. 시험/주석/정의는 아래에서 뺐다.

tx 인수는 기존 tx를 그대로 전달한다. pool-owning 함수의 begin은 아래에 적고, on-tx 함수는 그 인수 좌표를 적는다. 실제 상위 root와의 관계는 caller→callee 연결로 재현할 수 있다. pending/dispatched 입력처럼 완료 분기에 가지 않는 기존 호출도 폐쇄에서 빼지 않았다.

상위 caller는 `services::auto_queue::transactions`와 `services::kanban_cards::entry_transactions` facade 이름으로 호출한다. facade는 같은 DB 함수를 재수출하므로 아래 tx 시작·인수 좌표와 관측 시점은 DB 함수의 것 그대로다.

S14d부터 DB 함수는 완료 직전 지점에서 멈추고(`PreparedCompletion`, staged entry tx, `run_finalize_due`, `Cleared{run_finalized:false}`) 서비스가 같은 tx에서 완료를 시도한다. DB 안에서 완료 서비스를 부르는 행은 없다. `dispatched`·`pending`·`done`만 쓰는 DB 함수(consultation, done 재활성화와 그 kanban caller, failed→done reconcile, retry의 entry 재부착)는 완료를 시도할 일이 없어 DB writer를 직접 부르며 이 폐쇄에서 빠졌다.

|호출 좌표 / caller→callee|tx 시작 또는 인수|관측 시점|입력 출처|
|---|---|---|---|
|`src/services/auto_queue/transactions/completion.rs:18` `finalize_run_if_ready_on_pg_tx→db::auto_queue::runs::prepare_run_completion_on_pg_tx`|caller tx 인수 `src/services/auto_queue/transactions/completion.rs:15`|try-lock·readiness·완료 UPDATE(`src/db/auto_queue/runs.rs:307`)·slot 해제(`src/db/auto_queue/runs.rs:360`) 뒤 tx를 빌린 `PreparedCompletion` 반환; 거절은 `Unchanged`로 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/completion.rs:22` `finalize_run_if_ready_on_pg_tx→stage_completion_notice`|PreparedCompletion이 빌린 같은 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/completion.rs:34` `stage_completion_notice→enqueue_completion_notices`|`PreparedCompletion::transaction`(`src/db/auto_queue/completion_write.rs:51`)|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음; target·count는 `src/db/auto_queue/completion_write.rs:64`|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/completion.rs:65` `enqueue_completion_notices→services::message_outbox::enqueue_session_notice_pg_on_tx`|같은 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/completion.rs:45` `queue_run_completion_notify_on_pg→enqueue_completion_notices`|caller tx 인수 `src/services/auto_queue/transactions/completion.rs:39`|operator end가 이미 완료한 run; facts→V/D→INSERT→C, 같은 tx|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/completion.rs:93` `maybe_finalize_run_if_ready_pg→finalize_run_if_ready_on_pg_tx`|caller tx 인수 `src/services/auto_queue/transactions/completion.rs:90`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/completion.rs:109` `finalize_run_if_ready_pg→finalize_run_if_ready_on_pg_tx`|`src/services/auto_queue/transactions/completion.rs:104`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/entries.rs:33` `update_entry_status_on_pg→maybe_finalize_run_if_ready_pg`|DB `stage_entry_status_on_pg`의 loop별 tx `src/db/auto_queue/entries.rs:509`; 서비스 commit `src/services/auto_queue/transactions/entries.rs:35`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음; skipped/failed 변경일 때만|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/entries.rs:51` `update_entry_status_on_pg_tx→maybe_finalize_run_if_ready_pg`|caller tx 인수 `src/services/auto_queue/transactions/entries.rs:42`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음; skipped/failed 변경일 때만|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/entries.rs:88` `sync_dispatch_terminal_entries→update_entry_status_on_pg_tx`|caller tx 인수 `src/services/auto_queue/transactions/entries.rs:77`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/entries.rs:131` `finalize_completed_dispatch_terminal_entry_on_pg_tx→sync_dispatch_terminal_entries`|caller tx 인수 `src/services/auto_queue/transactions/entries.rs:126`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/entries.rs:142` `finalize_completed_dispatch_terminal_entry_on_pg_tx→maybe_finalize_run_if_ready_pg`|caller tx 인수 `src/services/auto_queue/transactions/entries.rs:126`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/dispatch_failure.rs:18` `record_entry_dispatch_failure_on_pg→EntryDispatchFailureResult::record_on_pg_tx`|`src/services/auto_queue/transactions/dispatch_failure.rs:14`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/dispatch_failure.rs:37` `record_on_pg_tx→maybe_finalize_run_if_ready_pg`|caller tx 인수 `src/services/auto_queue/transactions/dispatch_failure.rs:30`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음; failed 변경일 때만|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/phase_gates.rs:40` `reconcile_phase_gate_for_terminal_dispatch_on_pg_tx→reconcile_terminal_dispatch`|caller tx 인수 `src/services/auto_queue/transactions/phase_gates.rs:34`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/phase_gates.rs:74` `reconcile_terminal_dispatch→maybe_finalize_run_if_ready_pg`|caller tx 인수 `src/services/auto_queue/transactions/phase_gates.rs:52`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음; Cleared일 때만|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/phase_gates.rs:121` `repair_phase_gates_for_run_on_pg→reconcile_terminal_dispatch`|`src/services/auto_queue/transactions/phase_gates.rs:99`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/transactions/retry.rs:33` `create_retry_dispatch_pg→dispatch::set_dispatch_status_on_pg_tx_async`|`src/services/auto_queue/transactions/retry.rs:26`|origin dispatch failed가 아래 dispatch_status 행의 terminal 동기화로 이어짐; 같은 tx|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/kanban_cards/entry_transactions.rs:31` `soft_delete_card_pg→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/services/kanban_cards/entry_transactions.rs:22`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/dispatch/dispatch_cancel.rs:460` `cancel_dispatch_and_reset_auto_queue_on_pg_tx_inner→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/dispatch/dispatch_cancel.rs:255`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/dispatch/dispatch_status.rs:726` `set_dispatch_status_on_pg_tx_with_sync→services::auto_queue::transactions::finalize_completed_dispatch_terminal_entry_on_pg_tx`|`src/dispatch/dispatch_status.rs:506`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/dispatch/dispatch_status.rs:745` `set_dispatch_status_on_pg_tx_with_sync→services::auto_queue::transactions::sync_dispatch_terminal_entries_on_pg_tx`|`src/dispatch/dispatch_status.rs:506`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/dispatch/dispatch_status.rs:793` `set_dispatch_status_on_pg_tx_with_sync→services::auto_queue::transactions::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx`|`src/dispatch/dispatch_status.rs:506`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/engine/ops/auto_queue_ops.rs:382` `complete_run_raw→services::auto_queue::transactions::finalize_run_if_ready_pg`|caller tx 없음; callee 소유: `src/services/auto_queue/transactions/completion.rs:104`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/engine/ops/auto_queue_ops.rs:518` `record_consultation_dispatch_raw→services::auto_queue::transactions::record_consultation_dispatch_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/consultation.rs:56`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/engine/ops/auto_queue_ops.rs:561` `record_entry_dispatch_failure_raw→services::auto_queue::transactions::record_entry_dispatch_failure_on_pg`|caller tx 없음; callee 소유: `src/services/auto_queue/transactions/dispatch_failure.rs:14`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/engine/ops/auto_queue_ops.rs:625` `update_entry_status_raw→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/engine/ops/kanban_ops.rs:487` `reopen_done_auto_queue_entries_on_pg_tx→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/engine/ops/kanban_ops.rs:449`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/github/sync.rs:1184` `sync_auto_queue_terminal_on_pg→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/github/sync.rs:1153`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/github/sync.rs:1237` `sync_auto_queue_terminal_on_pg→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/github/sync.rs:1153`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/github/sync.rs:1255` `sync_auto_queue_terminal_on_pg→services::auto_queue::transactions::maybe_finalize_run_if_ready_pg`|`src/github/sync.rs:1153`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/github/sync.rs:1264` `sync_auto_queue_terminal_on_pg→services::auto_queue::transactions::maybe_finalize_run_if_ready_pg`|`src/github/sync.rs:1153`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/kanban/transition_cleanup.rs:70` `skip_live_auto_queue_entries_for_card_on_pg_tx→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/kanban/transition_cleanup.rs:48`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/kanban/transition_cleanup.rs:204` `cancel_active_dispatches_for_card_on_pg_tx→services::auto_queue::transactions::sync_dispatch_terminal_entries_on_pg_tx`|`src/kanban/transition_cleanup.rs:139`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/server/routes/dispatches/crud.rs:771` `update_dispatch_result_pg→services::auto_queue::transactions::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx`|`src/server/routes/dispatches/crud.rs:693` / `src/server/routes/dispatches/crud.rs:694`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/server/routes/kanban.rs:1732` `rereview_card→services::kanban_cards::entry_transactions::move_auto_queue_entry_to_dispatched_on_pg`|caller tx 없음; callee 소유: `reactivate_done_entry_on_pg`에 위임: `src/db/auto_queue/entries.rs:98` 또는 non-done `src/db/auto_queue/entries.rs:509`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/server/routes/kanban.rs:2016` `reopen_card→services::kanban_cards::entry_transactions::reactivate_done_auto_queue_entries_pg`|caller tx 없음; callee 소유: 각 entry의 `move_auto_queue_entry_to_dispatched_on_pg`→`reactivate_done_entry_on_pg`: `src/db/auto_queue/entries.rs:98` 또는 `src/db/auto_queue/entries.rs:509`(entry마다 별도 tx)|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/server/routes/kanban/card_delete.rs:11` `delete_card→services::kanban_cards::entry_transactions::soft_delete_card_pg`|caller tx 없음; callee 소유: `src/services/kanban_cards/entry_transactions.rs:22`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/activate_command.rs:242` `activate_with_deps_pg→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/activate_command.rs:285` `activate_with_deps_pg→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/activate_command.rs:604` `activate_with_deps_pg→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/activate_command.rs:681` `activate_with_deps_pg→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/activate_command.rs:903` `complete_run_if_empty→services::auto_queue::transactions::finalize_run_if_ready_on_pg_tx`|`src/services/auto_queue/activate_command.rs:887`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/activate_command.rs:1334` `finalize_activate_run_and_build_response→services::auto_queue::transactions::finalize_run_if_ready_pg`|caller tx 없음; callee 소유: `src/services/auto_queue/transactions/completion.rs:104`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/cancel_run/terminalize_tx.rs:134` `terminalize_selected_runs_on_pg_tx→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/services/auto_queue/cancel_run/terminalize_tx.rs:22`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/cancel_run/terminalize_tx.rs:177` `terminalize_selected_runs_on_pg_tx→services::auto_queue::transactions::queue_run_completion_notify_on_pg`|`src/services/auto_queue/cancel_run/terminalize_tx.rs:22`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/control_routes.rs:638` `repair_phase_gates→services::auto_queue::transactions::repair_phase_gates_for_run_on_pg`|caller tx 없음; callee 소유: `src/services/auto_queue/transactions/phase_gates.rs:99`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/fsm.rs:322` `apply_restore_state_changes_pg→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/services/auto_queue/fsm.rs:286` / `src/services/auto_queue/fsm.rs:287`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/fsm.rs:342` `apply_restore_state_changes_pg→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/services/auto_queue/fsm.rs:286` / `src/services/auto_queue/fsm.rs:287`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/fsm.rs:363` `apply_restore_state_changes_pg→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/services/auto_queue/fsm.rs:286` / `src/services/auto_queue/fsm.rs:287`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/fsm.rs:519` `update_entry_status_prefer_pg→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/order_routes.rs:148` `store_submitted_order_on_pg→services::auto_queue::transactions::finalize_run_if_ready_on_pg_tx`|`src/services/auto_queue/order_routes.rs:100`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/phase_gate.rs:480` `create_activate_dispatch_pg_inner→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/services/auto_queue/phase_gate.rs:327` / `src/services/auto_queue/phase_gate.rs:328`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/phase_gate.rs:595` `create_activate_dispatch_pg_inner→services::auto_queue::transactions::update_entry_status_on_pg_tx`|`src/services/auto_queue/phase_gate.rs:327` / `src/services/auto_queue/phase_gate.rs:328`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/planning.rs:42` `record_entry_dispatch_failure→services::auto_queue::transactions::record_entry_dispatch_failure_on_pg`|caller tx 없음; callee 소유: `src/services/auto_queue/transactions/dispatch_failure.rs:14`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/slot_routes.rs:243` `skip_entry_with_pg→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/view_admin_routes.rs:186` `update_entry_with_pg→services::auto_queue::transactions::reconcile_failed_entry_done_on_pg`|caller tx 없음; callee 소유: `update_entry_status_on_pg`에 위임: `src/db/auto_queue/entries.rs:509`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/auto_queue/view_admin_routes.rs:189` `update_entry_with_pg→services::auto_queue::transactions::update_entry_status_on_pg`|caller tx 없음; callee 소유: `src/db/auto_queue/entries.rs:509`의 loop별 tx|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/discord/turn_bridge/completion_guard/completion_postgres.rs:216` `runtime_pg_complete_dispatch_with_result→services::auto_queue::transactions::finalize_completed_dispatch_terminal_entry_on_pg_tx`|`src/services/discord/turn_bridge/completion_guard/completion_postgres.rs:127` / `src/services/discord/turn_bridge/completion_guard/completion_postgres.rs:128`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/discord/turn_bridge/completion_guard/completion_postgres.rs:310` `record_runtime_dispatch_failure_entries_on_pg_tx→services::auto_queue::transactions::EntryDispatchFailureResult::record_on_pg_tx`|`src/services/discord/turn_bridge/completion_guard/completion_postgres.rs:289`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/discord/turn_bridge/completion_guard/completion_postgres.rs:431` `fail_runtime_dispatch_on_pg_tx→services::auto_queue::transactions::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx`|`src/services/discord/turn_bridge/completion_guard/completion_postgres.rs:323`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|
|`src/services/dispatched_sessions/force_kill.rs:373` `fail_or_retry→services::auto_queue::transactions::create_retry_dispatch_pg`|caller tx 없음; callee 소유: `src/services/auto_queue/transactions/retry.rs:26`|완료 UPDATE→slot 해제 후 V/D→notice INSERT→C, 같은 tx; 거절 분기는 관측/INSERT 없음|자동큐 lifecycle 또는 end/skip/repair 운영 명령; entry/dispatch/kanban/GitHub terminal 경로|

## live 관측 leaf와 입력 범위

|관측 지점|입력/관측 시점과 tx 관계|값/단락 평가|
|---|---|---|
|`source_registry.rs:71`→`settings/content.rs:493`|V 각 호출 시점(위 경로마다 전/안이 다름); raw source, 상위 producer/명령 입력|빈 문자열·padded는 exact=false로 live 읽기 없음. static_allowed여도 exact=true이면 known-agent를 계산. 등록 agent id의 정확한 대소문자 일치/부재|
|`discord/agentdesk_config.rs:601~603`, loader :120~121|설정 snapshot 관측; SQL connection과 무관|runtime config 존재하면 Some(true/false) 우선; Some(false)에서 org fallback하지 않음|
|`settings/content.rs:497~500`→`discord/org_schema.rs:591~598`|runtime config None 뒤 org 경로 exists/load; SQL tx로 잠기지 않음|org map 포함/부재 또는 unavailable→false. 이 레인은 실제 파일/설정값을 열지 않음|
|`message_outbox/session_notice.rs:78~99`|D: 같은 전달 connection으로 agents SELECT; exact만 pool acquire로 tx 전|raw bindings 0/1/복수; empty rows면 locator fallback :87; 존재하나 ambiguous/unknown인 row는 None이며 locator fallback하지 않음; 첫 실패에서 루프 중단|
|`services/agent_channels.rs:36~59`→:18~20→`provider/runtime.rs:46~51`|D: primary fallback이 필요한 각 row에서만 live reload current(); 쿼리 뒤, INSERT 전|explicit cc/cdx/alt wins; 양쪽 match→None; configured provider가 있으면 default 미관측. legacy primary마다 default를 별도 관측, 복수 row의 서로 다른 provider→None (:102~106)|
|`body_census.rs:8`→`source_registry.rs:58~61`|C: INSERT 결과가 실제 새 row인 뒤; 같은 task/tx 안; raw source|config::snapshot Some이며 같은 source id agent가 없고 static notice kind일 때만 NonBody. config 없음/agent 충돌이면 None. live known-agent 검사와 별도 snapshot/시점|
|`body_census.rs:16`→`discord/outbound/raw_output.rs:29~33`|C: source=headless_turn인 경우만 task-local CONTEXT.census clone|None 또는 보존된 body tag. 같은 future에서 읽음; spawned task로 옮기면 자동 전파되지 않음. sibling task가 이 task-local 값을 변경하지 않음|
|`body_census.rs:13,:18`→`legacy_body_census/mod.rs:154~164`|C: 위 Some 분기에서만 producer stamp 관측; capture 함수 안의 순서 그대로|JOURNAL 초기화 여부에 따른 host null/문자열; process_boot initialize의 UUID. task tag→producer 순서; restart는 새 boot이며 저장된 옛 stamp를 다시 쓰지 않음|
|`body_census.rs:105~136`|capture 뒤 같은 tx에서 SAVEPOINT :113, UPDATE :114, rollback :123 또는 release :130|begin Err는 전파. UPDATE Err+savepoint rollback 성공은 notice 성공/metadata 없음; rollback/release Err는 warn 뒤 다음 statement/COMMIT가 sqlx pending rollback 결과를 드러냄. 성공을 임의로 만들어내지 않음|

모델의 source 범위는 위 경로에서 확인한 system, lifecycle_notifier, headless_turn, configured-agent/unknown/padded의 동작 동치류다. provider는 실제 enum(src/domain/provider.rs:19)의 Claude/Codex 두 대표값만 사용하며 나머지 값별 해석은 모델 밖 동등성 시험으로 남긴다. member는 config snapshot의 source-id 충돌 여부, host는 초기화 전/후 대표값이다. task-local은 같은 실행 중 동시 변경으로 모델링하지 않고 fresh-task 결함 변형만 둔다.

## 공유 reader 전수 참고 (outbox INSERT 밖)

`shared-references.git-grep.txt`는 위 다섯 심볼의 괄호 없는 참조도 포함하므로 AgentChannelBindings의 `.or_else(ProviderKind::default_channel_provider)`(:20)를 놓치지 않는다. 아래 원시 37행은 함수 호출/정의/시험이 섞인 별도 모집단이며 outbox 수치에 더하지 않는다. footer/relay/transport의 census 읽기는 기존 outbound-port 및 TUI 모델의 송신 관측이고 이번 INSERT 전이에 편입하지 않는다. 공통 source/default 구현은 이 설계에서 복제하지 않는다.

|좌표|현재 역할/tx·입력 출처|
|---|---|
|`src/services/discord/agentdesk_config.rs:601`|관측 leaf/경로: 위 표 참조|
|`src/services/discord/footer_view_reconciler/mod.rs:332`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/footer_view_reconciler/mod.rs:333`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/footer_view_reconciler/mod.rs:525`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/org_schema.rs:596`|관측 leaf/경로: 위 표 참조|
|`src/services/discord/outbound/body_census.rs:21`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/body_census.rs:188`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/body_census.rs:234`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/body_census.rs:294`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/body_census.rs:308`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/body_census.rs:326`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/raw_output.rs:29`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/raw_output.rs:144`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/outbound/source_registry.rs:58`|관측 leaf/경로: 위 표 참조|
|`src/services/discord/outbound/source_registry.rs:71`|관측 leaf/경로: 위 표 참조|
|`src/services/discord/session_relay_sink.rs:294`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/settings/content.rs:493`|관측 leaf/경로: 위 표 참조|
|`src/services/discord/settings/content.rs:494`|관측 leaf/경로: 위 표 참조|
|`src/services/discord/settings/content.rs:498`|관측 leaf/경로: 위 표 참조|
|`src/services/discord/settings/validation.rs:459`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/tmux_watcher/terminal_direct_fallback_edit.rs:75`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/turn_bridge/terminal_controller_cutover.rs:1306`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/turn_bridge/terminal_controller_cutover.rs:1941`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/turn_bridge/terminal_controller_cutover.rs:2169`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests.rs:506`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/discord/turn_bridge/terminal_outcome_delivery/prompt_too_long_guidance.rs:65`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/legacy_body_census/mod.rs:154`|관측 leaf/경로: 위 표 참조|
|`src/services/message_outbox/body_census.rs:8`|관측 leaf/경로: 위 표 참조|
|`src/services/message_outbox/body_census.rs:13`|관측 leaf/경로: 위 표 참조|
|`src/services/message_outbox/body_census.rs:16`|관측 leaf/경로: 위 표 참조|
|`src/services/message_outbox/body_census.rs:18`|관측 leaf/경로: 위 표 참조|
|`src/services/provider/channel_rules.rs:95`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/provider/channel_rules.rs:237`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/provider/runtime.rs:22`|관측 leaf/경로: 위 표 참조|
|`src/services/provider/runtime.rs:46`|관측 leaf/경로: 위 표 참조|
|`src/services/tui_o/cutover/channel_gate.rs:297`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|
|`src/services/tui_o/cutover/channel_gate/tests.rs:253`|공유 reader/정의/시험; outbox INSERT tx 밖의 기존 send/relay/task context. 대상 서비스 인수 또는 test fixture|

## 시험 fixture 좌표 (production 집계 제외)

각 행은 검색에 실제로 걸린 시험 입력이다. fixture 트랜잭션에서 선택 API의 V/D/C 순서를 검증하며 운영 입력/운영 DB가 아니다. 아래는 실행 증거가 아니다(이 설계 레그에서 cargo/PG 시험 0).

|좌표|검색 종류|fixture 함수|tx 시작/인수|
|---|---|---|
|`src/db/o_channel_activation_fence_tests.rs:51`|i7353-d1314-calls.txt|`message`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/db/o_channel_activation_fence_tests.rs:53`|dto.git-grep.txt|`message`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/db/status_check_migration_tests.rs:148`|i7353-d1314-calls.txt|`base_row`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/db/status_check_migration_tests.rs:150`|dto.git-grep.txt|`base_row`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/mod.rs:2064`|i7353-d1314-inserts.txt|`old_owner_completion_after_stale_reclaim_is_noop_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/mod.rs:2135`|i7353-d1314-calls.txt|`stall_alert_dm_row_drains_with_provider_bot_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/mod.rs:2137`|dto.git-grep.txt|`stall_alert_dm_row_drains_with_provider_bot_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/mod.rs:2223`|extra-calls.git-grep.txt|`drain_fences_superseded_circuit_row_before_delivery_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/mod.rs:2225`|dto.git-grep.txt|`drain_fences_superseded_circuit_row_before_delivery_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker.rs:133`|i7353-d1314-calls.txt|`output_origin_stampless_current_owner_outbox_pg`|src/server/outbox_worker.rs:132|
|`src/server/outbox_worker.rs:135`|dto.git-grep.txt|`output_origin_stampless_current_owner_outbox_pg`|src/server/outbox_worker.rs:132|
|`src/server/outbox_worker/attachment_pg_tests.rs:28`|dto.git-grep.txt|`message`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/attachment_pg_tests.rs:51`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49|
|`src/server/outbox_worker/attachment_pg_tests.rs:56`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55|
|`src/server/outbox_worker/attachment_pg_tests.rs:67`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55|
|`src/server/outbox_worker/attachment_pg_tests.rs:77`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55|
|`src/server/outbox_worker/attachment_pg_tests.rs:88`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55|
|`src/server/outbox_worker/attachment_pg_tests.rs:97`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55|
|`src/server/outbox_worker/attachment_pg_tests.rs:110`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55, src/server/outbox_worker/attachment_pg_tests.rs:108|
|`src/server/outbox_worker/attachment_pg_tests.rs:123`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55, src/server/outbox_worker/attachment_pg_tests.rs:108|
|`src/server/outbox_worker/attachment_pg_tests.rs:128`|i7353-d1314-calls.txt|`image_outbox_paths_preserve_retry_restart_and_dedupe_pg`|src/server/outbox_worker/attachment_pg_tests.rs:49, src/server/outbox_worker/attachment_pg_tests.rs:55, src/server/outbox_worker/attachment_pg_tests.rs:108, src/server/outbox_worker/attachment_pg_tests.rs:127|
|`src/server/outbox_worker/attachment_pg_tests.rs:245`|i7353-d1314-calls.txt|`partial_outbox_image_never_invokes_text_delivery_pg`|src/server/outbox_worker/attachment_pg_tests.rs:244|
|`src/server/outbox_worker/attachment_pg_tests.rs:289`|i7353-d1314-calls.txt|`legacy_enqueue_wrappers_keep_null_attachment_bytes_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/attachment_pg_tests.rs:292`|i7353-d1314-calls.txt|`legacy_enqueue_wrappers_keep_null_attachment_bytes_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/attachment_pg_tests.rs:295`|i7353-d1314-calls.txt|`legacy_enqueue_wrappers_keep_null_attachment_bytes_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/attachment_pg_tests.rs:304`|i7353-d1314-calls.txt|`legacy_enqueue_wrappers_keep_null_attachment_bytes_pg`|src/server/outbox_worker/attachment_pg_tests.rs:303|
|`src/server/outbox_worker/attachment_pg_tests.rs:307`|i7353-d1314-calls.txt|`legacy_enqueue_wrappers_keep_null_attachment_bytes_pg`|src/server/outbox_worker/attachment_pg_tests.rs:303|
|`src/server/outbox_worker/attachment_pg_tests.rs:311`|i7353-d1314-calls.txt|`legacy_enqueue_wrappers_keep_null_attachment_bytes_pg`|src/server/outbox_worker/attachment_pg_tests.rs:303|
|`src/server/outbox_worker/attachment_pg_tests.rs:339`|i7353-d1314-calls.txt|`outbox_image_constraints_reject_partial_and_oversized_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/attachment_pg_tests.rs:378`|i7353-d1314-calls.txt|`image_prod_options_and_sent_cleanup_preserve_sentinel_pg`|src/server/outbox_worker/attachment_pg_tests.rs:376|
|`src/server/outbox_worker/attachment_pg_tests.rs:416`|i7353-d1314-calls.txt|`image_prod_options_and_sent_cleanup_preserve_sentinel_pg`|src/server/outbox_worker/attachment_pg_tests.rs:376|
|`src/server/outbox_worker/attachment_pg_tests.rs:422`|i7353-d1314-calls.txt|`image_prod_options_and_sent_cleanup_preserve_sentinel_pg`|src/server/outbox_worker/attachment_pg_tests.rs:376, src/server/outbox_worker/attachment_pg_tests.rs:420|
|`src/server/outbox_worker/attachment_pg_tests.rs:459`|i7353-d1314-calls.txt|`enqueue_kinds`|src/server/outbox_worker/attachment_pg_tests.rs:456|
|`src/server/outbox_worker/attachment_pg_tests.rs:539`|i7353-d1314-calls.txt|`image_claim_batch_budget_and_old_consumer_barrier_pg`|src/server/outbox_worker/attachment_pg_tests.rs:537|
|`src/server/outbox_worker/attachment_pg_tests.rs:542`|i7353-d1314-calls.txt|`image_claim_batch_budget_and_old_consumer_barrier_pg`|src/server/outbox_worker/attachment_pg_tests.rs:537|
|`src/server/outbox_worker/attachment_pg_tests.rs:545`|i7353-d1314-calls.txt|`image_claim_batch_budget_and_old_consumer_barrier_pg`|src/server/outbox_worker/attachment_pg_tests.rs:537|
|`src/server/outbox_worker/attachment_pg_tests.rs:548`|i7353-d1314-calls.txt|`image_claim_batch_budget_and_old_consumer_barrier_pg`|src/server/outbox_worker/attachment_pg_tests.rs:537|
|`src/server/outbox_worker/attachment_pg_tests.rs:625`|i7353-d1314-calls.txt|`image_preflight_400_fails_first_attempt_pg`|src/server/outbox_worker/attachment_pg_tests.rs:623|
|`src/server/outbox_worker/guard_retry_pg_tests.rs:11`|dto.git-grep.txt|`output_origin_guard_database_error_retries_after_json_read_pg`|src/server/outbox_worker/guard_retry_pg_tests.rs:10|
|`src/server/outbox_worker/guard_retry_pg_tests.rs:28`|i7353-d1314-calls.txt|`output_origin_guard_database_error_retries_after_json_read_pg`|src/server/outbox_worker/guard_retry_pg_tests.rs:10|
|`src/server/outbox_worker/guard_retry_pg_tests.rs:107`|dto.git-grep.txt|`output_origin_confirmed_denials_remain_held_on_real_drain_pg`|src/server/outbox_worker/guard_retry_pg_tests.rs:106|
|`src/server/outbox_worker/guard_retry_pg_tests.rs:116`|i7353-d1314-calls.txt|`output_origin_confirmed_denials_remain_held_on_real_drain_pg`|src/server/outbox_worker/guard_retry_pg_tests.rs:106|
|`src/server/outbox_worker/session_notice_pg_tests.rs:247`|i7353-d1314-calls.txt|`eligible_lifecycle_notice_pins_provider_display_and_deduplicates_after_restart_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:258`|i7353-d1314-calls.txt|`eligible_lifecycle_notice_pins_provider_display_and_deduplicates_after_restart_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:311`|i7353-d1314-calls.txt|`eligible_lifecycle_notice_pins_provider_display_and_deduplicates_after_restart_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:331`|i7353-d1314-calls.txt|`eligible_lifecycle_notice_pins_provider_display_and_deduplicates_after_restart_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:333`|dto.git-grep.txt|`eligible_lifecycle_notice_pins_provider_display_and_deduplicates_after_restart_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:398`|i7353-d1314-calls.txt|`unmarked_legacy_pending_and_stale_processing_keep_notify_and_original_payload_pg`|src/server/outbox_worker/session_notice_pg_tests.rs:397|
|`src/server/outbox_worker/session_notice_pg_tests.rs:400`|dto.git-grep.txt|`unmarked_legacy_pending_and_stale_processing_keep_notify_and_original_payload_pg`|src/server/outbox_worker/session_notice_pg_tests.rs:397|
|`src/server/outbox_worker/session_notice_pg_tests.rs:461`|i7353-d1314-calls.txt|`designated_unknown_owner_retries_without_notify_fallback_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:556`|i7353-d1314-calls.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:558`|dto.git-grep.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:596`|i7353-d1314-calls.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:598`|dto.git-grep.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:638`|i7353-d1314-calls.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:640`|dto.git-grep.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:667`|i7353-d1314-calls.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:669`|dto.git-grep.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/outbox_worker/session_notice_pg_tests.rs:689`|i7353-d1314-calls.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|src/server/outbox_worker/session_notice_pg_tests.rs:688|
|`src/server/outbox_worker/session_notice_pg_tests.rs:691`|dto.git-grep.txt|`user_content_ops_and_unlisted_sources_remain_sql_null_display_pg`|src/server/outbox_worker/session_notice_pg_tests.rs:688|
|`src/server/outbox_worker/session_notice_pg_tests.rs:721`|dto.git-grep.txt|`explicit_autoqueue_notice_does_not_convert_arbitrary_system_content_pg`|src/server/outbox_worker/session_notice_pg_tests.rs:720|
|`src/server/outbox_worker/session_notice_pg_tests.rs:729`|i7353-d1314-calls.txt|`explicit_autoqueue_notice_does_not_convert_arbitrary_system_content_pg`|src/server/outbox_worker/session_notice_pg_tests.rs:720|
|`src/server/outbox_worker/session_notice_pg_tests.rs:732`|i7353-d1314-calls.txt|`explicit_autoqueue_notice_does_not_convert_arbitrary_system_content_pg`|src/server/outbox_worker/session_notice_pg_tests.rs:720|
|`src/server/routes/imessage_input_notice_tests.rs:543`|i7353-d1314-calls.txt|`imessage_echo_enqueue_failure_preserves_result_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/server/routes/imessage_input_notice_tests.rs:545`|dto.git-grep.txt|`imessage_echo_enqueue_failure_preserves_result_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/cluster/input_fallback/notice/pg_tests.rs:30`|i7353-d1314-calls.txt|`routing_notice_real_outbox_dedupe_reuses_event_and_separates_switches_pg`|src/services/cluster/input_fallback/notice/pg_tests.rs:29|
|`src/services/cluster/input_fallback/notice/pg_tests.rs:32`|dto.git-grep.txt|`routing_notice_real_outbox_dedupe_reuses_event_and_separates_switches_pg`|src/services/cluster/input_fallback/notice/pg_tests.rs:29|
|`src/services/cluster/intake_router_hook/owner_record/adoption_tests.rs:49`|i7353-d1314-calls.txt|`original_adoption_binding_history_and_legacy_artifacts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/cluster/intake_router_hook/owner_record/adoption_tests.rs:51`|dto.git-grep.txt|`original_adoption_binding_history_and_legacy_artifacts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/cluster/intake_router_hook/owner_record/event_tests.rs:36`|extra-calls.git-grep.txt|`routing_event_real_outbox_history_and_failed_pointer_reuse_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/cluster/intake_router_hook/owner_record/event_tests.rs:37`|extra-calls.git-grep.txt|`routing_event_real_outbox_history_and_failed_pointer_reuse_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/cluster/intake_router_hook/owner_record/event_tests.rs:38`|extra-calls.git-grep.txt|`routing_event_real_outbox_history_and_failed_pointer_reuse_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/cluster/intake_router_hook/owner_record/event_tests.rs:262`|i7353-d1314-calls.txt|`general`|src/services/cluster/intake_router_hook/owner_record/event_tests.rs:261|
|`src/services/cluster/intake_router_hook/owner_record/event_tests.rs:264`|dto.git-grep.txt|`general`|src/services/cluster/intake_router_hook/owner_record/event_tests.rs:261|
|`src/services/discord/outbound/source_registry.rs:126`|extra-calls.git-grep.txt|`imessage_input_notice_is_authorized_and_classified_as_nonbody`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/outbound/source_registry.rs:137`|extra-calls.git-grep.txt|`producer_contract_lists_every_loopback_source`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/outbound/source_registry.rs:144`|extra-calls.git-grep.txt|`producer_contract_lists_every_loopback_source`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/outbound/source_registry.rs:157`|extra-calls.git-grep.txt|`new_sources_are_loopback_only`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/outbound/source_registry.rs:164`|extra-calls.git-grep.txt|`new_sources_are_loopback_only`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/outbound/source_registry.rs:174`|extra-calls.git-grep.txt|`original_external_contract_and_fail_closed_variants_are_pinned`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/outbound/source_registry.rs:176`|extra-calls.git-grep.txt|`original_external_contract_and_fail_closed_variants_are_pinned`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/outbound/source_registry.rs:184`|extra-calls.git-grep.txt|`original_external_contract_and_fail_closed_variants_are_pinned`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/relay_recovery_circuit_alert_producer.rs:274`|extra-calls.git-grep.txt|`activate_unknown_status_fails_safe_without_reopen_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/runtime_bootstrap/o_writer_host_custody_tests.rs:133`|dto.git-grep.txt|`takeover`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/runtime_bootstrap/o_writer_host_custody_tests.rs:141`|i7353-d1314-calls.txt|`takeover`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/terminal_delivery_custody/pg_tests.rs:30`|i7353-d1314-calls.txt|`foreign_terminal_custody_pg_failure_restart_and_exact_dedupe`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/terminal_delivery_custody/pg_tests.rs:32`|dto.git-grep.txt|`foreign_terminal_custody_pg_failure_restart_and_exact_dedupe`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/tmux_watcher/discrete_trigger_marker.rs:214`|i7353-d1314-calls.txt|`background_marker_provider_outbox_survives_restart_replay_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/tmux_watcher/o_evidence_tests.rs:1798`|i7353-d1314-calls.txt|`headless_restart_frame`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/tmux_watcher/o_evidence_tests.rs:1800`|dto.git-grep.txt|`headless_restart_frame`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/durable_outbox.rs:262`|dto.git-grep.txt|`output_origin_delayed_producer_consumer_restore_and_old_format_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/durable_outbox.rs:385`|dto.git-grep.txt|`output_origin_delayed_producer_consumer_restore_and_old_format_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:27`|i7353-d1314-calls.txt|`enqueue`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:29`|dto.git-grep.txt|`enqueue`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:88`|i7353-d1314-calls.txt|`registered_agent_labels_do_not_certify_absent_body_metadata`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:93`|i7353-d1314-calls.txt|`registered_agent_labels_do_not_certify_absent_body_metadata`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:95`|dto.git-grep.txt|`registered_agent_labels_do_not_certify_absent_body_metadata`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:134`|i7353-d1314-calls.txt|`registered_agent_labels_do_not_certify_absent_body_metadata`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:136`|dto.git-grep.txt|`registered_agent_labels_do_not_certify_absent_body_metadata`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:247`|dto.git-grep.txt|`old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:258`|i7353-d1314-calls.txt|`old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg`|src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:257|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:264`|i7353-d1314-calls.txt|`old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg`|src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:257|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:268`|i7353-d1314-calls.txt|`old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg`|src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:257|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:273`|i7353-d1314-calls.txt|`old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg`|src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:257|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:283`|i7353-d1314-calls.txt|`old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg`|src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:257|
|`src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:295`|i7353-d1314-calls.txt|`old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg`|src/services/discord/turn_bridge/headless_delivery/observation_tests.rs:257|
|`src/services/discord/turn_bridge/headless_delivery/production_seam_tests.rs:210`|dto.git-grep.txt|`durable_exact_path_observes_cancellation_before_database_work`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/production_seam_tests.rs:256`|dto.git-grep.txt|`durable_enqueue_with`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/discord/turn_bridge/headless_delivery/production_seam_tests.rs:351`|dto.git-grep.txt|`durable_exact_conflict_keeps_first_producer_turn_nonce_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention.rs:609`|i7353-d1314-calls.txt|`db_retention_preserves_permanent_outbox_dedupe_sentinels`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention.rs:611`|dto.git-grep.txt|`db_retention_preserves_permanent_outbox_dedupe_sentinels`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention.rs:622`|i7353-d1314-calls.txt|`db_retention_preserves_permanent_outbox_dedupe_sentinels`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention.rs:624`|dto.git-grep.txt|`db_retention_preserves_permanent_outbox_dedupe_sentinels`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention.rs:637`|i7353-d1314-calls.txt|`db_retention_preserves_permanent_outbox_dedupe_sentinels`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention.rs:639`|dto.git-grep.txt|`db_retention_preserves_permanent_outbox_dedupe_sentinels`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention/record_caps.rs:515`|i7353-d1314-calls.txt|`db_retention_keeps_ledger_rows_live_readers_still_use`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/maintenance/jobs/db_retention/record_caps.rs:517`|dto.git-grep.txt|`db_retention_keeps_ledger_rows_live_readers_still_use`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:966`|dto.git-grep.txt|`headless_message`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1000`|i7353-d1314-calls.txt|`typed_outbox_core_preserves_cancel_at_the_observation_site`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1020`|dto.git-grep.txt|`forbidden_message`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1031`|dto.git-grep.txt|`scheduled_message_for_slot`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1062`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1064`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1067`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1070`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1079`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1081`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1082`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1083`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1086`|i7353-d1314-calls.txt|`every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg`|src/services/message_outbox.rs:1085|
|`src/services/message_outbox.rs:1124`|dto.git-grep.txt|`enqueue_acceptance_matches_worker_loopback_source_gate_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1136`|i7353-d1314-calls.txt|`enqueue_acceptance_matches_worker_loopback_source_gate_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1157`|i7353-d1314-calls.txt|`fixed_lifecycle_enqueue_uses_registered_source_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1182`|i7353-d1314-calls.txt|`persistent_dedupe_returns_old_row_and_distinct_slot_inserts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1198`|i7353-d1314-calls.txt|`persistent_dedupe_returns_old_row_and_distinct_slot_inserts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1214`|i7353-d1314-calls.txt|`persistent_dedupe_returns_old_row_and_distinct_slot_inserts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1230`|dto.git-grep.txt|`staged_message`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1251`|i7353-d1314-calls.txt|`gc_prunes_only_expired_held_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1258`|i7353-d1314-calls.txt|`gc_prunes_only_expired_held_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1300`|i7353-d1314-calls.txt|`gc_preserves_persistent_dedupe_sentinels_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1302`|dto.git-grep.txt|`gc_preserves_persistent_dedupe_sentinels_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1313`|i7353-d1314-calls.txt|`gc_preserves_persistent_dedupe_sentinels_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox.rs:1315`|dto.git-grep.txt|`gc_preserves_persistent_dedupe_sentinels_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox/body_census_tests.rs:21`|dto.git-grep.txt|`failed_census_savepoint_fails_the_enqueue_instead_of_committing_a_lost_row_pg`|src/services/message_outbox/body_census_tests.rs:20|
|`src/services/message_outbox/body_census_tests.rs:29`|i7353-d1314-calls.txt|`failed_census_savepoint_fails_the_enqueue_instead_of_committing_a_lost_row_pg`|src/services/message_outbox/body_census_tests.rs:20|
|`src/services/message_outbox_circuit_authority_tests.rs:40`|dto.git-grep.txt|`message`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:258`|extra-calls.git-grep.txt|`vouch_then_new_epoch_reopens_and_old_activation_is_stale_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:282`|extra-calls.git-grep.txt|`vouch_then_new_epoch_reopens_and_old_activation_is_stale_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:319`|extra-calls.git-grep.txt|`exact_stage_is_idempotent_and_mismatched_coordinate_conflicts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:327`|extra-calls.git-grep.txt|`exact_stage_is_idempotent_and_mismatched_coordinate_conflicts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:333`|extra-calls.git-grep.txt|`exact_stage_is_idempotent_and_mismatched_coordinate_conflicts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:340`|extra-calls.git-grep.txt|`exact_stage_is_idempotent_and_mismatched_coordinate_conflicts_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:369`|i7353-d1314-inserts.txt|`non_circuit_collision_conflicts_and_expired_held_is_replaced_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:371`|extra-calls.git-grep.txt|`non_circuit_collision_conflicts_and_expired_held_is_replaced_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:381`|extra-calls.git-grep.txt|`non_circuit_collision_conflicts_and_expired_held_is_replaced_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:395`|extra-calls.git-grep.txt|`non_circuit_collision_conflicts_and_expired_held_is_replaced_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:430`|extra-calls.git-grep.txt|`non_owner_reserve_and_stage_leave_zero_rows_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:452`|extra-calls.git-grep.txt|`resume_activation_maps_deliverable_terminal_and_unknown_states_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:509`|extra-calls.git-grep.txt|`activation_then_vouch_cancels_pending_and_releases_dedupe_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:561`|extra-calls.git-grep.txt|`processing_circuit_row`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_circuit_authority_tests.rs:739`|i7353-d1314-inserts.txt|`fence_non_circuit_row_clears_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/message_outbox_recovery_tests.rs:25`|i7353-d1314-inserts.txt|`seed`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/alarm.rs:1114`|i7353-d1314-calls.txt|`suppression_settles_existing_rows_and_errors_retry_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/alarm.rs:1116`|dto.git-grep.txt|`suppression_settles_existing_rows_and_errors_retry_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/writer/deferred_pg_tests.rs:693`|i7353-d1314-calls.txt|`normal_deferred_rejects_headless_enqueue_completed_after_fence_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/writer/deferred_pg_tests.rs:695`|dto.git-grep.txt|`normal_deferred_rejects_headless_enqueue_completed_after_fence_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/writer/deferred_pg_tests.rs:782`|i7353-d1314-calls.txt|`queued_headless_pending_and_processing_block_expired_handoff_until_worker_sent_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/writer/deferred_pg_tests.rs:784`|dto.git-grep.txt|`queued_headless_pending_and_processing_block_expired_handoff_until_worker_sent_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/writer/deferred_pg_tests.rs:917`|i7353-d1314-calls.txt|`normal_deferred_rejects_headless_enqueue_completed_after_fence_from_a_preexisting_reservation_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
|`src/services/tui_o/writer/deferred_pg_tests.rs:919`|dto.git-grep.txt|`normal_deferred_rejects_headless_enqueue_completed_after_fence_from_a_preexisting_reservation_pg`|fixture가 pool API에 위임; fixture 자체 관측 없음|
