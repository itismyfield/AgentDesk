# 메신저 중립 선행 P0~P2 설계 (#8192)

기준: **d40c06eb47a358373a740b8596144640a3b8ba63**, 2026-11-16 WSL. 이 레그의 변경은 이 문서 하나다. 실행 코드·시험·기준선은 변경하지 않으며 빌드·cargo·운영 저장소 접근을 하지 않는다.

**입력과 실제 코드 사이의 차이부터 고정한다.** #8189의 `SharedData` 255파일은 구조체 전체 참조 수가 아니라 B 분류 부분집합이다. 원문 `classify.tsv`와 분류 스크립트는 전달된 보고서에 포함되지 않았다. 따라서 C1 106파일/약 38,664줄을 이 SHA에서 재현했다고 주장하지 않는다. 아래 HTTP 인자 전수표는 현재 git grep 결과 전부를 먼저 보존하고, 이 설계의 C1 경계를 별도로 표시한다. 원래 580/206도 prod 중심 집계이고, 아래 전체 src 문자열 집계와 단위가 다르다.

더 중요한 차이는 경계다. `RestartLifecycle`은 CLI 소유 증거를, `TurnFinalizer`는 `Arc<SharedData>`를 품는다. 이 타입을 통째로 application으로 내리는 것만으로 역의존 0이 되지 않는다. `TurnGateway` 19메서드 중 11개는 송신 자체가 아니다. 이 문서는 **순수 엔진 저장 상태를 EngineRuntime으로 분리하고, 송신 trait은 하나로 합치되 실행 확장은 Discord에 유지**하는 계획을 권고한다. #8192 DoD의 “두 trait 사용처가 하나”를 모든 실행 메서드까지 단일 vocabulary trait에 넣는 뜻으로 읽으면 이 권고와 다르다. 그 해석은 코디네이터의 결정 사항이며, 아래 제외 목록을 완료로 숨기지 않는다. 혼합 실행 객체 전체의 이전과 P3 C2 분리는 이번 계획에 넣지 않는다.

입력은 `gh issue view 8192`, `gh issue view 7353 --comments`의 마지막 “재개 (사용자 결정 11-16 06시)” 댓글, 제공된 #8189 보고서 §4-1·4-4·4-5와 #7353 설계 G06·G08이다. 51조각/CUT/V01~V21은 재개하지 않는다. #7651 전체 완료를 기다리지 않는다. Slack 구현·고유 ID 대응표 구현·DB schema 변경·이벤트 ingress 변경도 범위 밖이다.

<!-- doc-path-refs: proposed src/services/engine_runtime.rs -->
<!-- doc-path-refs: proposed src/services/engine_runtime/session.rs -->
<!-- doc-path-refs: proposed src/services/engine_runtime/relay_state.rs -->
<!-- doc-path-refs: proposed src/services/engine_runtime/execution_state.rs -->
<!-- doc-path-refs: proposed src/services/engine_runtime/events.rs -->
<!-- doc-path-refs: proposed src/domain/outbound/messenger_types.rs -->
<!-- doc-path-refs: proposed src/services/discord/gateway/turn_context.rs -->
<!-- doc-path-refs: proposed src/services/messenger_adapters -->

## 1. 운영 전제와 재현 단위

| 전제 | main의 코드 좌표 | 보존할 의미 |
|---|---|---|
| 봇별 상태 생성 | `src/services/discord/runtime_bootstrap/shared_data.rs:88`, 시험 생성 `src/services/discord/mod.rs:1333` | 생성 횟수·provider/token namespace·주입 Arc의 동일성 유지 |
| 원자 세션 상태 | `src/services/discord/mod.rs:962` | sessions를 한 Mutex로 잠그는 단위 유지 |
| transition lease | `src/services/discord/session_transition.rs:136`, `src/services/discord/session_transition.rs:157` | Drop 뒤 기존 owner wake를 동일 시점에 실행; map 이전과 wake 구현 이전을 구별 |
| finalizer | `src/services/discord/turn_finalizer.rs:350`, `src/services/discord/turn_finalizer.rs:618` | 한 actor·기존 Weak 순환 방지·토큰 해제 뒤 완료 발행을 보존 |
| 완료 버스 | `src/services/discord/turn_completion_events.rs:13`, `src/services/discord/turn_completion_events.rs:82` | capacity 64·phase·lag 후 reconcile 동작 유지 |
| HTTP fallback | `src/services/discord/shared_state.rs:62` | ready ctx 우선, token REST fallback, 둘 다 없으면 hard failure 유지 |
| answer flush | `src/services/discord/mod.rs:1042`, `src/services/discord/gateway/outbound_messages.rs` | RAII 해제와 queued notice bounded wait의 위치 유지 |
| 실제 nonce | `src/services/discord/outbound/transport.rs:22` | nonce 문자열·enforce_nonce 플래그를 요청까지 보존 |
| 관측 scope | `src/services/discord/outbound/raw_output.rs:26`, `src/services/discord/outbound/transport.rs:292` | task-local census를 현재 future·tx/savepoint에서 실행; spawn/선관측 금지 |
| replay 재시도 권한 | `src/services/discord/replay_policy.rs:138` | sealed permit을 DTO 생성자로 대체하지 않으며 exact retry의 Refused/Started 유지 |
| review cap | `scripts/pr_cap_prod.py:14` | FILES=30, NET=1500; +1200 예상 초과 시 사전 재분할; no-renames 계수 |

상태 차트: 해당 없음. 상태의 소유 위치·타입 경계·trait 묶음을 바꾸며 상태 전이·깨우기·재시작 경로를 바꾸지 않는다. 어느 조각에서든 wake callback·permit 소비·spawn 시점·retry admission·rollback 정책을 바꾸어야 하면 **그 조각 착수 전 0단계 상태 차트와 별도 수용 기준**을 요구하고 이 계획의 단순 이전으로 처리하지 않는다.

현재 전체 tracked src의 git grep 파일 집계(시험/주석 포함)는 SharedData **700**, ChannelId **1,182**, 정확한 `channel_id: u64` **320**, `message_id: u64` **78**이다. prod B 분류 255, prod 중심 ChannelId 약 580/channel_id:u64 약 206과 같은 분모가 아니다. 문자열 등장 수를 엔진 이동 파일 수로 다시 사용하지 않는다.

### 1.1 SharedData 전 필드

정의는 `src/services/discord/mod.rs:1007`부터 34필드다. E는 엔진 저장 상태, B는 봇 상태, M은 혼합 객체다. bot event에서도 쓴다는 이유만으로 E를 B로 분류하지 않았다. E의 mutex/map/atomic은 한 번만 생성한다. M의 순수 부분만 떼고 실제 봇 실행 객체는 남긴다.

참조 파일 수는 **S/D 두 문자 검색 지표**다. S=`rg -l -U '\bshared\s*\.\s*FIELD\b' src`, D=`rg -l -U '\.\s*FIELD\b' src`의 파일 중복 제거 개수다. S는 shared라는 receiver의 직접 접근(줄바꿈 포함), D는 self/다른 receiver까지 포함한 후보 상한이다. 문자열·시험·동명 필드가 포함되며 S는 self·별칭 접근을 놓친다. 특히 provider/http의 D를 타입 해석된 참조 수로 쓰면 안 된다. cargo 금지 레그에서 rustc 기반 전 참조 수를 꾸미지 않는다. 아래 분류 근거는 실제 소유 타입과 대표 생산자/소비자를 따로 대조했다.

| 필드(선언 줄) | 현재 타입 | S/D 파일 | 분류·이전 판정과 근거 |
|---|---|---:|---|
| `core` (1009) | `Mutex<CoreState>` | 189/193 | E → P0-B. 세션 map; DiscordSession은 이름과 달리 세션 값. session_runtime의 IO 메서드는 상위에 남기고 값 선언만 내림. |
| `mailboxes` (1011) | `ChannelMailboxRegistry` | 80/89 | E → P0-B. registry 정본은 이미 turn_orchestrator; 봇 이벤트도 admission에 쓰지만 실행 큐의 단일 소유자. |
| `session_transition_locks` (1016) | `dashmap::DashMap<ChannelId, std::sync::Weak<tokio::sync::Mutex<()>>>` | 1/2 | E → P0-A. 세션 선택/재연결 직렬화. session_transition의 wake/SharedData identity는 이전하지 않음. |
| `settings` (1018) | `tokio::sync::RwLock<DiscordBotSettings>` | 141/154 | B 유지. DiscordBotSettings의 allow users·bot IDs·role/channel 설정과 save_bot_settings IO. |
| `api_timestamps` (1020) | `dashmap::DashMap<ChannelId, tokio::time::Instant>` | 2/2 | B 유지. rate_limit_wait의 Discord API 속도 제한 기록. |
| `skills_cache` (1022) | `tokio::sync::RwLock<Vec<(String, String)>>` | 3/4 | E → P0-A. prompt 조립용 이름/설명 cache; Discord 이벤트가 cache를 읽어도 내용은 메신저 독립. |
| `tmux_watchers` (1024) | `TmuxWatcherRegistry` | 181/189 | E 저장부 → P0-C. 실제 핸들/registry 값 선언만 내림; claim/teardown/authority impl은 기존 모듈에 유지. |
| `tmux_relay_coords` (1029) | `Arc<dashmap::DashMap<ChannelId, Arc<TmuxRelayCoord>>>` | 4/6 | E 저장부 → P0-C. emission slot·offset·lease 공유. relay_health/DeliveryLeaseCell 값 폐쇄가 필요. |
| `ui` (1033) | `PlaceholderState` | 68/88 | B 유지. placeholder cleanup/controller/live events 및 표시 gate. |
| `queued` (1041) | `QueuedPlaceholderState` | 13/49 | B 유지. Discord queued message ID와 write-through 표시 원장; queue 자체는 mailboxes. |
| `answer_flush_barrier` (1051) | `Arc<answer_flush_barrier::AnswerFlushBarrier>` | 4/4 | B 유지. Discord 분할 answer와 queued card POST 사이의 송신 순서. |
| `restart` (1060) | `RestartLifecycle` | 246/285 | M → P0-D 순수 counters/flags/times만 E; replay_restart·gateway lease diagnostics·waiter cancellation은 B wrapper. CLI 증거를 app에 끌어내리지 않음. |
| `turn_finalizer` (1066) | `Arc<turn_finalizer::TurnFinalizer>` | 57/63 | M 유지. 엔진 의미지만 FinalizeMsg에 Arc<SharedData>; storage만 아래로 내릴 수 없는 실행 객체. 전체 이전은 후속 엔진 이동. |
| `dispatch` (1071) | `DispatchRoutingState` | 44/74 | E → P0-D. dedup·parent/thread routing 값; channel/thread ingress 해석은 봇이 계속 담당. |
| `bot_connected` (1073) | `std::sync::atomic::AtomicBool` | 5/6 | B 유지. ready gateway event 관측. |
| `last_turn_at` (1075) | `std::sync::Mutex<Option<String>>` | 2/3 | E → P0-A. 완료 턴 health timestamp; bot 연결 시각과 다름. |
| `overrides` (1083) | `SessionOverrideState` | 21/26 | M → P0-D. 실행 model/fast/goals/node/reset 상태는 E; MessageId/ModelPickerPendingState 선택 UI map만 B wrapper. |
| `last_message_ids` (1085) | `dashmap::DashMap<ChannelId, u64>` | 30/31 | B 유지. Discord startup catch-up checkpoint. |
| `catch_up_retry_pending` (1090) | `dashmap::DashMap<ChannelId, CatchUpRetryState>` | 17/17 | B 유지. REST catch-up pinned after checkpoint와 fetch 실패 재시도. |
| `catch_up_scan_in_progress` (1092) | `dashmap::DashMap<ChannelId, (bool, Option<CatchUpRetryState>)>` | 3/3 | B 유지. Discord catch-up sweep 직렬화. |
| `turn_start_times` (1095) | `dashmap::DashMap<ChannelId, std::time::Instant>` | 11/12 | E → P0-A. metrics duration start; intake도 기록하지만 bot 전용 상태가 아님. |
| `channel_rosters` (1097) | `dashmap::DashMap<ChannelId, Vec<UserRecord>>` | 0/1 | B 유지. incoming Discord UserId/UserRecord의 채널 roster. |
| `http` (1103) | `RuntimeHttpCache` | 65/274 | B 유지. cached serenity Context/token fallback. |
| `token_hash` (1106) | `String` | 260/319 | B 유지. 현재 pending queue/provider별 디렉터리 namespace; EngineRuntime에 token을 복제하지 않음. 저장 namespace는 명시적 입력. |
| `provider` (1111) | `ProviderKind` | 65/895 | E → P0-A. 실행 provider 정본(타입 정본은 domain::provider). 봇당 provider 하나인 현재 배치 관계는 유지. |
| `api_port` (1113) | `u16` | 26/54 | E → P0-A. 엔진 self-request 주소; 설정 읽기/선택은 기존 bootstrap에 유지. |
| `pg_pool` (1115) | `Option<sqlx::PgPool>` | 209/279 | E → P0-A. 기존 Option<PgPool> handle 그대로; Some/None 및 pool 수 불변. |
| `policy` (1116) | `PolicyRuntime` | 5/33 | E → P0-A. PolicyEngine Option handle; PolicyRuntime의 값 선언만 아래로, policy 실행 순서 유지. |
| `health_registry` (1119) | `std::sync::Weak<health::HealthRegistry>` | 17/95 | M 유지. Weak<Discord HealthRegistry>; registry가 Http/provider entry를 보유(health.rs). 순수 엔진 registry로 간주하면 역의존 증가. |
| `known_slash_commands` (1123) | `tokio::sync::OnceCell<std::collections::HashSet<String>>` | 5/6 | B 유지. 등록된 Discord slash command 이름 set. |
| `inflight_signals` (1130) | `tokio::sync::broadcast::Sender<inflight::InflightSignal>` | 9/10 | E → P0-E. Completed{channel_id:u64,turn_id:u64} 값 선언과 sender만 이동; publish/subscribe 시점 유지. |
| `turn_completion_events` (1135) | `tokio::sync::broadcast::Sender<turn_completion_events::TurnCompletionEvent>` | 4/4 | E → P0-E. phase 포함 완료 값/sender; mailbox release 순서 그대로. |
| `turn_view_reconciler` (1137) | `turn_view_reconciler::TurnViewReconciler` | 40/40 | M 유지. reaction/view reconciliation 실행 객체; engine 완료 bus와 구별. |
| `readopted_mailbox_ledger` (1138) | `readopted_mailbox_ledger::ReadoptedMailboxLedger` | 0/1 | E 저장/M 접근 경계로 잔류. 값 필드는 ProviderKind/u64/String/bool뿐이고 엔진 원장이다. 단 from_state(26행)는 D 범위 InflightTurnState를 읽고 SharedData helper가 private entries를 갱신하므로 이번 P0의 pure state 폐쇄에서 보류; 봇 전용이라고 분류하지 않음. |

