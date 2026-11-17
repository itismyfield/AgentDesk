# Herdr `/restart` — Codex resume A 모델

> B PLAN 경계(588241083ccf4c2cda6d147e89e1d9fd10725387): A offset/cause/fresh 계약과23cfg 모델 바이트를 유지한다. 아래 s 영역은 r1 direct-launch의 역사적 투영이며 현재 B1/B2 정본은 [Owner 보고서](herdr-restart-owner.report.md)다. 2c887338의 “A 구현 필요”는 당시 Before 기록이고 A 코드는 이후 병합됐다. owner seed·idle repair·effect permission·부팅의 생산 완료를 이 A 보고서로 주장하지 않는다.

A 코드 조사 기준: `2c8873381478c7179f0347d5bfc5b850d5cb90e2`. 기존 B 설계 기준은 `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`다. **이 모델 작업은 생산 코드 수정·Rust 빌드·PG/herdr 운영 조작 0**이며, 부모 레인의 A 구현·Rust 시험 증거는 별도다.

`HerdrRestart`의 기존 23 cfg에 A 변수를 추가했다. 새 cfg는 0개다. 기존 B (`s`)와 Codex provider helper A (`a`)는 독립 계약을 Cartesian product로 검사한다. 그림의 두 병렬 영역은 그 두 계약의 투영이며, B의 `/restart`와 A helper가 실제로 연결되어 같은 채널에서 동시에 실행된다는 증명이 아니다. 그 연결·권한 전달·owner seed 부팅 재개는 B 구현과 실제 시험이 필요하다. 기존 B 모델의 production 미구현 포트는 그대로 남는다.

## A 진입점·값 범위

| 값/단계 | 기준 코드 좌표 | 모델 범위 |
|---|---|---|
| 일반 launch `resume=false` | `src/services/codex/herdr_turn.rs:188`, `:193`, `:194` | `AResumeIntent=FALSE`이면 mode는 항상 `fresh`; native session이 있다는 이유로 resume으로 바꾸지 않음 |
| immutable incarnation | `src/services/herdr_launch.rs:733`, `:744`, `:749`, `:750`, `:758`; `src/services/tui_prompt_dedupe/binding_context.rs:21` | 새 nonce와 explicit `fresh/resume` 모드. A의 reader start offset은 같은 incarnation에 내구 기록하는 추가 포트 |
| 내구 기록·복원 | `src/services/tui_prompt_dedupe/binding_context.rs:366`, `:379`, `:382`, `:389`, `:495`, `:500` | 파일·directory durability 뒤 launch, boot는 같은 nonce의 기록을 읽음. 기준 구조에는 resume offset 필드가 없으므로 A 구현이 필요 |
| old End 이후 EOF | `src/services/discord/herdr_terminate.rs:590`, `:594`, `:596`, `:603` | `endConfirmed` proof가 있는 뒤에만 resume EOF 캡처. 이 proof 전달은 실제 호출자 검증 대상 |
| offset `0/1/2` | 기존 first-input `src/services/codex/herdr_turn/first_input.rs:211`, `:216`, `:225`; tail offset 인자 `src/services/codex/herdr_turn.rs:658`, `:676` | 0=fresh 시작, 1=확인된 old End의 nonzero EOF, 2=새 startup metadata가 붙은 EOF. 바이트 길이의 작은 동치류이며 실제 파일 크기 값이 아님 |
| O cursor와 EOF | 기존 source/offset를 소비하는 위 경로와 A capture 포트 | 캡처 시 `ACursorMatches=TRUE/FALSE`. 불일치는 A에서 resume 거절; fresh fallback은 B 정책이므로 A 모델에 없음 |
| `Startup/Resume` cause | consumer `src/services/codex/herdr_turn.rs:279`, `:288`, `:290`; producer `src/services/tui_prompt_dedupe/binding_events/codex.rs:845`, `:857` | explicit resume의 자기 nonce/source는 Resume도 허용. ordinary fresh는 Startup. 임의 Resume 이벤트 전체 허용을 증명하지 않음 |
| seed 전 crash·boot | 위 incarnation read/write와 first-input `observe(Submitted)` (`first_input.rs:185`) | launch/startup metadata 후 첫 seed 전 1회 crash. volatile offset/nonce 소실 후 durable 기록 복원. `AObserve`는 첫 seed가 제출된 뒤 Source attach·tail을 관찰하는 포트이며 seed exactly-once 자체는 B 영역 계약 |

