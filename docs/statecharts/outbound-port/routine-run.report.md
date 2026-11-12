# 루틴 실행 방식 상태 모델

코드 기준: `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`, 2026-11-12 KST. 설계 모델이며 production 변경·Rust 빌드·시험·운영 실행은 하지 않았다. 기존 `outbound-port/OutboundCallerFlow.tla`의 routine log caller 선례와 같은 영역에 실행 수명 모델을 추가했다. README에 routine 실행 수명 전용 모델은 없었다. 기존 outbound transport 모델은 그대로 보존한다.

## 규칙과 범위

- `NoDuplicateTurn`: 하나의 run이 수용시킨 실제 routine prompt는 최대 1개다. HTTP start ACK·Enter ACK·owner admission ACK를 실제 native prompt 수용과 같다고 보지 않는다.
- `SuccessfulAgentExactlyOne`: action=agent이며 succeeded인 run은 실제 routine prompt가 정확히 1개 수용됐다. JS complete/skip/pause와 영구 pre-start 실패·deadline-before-submit는 0개가 정상이다. "모든 terminal run이 무조건 agent 1개"는 script/skip/실패 요구와 모순되므로 그렇게 주장하지 않는다. 기존 command Consumed는 agent 실행 성공이 아니라 `complete`인 별도 typed terminal로 투영할 설계 전제다.
- `ScriptHasNoAgentTurn`: script 모드 및 JS skip/pause는 agent 턴이 0개다.
- `NoSilentLoss`: terminal run은 이력에 결과를 남기며 failed/interrupted/paused는 조치 가능한 표시가 있다. `notified`는 대시보드의 실패/일시정지 표시와 해당 mode가 허용한 기존 notice를 뜻한다. headless/script에 Discord 표면을 강제하지 않는다.
- `NoLateDispatch`: known-unsent 대기 입력을 취소하지 않은 채 run만 terminal로 만들면 그 입력이 나중에 실행될 수 있으므로 이를 금지한다. 이미 첫 키를 보낸 입력의 native 수용·receipt가 늦게 관측되는 것과 다르다.
- `SharedSessionPreserved`: routine은 채널의 shared 세션을 teardown/reset/kill하지 않는다. 이 모델에는 shared 세션을 파괴하는 정당한 전이가 없다. 실제 호출부에서 `owner_input` no-op 보존 여부는 구현 시험으로 검증해야 한다.
- `EveryRunTerminates`: 각 run은 결국 succeeded/failed/interrupted/skipped/paused 중 하나에 도달한다. failure도 명시적 종결이며 결과 성공과 다르다.

재시작의 상태 복원은 같은 durable run/original/ticket/attempt 상태를 그대로 복원하는 stutter다. 실제 boot/periodic lease recovery는 별도 `BeginRecovery → RecoverWithdraw → RecoverUnsent` 전이로 도달 가능한 interrupted를 만든다. 복구 선택 뒤에는 이 run의 새 제출을 막고 running+복구 진단을 유지한다. 정확한 original/ticket을 기존 owner actor에서 취소하여 commit ACK를 받은 `RecoverWithdraw` 뒤에도 run은 running이며, `input=cancelled, queued=FALSE`인 상태에서만 `RecoverUnsent`가 PG interrupted를 기록한다. 두 저장 사이 재시작은 취소 증거를 그대로 보존한다. 이미 첫 키를 보냈거나 불확실한 입력은 `RecoverUncertain`에서 interrupted/unconfirmed로 기록하며 새 턴·재전송·shared session stop/kill을 하지 않는다. 원본·ticket의 중복 생성 및 저장 실패를 성공으로 바꾸는 구현은 이 전제를 만족하지 않으므로 통합 시험 대상이다. Mode=headless/thread/channel은 같은 agent 입력 수명 규칙을 사용하며 Discord 출력 정책의 차이는 [실행 모드 설계](../../routines/execution-modes.design.md)가 정한다. Mode=script만 agent admission을 하지 않는다.

`InputIdentity`·`CurrentSource`·exact incarnation·공통 ClaudeBusy 순서는 #8114 설계 계약을 전제로 하며 다시 설계하지 않는다. Source/currentness/physical writer 검사는 이 작은 모델의 외부 전제다. 채널 실행에서 routine이 사람 입력으로 승격되거나 새 physical writer를 가지는 동작은 없다.

## 변수의 코드 출처