분류의 추가 좌표: `src/services/discord/shared_state.rs:26`(ui), `src/services/discord/shared_state.rs:48`(http), `src/services/discord/shared_state.rs:58`(policy), `src/services/discord/shared_state.rs:410`(override UI 혼합), `src/services/discord/shared_state.rs:440`(dispatch), `src/services/discord/shared_state.rs:622`(restart), `src/services/discord/session_runtime.rs:34`(session 값), `src/services/discord/tmux_watcher_registry.rs:209`(registry 저장), `src/services/discord/relay_coord.rs:14`(lease 포함), `src/services/discord/inflight.rs:947`(signal), `src/services/discord/readopted_mailbox_ledger.rs:87`(ledger). 대표 실제 소비자는 session_runtime/turn_bridge, bot event router/catch_up, bootstrap/shared_data이며 전수 검색 원문은 레인 logs에 보관한다.

### 1.2 기존 송신 port와 구현

`src/domain/outbound/port.rs:88`의 **6메서드**다. String ID는 이미 중립 경계지만 Discord 용어와 default fallback 의미는 남아 있다.

| 메서드 | 선언 줄 | 기본 동작/주의 |
|---|---:|---|
| post_message | 89 | 필수, 생성 message ID 반환 |
| post_message_with_reference | 96 | reference 미지원이면 plain post |
| post_message_with_nonce | 107 | 미지원이면 nonce/enforce를 버리고 plain post |
| post_message_with_reference_and_nonce | 118 | nonce 미지원이어도 reference 경로 유지 |
| edit_message | 136 | 미지원 Err; 편집 ID 반환 |
| resolve_dm_channel | 150 | 미지원 Err; DM channel ID 반환 |

`OutboundDelivery::deliver`는 같은 파일 159행의 **별도 caller-facing delivery trait**다. 이번 “두 trait”은 이것과 DiscordOutboundClient가 아니라 DiscordOutboundClient와 TurnGateway를 뜻한다. dedup reservation·intent delivery 소유를 삭제하지 않는다.

| prod 구현 5개 | impl 좌표 | override | 기본값을 통해 남는 의미 |
|---|---|---|---|
| SerenityTurnOutboundClient | `src/services/discord/gateway.rs:323` | post, reference, nonce, reference+nonce, edit | DM Err |
| HttpOutboundClient | `src/services/discord/outbound/transport.rs:286` | post, edit, DM | reference/nonce 무시하는 기존 fallback |
| SerenityManualOutboundClient | `src/services/discord/outbound/manual_delivery.rs:185` | post, nonce, DM | edit Err; reference default는 post/nonce 보존 조합에 주의 |
| SerenityReferenceOutboundClient | `src/services/discord/outbound/serenity_reference.rs:92` | post, reference, reference+nonce | nonce 단독은 default plain post; edit/DM Err |
| MonitoringOutboundClient | `src/services/discord/monitoring_status.rs:35` | post, edit | reference/nonce plain fallback; DM Err |

수는 `rg -n 'impl DiscordOutboundClient for' src`에서 prod impl을 확인한 값이다. mock 5개는 prod 구현에 더하지 않는다. 기능은 “Discord니까 모두 지원”으로 추론하지 않고 위 override 조합별로 고정한다.

## 2. P0: 상태 소유 분리

권고 흐름은 **bootstrap → Arc<EngineRuntime> 한 번 생성 → SharedData가 보유 → 엔진 함수에는 shared.engine.as_ref() 전달**이다. EngineRuntime은 application(3), SDK ID map을 보관하는 임시 legacy_state는 adapter(2), 값 DTO는 vocabulary(0)다. 기존 메서드의 field access와 생성자 위치를 한꺼번에 수백 파일 고치지 않는다.

P0-A에서 SharedData에 `engine: Arc<EngineRuntime>`과 한 방향 `Deref<Target=EngineRuntime>` 호환 경로를 둔다. 기존 `Arc<SharedData> → SharedData → EngineRuntime` autoderef가 필드 읽기/락/atomic을 같은 객체로 연결한다. EngineRuntime에서 SharedData로의 Deref/AsRef/callback 저장은 금지한다. 이전할 필드와 같은 이름의 shadow 필드는 SharedData에서 지운다. 전체 SharedData 가변 borrow/struct destructuring/필드 projection은 grep으로 점검하고, Arc 내부 상태를 복제하는 clone migration을 하지 않는다. 생성자 두 곳을 같은 조각에서 바꾼다.

P0-B/C는 **데이터 선언만** 아래로 소유 이전한다. 같은 crate에서는 공개된 기존 API만 사용하는 상위 Discord의 inherent impl이 허용된다. private 필드를 직접 읽는 impl은 아래의 가시성 폐쇄 규칙을 먼저 따른다. struct 필드 타입을 아래로 닫지 않은 `pub use`만으로 이전을 완료했다고 하지 않는다. SDK ID map은 legacy_state adapter가 소유하고 EngineRuntime이 이 객체를 보유한다. 새 engine API는 ConversationId를 받고 그 adapter 경계에서만 변환한다. 기존 map key를 이번에 ConversationId로 교체하면 수백 caller가 바뀌므로 그렇게 하지 않는다. 따라서 **새 API는 중립이지만 기존 private 저장 타입에는 Serenity 의존이 임시로 남는다**. SDK 제거 완료나 Slack 실행 가능을 P0/P1 완료로 보고하지 않는다.

P0-D의 `overrides`와 `restart`는 bot wrapper의 별도 UI/gateway 필드를 보존한다. wrapper는 EngineRuntime 소유 실행 상태의 **같은 Arc**를 보유하고 한 방향 Deref한다. EngineRuntime의 overrides/restart는 engine 부분만 가진다. 예를 들어 `shared.overrides.model_overrides`와 `shared.engine.overrides.model_overrides`는 같은 map이고, model_picker_pending은 첫 경로에만 있다. replay_restart(OwnedRuntimeRoot), gateway_lease_diagnostics, gateway_waiter_cancellation을 engine 타입으로 복사하지 않는다. deferred_hook_channels는 BackstopSlot 저장 선언만 아래에 닫을 수 있을 때 이전하며 wake/Drop guard는 상위에 남긴다. cfg(test) request_acks의 지원 타입도 같은 compile view에서 닫지 못하면 그 map 전체는 wrapper에 남긴다.

P0-E에서 완료 값과 sender를 이전하고, **메일박스 조회·cache 관측처럼 봇 IO가 없는 실제 함수만** `&EngineRuntime`으로 바꾼다. `session_transition_completion`, finalizer, health_registry, reaction/view 등 혼합 실행 객체는 SharedData를 계속 받는다. 이 조각은 그 실행 객체를 추상화한 G05/G07/G08 port를 새로 만들지 않는다. 전체 엔진 이동은 후속 계획이며, 그때 이동할 함수에 SharedData를 다시 넣는 우회는 허용하지 않는다.

P0-E의 첫 실제 engine borrow 대상은 `src/services/discord/mod.rs:1146`의 has_runtime_storage와 `src/services/discord/mod.rs:1163`의 mailbox_snapshots_for_idle_detector, `src/services/discord/session_transition.rs:165`의 session_transition_lock 및 acquire_session_transition이다. 순수 조회/락 획득 본체는 EngineRuntime inherent impl로 옮기고 SharedData에서는 필요 시 같은 호출만 전달한다. 락 획득의 반환값 SessionTransitionBusy와 SESSION_TRANSITION_LOCK_WAIT_TIMEOUT(기존 session_transition 4/8행)은 legacy_state의 아래 값 계약으로 함께 소유 이전하고 기존 경로에 같은 이름으로 재수출한다. engine helper가 그 상수를 얻으려고 Discord 모듈을 import하지 않는다. session_transition_completion의 SharedData identity/Drop wake는 유지한다. helper를 부르는 engine 함수는 &EngineRuntime으로 좁히되 mixed caller에 &SharedData를 다시 cast하는 API를 만들지 않는다.

### 타입 폐쇄와 가시성의 구체 경계

EngineRuntime이 mailboxes(application의 ChannelMailboxRegistry)를 **직접** 보유한다. adapter로 분류한 legacy_state에 그 registry를 넣으면 adapter→application 새 역의존이므로 금지한다. session 값/legacy map storage는 adapter, 실행 cluster는 application, 완료 값은 vocabulary로 각각 더 구체적인 rule을 둔다. EngineSession의 HistoryItem 정본은 `src/ui/ai_screen.rs:7`(foundation override), PendingUploads는 `src/services/cluster/attachment_transfer/uploads.rs`(vocabulary)라 session 저장은 adapter 이상이어야 한다.

P0-C의 최소 닫힘은 TmuxWatcherHandle, ReservationRecord, TmuxWatcherRegistry 저장 필드, TmuxRelayCoord, FrontierResetState, DeliveryLeaseKey, DeliveryLeaseCell→DeliveryLeasePayload→LeaseState/WatcherReservation 및 그 값 의존이다. `src/services/discord/delivery_lease_cell.rs:295`의 payload mutex와 `src/services/discord/tmux_watcher_registry.rs:209`의 private maps를 pub로 풀어 아래 struct를 상위 impl에서 만지는 방식은 권고하지 않는다. private 필드를 읽는 순수 inherent impl도 **같은 아래 소유 모듈**로 옮기고, IO/SharedData가 필요한 기존 impl은 공개된 기존 목적별 연산만 호출하도록 나눈다. 새로운 remove/kill 연산을 열지 않는다. 이 폐쇄가 15파일 또는 +1200 예상선을 넘으면 P0-C를 저장 값/순수 impl의 두 조각으로 나눠야 한다. bound/수명/visibility를 확인하지 않은 상태에서 “값 선언만 옮기면 컴파일”이라고 완료 판정하지 않는다.