`AResumeIntent`는 검증한 explicit resume 준비 요청이다. 소스 문자열/설정 추측으로 만든 bool이 아니다. 모델은 source nonce/native session/dev·ino 검증이 끝난 포트를 입력으로 가정하며 그 검증 자체를 증명하지 않는다. EOF가 0인 resume, 반복 crash, 제출 후 ACK 불명·임의 native 키는 이번 A축 밖이다.

## A 수정 전/후와 원인 대조

| 경계 | Before (`2c887338`) | After A 포트/양성 control | 원인 mutant·실제 검증 규칙 |
|---|---|---|---|
| resume 시작 가능 | 일반 경로 fresh-only, resume 요청을 거절 | explicit intent에서만 resume 준비·진행 | `Before`: `AResumeAvailable` 기대 RED. 공개 명령 자체가 열렸다는 뜻은 아님 |
| 관찰 시작점 | first-input offset/read-reply가 0 | old End 이후 EOF=1을 incarnation에 내구 기록 | `OldInput/OffsetZero`: `AOffsetPreserved`, `ANoHistoricalReplay` RED |
| seed 전 crash | reader offset 기록이 없어 복구 계약 없음 | offset1 기록 → 새 startup EOF2 → crash → 같은 nonce/offset1 복원 | `ReplayUnknown/BootRecapture`: `ABootOffsetStable`, `AOffsetPreserved`, `ANoNewPrefixSkip` RED |
| Resume 원인 | consumer는 Startup만 수락, producer는 Resume를 기록할 수 있음 | explicit resume의 정확한 자기 cause 수락 | `NoWake/StartupOnly`: `AResumeAvailable` RED. fresh-only validator를 열어도 consumer를 남기면 생기는 trap이며 현재 main에서 resume이 이미 실행된다는 주장이 아님 |
| 일반 launch | `resume=false` | `NoActive`, `IdentityChanged`: fresh/Startup/offset0 GREEN | `ReuseNonce/OrdinaryResume`: `AOrdinaryFresh` RED |
| End 시각 | 기존 exact end 증거를 공급해야 함 | EOF 캡처 전 confirmed old End | `SkipClose/BeforeEndCapture`: `AAfterConfirmedEnd` RED |
| O cursor != EOF | resume 캡처 포트 없음 | `SeedFailure`: visible refusal, A launch/tail 없음 | `DropQueue/IgnoreCursorMismatch`: `ACursorAgreement` RED |
| durable 기록 누락 | 기존 context에 reader offset 필드 없음 | fsync를 포함한 record가 launch 전에 존재 | `ReplaySeed/VolatileOffset`: `ADurableBeforeLaunch`, `AResumeAvailable` RED |

O cursor==EOF 검사는 **old End 직후 capture 시점**이다. boot/첫 입력 때 immutable offset1을 현재 EOF2로 갱신하지 않는다. 같은 파일에 새 metadata가 붙었다는 이유로 O cursor와 현재 EOF를 다시 동등 비교하면 정상 resume을 거절한다. 파일 교체·짧아짐·native mismatch는 실제 A 코드가 별도로 거절해야 한다.

## A INVARIANT·PROPERTY

- `AAfterConfirmedEnd`, `ACursorAgreement`: confirmed End 이후의 EOF만 준비하고 O cursor 불일치는 resume 허가가 아니다.
- `AOrdinaryFresh`: explicit resume intent 없는 일반 launch는 `fresh`다.
- `ADurableBeforeLaunch`, `ABootOffsetStable`: launch 전에 offset·nonce가 기록되고 crash 뒤에도 같은 offset·nonce를 쓴다.
- `AOffsetPreserved`, `ANoHistoricalReplay`, `ANoNewPrefixSkip`: tail은 캡처한 시작점을 그대로 사용하며 old prefix replay와 새 prefix 건너뜀은 0이다.
- `AScopedCause`: fresh는 Startup, resume은 Resume의 검증된 own-source 포트로 tail을 시작한다.
- `ARequestResolves`: helper는 완료 또는 보이는 실패로 끝난다.
- `AResumeAvailable`: 유효한 explicit resume/ordinary fresh는 완료해야 하며, O cursor 불일치에서는 보이는 거절이 올바른 결과다.