모델의 모든 값은 진입점 조사표의 실존 필드/분기 또는 그 관측값에서 가져왔다. 새 영구 권위나 ledger를 제안하는 변수는 없다.

| 변수/값 | 출처와 의미 |
|---|---|
| `Mode` | #8117의 headless/thread/channel/script 선택. 현재 `agent_executor/execution.rs:11-17` backend, `launch.rs:58-108` surface, `runtime.rs:675-829` JS/agent 분기를 분리한 제안 |
| `s.status`, `s.action` | `src/services/routines/runtime.rs:20-29,675-829`, `store.rs:2965-3047,3057-3132`의 running/succeeded/failed/interrupted/skipped/paused 및 complete/agent/skip/pause |
| `s.input` | `src/services/routines/agent_executor/owner_input.rs:21-25,257-277,384-385`의 input_pending/accepted/started; `start_deferral.rs:161`; `store.rs:2403-2410`의 retry_scheduled. `MayHaveSent`는 `src/services/turn_orchestrator/queued_head/v2/attempt.rs:103-115`; `NotSent`는 `src/services/discord/queue_io/input_owner/effect.rs:19-33,105-152`; cancelled는 `queued_head/v2/control.rs:242-256` |
| `s.queued` | `agent_executor/owner_input.rs:222-277`의 같은 ticket 보관·admission, `queued_head/v2/control.rs:230-264`의 정확한 queued original 제거/commit 여부 |
| `s.acceptedTurns` | 저장 필드가 아닌 관측 카운터. `launch.rs:214-271`의 start 확인과 구별되는 실제 native prompt 수용 수. 현행 재시도는 `agent_executor.rs:361-515,653-815`에서 새 turn_id를 시작할 수 있음. 모델 0/1/2는 두 번 수용 반례를 검출하는 최소 값 |
| `s.lateAcceptedTurns` | `finished_at` 뒤 대기하던 original의 실제 실행이 생겼는지 관측한 값. `queued_head/v2/control.rs:206-264`의 취소 commit을 생략하는 mutant로 검사 |
| `s.expired` | `agent_executor.rs:481-515,995-1007`의 attempt started_at+timeout<=DB NOW를 boolean으로 투영. Held admission에도 이 deadline 처분을 연결하는 부분은 신규 설계 |
| `s.recorded` | `store.rs:2224-2381,2493`, `agent_executor.rs:406-422`의 exact run 종결·result_json 저장. 저장 성공만 true |
| `s.notified` | `discord_log/failure_alert.rs:47-104`, `discord_log.rs:286-324`의 actionable 실패/paused notice 및 제안 대시보드 표시 |
| `s.recovering` | `src/server/mod.rs:2681-2688,2707-2712`와 `store.rs:3057-3132`의 boot/periodic expired-lease recovery 진입을 boolean으로 투영. 제안은 기존 상태값 running을 유지한 채 owner 취소 ACK를 기다린 뒤 interrupted. 새 영구 상태 enum이 아님 |
| `s.retryCount` | `store.rs:2383-2429`, `agent_executor.rs:1561-1606`의 max_retries/attempt 재실행. 0/1로 최소화 |
| `s.sharedDestroyed` | `agent_executor.rs:825-828`의 owner_input shared TUI teardown 금지 관측. initial false, 모든 허용 전이에서 false |
| `Policy` | 현행 BeforeRetry/BeforeHeld와 신규 After, 그리고 한 가드를 뺀 세 mutant를 고르는 cfg 전용 상수. production 설정으로 추가하지 않음 |

접두사 없는 `agent_executor/…`, `store.rs`, `runtime.rs`, `discord_log…`는 모두 `src/services/routines/` 아래다. 상세 전수표는 실행 모드 설계에 합친다.

## cfg별 실제 결과

지정 `statechart` CLI로 TLC 1.7.4를 실행했다. expected RED는 발견한 규칙 위반이며 PASS로 바꾸지 않았다. 복구 delta 최종 각각 2.9~5.9초, 전체 30분 한계 안에 종료했다.