이 지점은 소스 정적 대조를 통한 계획이며 코드 스파이크/컴파일 확인은 하지 않았다. 특히 private registry/fence 접근과 test-only BackstopSlot 지원 타입 때문에 예상보다 추가 조각이 필요할 수 있다. 그 경우 표의 5/2/9는 확정 실행 수가 아니라 리뷰 기준 추정이고, 코디네이터에게 DESIGN_DELTA로 보고한다.

### 층 래칫

현재 층은 `scripts/audit_maintainability/layers.toml:392`의 vocabulary 0 < foundation 1 < adapter 2 < application 3 < discord 4 < assembly 5다. 구체 규칙은 domain::outbound=0(44행), message_split=1(34행), services::discord=4(116행), turn_lifecycle=3(265행), tui_o=4 및 자식 override들이다.

새 `services::engine_runtime`은 application, legacy_state는 adapter로 **실제 타입 의존에 맞게** 등록한다. 단순 역의존 숨김을 위한 application→discord 재분류나 baseline 상향은 하지 않는다. 새 app struct의 필드/제네릭/alias/impl bound 전체가 아래 층에서 닫혀야 한다. 이전한 값 선언은 옛 Discord 경로로 재수출하되 canonical target이 새 아래 모듈이어야 한다. 아래 모듈의 `use super::*`가 Discord root를 끌어오는 이동은 불허한다.

각 구현 조각은 module_layers의 source/target/kind/count record를 base와 비교해 **새 역의존 record=0, 기존 record count 증가=0**을 확인한다. 총량이 줄었다고 새 backedge를 상쇄하지 않는다. logical owner/prod+test view/미해석 edge도 따로 대조한다. baseline은 공식 module_layer_baseline 생성기로 감소분만 동기화한다. 이 문서 레그는 mapping/baseline을 쓰지 않는다. 새 모듈 경계 때문에 기존 되먹임의 source 이름만 바뀌어도 기존과 같은 결함으로 자동 면제하지 않는다.

### 보유 방식 두 안

| 안 | 흐름·비용 | 위험과 선택 |
|---|---|---|
| SharedData가 EngineRuntime 보유(권고) | 기존 bootstrap 한 곳의 생성 흐름과 provider별 1:1 생명주기 유지. 호환 Deref로 수백 caller 수정 억제. 이동 함수만 명시적 engine borrow | wrapper/Deref를 영구 API로 굳히기 쉬움. 기존 SharedData identity와 engine allocation의 1:1을 시험으로 고정. 내부 SDK 저장은 transitional |
| EngineRuntime을 별도 Arc로 주입 | assembly가 engine과 bot을 각각 만들고 worker/registry/relay entrypoint마다 직접 주입. 나중에 여러 메신저 adapter가 engine을 공유하기 쉬움 | constructor·worker·weak registry·retry closure까지 변경 범위가 커짐. 동일 provider/token namespace의 다른 engine 오주입, 순환 Arc, transition lease owner 판정 회귀 위험. P0 선행에서는 비용에 비해 이득이 작음 |

별도 주입안으로 바꾸어도 현재가 하지 않는 provider 합치기/멀티봇 상태 공유는 이번에 하지 않는다. 코드 스파이크는 없다.

## 3. P1: 대화 키와 메시지 참조

새 vocabulary 값은 `domain::conversation::ConversationId(u64)`와 `MessageRef { conversation: ConversationId, raw: String }`로 정한다. ConversationId는 내부 키이고 Discord snowflake라는 의미를 부여하지 않는다. `new/get`, Copy/Eq/Hash/Ord/Debug를 제공한다. 기존 raw u64의 0 sentinel은 기존 경로에 그대로 두며, 신규 송신 경계에서는 유효 대화 키를 검사해 0을 transport에 보내지 않는다. ConversationId 자체의 생성은 내부 값 표현이므로 0을 저장할 수 있고 송신 가능 여부와 구분한다. 신규 serde는 transparent u64 숫자, MessageRef는 두 필드의 신규 값 표현으로만 정의한다. 기존 snowflake string serde와 SQL Type impl은 교체하지 않는다. MessageRef는 문자열 ts·opaque ID를 parse_u64 없이 보존한다. Discord adapter에서만 MessageId/OutboundMessageId와의 fallible 변환을 한다. 요청 conversation과 서버가 반환한 conversation/message를 구별한 receipt를 잃지 않는다.

**ChannelId를 새 타입의 별칭으로 교체하지 않고 변환으로 잇는다.** Discord 아래 `id_bridge`에 명시적 함수 `conversation_from_discord(ChannelId) -> ConversationId`와 nonzero 검증을 포함한 `discord_channel_from_conversation(ConversationId) -> Result<ChannelId, IdError>`를 둔다. SDK에 새 From/TryFrom impl을 추가하지 않아 기존 generic `.into()`의 후보 집합도 그대로 둔다. u64 동등 mapping은 기존 Discord 경로에서만 identity다. domain에서 Serenity를 import하지 않는다. 기존 OutboundChannelId/OutboundMessageId는 `src/domain/outbound/ids.rs:8`의 snowflake wire 계약을 계속 소유한다.

| 컴파일 단위 | 변경 | 기존 ChannelId/DB/serde가 그대로인 근거 |
|---|---|---|
| vocabulary 새 파일 | 중립 새 타입/값만 추가 | 기존 import 대상과 trait impl set을 바꾸지 않음. 신규 serde는 별도 새 wire에만 사용 |
| Discord id_bridge | 명시적 변환 함수와 실제 사용 경계 추가 | 기존 SDK의 trait impl 후보 집합도 변경 0. 새 함수 호출에만 새 타입이 관여하며 orphan 규칙을 우회하는 impl도 없음 |
| 기존 모든 ChannelId 모듈 | **import·형·impl 변경 0** | 여전히 Serenity::ChannelId; new/get, serde, method resolution, Eq/Hash, sqlx bind 전 `.get()` 등 기존 식이 동일 |
| 기존 channel_id:u64/SQL·파일 wire | 변경 0 | migration/DB 바인딩·serde 명명/형식·키 생성식을 바꾸지 않음. 새 MessageRef를 옛 wire 필드에 덮어쓰지 않음 |
| 새 MessengerOutbound caller | 경계에서 명명한 변환 함수 호출 | 새 API만 ConversationId/MessageRef. 기존 string outbound IDs는 adapter에서 변환 오류를 기존 error 분류로 환원 |

직접 type alias 교체는 `ChannelId::new/get`의 nonzero panic, 외부 impl/SDK 메서드, 문자열 snowflake serde를 같은 것으로 보장하지 못한다. 그래서 “새 wrapper가 layout상 u64”를 기존 580파일 무수정 근거로 쓰지 않는다. 무수정 근거는 **기존 타입 자체가 바뀌지 않는 것**이다. 이 레그에서는 빌드 증명이 아니라 소유/형 해석 논증을 제공하며, P1 구현 조각에서 모든 target check와 serde/DB compatibility 시험으로 확인해야 한다.

고유 ID↔내부 키 대응표의 미래 소유자는 `services::messenger_adapters::<messenger>::conversation_keys` adapter repository다(이번 미구현). key에는 installation/workspace/tenant, 외부 channel, thread root를 포함한다. Slack `(channel, thread_ts)`를 별도 내부 대화로 매핑할 자리를 마련한다. 전역 내부 키 allocator/충돌 회피·재시작 복원·migration 정책은 Slack 도입 설계에서 결정하며, 지금 임의 hash를 만들어 기존 Discord 키와 섞지 않는다. MessageRef.raw는 메시지 식별자를 잃지 않지만 기존 inflight의 u64 message_id wire를 해결하지는 않는다. 그 저장 wire migration은 이번 범위 밖이다.

## 4. P2: 송신 trait 하나와 실행 확장

새 정본은 `domain::outbound::port::MessengerOutbound: Send + Sync`다. object-safe를 위해 모든 async 메서드는 `Pin<Box<dyn Future<Output=...> + Send + 'a>>`를 반환한다. 현재 DiscordOutboundClient의 async_fn_in_trait를 이름만 바꿔 dyn으로 쓰지 않는다. self와 빌린 body/opts의 수명을 같은 `'a`로 묶는다. 기존 generic delivery는 `T: MessengerOutbound + ?Sized`를 받아 불필요한 Arc/box task를 만들지 않는다.

기본 메시지 주소는 ConversationId/MessageRef, SendOpts는 reply_to, idempotency_key, enforce_idempotency, notice 정책을 명시한다. UserRef는 신규 messenger_types 안의 opaque raw 문자열 값으로 정의하고 open_dm에서만 사용한다. 기존 OutboundUserId snowflake wire는 교체하지 않는다. 원래 transport 6개 중복 post variant는 send_message 옵션으로 모으되 **실제 client별 default fallback**을 그대로 dispatch하는 compatibility shim을 먼저 사용한다. 지원하지 않는 nonce가 갑자기 강제되거나 reference가 사라지면 운영 동작 변경이다. send/edit 반환은 MessageRef/Receipt로 모으고 edit의 old `Result<()>` wrapper만 기존처럼 ID를 버린다.

| 기존 trait·메서드(좌표) | 새 정본/보존 경로 | 빠지는 것·의미 조건 |
|---|---|---|
| DiscordOutboundClient::post_message (`src/domain/outbound/port.rs:89`) | send_message(conv, body, default opts) | plain post와 오류 분류 보존 |
| post_message_with_reference (96) | send_message(..., reply_to=MessageRef) | reference_channel/message 쌍 보존, legacy unsupported fallback도 보존 |
| post_message_with_nonce (107) | send_message(..., idempotency_key, enforce_idempotency) | nonce 문자열과 enforce bool을 별도 보존; legacy 무시 default를 caps로 갑자기 거절하지 않음 |
| post_message_with_reference_and_nonce (118) | send_message(..., reply_to + idempotency) | nonce 미지원 client에서도 reference를 잃지 않는 dispatch 분기 |
| edit_message (136) | edit_message(conv, message, body) → MessageRef | target/returned ID 및 typed length/transient error 유지 |
| resolve_dm_channel (150) | open_dm(user_ref) → ConversationId | 미지원 Err/실제 DM 생성 동작 보존 |
| TurnGateway::output_pool (`src/services/discord/gateway.rs:60`) | 기존 TurnContext의 관측 accessor | 송신 caps가 아님. pool handle/None 유지 |
| has_raw_output_boundaries (64) | TurnContext의 관측 accessor | task-local scope 존재 여부를 static caps로 캐시하지 않음 |
| output_provider (68) | TurnContext 관측 accessor | bot_owner_provider default 위임 보존 |
| send_message (72) | MessengerOutbound::send_message | send wrapper의 policy/dedup/raw boundary를 기존 위치에서 실행 |
| send_notice_once (79) | send_notice_once(conv, body, opts) | caller latch를 소비한 뒤 시도; transport가 새 dedup/latch를 발명하지 않음 |
| send_long_message_with_rollback (87) | send_long_message_with_rollback(conv, anchor:MessageRef, body, opts) | 순차 분할·역순 삭제·RollbackIncomplete·부분 성공 기록 유지 |
| edit_message (148) | MessengerOutbound::edit_message + 결과 ID discard wrapper | 기존 Result<(),String> caller의 오류 의미 보존 |
| delete_message (155) | delete_message(conv,msg) | 기존 fake의 default Ok는 fake에서만 명시. production에서 신규 무조건 Ok를 도입하지 않음 |
| replace_message_with_outcome (163) | replace_message(..., Immediate) | EditedOriginal/SentFallbackAfterEditFailure/PartialContinuationFailure 의미를 중립 ReplaceOutcome에 1:1 대응 |
| replace_message_deferred (171) | replace_message_deferred(...) | typed edit 실패를 range owner에게 돌려주고 권한 재검증 전 fallback 금지 |
| schedule_retry_with_history (184) | 기존 Discord TurnContext 실행 확장에 동일 메서드 | 송신에서 제외. 같은 future/await, retry permit·history·cancel authority 유지 |
| schedule_retry_with_history_with_completion (193) | 동일 실행 확장 | completion_tx의 관측 시점 보존; scheduling 완료와 실제 turn 완료를 합치지 않음 |
| schedule_exact_retry_with_history (208) | 동일 실행 확장 | ExactRetry/ExactHandoff/봉인된 permit을 vocabulary로 내리지 않음 |
| dispatch_queued_turn (224) | 동일 실행 확장 | Intervention·DispatchLease의 소비/반환/queue custody 유지 |
| validate_live_routing (233) | 동일 실행 확장; C1에는 검증 결과만 주입 | thread/category/directory IO는 송신 능력이 아님 |
| requester_mention (238) | TurnContext 렌더링 accessor | Discord mention 서식은 adapter 소유 |
| can_chain_locally (240) | TurnContext 실행 accessor | leader admission 권한을 capabilities로 대체하지 않음 |
| can_deliver_directly (243) | TurnContext 실행 accessor | REST delivery와 queue chain 권한 차이·기존 default 보존 |
| bot_owner_provider (247) | TurnContext 관측 accessor | 동일 provider binding 유지 |

