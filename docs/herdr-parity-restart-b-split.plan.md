# Herdr restart B 구현 분할 계획

기준은 A `0060c4dd50f68a7509217c29986dc58ff5e64d16`와 병합한 r3 설계 `a7957913f0a80c80061445c7ba74e4e3a4029303`다.
이 문서는 **PLAN_READY를 위한 구현 전 계획**이다. 생산 소스 변경·Rust/PG 시험·cargo 빌드는 0이다.
최신 수용 문장은 [정본 3.9](herdr-parity-restart.design.md#39-수용-기준-문장-b가-표로-대조할-것)에 있다.

## 크기 게이트

| 구현 묶음 | 실제 연결 근거 | 예상 순 코드 |
|---|---|---:|
| queue admission·상태·철회·gate 및 claim followers | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:708`, `src/services/turn_orchestrator/queued_head/v2.rs:395`, `src/services/discord/queue_io/input_owner/start.rs:13` | +354~448 |
| clear epoch·효과 허가·startup 추적 | `src/services/herdr_launch/prepared.rs:43`, `src/services/claude/herdr_turn/first_input.rs:99`, `src/services/codex/herdr_turn/first_input.rs:162`, `src/services/discord/commands/control/clear_teardown.rs:137` | +320~440 |
| 공개 명령·operation 응답·detached 종료/postlude | `src/services/discord/commands/restart.rs:259`, `src/services/discord/herdr_terminate/http_force.rs:30`, `src/services/discord/herdr_terminate/retired_exclusion.rs:100` | +250~300 |
| idle close-crash 복구·seed permit·headless/Codex 배선 | `src/services/discord/herdr_terminate.rs:217`, `src/services/discord/router/message_handler/headless_turn.rs:147`, `src/services/discord/router/message_handler/provider_dispatch.rs:614` | +200~250 |
| 합계 | 독립 조사에서 API·소유·worker 경계를 대조한 추정 | **+1124~1438 (중심 +1281)** |

r3의 +480~620 추정에는 아래 비용이 충분히 들어 있지 않았다.

- 완료 operation의 기존 ticket 선조회, alias operation의 내구 bind와 만료 receipt 처리.
- inflight가 없는 Bound·PendingPlaced의 close 뒤 crash를 permit 자체가 복구하는 경로.
- `CreatedSubmit`가 다른 blocking thread에서 실행되는 실제 경계. TLS 검사만 추가해서는 허가가 전달되지 않는다.
- clear가 startup과 겹친 뒤 startup이 먼저 끝나도 Retired 특수 보류로 되돌아가지 않는 sticky 관측.

1500줄 상한만 보고 한 조각으로 진행하지 않는다. 중심 추정이 **PLAN_READY 선 +1200**을 넘으므로 생산 코드 작성 전에 분할한다.
아래 두 조각은 각자 기존 생산 호출자가 소비하며, 미사용 API를 먼저 넣지 않는다.

## B1 — clear 효과 허가와 claimed followers

**예상 prod 11파일 / 순증 +334~468.** restart 명령은 아직 활성화하지 않는다.

| 생산 파일 | 변경 | 예상 순 코드 |
|---|---|---:|
| 새 `src/services/herdr_launch/clear_epoch.rs` (proposed) | namespace/channel 상태, epoch·clearing, startup/effect 카운터, Notify, RAII·TLS | +180~210 |
| `src/services/herdr_launch.rs` | scope capture, Pending 뒤 CheckEpoch, 기존 unsent retire 연결 | +10~15 |
| `src/services/herdr_launch/prepared.rs` | worker에 Arc 전달, 실제 create 전 BeginEffect/끝까지 보유 | +12~18 |
| `src/services/claude/herdr_turn.rs` | sync execute에서 Launching 등록 | +4~6 |
| `src/services/claude/herdr_turn/first_input.rs` | CreatedSubmit에 scope 전달, 실제 composer mutation 전 허가 | +18~25 |
| `src/services/codex/herdr_turn.rs` | sync execute 등록과 Bound followup write 허가 | +10~15 |
| `src/services/codex/herdr_turn/first_input.rs` | 별도 worker의 실제 submit 허가와 startup 종료 | +18~25 |
| `src/services/discord/commands/control.rs` | clear 시작~종료 ClearGuard | +4~6 |
| `src/services/discord/commands/control/clear_teardown.rs` | transition 해제 후 startup/effects 대기, row 재읽기, Retired selector CAS | +50~65 |
| `src/services/discord/commands/control/herdr_recovery.rs` | sticky startup overlap이면 기존 Held 특수 경로 대신 일반 clear | +6~10 |
| `src/services/turn_orchestrator/queued_head/v2.rs` | claim 시 followers 고정, reserve에서 같은 ordered originals 대조, revoke 때 해제 | +14~28 |

합계 범위에는 clear 묶음의 불확실성 여유를 포함한다. giant에는 선언·연결·거절 분기만 둔다.
SnapshotV2·serde variant·turn 필드 추가는 0이다. followers는 비내구 Owner 상태에만 둔다.

효과 계약은 `CheckEpoch → BeginEffect → 실제 I/O`다. BeginEffect와 clear epoch 갱신은 같은 mutex 아래 직렬화한다.
허가가 먼저면 I/O는 끝날 수 있고 clear는 그 반환을 기다린 뒤 세션을 exact 종료한다. clear가 먼저면 허가와 실제 I/O는 0이다.
허가는 물리 I/O가 끝날 때까지 살아 있어야 한다. 허가를 받자마자 놓고 backend를 호출하면 clear 완료 뒤 효과가 발생할 수 있다.

sync execute의 TLS에는 registration Arc를 둔다. async launch는 await 너머 TLS를 설치하지 않고 worker로 Arc만 넘긴다.
`src/services/discord/health/turn_deliver/inject/first_input.rs:114`의 별도 blocking thread에는 CreatedSubmit가 Arc를 명시 전달한다.
startup 완료는 물리 첫 제출 반환 때 내려 AI 답변 완료나 Source 대기를 clear가 기다리지 않게 한다.

B1은 기존 일반 launch·clear·claim 경로가 즉시 소비한다. restart seed와 새 headless 권한은 B2까지 만들지 않는다.

## B2 — durable seed와 실제 restart 활성화

**예상 prod 16~22파일 / 순증 +790~970.** B1 head를 base로 재측정한다.

- queue 묶음 +340~420: 안정 operation/ticket, 기존 ticket 선조회, active seed alias join, 보호 단위 뒤 삽입,
  ticket 상태·철회·origin receipt, 일반 ingress의 reserved source 거절, gate Drop wake.
- 명령/종료/permit 묶음 +450~550: `/restart`와 `!restart`의 공통 서비스, detached exact 종료와 postlude,
  idle close-crash 복구, seed-only headless 권한, Codex A resume 요청, dispatch·Ready 응답.
- B1의 followers 고정과 clear 효과 허가를 그대로 소비한다. 새 serde variant·V2 필드·새 Core/trait는 0이다.

queue child 후보는 새 `src/services/turn_orchestrator/queued_head/v2/source_admission/restart_seed.rs` (proposed),
서비스 gate 후보는 새 `src/services/discord/queue_io/input_owner/restart_seed.rs` (proposed)다.
명령 orchestration과 idle 복구는 작은 child에 두고 `restart.rs`·`headless_turn.rs`·`owner_input.rs`·`herdr_terminate.rs`에는 typed 연결·거절만 둔다.

operation의 기존 receipt/context를 **현재 row로 새 context를 만들기 전에** 읽는다. Existing/Joined/Expired/Conflict는 재종료하지 않는다.
Conflict는 Full/V2 미채택/설정 대기 같은 seedless 사유와 구별한다. alias도 같은 snapshot에 receipt+bind를 남긴다.
만료 receipt는 기존 요청임을 알리되 세부 결과를 복원할 수 없음을 표시한다. 이를 새 요청이나 clear의 증거로 바꾸지 않는다.

idle permit은 inflight 파일이 없어도 복구 주체다. old owner·nonce·dispatch·종료 증거를 transition/fence 아래 다시 고정하고,
Ended인 같은 row만 retire/disconnect한다. 새 close는 0이다. 확인 불가면 seed를 보이는 실패로 종결한다.
seed turn token 설치 뒤 전체 terminate를 호출해 seed 자체를 결산하지 않는다.
복구한 새 Bound/Pending과 합류하려면 그 seed의 ticket/attempt가 소유한 정확한 successor nonce여야 한다.

새 seed 턴만 옛 dispatch를 상속하지 않는다. 이후 backlog의 thread/card 재발견은 tmux 기준선과 대조한다.
seed 없는 crash는 r3의 기존 tmux 수준 계약을 유지한다. 새 영속 operation store를 이 계획에서 암묵적으로 추가하지 않는다.

## 3.9 수용 문장별 시험 계획

아래 이름은 **미작성·미실행 시험의 제안 이름**이다. 실행 수·PASS로 세지 않는다.

| 기준 | 실제 진입점/관측 | 제안 시험 이름 | 조각 |
|---|---|---|---|
| 1 exact restart·새 incarnation 하나 | 인증 뒤 공통 restart → owner seed → provider execute | `restart_command_starts_one_seed_successor_pg` | B2 |
| 2 old nonce 전달 경계·원본 custody | 실제 close/fence barrier와 owner pump의 physical sends·receipt | `restart_fences_new_old_nonce_sends_and_keeps_originals_pg` | B2 |
| 3 reserved seed·일반 headless 거절 | general start와 stored owner bind permit | `ordinary_ingress_cannot_admit_restart_seed`, `restart_seed_uses_herdr_owner_without_tmux_effects_pg` | B2 |
| 4 시작 row 전수 | Missing/Legacy/Bound/PendingPlaced/Unplaced/Retired/Unknown/Conflict | `restart_start_row_matrix_pg` | B2 |
| 5 dispatch·뒤 입력 parity | tmux/herdr 각각 dispatch 진행→restart→seed 결산→backlog 완료, ID·상태·게시 수 | `restart_dispatch_seed_and_backlog_match_tmux_pg` | B2 |
| 6 clear 허가 시각·효과·최종 세션 | Check/Begin/Effect 사이 실제 create·submit, clear entry | `clear_after_pending_before_create_sends_no_create_pg`, `authorized_create_finishes_then_clear_exact_closes_pg`, `authorized_submit_is_terminated_before_clear_returns_pg` | B1, B2 통합 |
| 7 Full/설정 대기 seedless | admission 실패 뒤 exact 종료와 원본 수·실패 문구 | `restart_seedless_capacity_and_configuration_wait_pg` | B2 |
| 8 보호 prefix·고정 followers | 실제 claim→뒤 arrival→pump→reserve→settle | `claim_followers_are_fixed_after_same_author_arrival`, `claimed_reserve_refuses_a_later_arrival_as_a_follower`, `restart_seed_is_durable_after_claimed_unit` | B1/B2 |
| 9 crash·liveness | idle Bound/Placed close 직후, launch/submit/receipt/bind, fail 뒤 철회 전 재부팅 | `restart_idle_close_crash_repairs_without_close_pg`, `restart_seed_crash_phase_matrix_pg` | B2 |
| 10 ticket 상태·operation 재처리 | 완료/진행/alias/Conflict/Gone/Cancelled/Failed 재호출, count 불변 | `restart_completed_operation_keeps_effect_counts_pg`, `restart_seed_context_conflict_is_not_seedless` | B2 |
| 11 옛 바이너리/형식 | 기존 SnapshotV2 deserialize·reserved source 비인식 경로의 보이는 거절 | `restart_seed_legacy_snapshot_and_rollback_visible_refusal` | B2 |
| 12 create 응답 불명 | 실제 launch host Unknown, create 재전송0·Pending 유지 | `restart_create_unknown_keeps_pending_without_replay_pg` | B2 |
| 13 tmux/인접 guard | 실제 tmux restart와 Cancel/Terminate/clear/stop/operator reset | `restart_tmux_and_adjacent_management_guards_unchanged_pg` | B1/B2 |

원인 변형은 B1의 `BeginEffect` 재검사 삭제·조기 허가 반납·clear wait 누락·sticky 누락·followers 재계산/대조 누락,
B2의 `Conflict→seedless`·operation 선조회/alias receipt 누락·idle repair 주체 누락·old target close 재전송·seed 앞지름·gate 조기 해제다.
각 변형은 대응하는 실제 진입점 시험에서 RED여야 하며 원복 파일은 바이트 동일로 확인한다.

## 검증 시간 배분

각 구현 조각은 cargo 전에 headroom을 통과하고 기존 target을 재사용한다. 실제 PG는 격리 data/port만 쓴다.
초기에 실제 진입점 RED와 모델을 병행하고, 타입·소스 구조가 동결된 뒤 두 원인 변형과 최종 GREEN을 돌린다.
0개 선택/skip를 성공으로 세지 않는다. clippy는 workspace/all-targets/all-features의 변경 파일 경고0을 확인한다.

150분 중 마지막 **최소 25~30분**은 committed head의 정적 prepush·수리·ref push/read-back에 남긴다.
문서 경로 검사는 먼저 돌려 A의 약식 경로 실패를 반복하지 않는다. source/의존성이 바뀐 head에 시험 바이너리를 재사용하지 않는다.
성공한 검증을 반복하지 않고, 합성 main 병합 충돌만 남으면 브리프의 명시 예외를 따른다. lane worktree에는 main을 병합하지 않는다.