| cfg | states / steps | 실제 결과 |
|---|---:|---|
| `RoutineRunHeadless.cfg` | 49 / 127 | safety 6개+NoDeadlock+EveryRunTerminates GREEN |
| `RoutineRunThread.cfg` | 49 / 127 | 동일 GREEN |
| `RoutineRunChannel.cfg` | 49 / 127 | 동일 GREEN |
| `RoutineRunScript.cfg` | 15 / 33 | 동일 GREEN, agent 수용 0 |
| `RoutineRunBeforeRetry.cfg` | 99 / 254 | NoDuplicateTurn, SuccessfulAgentExactlyOne RED; liveness와 나머지 safety GREEN |
| `RoutineRunBeforeHeld.cfg` | 17 / 43 | EveryRunTerminates RED; 모든 safety+NoDeadlock GREEN |
| `RoutineRunResendUnknown.cfg` | 116 / 298 | NoDuplicateTurn, SuccessfulAgentExactlyOne RED; 나머지 GREEN |
| `RoutineRunNoDeadlineCancel.cfg` | 51 / 130 | NoLateDispatch RED; 나머지 GREEN |
| `RoutineRunDropHeld.cfg` | 51 / 131 | NoSilentLoss RED; 나머지 GREEN |
| `RoutineRunNoRecoveryCancel.cfg` | 53 / 133 | NoLateDispatch RED; 나머지 GREEN |