이로써 TurnGateway **19개(송신 7 + 나머지 12)**와 transport 6개가 모두 대응된다. new 송신 목록(9개)은 capabilities, send_message, send_notice_once, send_long_message_with_rollback, edit_message, delete_message, replace_message, replace_message_deferred, open_dm이다. Reaction/typing/upload는 이 두 trait에 없었다. 그것을 이미 병합했다고 하지 않으며, 실제 C1 함수가 요구하는 reaction/첨부가 있으면 해당 adapter seam의 기존 메서드를 추가하는 별도 작은 조각으로 재견적한다. Directory(fetch/history/channel lookup), inbound event/command/interaction, renderer는 #8189 초안의 다른 port이며 이번에 dormant API로 추가하지 않는다.

TurnGateway는 과도기 `TurnContext: MessengerOutbound` 실행 확장으로 바꾸고 기존 이름 재수출을 잠깐 둔다. 송신 전용 generic/dyn caller는 MessengerOutbound만 받는다. 실제 retry/queue와 output_pool/raw boundary/provider 관측이 필요한 caller는 TurnContext를 받는다. 특히 turn_output_controller에서 관측 accessor를 사용하는 함수는 send-only로 오분류해 bound를 지우지 않는다. P2-I에서 옛 두 송신 이름을 정리한다. 최종에도 실행 확장은 남으므로 **trait 전체를 단 하나로 만드는 문자 그대로의 DoD는 미충족**이다. 이 차이를 승인하지 않으면 application 실행 port와 replay 권한 값의 폐쇄를 포함한 별도 설계가 필요하며, P3를 슬쩍 포함해 해결하지 않는다.

새 오류 값은 기존 length/other, HTTP status, platform error code, 원문 detail을 손실 없이 보존한다. 기존 DispatchMessagePostError accessor와 Display 문자열은 호환 wrapper가 유지하고 watcher의 문자열 기반 실패 분류가 바뀌지 않게 한다. ReplaceOutcome은 `src/services/discord/formatting/replace_long_message.rs:46`의 세 variant와 replacement_anchor, sent/total/index, sent_continuation IDs, cleanup_errors, error 전 필드를 보존한다. MessageId/u64 자리만 MessageRef로 변환하고 DeferredReplace의 Edited/EditFailed도 그대로 둔다. 새 반환은 ReplaceReceipt { outcome, last_chunk_anchor: Option<{ message: MessageRef, text: String }> }로 정해 마지막 chunk anchor/text를 enum과 분리 보존한다. 기존 wrapper는 필요한 결과만 꺼내고 EditedOriginal unit variant의 matches 의미를 유지한다. partial continuation을 bool 성공/실패로 줄이지 않는다. 마지막 chunk anchor와 그 chunk 자체 text를 넘기는 out-param 의미도 그대로 둔다.

capability는 단일 nonce bool이 아니라 Plain/Reference/Nonce/ReferenceAndNonce 모드 각각의 Native/LegacyFallback/Unsupported를 표현해야 한다. SerenityReference의 nonce 단독 default와 reference+nonce override 차이가 대표 반례다. 기존 legacy fallback은 그대로 연결하고 새 messenger client의 unsupported 정책은 해당 도입 설계에서 정한다.

과도기 bridge는 `LegacyTransportAdapter<'a, T>`와 `LegacyTurnAdapter<'a, T>`라는 명시적 wrapper를 사용한다(제안 타입 이름). TurnGateway 쪽은 이미 GatewayFuture가 Send를 보장하므로 generic 위임이 가능하다. 반면 DiscordOutboundClient의 native async trait은 Send+Sync인 self만 보장하고 **반환 future의 Send는 보장하지 않는다**. 따라서 transport wrapper struct를 generic하게 선언하더라도 MessengerOutbound impl은 다섯 concrete prod client별로 작성해 concrete future의 Send를 확인한다. mock도 필요한 concrete impl을 그 시험 조각에서 작성한다. `impl<T: DiscordOutboundClient> MessengerOutbound for T`라는 무조건 blanket impl은 쓰지 않는다. target이 서로 다른 명시 wrapper라 중복 impl도 피한다. 소유 future가 필요한 곳은 같은 target Arc를 소유하는 wrapper를 쓴다. 부모 caller가 아직 old trait bound인 동안에는 호출 경계에서 wrapper를 만들어 새 leaf에 전달한다. 범위가 닫힌 뒤 bound를 바꾸므로 C/D/E/F/G의 일부 전환만으로도 나머지 old caller의 trait 요구가 깨지지 않는다. scope를 wrapper 생성 시점으로 선관측하지 않으며 요청 future 실행 위치에서 기존 helper를 호출한다.

### 4.1 C1 인자 전환의 순서와 중간 상태

먼저 transport adapter를 새 trait에 연결하고 실제 prod caller 한 곳에서 사용한다(P2-A/B). 그 다음 송신 전용 TurnGateway caller, borrowed HTTP 전달 C1 leaf, 그 부모 순으로 바꾼다. C1 함수의 본문에서 기존 Discord 송신 helper를 부르는 곳은 adapter 구현으로 그 helper를 그대로 위임한다. 요청 옵션·rate wait·lease 검사·census scope가 wrapper 바깥으로 밀려나면 안 된다. 기존 타입 전용 builder/Context를 계속 요구하는 함수는 C2/A로 표에 남긴다.

`&Arc<Http>`에서 clone 후 tokio::spawn 하는 경로는 `&dyn`만으로 'static을 만들 수 없다. **소유 worker/closure는 Arc<dyn MessengerOutbound>를 받고 내부 leaf만 &dyn으로 borrow**한다. trait에 raw_http/downcast/SharedData accessor를 넣지 않는다. 단순히 빌림을 spawn으로 우회하지 않는다. Option<&Arc<Http>>는 Option<&dyn MessengerOutbound>로 같은 None 의미를 보존한다. 현재 fallback 선택은 bootstrap/Discord adapter가 하며 엔진은 새 Http를 생성하지 않는다.

C1에 routing/read/history/reaction 등이 함께 필요하면 outbound 인자 하나로 표현할 수 없음을 표에서 구분한다. 검증 결과나 명시적 기존 실행 callback을 같은 await 위치에서 주입할 수 없는 경로는 P3 이후로 보류한다. 이는 원본 classify.tsv 부재와 별도로 **P2 단일 송신 port만으로 모든 HTTP pass-through 함수를 지울 수 없다는 실제 경계**다. 아래는 누락 확인용 superset이며 보류를 C1 완료 수에 세지 않는다.

### 4.2 capabilities와 현재 길이 정책

Caps는 메시지 transport의 정적 능력이다. `max_message_units=2000`, `unit=Utf16CodeUnits`, `safe_split_units=1900`, `repair_margin_units=10`은 현재 Discord 구현의 값으로 명시한다. UTF-16은 `src/domain/message_split.rs:9`에 적힌 **저장소의 보수적 정책**이며 외부 Discord 보장으로 주장하지 않는다. `.len()` bytes나 `.chars().count()` scalar로 대체하지 않는다. split budget과 hard cap을 합치지 않는다.

edit/delete/reference/open_dm, idempotency(None/BestEffort/EnforcedNonce), rollback delete, deferred_replace 지원도 client별로 다르게 반환한다. 지원 조합은 §1.2의 실제 override 표를 따른다. route leadership, output_pool, raw-output-boundaries, provider binding은 caps에 넣지 않는다. rate-limit 시간/성능 수치를 관측 없이 invent하지 않고 기존 adapter rate_limit_wait를 유지한다. reactions/threads/components/upload는 이번 새 trait의 실행 경로가 생길 때만 caps에 추가한다.

splitter는 기존 fence/semantic boundary 순서를 유지한 채 limit/units 정책을 인자로 받는 순수 kernel을 추가한다. 기존 Discord wrapper는 같은 기본값으로 호출한다. 1900/1990/2000을 모두 2000으로 일괄 치환하면 분할 결과·nonce chunk key·멱등이 바뀌므로 금지한다. 테스트는 ASCII, 한글, surrogate pair emoji, exact/over hard cap, code fence, reference+nonce 조합, 중간 chunk 실패와 rollback 실패를 고정한다.

정의 좌표는 `src/domain/message_split.rs:5`(2000), `src/domain/message_split.rs:7`(1990), `src/services/discord/outbound/decision.rs:24`(hard 2000), `src/services/discord/outbound/decision.rs:26`(safe 1900), `src/services/discord/formatting.rs:16`(margin 10), `src/services/discord/http.rs:8`(hard cap alias), `src/services/discord/tui_task_card.rs:38`(limit alias)다. 600/300 등의 status byte budget, select menu 25/100, context preview 360/1000은 메시지 transport hard cap과 다르므로 같이 caps로 옮기지 않는다.

길이 관련 기호의 전체 위치는 git grep 위 8개 상수/함수 계열의 **290행, 43파일**이다(정의·재수출·주석·inline/별도 시험 포함). 원본의 '엔진 30파일'과 다른 집계다. 아래 행 번호는 범위로 생략하지 않았다. 새로 옮기는 엔진 호출부만 caps 인자로 바꾸며 봇 renderer/HTTP와 기존 시험 wrapper는 Discord 기본값을 유지한다.

| 파일 | 모든 grep 행 |
|---|---|
| `src/domain/message_split.rs` | 5, 7, 25, 29, 55, 63, 64, 65, 66, 70, 114, 123, 150, 208, 215, 221 |
| `src/domain/message_split/literal_code_block.rs` | 1, 7, 9, 14, 35, 49 |
| `src/services/discord/formatting.rs` | 8, 16, 203, 204 |
| `src/services/discord/formatting/delivery.rs` | 299, 363, 374, 376, 388, 487, 488, 494, 499, 510, 521, 523, 524, 533, 534, 547, 562, 570, 589 |
| `src/services/discord/formatting/footer_retirement_tests.rs` | 20, 26, 28, 39, 54 |
| `src/services/discord/formatting/status_panel_v2_formatter_tests.rs` | 15, 16, 39, 40, 43, 49, 60, 65, 69, 434, 452, 476, 507, 518, 631, 632 |
| `src/services/discord/formatting/streaming_status.rs` | 25, 32, 36, 55, 75, 77, 79, 80, 99, 100, 836, 841, 846, 851, 856, 875 |
| `src/services/discord/gateway.rs` | 943, 1080 |
| `src/services/discord/http.rs` | 8, 61, 62, 65, 361, 368, 369, 381, 383, 385, 386, 405, 406 |
| `src/services/discord/inflight/model.rs` | 588 |
| `src/services/discord/mod.rs` | 437 |
| `src/services/discord/outbound/decision.rs` | 14, 24, 26, 42, 43, 44, 157 |
| `src/services/discord/outbound/delivery.rs` | 14, 700, 707, 709, 713, 726, 1217 |
| `src/services/discord/outbound/delivery_record.rs` | 665 |
| `src/services/discord/outbound/manual_delivery.rs` | 365, 452 |
| `src/services/discord/outbound/manual_delivery/discord_units_tests.rs` | 7, 103, 120, 164 |
| `src/services/discord/outbound/manual_delivery/image_delivery.rs` | 48 |
| `src/services/discord/outbound/mod.rs` | 46, 47 |
| `src/services/discord/placeholder_live_events/status_panel.rs` | 720, 749, 755, 759, 761 |
| `src/services/discord/placeholder_live_events/tests.rs` | 729, 743, 748, 751, 772, 787, 790, 797, 813, 8690 |
| `src/services/discord/placeholder_live_events/tool_activity.rs` | 526 |
| `src/services/discord/session_relay_sink/tests.rs` | 263, 3858, 3862, 3864, 3868 |
| `src/services/discord/single_message_panel.rs` | 12, 108, 112, 119, 120, 138, 159, 172, 177, 178, 184, 192, 193, 194, 380, 381, 386, 391 |
| `src/services/discord/single_message_panel_tests.rs` | 1, 40, 349, 394, 433, 508, 514, 521, 523, 538, 556, 573, 579, 593, 598, 599, 698, 710, 711, 713, 714, 2206 |
| `src/services/discord/standby_relay.rs` | 1462, 1470, 1738, 1742, 1744, 1748 |
| `src/services/discord/task_notification_delivery/tests.rs` | 2245, 2354, 3249 |
| `src/services/discord/tmux_placeholder_suppression/mod.rs` | 8, 70, 74, 76, 77, 78, 84, 85, 86, 92, 93, 1312, 1332, 1333 |
| `src/services/discord/tmux_placeholder_suppression/ops.rs` | 415 |
| `src/services/discord/tmux_placeholder_suppression/unicode_units_tests.rs` | 5, 10, 15, 24, 39, 48 |
| `src/services/discord/tmux_watcher/completion_gate_tests.rs` | 1101 |
| `src/services/discord/tmux_watcher/session_bound_ack_tests.rs` | 713, 728 |
| `src/services/discord/tmux_watcher/single_message_footer.rs` | 399, 450 |
| `src/services/discord/tmux_watcher/tests.rs` | 2634, 2636, 3987, 3988, 4205, 5215, 5226, 5230, 5231 |
| `src/services/discord/tui_task_card.rs` | 23, 38, 472, 476, 480, 482, 487, 780, 784, 935, 1055 |
| `src/services/discord/turn_bridge/response_delivery.rs` | 135, 201, 202, 213, 223 |
| `src/services/discord/turn_bridge/retry_state.rs` | 774, 782 |
| `src/services/discord/turn_bridge/single_message_footer.rs` | 514, 806 |
| `src/services/discord/turn_bridge/status_panel_tests.rs` | 1156, 1157 |
| `src/services/discord/turn_bridge/terminal_controller_cutover.rs` | 1857, 1858, 2187, 2261, 2262, 2324 |
| `src/services/discord/turn_bridge/terminal_delivery.rs` | 1369, 1379, 1389, 1438, 2191, 2198, 2202, 2203 |
| `src/services/discord/turn_end_wip_warning.rs` | 186, 198, 202, 204, 205, 207, 224, 281, 284, 285, 312, 313, 326, 327, 343, 344, 351, 360, 362, 367, 374 |
| `src/services/tui_o/shadow/recap.rs` | 6, 28, 33 |
| `src/services/tui_o/shadow/unit_plan.rs` | 8, 33 |

