# Herdr restart B2b — seed 합류 소유권·인사말 1회 상태 차트

B2a canonical(`8b3d8c03…`)의 `HerdrRestartOwner`를 이어 고쳤다. TLA SHA256 `c5f6aae9b02d05534a7ef166cd17de43988f2e495ede1fbf9de8352576750de4`. 새 모듈 0, 새 cfg 2, 기존 cfg 27개에 불변식 2개 추가, expected.tsv 행 추가 2·행 변경 1(SeedBehind). 모델 담당의 cargo/PG/운영 실행 0, 생산 source 변경 0이다. 모델 GREEN은 B2b Rust/PG PASS가 아니다.

## 모델 변경

| 변경 | 내용 |
|---|---|
| 상태 `owner` | `InstallPending`이 successor를 띄운 launcher를 기록한다 |
| `SeedJoin` 소유 guard | 큐의 seed는 자기 attempt가 띄운 successor(`owner = SeedId`)에만 합류한다 |
| 새 행동 `SeedForeign` (WF) | 다른 입력이 띄운 `NewBound`를 만난 seed는 보이는 거절로 끝난다(`dropped` + `visible`). 인사말 0 |
| 불변식 `GreetingExactlyOnce` | `ready => seedEffects = 1` |
| 불변식 `GreetingOwnSession` | 이 restart가 만들지 않은 세션에 인사말 0 |
| `Behind` | 기존 `SeedBehind` 순서(인사말이 대기 입력 뒤)를 `JoinForeign`·`JoinHeld` 변형이 공유한다. foreign successor는 이 실제 순서 경로로만 도달한다(변형 전용 action 없음) |

Seeded 요청에 B2a detached worker·결과 cache 메타데이터(`a*`)를 켜는 일은 **하지 않았다**. 30분 안에 기존 63 cfg의 기대 집합 보존을 설명할 수 없어 남겼다. Seeded의 gate는 기존 `gate`/`SeedHeld`, 응답은 `Answer`/`Fail`이 계속 모델링한다. B2a의 cache-before-release 규칙은 Seedless 3 cfg에서만 검사된다.

## 실제 결과

**65 cfg / 638 TLC 실행, 기대 65/65 일치, GREEN 28·기대 RED 37, checker exit 0, 139.5초(jobs 12).** 기존 63 cfg 중 기대 집합이 바뀐 것은 SeedBehind 1개뿐이다.

| cfg | 이전 기대 | 현재 기대(실측 일치) | 사유 |
|---|---|---|---|
| `HerdrRestartOwnerSeedBehind` | NoOldPaneInput | NoOldPaneInput, **RestartAvailable** | 소유 guard가 생겨 뒤에 선 seed는 foreign successor에 합류하지 못하고 거절된다. Healthy인데 Ready가 없다. B2a 보고서가 이월한 "SeedBehind 추가 진행성"이며 RequestResolves는 보이는 거절로 성립한다 |
| `HerdrRestartOwnerJoinForeign` (새) | — | GreetingOwnSession, NoOldPaneInput | guard 제거 원인 변형 |
| `HerdrRestartOwnerJoinHeld` (새) | — | NoOldPaneInput; RequestResolves, RestartAvailable | guard는 있고 거절 행동이 없는 원인 변형: seed 영구 보류 |

## 수정 전 RED → 수정 후 GREEN

수정 전은 포트를 하나 제거한 **모델 원인 변형**이다(생산 RED 아님). 종료 코드는 checker가 규칙을 하나씩 떼며 반복한 TLC 실행의 것이다(안전 12, 진행성 13). timeout·parse 오류 0.

| 규칙 | 수정 전(변형) | 수정 후 |
|---|---|---|
| GreetingOwnSession (안전) | JoinForeign: RED exit 12 | SeedBehind(guard 적용, 같은 순서): 이 규칙 GREEN. 정상 26 cfg GREEN |
| RequestResolves (진행성) | JoinHeld: RED exit 13 | SeedBehind: 이 규칙 GREEN(`SeedForeign`의 보이는 거절) |
| RestartAvailable (진행성) | JoinHeld·SeedBehind: RED exit 13 (인사말을 뒤에 두는 순서 자체의 결과) | 정상 순서(After·Crash·Concurrent·DispatchRestart 등): GREEN |
| GreetingExactlyOnce (안전) | 등록하지 않은 대조 `CtlReadyOnAdmit`(ReadyOnAdmit + 이 규칙): RED exit 12, ReadyEvidence와 함께 | 정상 26 cfg GREEN |

