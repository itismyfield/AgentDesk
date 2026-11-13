# Herdr `/restart` 상태 모델 설계 검증

기준 코드: `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba` (main). 생산 코드 변경은 없다. 기존 색인의 `session-lifecycle` 안에 `HerdrRestart`를 추가해 `HerdrSession`의 물리 pane/record 모델과 `HerdrBusyHandoff`의 이전 handoff 증거를 유지한다. 이 모델은 `/restart`의 호출 순서와 입력 소유권만 유계화한다.

**검증 종류:** TLC가 제안 포트를 가정했을 때의 불변식·진행성 검사다. Rust/DB/실제 herdr 시험, 실제 실행 경합 재현, 운영 수용 PASS를 뜻하지 않는다. Before의 안전성 GREEN도 parity 달성을 뜻하지 않는다.

## 진입점에서 가져온 값 범위

아래 표가 모델 범위의 정본이다. `phase`는 새 production enum이 아니라 표의 단계에 붙인 표시용 이름이다. UUID/pane ID/server witness는 현재값·다른값 두 동치류로 줄였다. 입력 수 `N=1/2`는 기존 `Vec`/queue를 대표하는 작은 검증 범위이며 실제 큐 길이 제한이 아니다.

| 모델 값 | 코드의 값·행동과 좌표 | 축약·제외 |
|---|---|---|
| `Policy={before,after}` | 현재 refusal `src/services/discord/commands/restart.rs:219`, 보이는 응답 `:287`; 제안 연결은 본 설계 문서 | before는 old permit·queue를 바꾸지 않고 거절 |
| `StartKind={Bound,Legacy,Unknown}`, `row={Legacy,Unknown,Retired,Pending,Bound}` | `src/db/dispatched_sessions/hosted_execution.rs:27`, `:171`, `:228`; successor 거절/허용 `src/services/herdr_launch.rs:433` | Missing/Conflict/Unknown은 코드 조회에서 구분해야 한다. 모델의 Unknown은 명시적 거절 결과만 축약하며 legacy로 복원하지 않는다. placed/unplaced Pending 부팅 복구는 모델 밖. NoActive는 canonical Legacy+live pane 없음만 검사하며 Missing row 생성은 제외 |
| `CloseReply={confirmed,Indeterminate}` | `src/services/discord/herdr_terminate.rs:74`, exact-close·absence·retire·settle 순서 `:548`, `:590`, `:594`, `:596`, `:603` | NotSent·Disabled·Refused·provider survival은 successor를 허용하지 않는 실패 분기에 축약 |
| `CreateReply={Created,NotSent,Indeterminate,CreatedUnconfirmed}` | `src/services/herdr_launch.rs:111`, `:210`; Pending 선행 `:466`, `:480`, `:482` | ACK 불명은 pane이 실제 생긴 최악 경우로 고정. NotSent에서는 새 pane 0으로 보이는 실패 |
| `Identity={same,server,pane}` | nonce fence `src/services/session_host/herdr_gate.rs:455`, one-witness close `:496`, `:658`; row recheck `src/services/discord/herdr_terminate.rs:548` | 새 서버·pane 번호 재사용은 exact identity 불일치. 숫자만 재연결하는 동작은 mutant에만 존재 |
| `Busy={TRUE,FALSE}`, `inflight/settled/warned` | 기존 active 판정·경고·cancel `src/services/discord/commands/restart.rs:223`; captured turn recheck `src/services/discord/herdr_terminate.rs:485`, settlement `:603` | 재시작 수락 전 실행 중인 옛 턴은 중단 경고와 exact token 결산 대상으로 분리. 새 입력을 옛 턴으로 replay하지 않음 |
| `q={incoming,queued,held,delivered,cancelled,lost}`, `accepted`, `effects=0..2` | owner `SnapshotV2.items/head_injection/retained_inputs/terminal_identities` (`src/services/turn_orchestrator/queued_head/v2_types.rs:19`), target nonce `:117`, busy transition durable requeue (`src/services/discord/router/message_handler/intake_turn/runtime_transition.rs:35`, `:72`) | `lost`·두 번째 effect는 mutant 검출용. 기존 Reserved/MayHaveSent/Unknown (`src/services/turn_orchestrator/queued_head.rs:53`)의 이미 시도한 입력은 신규 수락 집합 밖이며 기존 불명확성 결산을 보존해야 함 |
| `generation/intentGeneration=0/1`, `clearReceipts` | 명시 clear는 transition timeout 뒤에도 진행 `src/services/discord/commands/control.rs:523`, memory discard `:552`; V2 terminal identity와 owed drop receipt `src/services/turn_orchestrator/queued_head/v2_types.rs:25`, `:35` | `ClearSupersedes`는 create 직전 1회만 검사. clear의 자체 pane 종료나 임의 횟수 경쟁을 증명하지 않음 |
| `SeedReply={accepted,failed}`, `seedEffects` | 기존 restart seed 및 두 번 이하 Conflict 재시도 `src/services/discord/commands/restart.rs:133`, `:166`; Codex first input→source attach `src/services/codex/herdr_turn.rs:434` | `accepted`는 단순 owner enqueue 응답이 아니라 **seed effect+정확한 startup/source binding 완료**를 뜻하는 제안 포트. Claude의 bind-before-seed와 Codex seed-before-bind 차이를 하나의 단계 안에서 다룸 |
| `phase={idle,closing,retiring,preparing,creating,binding,ready,notice,failed}` | 위 request→exact close→old token 결산→new nonce Pending→create→seed/source bind→Ready 또는 notice 경로 | 기존 enum 추가 주장이 아니다. 호출이 반환하거나 실패 알림이 재시도 끝에 도착한다는 공정성은 명시적 가정 |