숫자 literal `2000/2_000/1900/1_900/1990/1_990` 검색으로 별도 상수도 확인했다. 위 기호 집합 밖의 송신/표시 budget은 아래에 전수 기호 위치를 보완한다. server escalation과 help/status renderer는 이번 C1 엔진 이동의 대상이 아니며 기존 Discord 기본 budget을 유지한다.

| 추가 기호/파일 | 전체 행 | 처리 |
|---|---|---|
| `src/server/routes/escalation.rs` | 10, 396, 400 | Discord renderer/상위 조립 owner 유지 |
| `src/services/discord/commands/help.rs` | 6, 65 | Discord renderer/상위 조립 owner 유지 |
| `src/services/discord/placeholder_live_events/common.rs` | 19 | Discord renderer/상위 조립 owner 유지 |
| `src/services/discord/placeholder_live_events/status_panel.rs` | 5, 721, 750, 755, 761 | Discord renderer/상위 조립 owner 유지 |
| `src/services/discord/placeholder_live_events/tests.rs` | 7, 477, 748, 752, 772, 787, 788, 791, 798, 800, 809, 814, 829, 832, 6300, 6518, 8578, 8580, 8586, 8603, 8607, 8736, 9968 | Discord renderer/상위 조립 owner 유지 |
| `src/services/discord/placeholder_live_events/tool_activity.rs` | 527 | Discord renderer/상위 조립 owner 유지 |

`src/services/discord/prompt_builder/system_prompt_audit.rs:5`의 BODY_LIMIT=2_000-8은 진단 code block 출력 budget이며 hard cap과 구분한다. `src/services/discord/prompt_builder/manifest.rs:25`의 CURRENT_TASK_REDACTED_PREVIEW_MAX_BYTES=2_000은 bytes preview라 UTF16 hard cap과 합치지 않는다. 그 밖의 숫자 동형은 timeout·schedule·timestamp·test fixture여서 메시지 caps로 옮기지 않는다. 원문 literal 검색도 별도 로그에 남겼다.

### #8189의 Slack 장애 여섯 가지와 이번 종료선

| 장애 | 이번에 하는 것 | 이번에 남는 것 |
|---|---|---|
| 봇=provider | EngineRuntime에 실행 provider/state 정본을 둠 | 봇별 1:1 배치/lease·multi-provider 재조립 |
| 문자열 대화/스레드/message ts | 새 내부 ConversationId/MessageRef·adapter 변환 | 지속 대응표와 기존 u64 message wire migration |
| nonce 기반 중복 억제 | 모드별 지원·기존 key/enforce/fallback 보존 | 다른 메신저의 자체 원장/실제 exactly-once 보장 |
| edit 스트리밍/길이 | UTF16/limit/margin caps와 기존 분할 결과 보존 | 다른 메신저의 edit 요율/renderer/스케줄링 정책 |
| 명령/컴포넌트 | 송신 port에 섞지 않고 기존 Discord owner 유지 | inbound/interaction ack 및 명령 본체 분리 |
| tui_o가 discord 층 | 기존 canonical 값/송신 seam 재사용 | tui_o writer의 전체 중립화/층 하향과 engine 이동 |

## 5. 리뷰 조각 계획

새 파일은 모두 제안 경로다. prod 수에는 생성/수정하는 소유 파일을 포함하고 시험 파일은 별도다. 예상 상한은 구현 착수 시 부모 diff로 다시 잰다. **+1200 또는 prod 30 초과가 보이면 commit 전 더 나눈다.** giant 파일의 총 줄 수 때문에 금지하지 않는다. 새로운 giant/visibility/unused 경고는 별도 래칫에 맞춘다.

아래 P0 5개/P1 2개/P2 9개, **총 16개, 순증 +1,610~2,890** 추정이다. 구현 중 타입 폐쇄나 소스 스캔 시험 때문에 더 늘어날 수 있다. 원래 2~3/1~2/2~3 예상보다 많아진 이유는 두 타입 군의 혼합과 trait caller/HTTP 전달 caller의 합집합이다. 엔진 물리 이동·P3·CUT 비용은 이 합계에 포함하지 않는다.

표에서 ER은 제안 `src/services/engine_runtime.rs` 본체, L은 `scripts/audit_maintainability/layers.toml`이다. 나머지는 저장소 파일 경로다. '값 추출'은 기존 파일에서 선언만 새 파일로 소유 이전하고 상위 impl은 그대로 두는 뜻이다. 생성 baseline 변경은 표의 prod 수와 별도로 공식 스크립트만 사용한다.

| 조각 | 소유 파일/군(§5.1의 정확한 caller 배분 포함) | prod 예상 / 순증 | 선행 | 논리 이동 #[path] | 중간 상태의 안전성과 대상 시험 |
|---|---|---|---|---|---|
| P0-A runtime 기본 | services/mod.rs, ER, src/services/engine_runtime/legacy_state.rs, src/services/discord/mod.rs, src/services/discord/runtime_bootstrap/shared_data.rs, src/services/discord/shared_state.rs, L | 8 / +150~250 | P1-A 권장(독립 생성 가능) | 없음; 새 runtime+값 추출 | 단일 allocation/Deref, 두 constructor 동시 전환. 새 identity/Arc-ptr·pool None/Some·provider/cache 보존 시험 |
| P0-B core/mailbox | ER, src/services/engine_runtime/legacy_state.rs, src/services/engine_runtime/session.rs, src/services/discord/mod.rs, src/services/discord/session_runtime.rs, src/services/discord/session_runtime/worktree.rs, src/services/discord/runtime_bootstrap/shared_data.rs, L | 9 / +100~180 | P0-A | 없음; 값 추출 | CoreState/DiscordSession/WorktreeInfo 값만 이전, mutex 동일. session restore/clear, mailbox registry 초기화, pending uploads 보존 |
| P0-C watcher 저장 폐쇄 | ER, src/services/engine_runtime/legacy_state.rs, src/services/engine_runtime/relay_state.rs, src/services/discord/tmux_watcher_registry.rs, src/services/discord/relay_coord.rs, src/services/discord/relay_health/frontier.rs, src/services/discord/delivery_lease_cell.rs, src/services/discord/delivery_lease_key.rs, src/services/discord/mod.rs, src/services/discord/runtime_bootstrap/shared_data.rs 및 registry의 fences/owner_observation/session_binding 타입 참조, L | ≤15 / +120~220 | P0-B | 없음; 전체 모듈 #[path] 금지 | 값/identity/fence 상태만 이전, claim/remove/CAS 구현 그대로. registry identity/delivery fence·relay offset·lease 경합/rotation 시험 |
| P0-D 혼합 cluster 분리 | ER, src/services/engine_runtime/execution_state.rs, src/services/engine_runtime/legacy_state.rs, src/services/discord/shared_state.rs, src/services/discord/mod.rs, src/services/discord/runtime_bootstrap/shared_data.rs, src/services/discord/queue_io/backstop.rs, L | ≤9 / +80~160 | P0-A | 없음; 값 추출 | override UI/restart CLI는 wrapper에, engine maps 동일 Arc. model picker isolation·reset pending·restart counter/shutdown slot·backstop wake 불변 시험 |
| P0-E bus와 engine borrow | src/services/engine_runtime/events.rs, ER, src/services/discord/inflight.rs(값 enum만), src/services/discord/turn_completion_events.rs, src/services/discord/mod.rs, src/services/discord/runtime_bootstrap/shared_data.rs, src/services/discord/session_transition.rs(map lookup만), src/services/discord/relay_health/frontier.rs(순수 관측만), L | ≤12 / +50~120 | P0-B/C/D | 없음 | sender/capacity·동일 signal·다음 wake 순서 유지. session_transition 4개 기존 시험, completion phase/lag·mailbox snapshot/cache 시험; wake 구현은 이동하지 않음 |
| P1-A 중립 vocabulary | domain/mod.rs, domain/conversation.rs, src/services/discord/id_bridge.rs, src/services/discord/mod.rs, src/services/discord/outbound/message.rs | 5 / +100~160 | 없음 | 없음 | 실제 boundary 변환 caller 하나를 같은 조각에 연결. ID roundtrip, zero send rejection, arbitrary raw message, snowflake wire unchanged |
| P1-B port 경계 적용 | domain/outbound/ids.rs(시험만), src/services/discord/id_bridge.rs, src/services/discord/outbound/message.rs, src/services/discord/outbound/result.rs, src/services/discord/outbound/serenity_reference.rs | ≤5 / +60~100 | P1-A | 없음 | 기존 SDK 타입/import 0변경. requested/returned receipt·serde string/integer fixture·u64 binding guard 시험 |
| P2-A object-safe bridge | domain/outbound/port.rs, domain/outbound.rs, domain/outbound/messenger_types.rs, src/services/discord/outbound/mod.rs, src/services/discord/outbound/transport.rs, src/services/discord/discord_io.rs, src/services/discord/gateway.rs, src/services/discord/gateway/turn_context.rs, L | ≤11 / +200~350 | P1-B, P0-A | 없음 | 새 trait 실제 caller 사용, 옛 trait compatibility wrapper 유지. dyn compile contract·default fallback·error class·同task census 시험 |
| P2-B 5 구현 전환 | §1.2의 5 prod impl, src/services/discord/formatting/replace_long_message.rs, src/services/discord/gateway/outbound_messages.rs, src/services/discord/outbound/delivery.rs, src/services/discord/discord_io.rs, services/dispatches/discord_delivery/transport.rs, services/issue_announcements.rs, src/services/discord/outbound/mod.rs | ≤12 / +150~250 | P2-A | 없음 | legacy wrapper와 새 객체를 같은 helper에 위임. reference+nonce 4조합·default 능력 차이·DM/typed edit·dedup reservation 시험 |
| P2-C bridge 송신 caller 1 | §5.1 C-set | ≤27 / +100~200 | P2-B | 없음 | generic send/replace bound만 축소; retry/dispatch는 TurnContext 유지. turn_output_controller/fresh_send·deferred authority·bridge retry/receipt 시험 |
| P2-D bridge 송신 caller 2 | §5.1 D-set | ≤21 / +80~150 | P2-C | 없음 | terminal/stream caller 연결, nonce/chunk key·completion latch 유지. terminal_delivery·before_start_stop·stream_tick·foreign_terminal_handoff 시험 |
| P2-E watcher/relay | §5.1 E-set 및 HTTP C1 E군 | ≤20 / +100~180 | P2-C, P0-C | 없음 | 소유 Arc와 borrow 수명 구분, detached spawn 추가 0. relay/sink cfg(test) shim 양쪽·long chunks rollback·watcher fence 시험 |
| P2-F recovery C1 | §5.1 F-set 및 HTTP C1 F군 | ≤22 / +100~180 | P2-B, P0-E | 없음 | state/row removal·exact permit 동작 그대로. recovery outcome/probe/held-debt·idempotent terminal·restore old-wire 시험; #7651와 파일 소유 선결 |
| P2-G bootstrap/잔여 C1 | §5.1 G-set 및 HTTP C1 G군 | ≤21 / +80~150 | P2-E/F | 없음 | Http 생성/route lookup은 adapter에, 기존 None/failure 유지. bootstrap draft/held retry·abandon drain·restart report 시험. directory 필요 함수는 완료에서 제외 |
| P2-H caps/split | domain/message_split.rs, domain/message_split/literal_code_block.rs, src/services/discord/formatting.rs, src/services/discord/formatting/delivery.rs, src/services/discord/outbound/decision.rs, src/services/discord/outbound/delivery.rs, src/services/discord/single_message_panel.rs, src/services/discord/tui_task_card.rs 및 §4.2의 engine caller만 | ≤18 / +60~100 | P2-B | 없음 | 원래 chunk 결과/nonce 동일, 기존 wrapper 기본값 유지. UTF16/fence/1900·1990·2000 boundary·emoji·rollback 시험 |
| P2-I compatibility 정리 | domain/outbound/port.rs, domain/outbound.rs, src/services/discord/gateway.rs, src/services/discord/outbound/mod.rs, §5.1 I-set(잔여 import) | ≤16 / +80~140 | P2-C~H | 없음 | 옛 sender trait 제거 전 prod/test 잔여 grep=0; execution extension 유지. full target check·source-scan readers·clippy no-growth |