원시 결과: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8117/sc-out/<cfg 이름>-recovery-final/`의 report.md·tlc.out·tlc-<property>.out·graph.json. 복구 delta 최종 실행은 script의 JS skip/pause와 실제 interrupted 복구 전이까지 포함한 동일 source로 10cfg를 모두 실행했다. 이전 출력 폴더는 중간 증거이며 최종 증거와 혼용하지 않는다. CLI는 safety 위반을 제거하며 다음 규칙을 재검사했으므로 각 RED의 나머지 규칙도 실제로 확인됐다. 재생성된 cfg 전체를 CI checker의 expected.tsv에 명시한다.

## 짧은 반례와 실코드 대조

- BeforeRetry의 `NoDuplicateTurn` 최단 경로는 7 step: 등록→admit→native 수용(1)→started 확인→실패→retry→native 수용(2). `SuccessfulAgentExactlyOne`은 그 뒤 관측·성공 저장까지 9 step. main `agent_executor.rs:376-398` teardown 뒤 `:664-685`가 same run retry를 저장하고 `:581-597`에서 start_turn을 다시 호출한다. `store.rs:2395-2415`가 turn_id를 비우고 retry_count를 올리며 원인 사슬이 코드에 있다. 기존 정책의 재실행이므로 "이미 중복 버그를 고쳤다"고 주장하지 않는다. 새 계약은 started/MayHaveSent 이후 새 턴 retry/fallback을 금지한다.
- BeforeHeld의 `EveryRunTerminates`는 2 step 뒤 `보류 기한 종료`에 영원히 머무른다. main `agent_executor.rs:346-347`의 reconcile이 `owner_input.rs:316-347`에서 observer bind를 얻지 못하면 다음 run으로 넘어가고, `agent_executor.rs:481` timeout에 도달하지 않는다. `store.rs:1114-1126` heartbeat는 lease를 계속 연장한다. source 회복이 영원히 안 오면 종결이 없다는 구조적 반례다.
- ResendUnknown의 `NoDuplicateTurn`은 5 step, 성공1 조건은 7 step. 이것은 제안의 no-resend 가드를 뺀 mutant다. main `queued_head/v2/attempt.rs:112-115`에서 MayHaveSent commit 뒤 발생 가능성이 있는 입력을 `NotSent` 근거 없이 accepted로 재분류하면 같은 원본이 두 번 수용된다. 기존 `effect.rs:151-152`는 unproven NotSent를 WriteUncertain으로 바꾼다. 이를 우회하지 않아야 한다.
- NoDeadlineCancel의 `NoLateDispatch`는 4 step: 등록→기한→run만 실패 저장→남은 queued original 실행. 제안 exact actor cancel이 빠진 mutant이며 main 신규 deadline 호출부가 아직 없으므로 현재 구현 버그 재현이라고 주장하지 않는다. `queued_head/v2/control.rs:242-264`의 owe_queued(Cancelled)+remove+commit 성공 뒤 취소 영수증을 받아야 terminal-unsent라 말할 수 있다.
- NoRecoveryCancel의 `NoLateDispatch`는 4 step: 등록→만료된 run 복구 시작→owner 입력 취소 없이 PG interrupted만 저장→남은 queued original 실행. main `store.rs:3064-3084`는 run을 interrupted로 바꾸지만 owner queue를 취소하지 않고, `src/server/mod.rs:2681-2688,2707-2712`는 이 store 복구 뒤 logger/reap만 호출한다. 반면 `discord/runtime_bootstrap/queued_recovery.rs:48-51`은 V2 owner file을 재채택·drive하고, `discord/queue_io/input_owner/recovery.rs:15-25`는 known-not-sent 원본을 재큐잉한다. 두 권위 사이 exact withdraw handoff가 빠진 형태를 코드로 대조했으며, 실제 운영 interleaving 재현은 하지 않았다. 제안의 `RecoverWithdraw` 성공과 `RecoverUnsent` 저장을 분리해 그 사이 crash도 queued 입력 부활 없이 복원한다.
- DropHeld의 `NoSilentLoss`는 2 step: 등록→보류 입력을 record/notice 없이 버림. 제안의 진단 출력 제거를 run 이력/대시보드 표시까지 지워 구현한 mutant다. 진단 emitter를 지워도 `store` 종결 저장과 actionable 표시를 유지하는 시험이 필요하다.

위 실코드 대조는 읽기 전용 code-step mapping이다. 실제 Rust/Discord 재현·뮤턴트 실행은 `NOT_RUN`이다(브리프의 빌드·운영 실행 금지). 모델 반례만으로 구현 RED/GREEN 증거를 대체하지 않는다.

## 공정성·생략·재현

`Spec`에는 실제 적용된 `WF(Expire)`·`WF(Finish)`·`WF(RecoveryFinish)`가 있다. `BeginRecovery`는 optional 외부 복구 이벤트이고 공정성을 걸지 않는다. `RecoveryFinish`는 recovering=TRUE일 때만 활성이라 복구가 선택되지 않은 BeforeHeld 경로를 강제로 종결시키지 않는다. BeforeHeld는 delta 뒤에도 실제 EveryRunTerminates RED다. 복구가 시작되면 actor 취소 commit과 PG 종결 저장이 결국 성공한다는 회복 가정 아래만 WF(RecoveryFinish)를 적용한다. runtime tick/deadline 관측과 exact 취소/종결 저장이 계속 가능하면 결국 실행된다는 가정이다. 영구 PG·프로세스 정지에서도 실시간으로 끝난다는 주장이 아니다. deadline 취소/저장 오류가 복구되지 않으면 이 가정은 깨지며 실패를 기록하지 못한 것까지 PASS로 세지 않는다. 시간 값(60/600/timeout)은 논리 기한 도달 boolean으로 줄였다. 무한 timer retry를 fairness로 숨기지 않도록 expired 뒤 submission/rearm은 금지했다.

Restart·Done·AwaitRecoveryCommit stutter와 none/input_pending 이외 단계의 반복 BeginRecovery 진입선을 view에서 숨긴다. 같은 화면 안의 관측 전이는 CLI가 자동 합친다. fixed channel chart는 15 상태/29 화살표(총75 raw steps 숨김, 같은 라벨2), script chart는 9 상태/12 화살표(총19 raw steps 숨김). 복구 취소 ACK와 PG 저장의 경계를 별도 화면으로 남겨 그 사이 crash를 숨기지 않는다. 위반 상태는 서로 합치지 않았다. source file identity, InputIdentity 상세 성분, daemon lease 획득/해제의 교차 호스트 순서, Discord HTTP delivery·프롬프트 내용·shell stdout/stderr·실제 elapsed 시간은 모델 밖이다.

확인 명령(새 out 폴더 사용):

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/outbound-port/RoutineRun.tla \
  --config docs/statecharts/outbound-port/RoutineRunChannel.cfg \
  --view docs/statecharts/outbound-port/routine-run.view.mjs \
  --out /tmp/i8117-routine-run-new
```

고정 모델은 [agent 흐름](routine-run.chart.mmd)·[script 흐름](routine-run-script.chart.mmd), 현재 반례는 [started 재시도](routine-run-before-retry.chart.mmd)·[무기한 보류](routine-run-before-held.chart.mmd), 가드 제거 반례는 [불확실 재전송](routine-run-resend-unknown.chart.mmd)·[기한 취소 생략](routine-run-no-deadline-cancel.chart.mmd)·[보류 유실](routine-run-drop-held.chart.mmd)·[복구 취소 생략](routine-run-no-recovery-cancel.chart.mmd)에 있다.

최종 모델 SHA256: `8d475a68a8206231797e2618384fbcc4581afefba28761deb61c66637dd263b4`. 최종 cfg 실행 기록은 `sc-out/model-execution-recovery-final-summary.json`이며 10개 모두 명시된 기대 결과와 일치한다.