## 수정 전/후 비교

| 항목 | Before: main | After: 제안 포트 |
|---|---|---|
| healthy herdr `/restart` | 취소 전에 거절; 옛 세션 유지 | transition+old execution fence를 획득한 수락 시각 이후 old write 불허, exact close 후 successor |
| 불변식·요청 종료 | 유실/중복 없이 보이는 거절로 끝남 | 유실/중복 없이 새 Ready 또는 보이는 실패로 끝남 |
| 사용자 기능 `RestartAvailable` | healthy 환경에서도 새 Ready 없음: 기대 RED | healthy 환경이며 superseding clear가 없으면 새 Ready: GREEN |
| 대기 입력 | 본 모델에서는 기존 queued 소유권 그대로 | 기존 owner custody를 유지하고 successor의 정확한 source binding 뒤 한 번 전달; `!clear` 명시 폐기는 terminal receipt로 결산 |
| inflight | 거절 전에 cancel하지 않음 | 확인된 옛 execution·token만 결산하고 warning을 유지 |
| create ACK 불명/identity 불명 | 재시작 진입 전 거절 | visible failure, Pending/queue custody 유지, blind create 재시도 0 |
| clear 경합 | 기존 clear 동작 유지 | generation 변경 뒤 이전 restart의 create/Ready 게시를 금지하는 **실제 guard 구현 필요** |

## 검사할 규칙

- `InputCustody`: 수락한 원본은 기존 queue/hold/전달 결과 또는 명시 clear의 terminal receipt에 남는다. reset 자체 purge는 허용하지 않는다.
- `InputAtMostOnce`, `NoOldPaneInput`: 수락 이후 원본 effect는 최대 1회이고 옛 pane 전달은 0회다. 수락 시각은 Discord 도착이 아니라 transition+old fence 획득 선형화다.
- `SingleReadySession`, `FreshIncarnation`: Ready라면 옛 pane은 종료되고 새 pane은 정확히 하나이며 새 nonce를 쓴다.
- `NoForeignClose`, `NoBlindCreate`, `SeedAtMostOnce`: 바뀐 server/pane을 닫지 않고 ACK 불명 create와 seed를 맹목 반복하지 않는다.
- `InflightSettledBeforeReady`: Ready 전에 기존 inflight exact token이 결산되고 busy 경고가 존재한다.
- `NoSupersededReady`: explicit clear로 generation이 바뀌면 이전 restart는 새 Ready를 게시하지 않는다.
- `RequestResolves`: 요청은 결국 새 Ready 또는 사용자에게 보이는 실패로 끝난다.
- `RestartAvailable`: healthy+clear 없음은 새 Ready여야 한다. 실패 환경에서는 보이는 실패까지 허용한다. Before 안전성 GREEN을 기능 PASS로 오인하지 않는 별도 규칙이다.
- `AcceptedInputResolves`: 수락 원본은 결국 successor 전달, 명시 clear receipt, 보이는 실패 중 하나로 결산된다.