모든 조각은 no-renames 기준으로 ≤30이며 제안 순증 최대는 +350이다. closure가 표 소유 파일을 벗어나면 FILES/재분할 결정 후 착수한다. 이 레그의 소유는 설계 문서뿐이므로 위 소스 소유는 **미래 제안**이며 지금 다른 레인 파일을 편집하는 권한이 아니다.

### 5.1 caller 소유 배분의 고정 방법

모호한 glob으로 99파일을 한 조각에 넣지 않는다. 현재 `git grep -l -w TurnGateway -- src` 결과를 경로별로 아래처럼 나누며 같은 파일은 한 조각만 맡는다. P2-B의 16파일 client 검색 목록은 §1.2와 그 직접 caller이며 test-only mock은 같은 조각의 시험 소유에 들어간다. C/src/services/discord/E/F/G 사이에 연동되는 시그니처는 먼저 compatibility trait impl로 두어 각 commit이 닫히게 한다. 아래 파일은 물리 이동하지 않는다.

**A/B-set (3파일, 시험 포함)**

- `src/services/discord/gateway.rs`
- `src/services/discord/gateway/headless.rs`
- `src/services/discord/gateway/headless_exact.rs`

**C-set (34파일, 시험 포함)**

- `src/services/discord/outbound/confirmation.rs`
- `src/services/discord/outbound/confirmation/terminal_notice_tests.rs`
- `src/services/discord/outbound/turn_output_controller.rs`
- `src/services/discord/outbound/turn_output_controller/fresh_send.rs`
- `src/services/discord/outbound/turn_output_controller/fresh_send_tests.rs`
- `src/services/discord/outbound/turn_output_controller/raw_request_tests.rs`
- `src/services/discord/outbound/turn_output_controller/transport.rs`
- `src/services/discord/turn_bridge/bridge_entry_persist.rs`
- `src/services/discord/turn_bridge/context.rs`
- `src/services/discord/turn_bridge/current_message_anchor.rs`
- `src/services/discord/turn_bridge/entry_abort_cold_error_tests.rs`
- `src/services/discord/turn_bridge/entry_abort_cold_error_tests/provider_failure_tests.rs`
- `src/services/discord/turn_bridge/entry_abort_mailbox_tests.rs`
- `src/services/discord/turn_bridge/finalize_epilogue.rs`
- `src/services/discord/turn_bridge/footer_retirement_tests.rs`
- `src/services/discord/turn_bridge/mod.rs`
- `src/services/discord/turn_bridge/panel_lifecycle.rs`
- `src/services/discord/turn_bridge/post_loop_finalize.rs`
- `src/services/discord/turn_bridge/recovery_text.rs`
- `src/services/discord/turn_bridge/resume_pin_tests.rs`
- `src/services/discord/turn_bridge/retry_state.rs`
- `src/services/discord/turn_bridge/single_message_footer.rs`
- `src/services/discord/turn_bridge/status_panel.rs`
- `src/services/discord/turn_bridge/status_panel/provider_failure.rs`
- `src/services/discord/turn_bridge/status_panel_tests.rs`
- `src/services/discord/turn_bridge/stream_loop/content_arms/types.rs`
- `src/services/discord/turn_bridge/stream_loop/exit_reconcile.rs`
- `src/services/discord/turn_bridge/stream_loop/rewind_owner_tests.rs`
- `src/services/discord/turn_bridge/stream_loop/tool_arms/authority.rs`
- `src/services/discord/turn_bridge/stream_loop/tool_arms/authority_tests.rs`
- `src/services/discord/turn_bridge/stream_loop/tool_arms/types.rs`
- `src/services/discord/turn_bridge/stream_loop/types.rs`
- `src/services/discord/turn_bridge/two_message_panel.rs`
- `src/services/discord/turn_bridge/watcher_orphan_cleanup.rs`

**D-set (30파일, 시험 포함)**

- `src/services/discord/turn_bridge/before_start_stop.rs`
- `src/services/discord/turn_bridge/before_start_stop_retry_tests.rs`
- `src/services/discord/turn_bridge/completion_postlude/contracts.rs`
- `src/services/discord/turn_bridge/completion_postlude/legacy_summary.rs`
- `src/services/discord/turn_bridge/completion_postlude/legacy_summary_tests.rs`
- `src/services/discord/turn_bridge/stream_tick.rs`
- `src/services/discord/turn_bridge/stream_tick/footer_retirement_raw_history_tests.rs`
- `src/services/discord/turn_bridge/stream_tick/guarded_persist.rs`
- `src/services/discord/turn_bridge/stream_tick/guarded_persist_tests.rs`
- `src/services/discord/turn_bridge/stream_tick/o_adoption_tests.rs`
- `src/services/discord/turn_bridge/stream_tick/o_panel.rs`
- `src/services/discord/turn_bridge/stream_tick/output.rs`
- `src/services/discord/turn_bridge/stream_tick/rollover_guard.rs`
- `src/services/discord/turn_bridge/stream_tick/routine_stream_tests.rs`
- `src/services/discord/turn_bridge/stream_tick/runtime.rs`
- `src/services/discord/turn_bridge/terminal_controller_cutover.rs`
- `src/services/discord/turn_bridge/terminal_delivery.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/contracts.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/recovery_retry_guard_tests.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/recovery_retry_guard_tests/replay_turn_tests.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/recovery_retry_guard_tests/turn_mode_retry_tests.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/turn_mode_anchor_tests.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/empty_response_recovery/handler.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/prompt_too_long_guidance.rs`
- `src/services/discord/turn_bridge/terminal_outcome_delivery/recovery_retry.rs`
- `src/services/discord/turn_bridge/terminal_receipt_tests.rs`

**E-set (15파일, 시험 포함)**

- `src/services/discord/session_relay_sink.rs`
- `src/services/discord/session_relay_sink/delivery_orchestration_tests.rs`
- `src/services/discord/session_relay_sink/journal.rs`
- `src/services/discord/session_relay_sink/short_controller.rs`
- `src/services/discord/session_relay_sink/task_notification_context.rs`
- `src/services/discord/session_relay_sink/tests.rs`
- `src/services/discord/standby_relay.rs`
- `src/services/discord/tmux_overload_retry.rs`
- `src/services/discord/tmux_watcher/terminal_long_chunks.rs`
- `src/services/discord/tmux_watcher/terminal_send.rs`
- `src/services/discord/tmux_watcher/tests.rs`
- `src/services/discord/tmux_watcher/turn_identity_tests.rs`
- `src/services/discord/tui_prompt_relay/bridge_gateway.rs`
- `src/services/discord/tui_prompt_relay/claude_idle_bridge.rs`
- `src/services/discord/tui_prompt_relay/tests.rs`

**F-set (5파일, 시험 포함)**

- `src/services/discord/recovery_engine/completion_delivery.rs`
- `src/services/discord/recovery_engine/herdr_before_start_restart.rs`
- `src/services/discord/recovery_engine/idle_captured_response.rs`
- `src/services/discord/recovery_engine/restore_inflight/replay_hold_tests/routine_presentation_tests.rs`
- `src/services/discord/recovery_paths/controller_cutover.rs`

**G-set (12파일, 시험 포함)**

- `src/services/discord/inflight/save_store/identity_gate/runtime_stamp/claude_terminal_tests/o_stream_regression_tests.rs`
- `src/services/discord/live_bridge/guard_tests.rs`
- `src/services/discord/placeholder_controller.rs`
- `src/services/discord/router/intake_dispatch/queued/transition_busy_tests.rs`
- `src/services/discord/router/message_handler/headless_turn/process_mailbox/bridge_tests.rs`
- `src/services/discord/router/message_handler/headless_turn/process_mailbox/routine_presentation_tests.rs`
- `src/services/discord/router/message_handler/headless_turn/replay_start.rs`
- `src/services/discord/router/message_handler/intake_turn/first_key_gate_paths_tests.rs`
- `src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs`
- `src/services/discord/turn_end_summary.rs`
- `src/services/discord/turn_end_summary_tests.rs`
- `src/services/discord/watcher_summary_authority_tests.rs`

I-set은 A~G 전환 뒤 old 이름의 잔여 재수출/상위 import만 정리한다. 새 caller가 늘면 grep diff로 해당 set에 추가하며 30파일 상한을 다시 확인한다. §6의 HTTP 표에는 TurnGateway를 직접 쓰지 않는 함수도 있으므로 두 집합을 동일시하지 않는다.

## 6. &Arc<serenity::Http> 인자 전수와 전환/보류 경계

검색은 `git grep -n '&Arc<serenity::Http>' -- src`이며 Option<&Arc<...>>도 포함한다. 여기서는 정확히 이 철자만 센다. &Http, Arc<Http> 소유 인자, 별칭 HTTP client는 이 전수의 분모가 아니다. grep 행마다 직전 fn 선언과 body를 대조했으며 function 선언 줄과 인자 줄을 모두 기록했다. prod/test는 repository의 production token scanner로 보조 분류한다(플랫폼 cfg는 합집합).

표의 S는 송신 leaf라 C1 전환 대상, P는 HTTP 핸들 전달 후보이나 하위 route/read/cleanup callback 폐쇄를 확인한 뒤 전환, A는 봇 adapter, C2는 혼합으로 보류, D는 #7651 책임 이전 경계, T는 test-only다. **P를 무조건 완료 대상으로 세지 않는다.** 원본 C1 분류 전수와 같은 것이라고 주장하지 않는다. S/P의 모든 현재 함수 좌표를 남겨 C1 누락 후보도 검토할 수 있게 한다. A/C2/D는 이 계획에서 물리 이동하거나 HTTP를 강제로 없애지 않는다.