## 실제 모델 결과

새 모델 바이트로 **23 cfg / 156 TLC 하위 실행, 11 GREEN + 12 기대 RED, 예상 위반 집합 전부 일치, exit 0**. 총108.7초, cfg 최대12.8초로 300초/cfg 예산 안이다. safety 후 5 PROPERTY를 각각 실제 실행했으며 미검사/skip은 0이다. 옛 설계 모델 실행 증거는 재사용하지 않았다.

`After`는64 states/136 steps, `Concurrent`는128 states/336 steps다. 원시 N=2 DOT에서 `RAdmit` 56개 edge의 source가 closing/retiring/preparing/creating/binding/ready 전부임을 확인했다. A의 crash/boot/tail 각16개 edge에서 durable offset1·startup 후 EOF2·crash 후 volatile0·boot 복원1·tail1을 실제 assert했다: `A_DURABLE_OFFSET_BOOT_COVERAGE_OK`.

모델 SHA256: `11be1251b5e3854547c95dcecce55ffd26ee599fce748c39bc609871f5106f5d`. scratch manifest SHA256: `6c5124c779f25dd7ce8c6b8f75ac38a83df87bb2987359965b3d48f6ac3937ac`.
원시 로그: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8128/logs/impl-a/model/checker-a-final.log`, `render-a-final/<suffix>/tlc.out`·`tlc-<rule>.out`, `concurrent-final.dot`, `concurrent-coverage-final.json`. 최종 입력 사본: `logs/impl-a/model/inputs-final/`. 저장소 모델/cfg/view와 바이트 동일하다.
환경은 OpenJDK27.0.1.0.3 / Node26.13.0 / TLC2 2.19(tla2tools1.7.4), SHELL=/bin/zsh, TMPDIR 미설정. Rust/PG/herdr 실실행과 production PASS는 이 모델 작업에서 각각0이다.

첫 DOT 관측은 wrapper action label(RestartNext/ANext) 때문에 Admit 계수를0으로 읽어 assertion 실패했다(`concurrent-dump.log` 옆 초기 진단). 최종 모델은 행동별 이름을 보존했고 위 final DOT만 coverage 근거로 쓴다. 초기 시도의 exit0/0개를 PASS로 채택하지 않았다.

| cfg suffix | TLC states/steps | 실제 깨진 규칙 | 판정 |
|---|---:|---|---|
| [After](HerdrRestartAfter.cfg) | 64/136 | — | GREEN |
| [Before](HerdrRestartBefore.cfg) | 6/14 | `RestartAvailable`, `AResumeAvailable` | 기대 RED |
| [ClearSupersedes](HerdrRestartClearSupersedes.cfg) | 64/136 | — | GREEN |
| [CloseFailure](HerdrRestartCloseFailure.cfg) | 32/72 | — | GREEN |
| [Concurrent](HerdrRestartConcurrent.cfg) | 128/336 | — | GREEN |
| [CreateUnconfirmed](HerdrRestartCreateUnconfirmed.cfg) | 56/120 | — | GREEN |
| [CreateUnknown](HerdrRestartCreateUnknown.cfg) | 56/120 | — | GREEN |
| [DropQueue](HerdrRestartDropQueue.cfg) | 56/112 | `ACursorAgreement`, `InputCustody`, `AcceptedInputResolves` | 기대 RED |
| [IdentityChanged](HerdrRestartIdentityChanged.cfg) | 32/72 | — | GREEN |
| [NoActive](HerdrRestartNoActive.cfg) | 64/136 | — | GREEN |
| [NoEpoch](HerdrRestartNoEpoch.cfg) | 64/128 | `NoSupersededReady` | 기대 RED |
| [NoNotice](HerdrRestartNoNotice.cfg) | 24/48 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |
| [NoWake](HerdrRestartNoWake.cfg) | 56/112 | `AcceptedInputResolves`, `AResumeAvailable` | 기대 RED |
| [NotSent](HerdrRestartNotSent.cfg) | 56/120 | — | GREEN |
| [NumericTarget](HerdrRestartNumericTarget.cfg) | 56/136 | `NoForeignClose` | 기대 RED |
| [OldInput](HerdrRestartOldInput.cfg) | 112/240 | `NoOldPaneInput`, `AOffsetPreserved`, `ANoHistoricalReplay` | 기대 RED |
| [ReplayInput](HerdrRestartReplayInput.cfg) | 80/184 | `InputAtMostOnce` | 기대 RED |
| [ReplaySeed](HerdrRestartReplaySeed.cfg) | 63/133 | `ADurableBeforeLaunch`, `SeedAtMostOnce`, `AResumeAvailable` | 기대 RED |
| [ReplayUnknown](HerdrRestartReplayUnknown.cfg) | 64/136 | `ABootOffsetStable`, `NoBlindCreate`, `AOffsetPreserved`, `ANoNewPrefixSkip` | 기대 RED |
| [ReuseNonce](HerdrRestartReuseNonce.cfg) | 64/136 | `AOrdinaryFresh`, `FreshIncarnation` | 기대 RED |
| [SeedFailure](HerdrRestartSeedFailure.cfg) | 24/51 | — | GREEN |
| [SkipClose](HerdrRestartSkipClose.cfg) | 112/246 | `AAfterConfirmedEnd`, `SingleReadySession` | 기대 RED |
| [UnknownRow](HerdrRestartUnknownRow.cfg) | 32/72 | — | GREEN |

## A 반례 경로와 코드 대조

- [Before: `AResumeAvailable`](herdr-restart-Before.report.md): 0 → 1 → 2 → 3. Show the failure and retain input custody: Restart lifecycle B Visible failure; originals remain owned, Codex resume helper A Visible preparation or binding failure; no tail

- [DropQueue: `ACursorAgreement`](herdr-restart-DropQueue.report.md): 0 → 1 → 2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A Resume accepted a mismatched O cursor

- [DropQueue: `AcceptedInputResolves`](herdr-restart-DropQueue.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor

- [NoNotice: `AcceptedInputResolves`](herdr-restart-NoNotice.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Resume cause accepted; old history excluded

- [NoWake: `AcceptedInputResolves`](herdr-restart-NoWake.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Visible preparation or binding failure; no tail

- [NoWake: `AResumeAvailable`](herdr-restart-NoWake.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Visible preparation or binding failure; no tail

- [OldInput: `AOffsetPreserved`](herdr-restart-OldInput.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Resumed reader replays old history

- [OldInput: `ANoHistoricalReplay`](herdr-restart-OldInput.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Resumed reader replays old history

- [ReplaySeed: `ADurableBeforeLaunch`](herdr-restart-ReplaySeed.report.md): 0 → 1 → 2 → 3 → 4. Launch once and observe own Startup or Resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Missing durable offset at before_seed

- [ReplaySeed: `AResumeAvailable`](herdr-restart-ReplaySeed.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13 → 14. Submit an original to the successor once: Restart lifecycle B Restart seed executed twice, Codex resume helper A Missing durable offset at failed

- [ReplayUnknown: `ABootOffsetStable`](herdr-restart-ReplayUnknown.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6. Recover the recorded incarnation and reader offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Boot replaced the recorded offset

- [ReplayUnknown: `AOffsetPreserved`](herdr-restart-ReplayUnknown.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Recovered reader skips new prefix

- [ReplayUnknown: `ANoNewPrefixSkip`](herdr-restart-ReplayUnknown.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Recovered reader skips new prefix

- [ReuseNonce: `AOrdinaryFresh`](herdr-restart-ReuseNonce.report.md): 0 → 1 → 2 → 3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Ordinary launch acquired resume mode

- [SkipClose: `AAfterConfirmedEnd`](herdr-restart-SkipClose.report.md): 0 → 1. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF was captured before confirmed old End

모든 반례는 개별 generated report의 번호를 따른다. `OffsetZero`는 first_input의 현재0 좌표, `StartupOnly`는 현재 Startup-only consumer, `BootRecapture/VolatileOffset`은 새 내구 offset 포트의 guard 제거, `OrdinaryResume`은 일반 launch의 false를 바꾸는 원인을 각각 분리했다. 실제 Rust mutation이나 herdr 실재현 결과가 아니므로 부모 구현에서 동일 원인 RED→GREEN→뮤턴트 RED를 증명해야 한다.

## 기존 B 진입점 값 범위

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

## 기존 B 규칙

- `InputCustody`: 수락한 원본은 기존 queue/hold/전달 결과 또는 명시 clear의 terminal receipt에 남는다. reset 자체 purge는 허용하지 않는다.
- `InputAtMostOnce`, `NoOldPaneInput`: 수락 이후 원본 effect는 최대 1회이고 옛 pane 전달은 0회다. 수락 시각은 Discord 도착이 아니라 transition+old fence 획득 선형화다.
- `SingleReadySession`, `FreshIncarnation`: Ready라면 옛 pane은 종료되고 새 pane은 정확히 하나이며 새 nonce를 쓴다.
- `NoForeignClose`, `NoBlindCreate`, `SeedAtMostOnce`: 바뀐 server/pane을 닫지 않고 ACK 불명 create와 seed를 맹목 반복하지 않는다.
- `InflightSettledBeforeReady`: Ready 전에 기존 inflight exact token이 결산되고 busy 경고가 존재한다.
- `NoSupersededReady`: explicit clear로 generation이 바뀌면 이전 restart는 새 Ready를 게시하지 않는다.
- `RequestResolves`: 요청은 결국 새 Ready 또는 사용자에게 보이는 실패로 끝난다.
- `RestartAvailable`: healthy+clear 없음은 새 Ready여야 한다. 실패 환경에서는 보이는 실패까지 허용한다. Before 안전성 GREEN을 기능 PASS로 오인하지 않는 별도 규칙이다.
- `AcceptedInputResolves`: 수락 원본은 결국 successor 전달, 명시 clear receipt, 보이는 실패 중 하나로 결산된다.

## B 포트 한계와 잔여

1. `Request`의 after 분기는 기존 transition와 exact execution fence를 함께 잡고 입력 권한을 바꾸는 제안 포트다. 현재 `/restart`에 이미 있다는 주장이 아니다. transition 경합 재큐잉의 운영 호출자는 기존 `src/services/discord/router/message_handler/intake_turn.rs:661`, `src/services/discord/router/message_handler/intake_turn/runtime_transition.rs:37`·`:78`이며, restart operation intent와 source nonce를 실제 enqueue/submit 직전 재검사해야 한다.
2. exact close는 기존 `terminate_explicit_herdr`와 `RetiredExclusion` (`src/services/discord/herdr_terminate.rs:401`, `src/services/discord/herdr_terminate/retired_exclusion.rs:16`)의 보장에 의존한다. exclusion을 잡은 채 lower launch/seed로 이전하는 lock 순서는 이 모델이 증명하지 않는다. 기존 clear의 selector 삭제 postlude (`src/services/discord/commands/control/clear_teardown.rs:175`)를 restart에서 그대로 호출해서는 안 된다. `disconnect_session_keeping_provider` (`src/services/discord/herdr_terminate/retired_exclusion.rs:99`) 등 selector 보존 경로를 실제 연결하고 검사해야 한다.
3. `BindReady`는 provider별 seed/source attach port다. 특히 Codex는 첫 입력 뒤 attach하므로 pre-seed Bound를 기다리면 교착한다. seed만 queue에 accepted된 상태, evidence gap, ACK 불명 상태는 Ready가 아니다.
4. 각 close/create/notice I/O가 결국 결과를 내고 owner wake가 결국 실행된다는 weak fairness를 적용했다. 영구 네트워크 단절·반환하지 않는 future·B orchestration의 프로세스 crash는 이 모델 밖이다. A helper의 seed 전 crash만 이번 A축에서 검사한다. 실패 reply 전달 자체도 무한히 불가능한 경우 사용자 visible failure를 증명하지 못한다.
5. 새 optional restart intent의 fsync/deserialize/boot census는 아직 구현되지 않았다. `SnapshotV2` (`src/services/turn_orchestrator/queued_head/v2_types.rs:38`) 옆에 보존하고 입력 0인 intent도 `src/services/turn_orchestrator/queued_head/compat.rs:23`의 holds_input/census가 발견하게 해야 한다. intent-only crash, 옛 V1/V2 복원, source mismatch, ACK 유실 복구는 구현 레그 필수 시험이며 본 TLC GREEN으로 대체하지 않는다.
6. Discord 사람/봇의 accepted owner input만 이 모델의 입력이다. API `executor_not_wired`, 임의 TUI native typing, 설정 전환·다중 endpoint·독립 운영자·반복 restart 요청/코디네이터 crash는 제외한다. 입력 2개는 N=2에서 각 restart 단계와 interleaving하지만 무한 큐·무한 요청의 증명은 아니다.
7. clear 모델은 create 직전 한 번의 generation 변경과 명시 폐기 receipt만 검사한다. 물리 create 전송 중 clear, close 후 read-back 전 clear, seed effect 중 clear, 기존 clear 자신의 종료 효과, 실패 notice 재전송 dedup은 구현 시험으로 남는다. transition 하나만으로 clear가 직렬화된다는 가정은 쓰지 않았다.

## CI 등록과 재실행

새 cfg0개. 기존23cfg에 A 상수/INVARIANT/PROPERTY를 추가했으므로 부모가 아래 expected 위반 집합을 기존23행에 갱신한다. 이 모델 담당은 저장소 `expected.tsv`·README·설계를 변경하지 않았다. scratch manifest에서 정식 checker가 전부 맞았다는 결과와 저장소 전체 CI 등록/통과는 구별한다. 기존 B 위반은 없어지거나 완화되지 않았다.

```tsv
session-lifecycle/HerdrRestartAfter.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartBefore.cfg	HerdrRestart	-	AResumeAvailable,RestartAvailable
session-lifecycle/HerdrRestartClearSupersedes.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartCloseFailure.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartConcurrent.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartCreateUnconfirmed.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartCreateUnknown.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartDropQueue.cfg	HerdrRestart	ACursorAgreement,InputCustody	AcceptedInputResolves
session-lifecycle/HerdrRestartIdentityChanged.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartNoActive.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartNoEpoch.cfg	HerdrRestart	NoSupersededReady	-
session-lifecycle/HerdrRestartNoNotice.cfg	HerdrRestart	-	AcceptedInputResolves,RequestResolves,RestartAvailable
session-lifecycle/HerdrRestartNoWake.cfg	HerdrRestart	-	AResumeAvailable,AcceptedInputResolves
session-lifecycle/HerdrRestartNotSent.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartNumericTarget.cfg	HerdrRestart	NoForeignClose	-
session-lifecycle/HerdrRestartOldInput.cfg	HerdrRestart	ANoHistoricalReplay,AOffsetPreserved,NoOldPaneInput	-
session-lifecycle/HerdrRestartReplayInput.cfg	HerdrRestart	InputAtMostOnce	-
session-lifecycle/HerdrRestartReplaySeed.cfg	HerdrRestart	ADurableBeforeLaunch,SeedAtMostOnce	AResumeAvailable
session-lifecycle/HerdrRestartReplayUnknown.cfg	HerdrRestart	ABootOffsetStable,ANoNewPrefixSkip,AOffsetPreserved,NoBlindCreate	-
session-lifecycle/HerdrRestartReuseNonce.cfg	HerdrRestart	AOrdinaryFresh,FreshIncarnation	-
session-lifecycle/HerdrRestartSeedFailure.cfg	HerdrRestart	-	-
session-lifecycle/HerdrRestartSkipClose.cfg	HerdrRestart	AAfterConfirmedEnd,SingleReadySession	-
session-lifecycle/HerdrRestartUnknownRow.cfg	HerdrRestart	-	-
```

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --root /tmp/i8128-restart-model-a --jobs 2
/Users/itismyfield/.codex/skills/statechart/scripts/statechart \
  /tmp/i8128-restart-model-a/HerdrRestart.tla \
  --config /tmp/i8128-restart-model-a/HerdrRestartAfter.cfg \
  --view /tmp/i8128-restart-model-a/herdr-restart.view.mjs \
  --out /tmp/i8128-restart-model-a-render-new
```

[After Mermaid](herdr-restart-After.chart.mmd), [N=2](herdr-restart-Concurrent.chart.mmd), [offset0 mutant](herdr-restart-OldInput.chart.mmd), [boot 재캡처 mutant](herdr-restart-ReplayUnknown.chart.mmd), [ordinary fresh](herdr-restart-NoActive.chart.mmd). 각 chart는 같은 최종 모델·view의 wrapper 생성물이다. 같은 label 안의 내부 step·terminal stutter만 접었고, 개별 report가 숨긴 수를 기록한다. 두 영역을 이어 주는 production 호출자는 이 그림이 증명하지 않는다.