## 결과

정식 저장소 checker를 scratch root에 실행한 최종 결과: **23 cfg / 101 TLC 하위 실행, 11 GREEN + 12 기대 RED, 예상 위반 집합 전부 일치, exit 0**. 총 48.2초, cfg 최대 5.3초로 cfg 300초 예산 안이다. 저장소 전체 기본 root를 검사한 결과가 아니다.

별도 `statechart` wrapper 23 cfg가 Mermaid·개별 보고서를 생성했다. 기대 RED의 wrapper exit 1은 의도한 불변식/진행성 반례다. 미확인/skip 규칙은 0이다. 초기 작업본의 false-condition PROPERTY가 tautology로 최적화될 가능성을 제거하고, 정식 checker가 safety 후 temporal property를 각각 실제 실행한 최종 로그로 판정했다.

`Concurrent`는 **16 states / 26 steps**, 원시 DOT에서 `Admit` 7개 edge의 source phase가 `closing/retiring/preparing/creating/binding/ready` 여섯 종류 모두임을 확인했다(`CONCURRENT_ADMIT_COVERAGE_OK`). 이 직접 TLC dump 1회는 위 101 checker 하위 실행과 별도다.

모델 SHA256: `a921106c4fd2ac5455d59ca867a20791a88963c51e3841e2dec6089052380b16`. 제안 manifest SHA256: `4aeb4b8956087aacae38aa1015ade2c020c90869c9aa57f14b33e495eec539ef`.
환경: macOS, `SHELL=/bin/zsh`, `TMPDIR` 미설정, OpenJDK `27.0.1.0.3`, Node `v26.13.0`, `tla2tools-1.7.4.jar`의 TLC2 2.19. Cargo/Rust 시험·PG/E2E·herdr 실실행은 각각 0이다.