| 파일 | fn:선언 줄 (HTTP 인자 줄) | 판정·조각 |
|---|---|---|
| `src/services/discord/abandon_request_store.rs` | `drain:485` (486) | P → P2-G |
| `src/services/discord/catch_up.rs` | `catch_up_missed_messages:445` (446); `catch_up_missed_messages_for_retry:477` (478) | A |
| `src/services/discord/commands/config/provider_settings.rs` | `feature_command_channel_id:97` (99) | A |
| `src/services/discord/commands/control.rs` | `resolve_session_key_for_clear:151` (152); `reset_provider_session_if_pending:225` (226); `relaunch_on_fast_drift:388` (389); `clear_channel_session_state:490` (491); `clear_channel_session_state_with_session_key:510` (511) | A |
| `src/services/discord/commands/control/provider_reset.rs` | `reset_channel_provider_state:3` (4) | A |
| `src/services/discord/commands/control/relaunch.rs` | `run:232` (233) | A |
| `src/services/discord/commands/restart.rs` | `restart_managed_session:204` (205) | A |
| `src/services/discord/health.rs` | `utility_bot_http_matches_current:933` (935); `cache_utility_bot_user_id_if_current:944` (949) | C2 보류 |
| `src/services/discord/health/recovery/leak_recovery_ledger.rs` | `leak_recovery_fetch_continuation_contents:342` (343) | P → P2-G |
| `src/services/discord/monitoring_status.rs` | `render_channel_monitoring_from_store:244` (245) | A |
| `src/services/discord/outbound/o_delivery_evidence.rs` | `tail:384` (385) | C2 보류 |
| `src/services/discord/outbound/transport.rs` | `post_serenity_message_with_nonce:22` (23) | A |
| `src/services/discord/placeholder_sweeper.rs` | `edit_placeholder_safe:146` (147); `probe_placeholder_state:209` (210); `run_placeholder_sweep_pass:325` (326); `sweep_orphan_status_panel:773` (774) | A |
| `src/services/discord/placeholder_sweeper/tick.rs` | `run_placeholder_sweeper_tick:77` (78) | A |
| `src/services/discord/queue_io/auto_queue_eligibility.rs` | `queued_reach:46` (47) | P → P2-G |
| `src/services/discord/queue_io/transport/tests.rs` | `validate:103` (104) [T] | T |
| `src/services/discord/reaction_cleanup.rs` | `cleanup_recovered_catch_up_hourglass:8` (9) | A |
| `src/services/discord/recovery_engine/completion_delivery.rs` | `relay_captured_recovery_terminal_notice:112` (113); `relay_captured_recovery_terminal_notice_with_gateway:126` (127); `relay_recovery_terminal_notice:181` (182); `relay_recovery_terminal_notice_with_capture:197` (198); `relay_recovery_body_to_placeholder:307` (308); `relay_recovery_body_notice:360` (361); `relay_recovered_terminal_text_to_placeholder:415` (416); `relay_recovered_body_to_placeholder:447` (448); `complete_recovery_visible_turn:599` (600); `complete_recovery_visible_turn_with_sniffer:634` (635) | C2 보류 |
| `src/services/discord/recovery_engine/herdr_admitted_restart.rs` | `settle:68` (70); `settle_checked:83` (85) | P → P2-F |
| `src/services/discord/recovery_engine/herdr_before_start_restart.rs` | `settle_receipt:9` (10); `settle:115` (117) | S → P2-F |
| `src/services/discord/recovery_engine/herdr_before_start_restart_tests.rs` | `settle:94` (96) [T] | T |
| `src/services/discord/recovery_engine/herdr_row_end.rs` | `notice:302` (303); `routed:351` (352) | P → P2-F |
| `src/services/discord/recovery_engine/idle_captured_response.rs` | `recover_idle_partial_response:36` (37); `recover_idle_partial_response_from_ready_source:83` (84) | P → P2-F |
| `src/services/discord/recovery_engine/manual_rebind/live_bridge_guard.rs` | `rebind_inflight_for_channel:3` (4); `rebind_inflight_for_channel_with_minimum_start_offset:27` (28) | P → P2-F |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs` | `rebind_inflight_for_channel_inner:171` (172) | C2 보류 |
| `src/services/discord/recovery_engine/restore_inflight.rs` | `restore_inflight_turns:98` (99); `restore_inflight_states:120` (121) | D 보류 |
| `src/services/discord/recovery_engine/restore_inflight/kickoff_identity.rs` | `dispose_ownerless_row:93` (94) | D 보류 |
| `src/services/discord/recovery_engine/restore_inflight/replay_hold.rs` | `deliver_held_debt:86` (87); `deliver_held_debt_checked:103` (104) | D 보류 |
| `src/services/discord/recovery_engine/routing_orphan.rs` | `route_recovery_skip:39` (40); `cleanup_routing_orphaned_inflight:138` (139) | P → P2-F |
| `src/services/discord/recovery_engine/status_panel_completion_producer.rs` | `complete_recovery_status_panel_with_sniffer:4` (5) | P → P2-F |
| `src/services/discord/recovery_engine/terminal_text_idempotency.rs` | `relay_no_anchor_terminal_text:597` (598) | P → P2-F |
| `src/services/discord/recovery_paths/controller_cutover.rs` | `deliver_recovery_replace_via_controller:119` (123) | P → P2-F |
| `src/services/discord/recovery_paths/restart.rs` | `try_recover_anchor_repost:418` (419); `recover_committed_anchor_repost:672` (673) | P → P2-F |
| `src/services/discord/restart_report.rs` | `send_report:448` (448); `flush_restart_reports:463` (464) | P → P2-G |
| `src/services/discord/router/herdr_consent.rs` | `run:94` (95); `recover:388` (389) | C2 보류 |
| `src/services/discord/router/herdr_consent/startup.rs` | `restore:91` (92); `restore_excluding:99` (100); `restore_only:131` (132) | P → P2-G |
| `src/services/discord/runtime_bootstrap.rs` | `restore_queued_and_inflight_for_tests:26` (27) [T] | T |
| `src/services/discord/runtime_bootstrap/queued_placeholders.rs` | `delete_stale_queued_placeholder_cards:103` (104) | A |
| `src/services/discord/runtime_bootstrap/queued_recovery.rs` | `restore_queued_and_inflight_work:26` (27); `restore_and_hold:57` (58) | P → P2-G |
| `src/services/discord/runtime_bootstrap/queued_recovery_draft_tests.rs` | `run:383` (384) [T]; `catch_up_missed_messages:401` (402) [T]; `delete_stale_queued_placeholder_cards:418` (419) [T]; `flush_restart_reports:436` (436) [T]; `restore_queued_and_inflight_work:442` (443) [T]; `restore_tmux_watchers:461` (461) [T] | T |
| `src/services/discord/runtime_bootstrap/queued_recovery_held.rs` | `spawn_retries:109` (110) | P → P2-G |
| `src/services/discord/session_runtime/channel_routing.rs` | `validate_rest_channel_routing:123` (124); `validate_rest_channel_routing_with_parent:136` (137); `resolve_runtime_channel_binding_status:220` (221); `resolve_channel_name:264` (265); `resolve_thread_parent:276` (277); `checked_thread_parent:296` (297) | A |
| `src/services/discord/startup_reclaim.rs` | `read_reclaim_messages:208` (209); `reclaim_frozen_panels:231` (232); `reclaim_orphan_placeholders:279` (280); `run_startup_reclaim_sweep:361` (362); `run_startup_reclaim_plan:372` (373) | A |
| `src/services/discord/status_panel_orphan_store.rs` | `drain:825` (826) | A |
| `src/services/discord/tmux.rs` | `reconcile_orphan_suppressed_placeholder_for_restored_watcher:1433` (1434) | P → P2-E |
| `src/services/discord/tmux_placeholder_suppression/ops.rs` | `apply_placeholder_suppression:32` (33); `delete_terminal_placeholder:174` (175); `delete_nonterminal_placeholder:196` (197); `delete_nonterminal_placeholder_unless_delivered:219` (220); `delete_terminal_placeholder_unless_delivered:253` (254); `delete_placeholder_unless_delivered:285` (286); `delete_watcher_rollover_frozen_prefixes:440` (441); `delete_placeholder_with_operation:468` (469); `edit_terminal_placeholder:496` (497); `edit_preserve_placeholder:521` (522); `edit_placeholder_with_operation:546` (547) | A |
| `src/services/discord/tmux_restart_handoff.rs` | `start_restart_handoff_from_state:277` (279); `resume_aborted_restart_turn:490` (492) | P → P2-E |
| `src/services/discord/tmux_watcher/completion_producer.rs` | `complete_watcher_terminal_footer_or_status_panel_with_sniffer:44` (48) | P → P2-E |
| `src/services/discord/tmux_watcher/o_evidence_tests.rs` | `plan:92` (95) [T]; `prepare_then_plan:1377` (1380) [T] | T |
| `src/services/discord/tmux_watcher/orphan_status_panel_cleanup.rs` | `cleanup_orphan_external_input_status_panel:31` (32) | A |
| `src/services/discord/tmux_watcher/placeholder_reclaim.rs` | `reclaim_orphan_external_input_placeholder:34` (35) | A |
| `src/services/discord/tmux_watcher/single_message_footer.rs` | `refresh_watcher_completion_footer_if_due:162` (163); `complete_watcher_single_message_terminal_no_footer:196` (197); `complete_watcher_single_message_completion_footer:221` (222); `supersede_watcher_footer:251` (252); `refresh_watcher_registered_completion_footer:266` (267); `complete_watcher_terminal_footer_or_status_panel:283` (284) | A |
| `src/services/discord/tmux_watcher/task_response_authority.rs` | `prepare_watcher_task_response:76` (77); `ensure_watcher_task_card:171` (172); `promote_delegated_task_card:212` (213); `watcher_card_clients:234` (235); `apply_watcher_task_response:283` (284) | A |
| `src/services/discord/tmux_watcher/terminal_direct_fallback.rs` | `apply_watcher_direct_fallback_send:33` (34) | C2 보류 |
| `src/services/discord/tmux_watcher/terminal_long_chunks.rs` | `apply_watcher_long_chunks_controller:410` (411); `apply_watcher_long_chunks_legacy:477` (478); `apply_watcher_long_chunks_result:572` (574) | S → P2-E |
| `src/services/discord/tmux_watcher/terminal_send.rs` | `apply_watcher_short_replace_controller:411` (412) | S → P2-E |
| `src/services/discord/tmux_watcher/two_message_panel.rs` | `adopt_watcher_singleton_panel_after_fresh_bind:258` (259); `reanchor_watcher_two_message_status_panel_below_answer:343` (344) | A |
| `src/services/discord/tui_prompt_relay/synthetic_start_wiring.rs` | `post_suppressed_injection_note:39` (40) | S → P2-G |
| `src/services/discord/turn_bridge/before_start_stop.rs` | `send_stop_reply:274` (275); `reply_and_ack_stop:296` (297) | S → P2-D |
| `src/services/discord/turn_bridge/completion_postlude/legacy_summary_tests.rs` | `watcher:309` (310) [T] | T |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs` | `resume:364` (366) [T] | T |
| `src/services/discord/turn_end_summary.rs` | `post_watcher:292` (293) | S → P2-D |
| `src/services/discord/watchers/lifecycle/liveness.rs` | `handle_tmux_watcher_observed_death:102` (104) | P → P2-E |
| `src/services/discord/watchers/lifecycle/restore.rs` | `restore_selected_tmux_watchers:21` (22) | C2 보류 |
| `src/services/discord/watchers/lifecycle/restore_retry.rs` | `register:60` (60); `restore_tmux_watchers:83` (84); `begin_for_test:197` (199) [T] | P → P2-E |

합계: **138 HTTP 인자 함수/65파일**(production token으로 prod 124, test-only 14; read failure/module error 0). S/P는 구현 시 동일 helper가 요구하는 extra capability를 재확인한다. 특히 `queue_io/auto_queue_eligibility.rs`는 thread parent read, `router/herdr_consent/startup.rs`는 recovery 위임, `health/recovery/leak_recovery_ledger.rs`는 message read를 요구하므로 순수 송신 trait만으로 처리할 수 없다. 이 경계는 '문서상 가드만 선언하고 PASS'로 처리하지 않는다.

## 7. 진행 중 레인과 파일 겹침

측정 명령은 각 worktree에서 `git diff --name-only origin/main...HEAD`다. 작업 트리의 미커밋 diff나 미래 소유를 추측하지 않는다. 각 origin/main은 측정 당시 d40c06eb47이며 아래 head/파일 수는 raw 로그에 함께 보존했다. #8125 t1은 현재 문서 diff뿐이라 구현 파일 계획에 prod 겹침이 없다는 뜻이지 향후 API ingress와 무관하다는 뜻은 아니다.

- `i7651-b2r`: `8599b3021ff20b545c5a18ddee4513312f764a7c`, 전체 169파일.
- `i7583-s03`: `08e45a6b63f384d4f5a65b153e355c6c34d273f9`, 전체 72파일.
- `i8125-t1`: `ca904f560d008628d14bed9f6a46e7a807fbf1f0`, 전체 72파일.