## 브리프 규칙과 검사하는 cfg

정상 Seeded cfg 26개(After, ClearAny, ClearDuringLaunch, ClearOvertake, ClearSerial, CloseIndeterminate, CloseRefused, Concurrent, Crash, CrashCloseRefused, CrashLaunchUnknown, CrashRollback, DispatchCrash, DispatchRestart, GapClear 2종은 ClearForgets만 기대 RED, HeadBusy, HeadBusyCrash, LaunchNotSent, LaunchUnknown, Start 6종)가 아래를 모두 검사한다.

| 브리프 규칙 | 모델 규칙 | 상태 |
|---|---|---|
| 입력 유실 0 | InputCustody, AcceptedInputResolves | 기존 |
| 입력 중복 0 | InputAtMostOnce | 기존 |
| 인사말 정확히 1회 | SeedAtMostOnce(기존) + GreetingExactlyOnce(새) + GreetingOwnSession(새) | 추가 |
| 옛 pane 전달 0 | NoOldPaneInput, NoFalseReady | 기존 |
| 결국 Ready 또는 명시적 거절, 영구 보류 없음 | RequestResolves, RestartAvailable, CommandAnswers, EndedSeedRecovers | 기존(JoinHeld가 새 RED 대조) |

| 브리프 행동 | 기존 action | cfg |
|---|---|---|
| seed durable commit(수락) | `Request` (seed = queued) | After 외 전부 |
| 새 incarnation launch·successor Ready·인사말 | `LaunchBegin(SeedId)`→`InstallPending`→`Create`→`SubmitEffect`→`SettleReceipt`→`BindSource` | After, DispatchRestart |
| crash → 부팅 시 seed 복구 | `Crash`, `RowEndRecovery`(RecoveryPolicy = owned), `SeedRefused`, `Adopt`, 소유 `SeedJoin` | Crash, CrashCloseRefused, CrashLaunchUnknown, CrashRollback, DispatchCrash, HeadBusyCrash |
| 재시작 중 입력의 적재·seed 뒤 실행 | `Admit`, `FrontInput`(~SeedPending), `Deliver` | Concurrent, Crash(N=2), HeadBusy |

새 cfg가 필요한 미모델 행동은 없었다.

## 진입점 전수표

모델의 값 범위는 아래 실제 진입점에서만 가져왔다. 줄 번호는 이 보고서를 실은 커밋의 것이다.

