# #8114 구현 전 host boundary 판단

<!-- doc-path-refs: proposed src/services/discord/router/message_handler/provider_dispatch/owner_input/busy_target.rs src/services/discord/router/message_handler/intake_turn/owned_busy.rs src/services/claude_tui/host_input/interactive_session.rs -->

경계 재계획 당시 판정: **기존 경계의 확장이 필요하다. 원 설계 7 prod/+450를 그대로 승인할 수 없으며 DESIGN_DELTA로 경계·크기 계획을 고정한 뒤 구현하는 것이 맞다.** 새 trait는 필수가 아니다. 기존 `claude_tui::host_input::InputTransport`를 확장하는 쪽이 가장 작은 후보이며 Herdr Core 전용 메서드/상태는 필요 없다.

기준 HEAD `6e9bf145ce5f649bda77a9ffd6c071d54cc9b254`. 아래 미생성 child 경로는 계획 후보이며 이번 Slice1 구현 파일이 아니다. 역사 좌표 링크는 조사 기준 main 스냅샷을 고정한다. 새 사용자 댓글 `logs/issue-comments-impl-1.txt`를 읽었다. 프로덕션 편집·빌드·운영/금지 설정 접근 없음.

## 현재 구현 재개의 Slice1·Slice2 경계

현재 승인 PLAN은 **두 조각 누적 prod 13파일 / 순 코드 +850 상한**이다(11-12 15:0x 재개 지시). 아래 승인 전 비용 범위의 +900 표시는 조사 당시 추정이며 허용 예산이 아니다. 새 trait·flag·Herdr Core 구현은 포함하지 않는다.

| 조각 | 활성 호출과 완료 범위 | 이번 조각에 포함하지 않는 동작 |
|---|---|---|
| Slice1 — 기존 Busy 동작 공통화 | 기존 owner `ClaudeBusy`/`ClaudeHerdrBusy`를 한 `ClaudeBusy`와 `host_input::busy::BusyOperation::for_session` factory로 연결한다. 양쪽 기존 활성 호출이 공통 `BusyClaude` leaf의 frame 검사·panic 포착·notice·watch·settlement를 지나며, tmux `inject_owned`/서버 guard와 Herdr gate IO adapter는 보존한다. | source/identity 새 관측, observer 승인, `reobserve` 회복 분기, early/late Busy 신규 승인은 활성화하지 않는다. |
| Slice2 — 관측과 early/late 승인 | exact source/observer capability와 같은 operation의 `reobserve`를 연결하고, early 관측을 tmux diagnostic 앞에 두며 late Busy 판독에 같은 결과를 사용한다. 검증된 awaited clear는 기존 Idle 위임을 보존한다. | 새 host 의미를 common caller에 추가하거나 Unknown을 Busy 권한으로 승격하지 않는다. |

아래 typed source/observer/reobserve 및 early/late 수용 기준은 Slice2의 계약이다. Slice1은 이미 선택된 owner Busy 송신을 공통 operation으로 옮기는 동작 보존 조각이다. 기존 합산 파일·크기 추정은 두 조각의 설계 범위를 설명하며, Slice1이 Slice2의 관측 승인을 구현한 것으로 보고하지 않는다.

### d2: Held 재관측의 verified awaited clear

`reobserve()`의 verified `AwaitedClear`는 Busy 송신 권한이 아니며, **같은 target/front/generation의 아직 소유된 pre-key Held**에 한해 기존 Idle 경로를 다시 평가하도록 깨운다. `UnknownKeep` 또는 clear가 이미 discard한 원문을 그 wake로 부활·재송신하지 않는다. 예약 시험은 `held_unknown_reobserves_verified_clear_then_delegates_idle`이다. 이 시험은 source Unknown으로 보류한 원문이 같은 대상을 유지한 채 검증된 clear를 관측하면 native end를 기다리지 않고 기존 Idle에 위임되는지와 UnknownKeep/discard 대조군의 key0을 검증한다.

## 지금 경계로 이미 가능한 것