아래는 조각의 명시 소유 집합(조건부 HTTP P후보 포함)과 각 레인 diff의 **정확한 교집합**이다. generated module_layers는 이동 rule을 쓰는 조각에만 포함한다. P후보가 directory 등의 이유로 빠지면 교집합을 줄일 수 있다. 동일 파일을 서로 다른 순차 조각이 수정하는 것은 허용하지만 같은 시점에 두 레인이 수정하지 않는다.

| 조각 | #7651 b2r | #7583 s03 | #8125 t1 | 우선/소유 제안 |
|---|---|---|---|---|
| P0-A | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`src/services/discord/mod.rs`<br>`src/services/discord/runtime_bootstrap/shared_data.rs`<br>`src/services/discord/shared_state.rs` | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`scripts/audit_maintainability/layers.toml` | 없음 | SharedData/생성자 짧은 수직 슬롯 선합의; 비겹침은 즉시 시작 |
| P0-B | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`src/services/discord/mod.rs`<br>`src/services/discord/runtime_bootstrap/shared_data.rs` | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`scripts/audit_maintainability/layers.toml` | 없음 | SharedData/생성자 짧은 수직 슬롯 선합의; 비겹침은 즉시 시작 |
| P0-C | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`src/services/discord/mod.rs`<br>`src/services/discord/runtime_bootstrap/shared_data.rs` | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`scripts/audit_maintainability/layers.toml` | 없음 | SharedData/생성자 짧은 수직 슬롯 선합의; 비겹침은 즉시 시작 |
| P0-D | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`src/services/discord/mod.rs`<br>`src/services/discord/runtime_bootstrap/shared_data.rs`<br>`src/services/discord/shared_state.rs` | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`scripts/audit_maintainability/layers.toml` | 없음 | SharedData/생성자 짧은 수직 슬롯 선합의; 비겹침은 즉시 시작 |
| P0-E | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`src/services/discord/mod.rs`<br>`src/services/discord/runtime_bootstrap/shared_data.rs` | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`scripts/audit_maintainability/layers.toml` | 없음 | SharedData/생성자 짧은 수직 슬롯 선합의; 비겹침은 즉시 시작 |
| P1-A | `src/services/discord/mod.rs` | 없음 | 없음 | SharedData/생성자 짧은 수직 슬롯 선합의; 비겹침은 즉시 시작 |
| P1-B | 없음 | 없음 | 없음 | 즉시 독립 착수 가능 |
| P2-A | `scripts/audit_maintainability/baselines/generated/module_layers.json` | `scripts/audit_maintainability/baselines/generated/module_layers.json`<br>`scripts/audit_maintainability/layers.toml` | 없음 | s03 rule/bridge retry 선행 후 적용 |
| P2-B | 없음 | 없음 | 없음 | 즉시 독립 착수 가능 |
| P2-C | 없음 | `src/services/discord/turn_bridge/retry_state.rs` | 없음 | s03 rule/bridge retry 선행 후 적용 |
| P2-D | `src/services/discord/turn_bridge/stream_tick/o_adoption_tests.rs`<br>`src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests.rs` | 없음 | 없음 | b2가 기존 행위 변경을 먼저 소유; 송신 signature 슬롯 별도 합의 |
| P2-E | `src/services/discord/session_relay_sink/task_notification_context.rs`<br>`src/services/discord/standby_relay.rs`<br>`src/services/discord/tmux_restart_handoff.rs` | 없음 | 없음 | b2가 기존 행위 변경을 먼저 소유; 송신 signature 슬롯 별도 합의 |
| P2-F | `src/services/discord/recovery_engine/completion_delivery.rs`<br>`src/services/discord/recovery_engine/terminal_text_idempotency.rs`<br>`src/services/discord/recovery_paths/controller_cutover.rs`<br>`src/services/discord/recovery_paths/restart.rs` | 없음 | 없음 | b2가 기존 행위 변경을 먼저 소유; 송신 signature 슬롯 별도 합의 |
| P2-G | 없음 | 없음 | 없음 | 즉시 독립 착수 가능 |
| P2-H | `src/services/discord/standby_relay.rs` | 없음 | 없음 | b2가 기존 행위 변경을 먼저 소유; 송신 signature 슬롯 별도 합의 |
| P2-I | 없음 | 없음 | 없음 | 즉시 독립 착수 가능 |

겹치는 조각 **12/16개**(세 레인 중 하나 이상과 교집합). #8125 t1과 설계 문서 자체/모든 명시 소유 파일의 교집합은 0이다. 이 레그는 그 어떤 소스도 수정하지 않는다. #7651 전체 완료는 전제하지 않고, 실제 교집합만 코디네이터가 owner/선후를 정한다.

명시 manifest의 cap 대상 파일 수(시험 제외 규칙 적용, 생성 baseline 제외)는 아래와 같다. optional P후보도 포함하며, 추가 지원 시험/fixture가 prod로 계수되면 다시 잰다. 현재 HEAD의 실제 diff를 측정한 결과가 아니라 미래 소유 집합의 견적이다.

| 조각 | cap 대상 소유 파일 수 |
|---|---:|
| P0-A | 7 |
| P0-B | 8 |
| P0-C | 14 |
| P0-D | 8 |
| P0-E | 9 |
| P1-A | 5 |
| P1-B | 5 |
| P2-A | 11 |
| P2-B | 12 |
| P2-C | 23 |
| P2-D | 19 |
| P2-E | 18 |
| P2-F | 11 |
| P2-G | 11 |
| P2-H | 12 |
| P2-I | 4 |

## 8. 검증과 완료 판정

main에서 이미 충족된 부분은 다시 만들지 않는다. **d40c06eb47**의 `src/domain/outbound.rs:1`/`src/domain/outbound/port.rs:88`에 Serenity 없는 송신 값/port가 있고, `src/domain/message_split.rs:5`/`src/domain/message_split.rs:25`에 아래 층 splitter/단위 계산이 있다. `src/domain/outbound/ids.rs:8`의 snowflake serde 계약, `src/services/turn_orchestrator.rs:1133`의 mailbox registry도 재사용한다. 이것은 새 MessengerOutbound/EngineRuntime이 이미 완료됐다는 뜻은 아니다.

이 문서 레그에서는 cargo 금지에 따라 Rust 대상 시험·뮤턴트·clippy·빌드를 실행하지 않는다. prepush에 내장된 Python 정적 검사/CI wiring unittest는 수행한다. `check_doc_path_refs.py` rc=0, docs-only cap, `prepush_gates.sh --static-only`를 실행한다. `source_scan_readers.py --run`은 변경 문서를 읽는 module이 없는 경우 cargo 없이 반환하는 것을 스크립트에서 먼저 확인한 뒤 실행한다. 선택 0이면 **해당 없음(문서 독자 없음)**이지 시험 PASS가 아니다. Rust 시험 바이너리·PG·cargo profile 증거는 해당 없음으로 보고한다.

각 구현 조각은 다음을 충족해야 한다. 대상 test filter는 선택 수=통과 수, 실패/skip=0인지 출력에서 확인하며 0 tests를 성공으로 쓰지 않는다. SDK compatibility에는 lib/prod와 lib test의 cfg 쌍 및 Windows 대상을 포함한다. 소스 이동/alias가 있는 조각은 source_scan_readers의 관련 module을 같은 cargo 환경에서 실행한다. clippy workspace/all-targets/all-features의 자기 파일 warning 0, module_layers 새 역의존/기존 count 증가 0, cap≤30/+1500을 확인한다. 새 allow/baseline 상향은 하지 않는다.

대표 대상 시험 필터는 `services::discord::session_transition::tests::`(기존 4개), `services::discord::gateway::answer_flush_gate_tests::`(기존 2개), `services::discord::outbound::`(전송/조합/rollback), `services::discord::turn_bridge::before_start_stop_tests::`, `services::discord::recovery_engine::`, `services::discord::session_relay_sink::`, `domain::message_split::`다. 실제 이전으로 logical owner가 바뀐 시험은 새 모듈 ID를 정본 inventory에서 선택하고, 기존 0선택 필터를 재사용하지 않는다. P0-C의 registry/lease 시험은 cfg(test) shim 쌍을 유지한 시험 모듈을 같이 소유하며 source-scan이 요구하는 생성 inventory는 공식 스크립트로만 동기화한다. 이 문서에서는 해당 시험을 실행하지 않았다.

행동 변경이 없는 이전이라도 ID/옵션/closure wiring을 실제로 틀리게 만드는 뮤턴트는 유효하다. 같은 Arc 대신 새 registry 생성, nonce 옵션 누락, reference+nonce→nonce 단독 dispatch, UTF16→bytes, rollback 실패→성공, partial continuation→Edited, deferred fallback을 권한 재검증 전에 실행하는 변이를 각각 해당 대상 시험에서 RED로 확인하고 원복 SHA256를 남긴다. 재현 가능한 기존 결함을 수정하게 되면 별도 RED→GREEN→원인 제거 뮤턴트 RED가 필요하며 문서만으로 수정됐다고 하지 않는다.

기존 결함을 넓히지 않는 범위: nonce 미지원 client default(`src/domain/outbound/port.rs:106`), fake delete default Ok(`src/services/discord/gateway.rs:155`), bounded flush timeout, transitional SDK ID map, provider당 bot 배치 관계는 이번에 강한 보장으로 바꾸지 않는다. 신규 Slack delivery의 exactly-once/ledger 보장은 아직 설계하지 않았으며 지금 nonce를 일반 멱등 보장으로 이름만 바꾸지 않는다.

착수 전 코디네이터 결정이 필요한 것은 (1) 혼합 실행 객체가 SharedData에 남는 P0 종료 경계, (2) TurnContext 실행 확장을 보존하는 P2 DoD 해석, (3) P0-C private impl 폐쇄의 분할 여부, (4) §7 교집합의 파일 소유/순서다. 결정 전에도 P1 vocabulary와 겹치지 않는 읽기/설계 검토는 독립 진행 가능하다. 이 문서는 이 결정들이 이미 승인됐다고 기록하지 않는다.

## 9. 리뷰 집중 항목

- **SharedData 분류와 실제 참조:** 34필드 전수·S/D 문자열 파일 수와 타입 소유 근거를 함께 적었다. bot event가 쓰는 engine 상태도 E이며, FinalizeMsg/CLI root/registry는 M으로 남겼다. S/D는 타입 해석된 전체 참조 수가 아니라는 한계가 남는다.
- **ChannelId 무수정의 컴파일 논증:** 기존 SDK 타입/import/impl/serde/DB 식을 바꾸지 않고 새 bridge에서만 변환한다. type alias 교체 주장은 배제했다. 구현 target check 전이므로 아직 컴파일 PASS 증거는 없다.
- **두 송신 trait의 누락·의미 변화:** 6+19 전 메서드 행을 대응했고 송신 7/실행·관측 12를 구분했다. nonce 단독/조합 fallback, PartialContinuationFailure 전 필드, rollback incomplete, deferred 권한 재검증을 보존한다. 단일 trait이라는 문자 그대로의 DoD와의 차이는 미해결 결정이다.
- **각 조각의 독립 안전:** compatibility adapter→caller→제거 순서, constructor 쌍/동일 Arc/순수 impl 폐쇄를 지정했다. 아직 컴파일하지 않은 계획이며 P0-C는 private field와 cfg(test) 지원 타입 때문에 추가 분할 반례가 남는다.
- **경계 시각:** completion 버스는 mailbox release 뒤, notice latch는 전송 전, deferred fallback은 range owner 재검증 뒤라는 main 순서를 유지한다. 변경 필요 시 0단계 상태 차트를 먼저 요구한다.
- **동시성 경합:** locks/atomics/senders를 복제하지 않고 Arc 동일성·Weak 생명주기·Deref map 동일성을 검증한다. 별도 engine 주입에서 1:1 owner 판정이 깨지는 안은 이번 권고에서 제외했다.
- **옛 형식 복원:** ChannelId/u64 및 snowflake serde/SQL·inflight wire를 그대로 둔다. MessageRef.raw만 추가하며 옛 u64 메시지 저장의 Slack 지원은 아직 미해결이다.
- **문서 선언만 있는 가드:** 새 backedge 0, cap, 선택 수, cfg/test 쌍은 구현 조각의 실제 gate 결과로 확인한다. 디자인 레그에서 그 gate의 실행을 대신한 것으로 선언하지 않았다.
- **잘못된 0/PASS:** cargo 미실행·선택 0·source-reader 없음은 N/A로 보고한다. 현재 HTTP 전수의 test-only 함수와 보류 C2/D를 P2 완료 수에 합산하지 않는다.