| 경로·소비자 | file:line |
|---|---|
| slash 등록·인증 | `src/services/discord/commands/restart.rs:385`, `:336`, `:339` |
| `!restart` | `src/services/discord/commands/restart.rs:360` |
| Herdr 전용 분기 | `src/services/discord/commands/restart.rs:290` |
| operation·gate·detached worker | `src/services/discord/commands/restart/herdr.rs:30`, `:51`, `:67`, `:82`, `:88` |
| 기존 ticket 선조회 | `src/services/discord/commands/restart/herdr.rs:193` |
| canonical row 선택 | `src/services/discord/commands/restart/herdr.rs:122`, `:141`, `:147`, `:150` |
| seed 적재(수락) | `src/services/discord/commands/restart/herdr.rs:213`, `:217` |
| exact 종료·postlude | `src/services/discord/commands/restart/herdr.rs:276`, `:300`, `:314`, `:424` |
| commit 미확인 재확정·응답 | `src/services/discord/commands/restart/herdr.rs:231`, `:266`, `:262` |
| seed permit(부팅·재개) | `src/services/discord/commands/restart/herdr.rs:336`, `:380` |
| seed 유도·적재·철회 | `src/services/discord/queue_io/input_owner/restart_seed.rs:40`, `:76`, `:83`, `:126` |
| 일반 ingress의 source 거절 | `src/services/discord/queue_io/input_owner/admission.rs:93` |
| origin 분류 | `src/services/discord/queue_io/input_owner/origin.rs:38` |
| 단위 뒤 삽입·alias·상태·철회 | `src/services/turn_orchestrator/queued_head/v2/source_admission/restart_seed.rs:51`, `:61`, `:89`, `:113`, `:147` |
| admission 연결 | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:960`, `:977` |
| 사람 선행 이동 가드 | `src/services/turn_orchestrator/queued_head/v2.rs:130` |
| owner gate hold | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:175` |
| owner seed 시작·permit | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:878`, `:901`, `:527` |
| permit 거절의 종결 | `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:31`, `:47` |
| headless 판정 선택 | `src/services/discord/router/message_handler/headless_turn.rs:159`, `:160` |
| seed용 진입 판정 | `src/services/turn_host.rs:215`, `:177`, `:198` |
| operation→ticket | `src/services/turn_orchestrator/restart_operation.rs:30` |
| `!clear`·`/clear` | `src/services/discord/commands/control.rs:523` |
| clear의 owner 폐기 | `src/services/turn_orchestrator/queued_head/v2/control.rs:412`, `:207` |
| 강제 Cancel | `src/services/queue/http_force.rs:14` |
| `/stop` | `src/services/discord/commands/stop.rs:19` |
| provider reset | `src/services/discord/commands/control/provider_reset.rs:16` |
| 부팅 큐 복원 | `src/services/discord/runtime_bootstrap/queued_recovery.rs:50` |
| boot census 필터 | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:55` |
| 부팅 intake gate·wake | `src/services/discord/runtime_bootstrap.rs:41` |
| Herdr 새 launch | `src/services/herdr_launch.rs:382`, `:29` |
| Discord 사람·봇 intake | `src/services/discord/router/intake_queue_transaction.rs:43` |
| HTTP·루틴 시작 ingress | `src/services/discord/queue_io/input_owner/start.rs:59` |
| 루틴 ingress | `src/services/discord/queue_io/input_owner/routine.rs:70` |
| TUI 관측 턴 claim | `src/services/discord/tui_prompt_relay/synthetic_start/claim.rs:32` |

### 입력 출처 분류

| 출처 | 소유자 | 재시작 중 처리 |
|---|---|---|
| Discord 사람의 `/restart`·`!restart` | 명령(operation) | auth 뒤 operation 고정. 명령 자체는 입력이 아니고, 인사말 seed 하나를 owner 큐에 적재한다 |
| Discord 사람의 일반 메시지 | owner 큐 original | seed 뒤에 선다. Gate 동안 시작하지 않고 해제 뒤 wake. 사람 선행 이동도 seed 앞으로는 못 간다 |
| Discord 허용 봇 | owner 큐 original | 기존 intake 정책 그대로. 접수된 original만 보존 대상 |
| TUI 관측 턴 | captured actor/token | 옛 실행의 exact 종료에서 HostTerminated로 결산. 미제출 draft는 접수 원본이 아니다 |
| API·HTTP·루틴 시작 | 각 ingress | Herdr 채널에서는 계속 `executor_not_wired`. seed source는 admission 한 곳에서 거절 |
| restart 인사말 | restart 명령만 | 내구 bind의 context가 유일한 출처. 시작 시 permit이 대상 실행의 종료·retire를 확인 |


## 원시 결과·재현

로그 root `/home/adk/adk/lane-runs/i8128-b2b/logs/model/`: `checker-all65-run1.log`(SeedBehind 기대 수정 전, 1 불일치), `checker-delta4-run2.log`(변경 3 cfg + 대조 1), `checker-all65-final.log`(최종 65/65), `inputs-final/`, `control-inputs/`.

checker의 `--base/--head`는 커밋 diff로 고르므로 미커밋 상태에서는 `--root`에 HerdrRestartOwner 입력만 복사해 같은 checker로 돌렸다. 커밋 뒤에는 `python3 scripts/ci/statecharts_tlc.py --base 40e9c95b10 --head HEAD`가 같은 cfg를 고른다.

```sh
export TLA2TOOLS=/home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar
python3 scripts/ci/statecharts_tlc.py --root /home/adk/adk/lane-runs/i8128-b2b/logs/model/inputs-final --jobs 12
```

Mermaid chart·view wrapper(`herdr-restart-owner.view.mjs`)는 갱신하지 않았다. view는 새 필드 `owner`·`foreignGreeting`을 읽지 않는다.

TIME model_start=2026-11-14T07:19:29+09:00 cap=30m evidence_frozen=2026-11-14T07:26:19+09:00