- [InteractiveSessionHost (조사 기준) L9–L33](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/session_host/traits.rs#L9-L33)는 kind, presence/liveness, 화면 capture, text/keys, cwd/pid IO를 제공한다. 그러나 호출 대상은 `HostSessionRef {kind,name}`뿐이다(`model.rs:8-11`). `HostedRuntimeLocator`도 nonce/generation/provider source identity를 **명시적으로 운반하지 않는다**(`model.rs:155-157`). 이 trait만 호출해 Bound incarnation·논리 세션·native source를 복원할 수 없다.
- `session_host/resolve.rs:239-260 resolve_session_target`은 found sessions row+inflight+marker의 host-location을 fail-closed로 정규화한다. provider source 판정·Busy 여부·input gate는 아니다. 새 caller에서 raw 채널 설정으로 이 resolver를 대체하면 안 된다.
- 실제 registry의 Herdr `InteractiveSessionHost`는 `ReadOnlyHerdrHost`이고 입력을 의도적으로 거절한다(`herdr_registry.rs:49-87`). `sole_host:311`는 endpoint가 하나일 때만 host를 주므로 이를 공통 입력 factory로 오용하면 multi-endpoint와 exact target gate를 잃는다.
- `claude_tui/host_input.rs:154-169 InputTransport`는 입력 IO와 capture/boolean alive만 제공한다. 실제 tmux/Herdr 구현이 이미 있으며, `run_plan:365-389`/`Plan::step:411-424`는 하나의 순차 paste+Enter 흐름과 매 write 전 `MutationGate`를 제공한다. **전송만** 새로 만들 필요는 없다.
- `InputTarget::from_hosted_target:59-68`은 ResolvedSessionTarget+실제 HostedExecution을 받아 exact Herdr target을 만들 수 있다. `HerdrTarget::pin/send_*`의 incarnation/physical writer 계약을 유지하는 자료다.

따라서 원 설계의 busy_target에서 `TurnHost`를 match해 tmux source와 Herdr source를 나누고 early helper가 같은 것을 다시 판독하는 구현은, 파일만 작아질 뿐 새 사용자 요구의 host-neutral 흐름이 아니다.

## 가장 작은 후보: 기존 InputTransport 확장, 공통 ClaudeBusy operation

1. **host factory/adapter 안에서 실제 target을 정규화한다.** 기존 `ResolvedSessionTarget`/`InputTarget`/HostedExecution 증거를 사용한다. cfg endpoint 유무를 공통 busy_target/early path에서 읽지 않는다. Unknown/Conflict/Pending은 typed unavailable 증거로 반환한다. 호스트 생성 factory 한 곳만 구체 구현을 선택한다.
2. **기존 InputTransport에 typed input observation capability를 추가한다.** 대략 `observe_input(context) -> InputObservation`이며 observation은 논리키, execution nonce/physical writer/target, native source(path/dev/ino), liveness/capture 또는 typed unavailable 사실을 전달한다. tmux adapter는 marker/liveness/current source, Herdr adapter는 Bound record+exact target/current source를 읽는다. 기본 unsupported 구현이 필요한 비대상 transport는 fail-closed여야 한다. 새 영구 state, 새 registry, Herdr Core enum/상태는 만들지 않는다.
3. provider 공통 `Busy/Idle/Unknown` 정책과 `history.current/current.pin/verified pin/runtime binding` 대조는 Claude 입력 쪽 공통 helper에 둔다. `InteractiveSessionHost`에 Claude JSONL/Stop readiness 의미를 직접 넣지 않는다. #8080 private readiness Source를 공개/복제하지 않는다. 이미 공개된 binding/history primitive로 normalized source 증거를 읽는다.
4. `owner_input/busy_target.rs`와 early `owned_busy.rs`는 이 operation 결과만 소비하여 actor FrontClaim/held marker/probe/표시를 처리한다. host 문자열/TurnHost match가 없어야 한다. native end 없이 source recovery probe→Ready도 같은 observation을 다시 읽는다.
5. **send selection도 같은 host operation에 연결해야 한다.** 현재 `target.rs:84-86`은 ClaudeBusy와 ClaudeHerdrBusy를 host 문자열로 고르고 `owner_input.rs:1446,1483`이 두 leaf를 구성한다. 이 분기를 공통 경로에 그대로 두면 세 번째 host 때 그 두 공통 파일을 다시 바꿔야 한다. Claude Busy operation 하나에 host adapter/IO를 실어 넘기는 형태로 선택을 기존 host_input 경계 쪽으로 옮긴다. 공통 effect는 frame/history/ready_payload/start_payload/receipt/Held·Unknown settlement를 한 번만 수행한다.

이 후보는 기존 trait를 활용하지만 **메서드와 typed observation 자료를 추가하는 경계 확장**이다. `observe_input` 같은 메서드명을 구현 전에 그대로 확정할 필요는 없지만 반환 자료·권한·unknown 의미와 책임 배치는 PLAN에서 고정해야 한다.

## 송신 공통화 시 보존해야 하는 실제 차이

- tmux busy core는 `busy_inject::Pane`에 attach-generation, 서버 `if-shell` guard, load-buffer/guarded paste+Enter가 따로 있다. 단순 `run_legacy(PasteBuffer,Enter)`로 교체하면 기존 서버 attach 원자 판정을 잃는다. adapter IO hook으로 기존 guarded tmux primitive를 재사용해야 한다.
- Herdr는 `HerdrTarget`의 per-mutation pin과 physical writer lease를 유지한다(`herdr_gate.rs:433-451,525-565`). read-only `HerdrHost.send_text`를 호출하거나 sole_host로 target을 재선택하면 안 된다.
- host mutation ACK와 transcript receipt는 구분한다. 공통 owner는 현 Enter-terminal/no-resend 정책을 유지하며 post-paste 불확실성을 Held로 재분류하지 않는다.
- ANSI draft capture는 `InputTransport::capture_draft` default None이며 HerdrInput은 현재 override하지 않는다. 공통 preflight가 draft capture를 쓰면 Herdr override(기존 `target.capture_ansi`)가 필요하다. 이 누락을 일반 capture로 조용히 대체하지 않는다.

## 승인 가능한 concrete PLAN 후보

선택은 **새 trait 없이 InputTarget/InputTransport 기존 경계 확장**이다. `session_host::InteractiveSessionHost`는 raw pane IO 관측에 그대로 사용하고 provider source 의미를 그 범용 trait에 추가하지 않는다. 기존 `InputTarget`의 구체 adapter 선택은 host_input 내부 factory에서만 수행한다. common busy_target/early/effect는 반환된 operation만 보며 Herdr/Core 이름을 알지 않는다.

경계가 반환할 자료와 소유는 다음으로 고정한다(최종 Rust 이름은 구현 시 조정 가능).

| capability/자료 | 생산 책임 | 공통 소비 및 금지 |
|---|---|---|
| `InputIdentity` | tmux adapter는 기존 spawn/pane epoch+account 판독, Herdr adapter는 Bound record+HerdrTarget/physical writer | logical/native/execution nonce/host target와 physical writer를 하나의 operation에 고정. common code에서 endpoint 재선택/설정 probe 금지 |
| `CurrentSource::{Ready,AwaitedClear,Unknown}` | 해당 host의 source adapter가 existing runtime binding/claude_history를 읽고 normalize | Ready는 native/path/dev/ino+current nonce history 조건을 만족. AwaitedClear는 기존 public `claude::herdr_turn::awaited_clear:320` 등 현재 Pending 증거를 전달하고 old source Busy로 오인하지 않음. 새 API가 #8080 private Source를 공개하지 않음 |
| `ObserverOwnership::{CallerReader,TranscriptO,Unavailable}` | host adapter factory가 기존 tmux busy observer 또는 confirmed+accepts O 증거를 반환 | common은 ownership capability로 `until_done`/declared를 정함. Herdr 이름을 보고 observer를 생략하지 않음. TranscriptO는 실제 O-ready 가드, CallerReader는 준비된 observer를 포함. Unavailable은 key0 Held |
| read-only `reobserve()` | 같은 operation의 exact target/current-source adapter | Held watch가 동일 target+front/generation을 다시 판독. native Busy 상태 자체는 Ready를 막지 않음. verified AwaitedClear는 Busy 권한 없이 같은 소유 원문의 기존 Idle 재평가를 깨우고, source Unknown은 보류 유지. UnknownKeep/clear-discard 원문은 부활하지 않음 |
| prepared input IO | 기존 InputTransport+MutationGate의 tmux/Herdr adapter | host-specific capture/draft verdict 및 guarded load/park/paste/Enter만 맡음. common은 프레임/원문 소유·source offset·receipt scope·NotSent/Unknown 정산을 한 번만 수행 |

`AwaitedClear`는 새 persistent 상태가 아니라 현재 source observation의 결과다. 새 launch/clear/실행을 생성하거나 pending source를 임의 채택하는 권한을 주지 않는다. `ObserverOwnership` 역시 새 게시 소유자가 아니라 이미 존재하는 reader/O 소유의 관측 증거다. producer가 불명확하면 Unavailable이며 native busy가 끝나야만 회복된다고 가정하지 않는다.

tmux의 hook-free/legacy source 승인 조건은 이번에 Herdr의 stricter nonce-history 조건으로 재정의하지 않는다. 각 adapter는 현재 허용하던 source 증거를 같은 typed 결과로 정규화하고, 공통 caller가 host 이름으로 증거 수준을 완화하지 않는다. observer callback은 일반 input capability 자료로 전달하며 `host_input`이 Discord의 StreamingTurn/OwnerTurn 타입을 직접 의존하게 만들지 않는다.

공통 `ClaudeBusy` operation은 다음 순서 하나다: actual operation 취득 → current source/observer 증거 판정 → original/frame+history source registration → owner ready → host-specific optional draft park → payload-start → host-gated paste+Enter → Enter-terminal Completion → 기존 actor settlement와 receipt diagnostic. pre-key failure만 positive NotSent proof 또는 hold_front marker를 이용해 same-original Held로 돌린다. post-key outcome은 Unknown/no-resend다. tmux 기존 first-key stop sink는 adapter의 first-key hook으로 그대로 이어지고 Herdr에는 새 stop authority를 만들지 않는다.

### 파일 배치·규모(최대 13 prod, 계획값 +850)

| 파일 | 변경 책임 | 순증 예산 방향 |
|---|---|---:|
| `provider_dispatch/owner_input.rs` | 기존 host/source/leaf 선택을 operation 호출로 교체, tmux observer 생성도 adapter 요청으로 이동 | -70 |
| `provider_dispatch/owner_input/busy_target.rs` 신규 | operation의 공통 Busy/held/marker/watch 연결. host 분기 0 | +100 |
| `message_handler/intake_turn.rs` | 두 host 조건을 early common 호출로 교체 | -20 |
| `message_handler/intake_turn/owned_busy.rs` 신규 | 실제 early entry와 operation 전달. host 분기 0 | +70 |
| `message_handler/provider_dispatch.rs` | 기존 parent child/reexport glue | +3 |
| `message_handler/busy_retry.rs` (이번 변경 제외) | #8113 정책 착지 뒤 별도 델타에서만 적응; 이번에는 결과 계약/겹침 좌표만 보고 | 0 |
| `input_owner/target.rs` | Claude busy의 host별 route 선택 제거, capability 기반 한 route | 0 |
| `input_owner/effect.rs` | 공통 ClaudeBusy effect 및 기존 settlement, 두 host leaf 선택 제거 | +90 |
| `input_owner/effect/herdr.rs` | 별도 Herdr 정책·receipt 중복 제거/host IO adapter 쪽 이동. 필요 없어지면 삭제(삭제 파일도 cap 산정) | -150 |
| `claude_tui/host_input.rs` | existing InputTarget/InputTransport capability 추가와 child glue, 다른 input caller 의미 유지 | +45 |
| `claude_tui/host_input/interactive_session.rs` 신규 | typed capability 자료/host factory, 현재 두 host adapter의 identity/source/ownership 관측. 독립 parent 책임 추가 없음 | +300 |
| `claude_tui/busy_inject.rs` | owned busy 공통 preflight/frame ledger/receipt 부분 재사용 가능하게 연결. legacy caller 정책은 유지 | +130 |
| `claude_tui/busy_inject/stash.rs` | 기존 pinned/server-guarded IO를 adapter에서 재사용하는 최소 노출/연결. guard 조건 완화 0 | +75 |
| 직접 합계/추정 여유 | 대략 +573, 경계 타입·관측/ownership 이동 오차 포함 | **+850 (두 조각 누적 상한)** |

위 표의 이번 대상은 최대 12개이고, boundary export/factory의 최소 glue가 별도 파일에 필요할 수 있어 계획 상한 13개를 둔다. `busy_retry.rs` 적응은 이번 대상에서 제외하며 #8113 착지 뒤 델타로만 다룬다. `effect/herdr.rs` 삭제를 택해도 삭제 파일은 cap에 센다. 독립 호스트/소스 메서드를 giant parent 본문에 더해 파일 수만 줄이지 않는다. 이 PLAN은 +450 이내라는 원 약속을 철회하고, 추가 4~6 prod/+250~450 비용을 명시한다. 구현 중 13파일/+850 예상 밖 책임이나 guard 재설계가 나오면 다시 DESIGN_DELTA로 멈추며 +1200 PLAN 선을 넘겨 진행하지 않는다.

### tmux safeguard 보존 좌표와 필수 RED

- `busy_inject.rs:548-605`의 ledger 등록/offset/withdraw 계약은 common source transaction으로 옮겨도 바이트 의미를 유지한다. legacy busy caller에는 기존 entry를 adapter로 연결하며 별도 ledger registration을 두 번 하지 않는다.
- `busy_inject/stash.rs:52-64`의 ready_payload→native_stop first_key, `:67-90` optional C-s+payload start, `:97-109` paste→settle→Enter 순서를 유지한다.
- `stash.rs:135-155`의 owner-pinned `if-shell -F` 조건과 pinned Applied/Held 결과 판독을 **그대로** 호출한다. plain run_legacy/send_keys 대체 금지. 이 현재 owner 경로는 최초 #6692의 attach guard 설명보다 나중에 pinned pane identity guard로 발전했으므로, 구현 base의 guard를 정본으로 삼는다.
- Herdr는 `HerdrTarget::pin/send_paste/send_keys` 및 writer lease를 그대로 사용한다. 양쪽의 key-time target replacement·first-key stop·post-paste refusal mutant를 공통 효과를 통해 실행한다.

기존 `InputTransport`의 raw operation을 이용해도 위 tmux guard를 지킬 수 있는 최소 IO hook을 먼저 source-level로 고정한다. 불가능하면 새 trait를 자동 추가하지 않고 DESIGN_DELTA에서 구체적인 누락 operation과 비용을 다시 제시한다.

## 예상 규모와 정지선

원 7파일은 새 관측 helper/early glue만 계산했고 기존 trait·typed 자료·host 선택 factory·공통 ClaudeBusy routing을 계산하지 않았다. 정확한 delta는 구현 방식에 따라 달라지지만 다음 범위를 PLAN 후보로 잡는 것이 정직하다.

- **관측만 기존 경계에 연결:** 원 계획 외 `host_input.rs` 및 child adapter/source helper, 필요시 `session_host` export/factory를 포함해 대략 추가 3~4 prod/+180~300. 그러나 현재 ClaudeBusy/ClaudeHerdrBusy host 선택이 공통 caller에 남으므로 “세 번째 host 때 host 구현 하나만 변경” 전체 조건을 충족하지 못한다.
- **사용자 요구 전체 선택 후보:** 위 concrete PLAN에서 기존 owner/effect 선택을 공통 ClaudeBusy operation으로 옮기는 것까지 포함해 **총 11~13 prod, 순증 약 +700~900**(계획값 +850). 기존 중복 로직 이동·삭제로 순증이 줄 수 있으나 조사만으로 +450 이내라고 보장할 근거는 없다. +1200 PLAN 정지선과 30파일/+1500 상한은 유지한다.
- 새로운 trait를 별도로 세우는 안은 기존 InputTransport와 역할이 겹쳐 기본안으로 추천하지 않는다. 정말 필요하면 InputTransport를 사용할 수 없는 구체 타입/lifetime/권한 사유와 순증을 DESIGN_DELTA에 적어야 한다.

즉 지금은 새 호스트 분기를 추가한 구현을 시작할 시점이 아니다. **DESIGN_DELTA: existing InputTransport 관측/호스트 선택 확장 + 공통 ClaudeBusy operation, prod/추정 상향**으로 한 번 고정한 뒤 진행하는 것이 사용자 지시에 맞다. Herdr Core 구현은 #7583에 남긴다.

## 이 경계의 수용 시험

- common busy/early 소스에서 `herdr_endpoint`, `HostKind::Herdr`, `TurnHost::Herdr` 분기 0을 정적 assertion 또는 source census로 확인한다(구체 선택은 host factory 안).
- 동일 common flow에 tmux/Herdr adapter fixture를 각각 넣어 same-native-busy→1 paste+Enter, source/target Unknown→key0+Held, source recovery→end 없이1회, partial write→no resend를 검증한다.
- tmux key-time attach guard 및 Herdr nonce/server replacement를 각각 기존 의미대로 유지하는 mutant가 RED여야 한다.
- 임의 fake 세 번째 adapter를 공통 flow에 주입하는 시험으로, core 분기/새 route 추가 없이 observation→effect→settlement를 실행할 수 있어야 한다. 실제 Herdr Core capability/상태/구현을 만드는 시험은 아니다.


## 재개 전 레인의 정지 판정과 다음 입력

코디네이터 브리프의 “새 경계가 필요하면 30분 안에 DESIGN_DELTA로 멈춘다”에 해당한다. source/target/clear 관측 capability와 공통 ClaudeBusy 선택은 기존 trait 메서드 추가를 선택하더라도 원 계획의 책임 경계를 바꾸므로 구현 전에 재계획한다. 프로덕션 파일·Rust 시험·fixture 편집 0, cargo/PG/운영 입력 실행 0이다.

다음 단계에 권고하는 입력은 기존 InputTarget/InputTransport 경계 확장과 공통 ClaudeBusy op 계약을 고정하는 것이다. 원 설계의 FrontClaim/physical lease/Held/Enter-terminal/receipt 재사용과 P2 early/clear 수용 기준은 유지한다. Herdr Core 구현과 #7583 전체 호스트 추상화는 계속 범위 밖이다.

최대 13 prod/+850은 두 조각 누적 PLAN으로 승인됐다. Slice1의 행동 보존 리뷰 뒤에만 Slice2를 활성화한다. PLAN +1200 및 prod30/+1500은 유지한다. raw host 분기 이동이나 generic run_legacy로 안전 원자성을 희생하는 대체는 채택하지 않는다. P2 문서·모델 보완은 같은 docs-only 커밋에 남기며 구현 완료로 보고하지 않는다.


## A안 — 공개 Idle 위임 승인 (2026-11-13)

[주 설계 A안 절](herdr-busy-input.design.md#a안--busy-strict-권한과-공개-idle-위임-분리-2026-11-13-승인)을 현재 승인 경계로 따른다. Busy의 strict pin/observer/physical-target 계약을 유지하면서 public B1 Proof/reserve/current의 검증 결과만 Idle 위임에 쓴다. 판독 reservation은 pre-key 반환하고 기존 Idle driver가 실제 송신을 다시 준비한다. 같은 owner/target/front/generation/nonce의 Held 재관측도 현재 공개 Idle 근거로만 깨우며 UnknownKeep/discard 원문은 부활하지 않는다. producer 변경으로 후속 hook0을 보완하는 B안은 별도 검토로 보류했다. 새 production 파일·trait·flag는 추가하지 않는다.

기존60 모델은 그대로이고 [A안 별도 projection](herdr-cold-idle.report.md)이 cold pin0+validIdle+hook0, stale proof/reservation, 같은 owner 재관측을 검사한다. 최종 Rust 동작·cap 판정은 구현 보고서의 증거와 구분한다.