원시 로그: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8128/logs/model/checker-final-4.log`, `logs/model/final-4/<suffix>/tlc.out` 및 `tlc-<rule>.out`; N=2 `logs/model/concurrent-final.dot`, `concurrent-dump-final.log`, `concurrent-coverage-final.json`. 최종 입력 사본은 `logs/model/inputs-final/`과 `/tmp/i8128-restart-model`이며, 커밋한 입력과 바이트 동일하다.

| cfg suffix | TLC states/steps | 실제 깨진 규칙 | 판정 |
|---|---:|---|---|
| [After](HerdrRestartAfter.cfg) | 8/9 | — | GREEN |
| [Before](HerdrRestartBefore.cfg) | 3/4 | `RestartAvailable` | 기대 RED |
| [ClearSupersedes](HerdrRestartClearSupersedes.cfg) | 8/9 | — | GREEN |
| [CloseFailure](HerdrRestartCloseFailure.cfg) | 4/5 | — | GREEN |
| [Concurrent](HerdrRestartConcurrent.cfg) | 16/26 | — | GREEN |
| [CreateUnconfirmed](HerdrRestartCreateUnconfirmed.cfg) | 7/8 | — | GREEN |
| [CreateUnknown](HerdrRestartCreateUnknown.cfg) | 7/8 | — | GREEN |
| [DropQueue](HerdrRestartDropQueue.cfg) | 7/7 | `InputCustody`, `AcceptedInputResolves` | 기대 RED |
| [IdentityChanged](HerdrRestartIdentityChanged.cfg) | 4/5 | — | GREEN |
| [NoActive](HerdrRestartNoActive.cfg) | 8/9 | — | GREEN |
| [NoEpoch](HerdrRestartNoEpoch.cfg) | 8/8 | `NoSupersededReady` | 기대 RED |
| [NoNotice](HerdrRestartNoNotice.cfg) | 3/3 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |
| [NoWake](HerdrRestartNoWake.cfg) | 7/7 | `AcceptedInputResolves` | 기대 RED |
| [NotSent](HerdrRestartNotSent.cfg) | 7/8 | — | GREEN |
| [NumericTarget](HerdrRestartNumericTarget.cfg) | 7/10 | `NoForeignClose` | 기대 RED |
| [OldInput](HerdrRestartOldInput.cfg) | 14/16 | `NoOldPaneInput` | 기대 RED |
| [ReplayInput](HerdrRestartReplayInput.cfg) | 10/13 | `InputAtMostOnce` | 기대 RED |
| [ReplaySeed](HerdrRestartReplaySeed.cfg) | 9/10 | `SeedAtMostOnce` | 기대 RED |
| [ReplayUnknown](HerdrRestartReplayUnknown.cfg) | 8/9 | `NoBlindCreate` | 기대 RED |
| [ReuseNonce](HerdrRestartReuseNonce.cfg) | 8/9 | `FreshIncarnation` | 기대 RED |
| [SeedFailure](HerdrRestartSeedFailure.cfg) | 8/9 | — | GREEN |
| [SkipClose](HerdrRestartSkipClose.cfg) | 8/9 | `SingleReadySession` | 기대 RED |
| [UnknownRow](HerdrRestartUnknownRow.cfg) | 4/5 | — | GREEN |

## 반례와 실제 코드 대조

- [Before: `RestartAvailable`](herdr-restart-Before.report.md): 0. Old session and original queued input → 1. User /restart is refused before cancellation: Failure awaiting a visible result → 2. Show the failure and retain input custody: Visible failure; originals remain owned `src/services/discord/commands/restart.rs:219` → `admin_host_guard.rs:143` → `restart.rs:287`로 실제 거절 분기 정적 추적 확인.

- [DropQueue: `InputCustody`](herdr-restart-DropQueue.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Accepted input lost its owner clear의 queue discard(`src/services/discord/commands/control.rs:552`)를 restart에 재사용하면 같은 유실 원인이 된다. 제안에서는 재사용 금지.

- [DropQueue: `AcceptedInputResolves`](herdr-restart-DropQueue.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Accepted input lost its owner → 4. Commit new incarnation before create: Accepted input lost its owner → 5. Create reply is observed once: Accepted input lost its owner → 6. Evidence and binding admit the successor: Accepted input lost its owner clear의 queue discard(`src/services/discord/commands/control.rs:552`)를 restart에 재사용하면 같은 유실 원인이 된다. 제안에서는 재사용 금지.

- [NoEpoch: `NoSupersededReady`](herdr-restart-NoEpoch.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Old incarnation retired; originals still held → 4. Commit new incarnation before create: Fresh incarnation Pending before create → 5. User clear discards originals with terminal receipts: Fresh incarnation Pending before create → 6. Create reply is observed once: New pane created; wait for evidence and binding → 7. Evidence and binding admit the successor: Superseded restart published Ready clear timeout 진행(`src/services/discord/commands/control.rs:523`) 뒤 old restart generation 재검사 제안. 현재 restart guard는 아직 없음.

- [NoNotice: `RequestResolves`](herdr-restart-NoNotice.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Exact close is unconfirmed: Failure awaiting a visible result 기존 보이는 refusal/result(`src/services/discord/commands/restart.rs:287`, `:294`)와 새 실패 알림 debt를 연결해야 함.

- [NoNotice: `RestartAvailable`](herdr-restart-NoNotice.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Exact close is unconfirmed: Failure awaiting a visible result 기존 보이는 refusal/result(`src/services/discord/commands/restart.rs:287`, `:294`)와 새 실패 알림 debt를 연결해야 함.

- [NoNotice: `AcceptedInputResolves`](herdr-restart-NoNotice.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Exact close is unconfirmed: Failure awaiting a visible result 기존 보이는 refusal/result(`src/services/discord/commands/restart.rs:287`, `:294`)와 새 실패 알림 debt를 연결해야 함.

- [NoWake: `AcceptedInputResolves`](herdr-restart-NoWake.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Old incarnation retired; originals still held → 4. Commit new incarnation before create: Fresh incarnation Pending before create → 5. Create reply is observed once: New pane created; wait for evidence and binding → 6. Evidence and binding admit the successor: New Ready session has no input wake 기존 transition release owner wake(`src/services/discord/router/message_handler/intake_turn/runtime_transition.rs:58`)를 유지·연결해야 함.

- [NumericTarget: `NoForeignClose`](herdr-restart-NumericTarget.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Reuse a numeric target after replacement: Reused numeric target was closed 같은 witness close(`src/services/session_host/herdr_gate.rs:496`, `:658`)를 숫자 ID 재접속으로 대체하지 않음.

- [OldInput: `NoOldPaneInput`](herdr-restart-OldInput.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Stale input permission types into old pane: Old pane received a restart input old fence와 transition requeue(`src/services/discord/herdr_terminate.rs:519`, `intake_turn/runtime_transition.rs:78`)가 필요한 제안 경계.

- [ReplayInput: `InputAtMostOnce`](herdr-restart-ReplayInput.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Old incarnation retired; originals still held → 4. Commit new incarnation before create: Fresh incarnation Pending before create → 5. Create reply is observed once: New pane created; wait for evidence and binding → 6. Evidence and binding admit the successor: One new Ready session; originals await the owner → 7. Submit an original to the successor once: One new Ready session; originals settled once → 8. Forget an original receipt: One new Ready session; originals await the owner → 9. Submit an original to the successor once: Original input executed twice 기존 original/terminal identity(`src/services/turn_orchestrator/queued_head/v2_types.rs:22`, `:25`)를 유지하는 제안.

- [ReplaySeed: `SeedAtMostOnce`](herdr-restart-ReplaySeed.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Old incarnation retired; originals still held → 4. Commit new incarnation before create: Fresh incarnation Pending before create → 5. Create reply is observed once: New pane created; wait for evidence and binding → 6. Evidence and binding admit the successor: New pane created; wait for evidence and binding → 7. Evidence and binding admit the successor: Restart seed executed twice 기존 seed는 accepted 요청을 반복하지 않음(`src/services/discord/commands/restart.rs:174`). typed seed ticket/receipt에도 동일한 1회 결산 필요.

- [ReplayUnknown: `NoBlindCreate`](herdr-restart-ReplayUnknown.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Old incarnation retired; originals still held → 4. Commit new incarnation before create: Fresh incarnation Pending before create → 5. Create reply is observed once: Fresh incarnation Pending before create → 6. Create reply is observed once: Unknown create acknowledgement was replayed 현재 launch ACK 불명은 Pending 보존(`src/services/herdr_launch.rs:218`); successor CAS 전 create를 새로 호출하지 않음.

- [ReuseNonce: `FreshIncarnation`](herdr-restart-ReuseNonce.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Old incarnation retired; originals still held → 4. Commit new incarnation before create: Fresh incarnation Pending before create → 5. Create reply is observed once: New pane reused the old incarnation 기존 incarnation 준비와 Pending CAS(`src/services/herdr_launch.rs:466`, `:480`)를 사용해야 함.

- [SkipClose: `SingleReadySession`](herdr-restart-SkipClose.report.md): 0. Old session and original queued input → 1. User /restart is accepted under transition and fence: Restart excludes old input and captures exact pane → 2. Close only the captured pane: Exact close confirmed; settle old inflight → 3. Retire and settle only the old token: Old incarnation retired; originals still held → 4. Commit new incarnation before create: Fresh incarnation Pending before create → 5. Create reply is observed once: New pane created; wait for evidence and binding → 6. Evidence and binding admit the successor: Two live sessions after restart 기존 exact close→read_end→retire→settle(`src/services/discord/herdr_terminate.rs:590`, `:594`, `:596`, `:603`)를 건너뛰지 않음.

위 mutant는 생산 코드를 바꾸거나 실행한 결과가 아니다. main에서 도달 불가능한 경로는 제안 guard를 제거했을 때의 반례이며, 실제 구현 레그에서 같은 입구와 fault 주입으로 RED→GREEN→원인 뮤턴트 RED를 확인해야 한다. 확인된 현재 기능 공백은 Before의 `/restart` 거절뿐이다. 실환경 herdr 조작이 금지된 설계 레그이므로 실재현 PASS는 0건이다.

## 포트 가정과 한계

1. `Request`의 after 분기는 기존 transition와 exact execution fence를 함께 잡고 입력 권한을 바꾸는 제안 포트다. 현재 `/restart`에 이미 있다는 주장이 아니다. transition 경합 재큐잉의 운영 호출자는 기존 `src/services/discord/router/message_handler/intake_turn.rs:661`, `src/services/discord/router/message_handler/intake_turn/runtime_transition.rs:37`·`:78`이며, restart operation intent와 source nonce를 실제 enqueue/submit 직전 재검사해야 한다.
2. exact close는 기존 `terminate_explicit_herdr`와 `RetiredExclusion` (`src/services/discord/herdr_terminate.rs:401`, `src/services/discord/herdr_terminate/retired_exclusion.rs:16`)의 보장에 의존한다. exclusion을 잡은 채 lower launch/seed로 이전하는 lock 순서는 이 모델이 증명하지 않는다. 기존 clear의 selector 삭제 postlude (`src/services/discord/commands/control/clear_teardown.rs:175`)를 restart에서 그대로 호출해서는 안 된다. `disconnect_session_keeping_provider` (`src/services/discord/herdr_terminate/retired_exclusion.rs:99`) 등 selector 보존 경로를 실제 연결하고 검사해야 한다.
3. `BindReady`는 provider별 seed/source attach port다. 특히 Codex는 첫 입력 뒤 attach하므로 pre-seed Bound를 기다리면 교착한다. seed만 queue에 accepted된 상태, evidence gap, ACK 불명 상태는 Ready가 아니다.
4. 각 close/create/notice I/O가 결국 결과를 내고 owner wake가 결국 실행된다는 weak fairness를 적용했다. 영구 네트워크 단절·반환하지 않는 future·프로세스 crash는 이 모델 밖이다. 실패 reply 전달 자체도 무한히 불가능한 경우 사용자 visible failure를 증명하지 못한다.
5. 새 optional restart intent의 fsync/deserialize/boot census는 아직 구현되지 않았다. `SnapshotV2` (`src/services/turn_orchestrator/queued_head/v2_types.rs:38`) 옆에 보존하고 입력 0인 intent도 `src/services/turn_orchestrator/queued_head/compat.rs:23`의 holds_input/census가 발견하게 해야 한다. intent-only crash, 옛 V1/V2 복원, source mismatch, ACK 유실 복구는 구현 레그 필수 시험이며 본 TLC GREEN으로 대체하지 않는다.
6. Discord 사람/봇의 accepted owner input만 이 모델의 입력이다. API `executor_not_wired`, 임의 TUI native typing, 설정 전환·다중 endpoint·독립 운영자·반복 restart 요청/코디네이터 crash는 제외한다. 입력 2개는 N=2에서 각 restart 단계와 interleaving하지만 무한 큐·무한 요청의 증명은 아니다.
7. clear 모델은 create 직전 한 번의 generation 변경과 명시 폐기 receipt만 검사한다. 물리 create 전송 중 clear, close 후 read-back 전 clear, seed effect 중 clear, 기존 clear 자신의 종료 효과, 실패 notice 재전송 dedup은 구현 시험으로 남는다. transition 하나만으로 clear가 직렬화된다는 가정은 쓰지 않았다.

## CI 등록 경계

`SC_CI=NEEDS_FILES`: 저장소 `docs/statecharts/expected.tsv`는 변경하지 않았다. 공통 레인 규칙의 baseline 항목 변경 FILES 경계를 보수적으로 적용해 코디네이터의 등록 승인을 기다린다. 따라서 저장소 기본 root 전체 검사 통과를 주장하지 않는다. 아래 결과는 새 cfg 23개만 복사한 scratch root와 제안 manifest의 실제 검사다. 신규 cfg 등록 후 같은 rows를 저장소 expected.tsv에 추가하고 changed-config checker를 실행해야 CI 통합이 끝난다. 기존 violation baseline을 바꾸거나 완화한 항목은 없다.

요청할 정확 등록 행:

```tsv
session-lifecycle/HerdrRestartAfter.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartBefore.cfg	HerdrRestart	-	RestartAvailable
session-lifecycle/HerdrRestartClearSupersedes.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartCloseFailure.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartConcurrent.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartCreateUnconfirmed.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartCreateUnknown.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartDropQueue.cfg	HerdrRestart	InputCustody	AcceptedInputResolves
session-lifecycle/HerdrRestartIdentityChanged.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartNoActive.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartNoEpoch.cfg	HerdrRestart	NoSupersededReady	-
session-lifecycle/HerdrRestartNoNotice.cfg	HerdrRestart	-	RequestResolves,RestartAvailable,AcceptedInputResolves
session-lifecycle/HerdrRestartNoWake.cfg	HerdrRestart	-	AcceptedInputResolves
session-lifecycle/HerdrRestartNotSent.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartNumericTarget.cfg	HerdrRestart	NoForeignClose	-
session-lifecycle/HerdrRestartOldInput.cfg	HerdrRestart	NoOldPaneInput	-
session-lifecycle/HerdrRestartReplayInput.cfg	HerdrRestart	InputAtMostOnce	-
session-lifecycle/HerdrRestartReplaySeed.cfg	HerdrRestart	SeedAtMostOnce	-
session-lifecycle/HerdrRestartReplayUnknown.cfg	HerdrRestart	NoBlindCreate	-
session-lifecycle/HerdrRestartReuseNonce.cfg	HerdrRestart	FreshIncarnation	-
session-lifecycle/HerdrRestartSeedFailure.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartSkipClose.cfg	HerdrRestart	SingleReadySession	-
session-lifecycle/HerdrRestartUnknownRow.cfg	HerdrRestart	-	-
```

## 다시 실행

설계용 scratch manifest는 위 23개 행에서 `session-lifecycle/` prefix만 제거한 값이다. 모델·cfg·view를 새 scratch root에 복사하고 manifest를 그 root의 `expected.tsv`로 만든 뒤 실행한다(저장소 manifest 승인을 우회하는 production 절차가 아니다).

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --root /tmp/i8128-restart-model --jobs 2
/Users/itismyfield/.codex/skills/statechart/scripts/statechart \
  /tmp/i8128-restart-model/HerdrRestart.tla \
  --config /tmp/i8128-restart-model/HerdrRestartAfter.cfg \
  --view /tmp/i8128-restart-model/herdr-restart.view.mjs \
  --out /tmp/i8128-restart-model-render-new
```

[제안 정상 흐름 Mermaid](herdr-restart-After.chart.mmd), [N=2 흐름](herdr-restart-Concurrent.chart.mmd), [현재 거절](herdr-restart-Before.chart.mmd), [clear supersession](herdr-restart-ClearSupersedes.chart.mmd). 각 cfg의 `<suffix>.chart.mmd`와 `<suffix>.report.md`는 모두 같은 모델·view로 생성되었다.

Mermaid는 statechart wrapper가 같은 모델·view에서 생성했다. 원본 입력 수락·전달이 같은 chart label 안에서 상태를 바꾸는 내부 step과 terminal stutter만 접히며, 모든 reachable phase와 bug state·화살표는 남긴다. 각 generated report가 숨긴 step 수를 기록한다. `Concurrent`는 N=2 전체 유계 interleaving을 검사하며 `After`의 단일 입력 8-state 결과만으로 동시성 안전을 주장하지 않는다.
