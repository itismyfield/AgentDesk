# 루틴 실행 방식 상태 모델 r2

코드 기준 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`, r1 문서 커밋 `e69051beeb6a268aba286461c3fb67c641ffcc2c`, 2026-11-12 KST. 모델·문서만 개정했다. Rust 빌드·시험·운영 실행은 `NOT_RUN`이다.

r1의 `outbound-port/OutboundCallerFlow`는 routine logger transport 선례이며 실행 수명 모델은 아니었다. r2는 브리프의 경로에 맞춰 **RoutineRun 모델군만 이 `routine-run/` 폴더로 이동**했다. 기존 OutboundCallerFlow·OutboundPort는 보존했다. [실행 방식 설계](../../routines/execution-modes.design.md)의 진입점 전수표가 생산 코드의 기준이다.

## r1에서 바뀐 증명 범위

리뷰 P2-3: r1의 Submit은 `~expired /\ ~recovering`을 요구하여 owner가 루틴의 기한/복구를 원자적으로 안다고 가정했다. r2는 이 두 가드를 **Submit에서만 제거**했다. 이미 accepted된 원본의 물리 제출은 기존 owner가 독립적으로 실행할 수 있다. 루틴의 새 admission·재시도인 Rearm은 기한/복구 가드를 유지한다. 이는 새 writer·취소 권한이 아니라 기존 actor의 first-key와 exact withdraw 선형화 순서를 모델에 드러낸 것이다.

- withdraw가 먼저면 queue가 비고 input=cancelled이므로 Submit이 불가능하다.
- Submit이 먼저면 MayHaveSent로 바뀌어 withdraw가 성공할 수 없다. deadline/복구는 unconfirmed로 종결하며 재전송하지 않는다.
- NativeAccept는 이미 보낸 키의 native 수용이 늦게 확인되는 경우다. post-key Unknown을 취소 성공으로 거짓 투영한 경우만 NoLateDispatch가 깨진다. 정상 unconfirmed 종결 뒤 pre-terminal 제출의 관측이 늦는 것과 구별한다.

리뷰 P2-4: `TerminalLeavesNothingQueued`를 추가하고 LateDispatch의 Policy 조건을 없앴다. 원본이 남아 있으면 모든 cfg에서 늦게 실행될 수 있다. channel_teardown·script_agent·consumed_as_agent cfg로 각각 공유 세션 보존·script 턴0·Consumed 분류 가드를 실제로 깨서 RED를 확인했다. 기존 NoDeadlineCancel/NoRecoveryCancel은 새 불변식에도 RED이고, ResendUnknown은 늦은 native 수용까지 드러나 NoLateDispatch도 RED다.

**headless/thread/channel 세 cfg는 같은 상태 공간**이다(Mode 레이블만 다르며 off 58/158, enabled 196/524). 이 모델은 공통 입력 수명을 검증한다. Discord 표면·backend·워크스페이스·primary mailbox 간섭이 다르다는 것을 세 번 검증한 결과가 아니다. 그 차이는 구현 fixture/수용 시험 대상이다.

## 규칙

| 규칙 | 사용자 계약 |
|---|---|
| NoDuplicateTurn | run의 routine prompt native 수용은 최대1. HTTP/Enter/admission ACK를 native 수용과 혼동하지 않음 |
| SuccessfulAgentExactlyOne | succeeded이면서 **outcomeKind=agent**인 결과는 prompt 수용 정확히1 |
| CommandConsumedHasNoAgent | command_consumed 결과는 agent0. ConsumeCommand가 이를 명시하며 consumed_as_agent가 잘못된 agent 성공 분류를 검출 |
| ScriptHasNoAgentTurn | script 및 JS skip/pause는 agent0. script_agent는 실제 Admit→Submit으로 이 규칙을 깸 |
| NoSilentLoss | terminal 결과를 이력에 저장하고 failed/interrupted/paused는 대시보드/허용된 기존 알림에 표시 |
| TerminalLeavesNothingQueued | terminal run에 아직 실행 가능한 known-unsent queued original이 없음 |
| NoLateDispatch | 취소/종결되었다고 한 원본이 나중에 실행되지 않음. 원래 제출한 키의 늦은 정상 관측은 제외 |
| SharedSessionPreserved | routine teardown/reset/kill이 shared channel 세션을 파괴하지 않음. channel_teardown으로 가드 제거 RED |
| PrejudgeAtMostOnce | 사전 판정 호출은 run당 최대1 |
| ScriptHasNoPrejudge | script 모드는 optional 판정 설정을 켠 cfg에서도 호출0 |
| PrejudgeSkipHasNoAgent | reason=prejudge_no_work이면 skipped, prompt 수용0, queue0 |
| EveryRunTerminates | 계속 가능한 기존 tick/종결/복구 commit은 결국 실행되어 run이 terminal에 도달 |

JS complete/skip/pause, command 소비, 제출 전 실패, 사전 판정 no-work는 agent0이 정상이다. 모든 terminal에 무조건 agent1을 요구하면 사용자 요구와 모순된다. native turn 하나에 사람 입력과 routine prompt가 합류할 수 있으므로 측정 대상은 routine prompt의 수용 수다.

## 선택적 사전 판정

기본 `PrejudgeEnabled=FALSE`는 r1 실행을 보존한다. headless/thread/channel에서만 optional TRUE를 적용한다. script가 자료를 모으고 agent action을 요청한 뒤, **agent admission 전에 Prejudge가 한 번** 질문한다. `Admit`·`ConsumeCommand`는 판정이 꺼져 있거나 완료되어야 진행한다.

Prejudge 결과는 스킬 스크립트의 종료 코드와 답을 추상화했다. `rc0_no`는 정상 응답+설정 confidence threshold 충족+답=false인 경우만 뜻한다. 이때 기존 skip 종결을 사용하여 reason=prejudge_no_work, agent0, queue0을 기록한다. `rc0_yes`는 진행, 키가 있는 enabled 상태의 rc2(네트워크/API/차단/답 없음)와 rc3(신뢰도 미달)는 답을 사용하지 않고 원래 agent 흐름으로 돌아간다. 실패가 skip을 만들거나 이 호출 때문에 run을 보류하지 않는다. 실제 API 호출·비밀값 접근은 하지 않았다.

정본 스킬 `/Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/gemini-decision/SKILL.md`와 `~/ObsidianVault/RemoteVault/99_Skills/gemini-decision/scripts/decide.py:4-5,106-120,122-136`의 계약을 따랐다. 세 enabled cfg에서 no/yes/rc2/rc3를 모두 탐색했고, script enabled cfg는 호출0을 확인했다. 입력 원문·질문·신뢰도·비용 측정값은 모델 밖이며 설계의 run 이력 계약이 보관한다.

## 변수·값의 생산 코드 출처

아래 값은 진입점 전수표의 실존 값 또는 관측 투영이다. 새 persistent ledger나 입력/배달 권위를 뜻하지 않는다. 접두사 없는 routines 경로는 `src/services/routines/`다.

| 변수 | 코드/계약 출처 |
|---|---|
| mode/action/status | #8117 mode 선택, `runtime.rs:20-29,675-829`, `store.rs:2965-3047,3057-3132`의 action/terminal 값 |
| input/queued | `agent_executor/owner_input.rs:21-25,222-277,384-385`의 input_pending/accepted/started와 same ticket; `src/services/turn_orchestrator/queued_head/v2/control.rs:230-264`의 queued 취소/commit |
| MayHaveSent/keyPending | `queued_head/v2/attempt.rs:103-115`의 arm commit 뒤 first-key 가능성, `src/services/discord/queue_io/input_owner/effect.rs:105-152`의 positive NotSent vs WriteUncertain. keyPending은 실제 native 수용 관측을 나눈 모델용 boolean이며 durable authority 아님 |
| acceptedTurns/lateAcceptedTurns | `launch.rs:214-271` start 확인과 별개인 실제 routine prompt 수용 관측. `agent_executor.rs:361-515,653-815`가 현행 새 turn retry를 할 수 있음. 0/1/2는 최소 반례 경계 |
| expired/retryCount | `agent_executor.rs:481-515,995-1007`, `reliability.rs:6-25`의 current_attempt_started_at+timeout; `store.rs:2383-2429`의 retry_count. 실제 60/600초·timeout 값은 boolean 기한으로 축약 |
| recovering | `src/server/mod.rs:2681-2688,2707-2712`의 boot/periodic expired lease recovery. 제안은 owner 취소 commit ACK 동안 기존 status=running 유지 |
| recorded/notified | `store.rs:2224-2381,2493`, `agent_executor.rs:406-422`의 exact terminal 저장; `discord_log/failure_alert.rs:47-104`, `discord_log.rs:286-324` 및 설계의 사건×방식 대시보드/알림 표 |
| sharedDestroyed | `agent_executor.rs:825-828`의 owner_input shared TUI teardown no-op. mutant는 이 no-op 제거를 모델링 |
| outcomeKind | `agent_executor/owner_input.rs:95-137`의 Consumed(현재 succeeded+agent 표기, 실제턴0)와 설계의 command_consumed 분리. agent 성공 불변식이 action 문자열만 보지 않음 |
| reason | JS 오류/이전 실패/deferral 종료 등 전수표의 실패 사유 및 사용자 승인 prejudge_no_work. 모델은 종류를 보관하고 원문 오류·exit code·stdout를 대체하지 않음 |
| prejudgeDone/Calls/Result | 사용자 승인 사전 판정 계약, 위 decide.py rc0/2/3. 호출 수와 반환 분기를 관측한 값 |
| Policy | BeforeRetry/BeforeHeld 또는 한 가드 제거 mutant 선택 상수. production 설정으로 도입하지 않음 |

## 진입점과 모델 action 연결

| 진입점/값 | 모델 action / 추상화 경계 |
|---|---|
| 예정 claim·run-now | Init. `store.rs:685-717,2197-2221,3168-3224`의 기존 in_flight CAS를 전제로 중복 claim 세부 경합은 밖 |
| JS complete/skip/pause | CompleteScript / SkipOrPause, `runtime.rs:675-785` |
| JS 오류 | FailBeforeSubmit(js_error), `runtime.rs:274-312` |
| migration_invalid | FailBeforeSubmit(migration_invalid), `runtime.rs:47-84`; routine pause_reason의 별도 상태는 밖, run failed+표시는 포함 |
| thread setup permanent | FailBeforeSubmit(thread_setup_permanent), `launch.rs:72-100`, `agent_executor.rs:1010-1079`; r2는 primary fallback 없이 종결하는 제안. validate/create HTTP 자체는 밖 |
| thread 일시 오류·provider/mailbox deferral | RefuseBeforeKey→RearmKnownUnsent의 no-effect 대기 추상화; `start_deferral.rs:6-9,114-125,143-235`. 실제 thread resolve가 owner admission 전에 일어나는 위치·60초 간격은 밖 |
| deferral 소진 | FailBeforeSubmit(deferral_exhausted), `start_deferral.rs:124-125`→`agent_executor.rs:175-193`의 기존 정책으로 이행하는 경계. r2의 primary_busy 진단은 설계가 구체화 |
| command 비0 종료 | FailBeforeSubmit(command_nonzero), script 모드에서만. main의 임의 셸 runner는 아직 없으므로 신규 command runner의 exit!=0 계약. child 생성/PGID/출력 drain은 밖 |
| 선택적 자료 판정 | Prejudge. rc0/no→skip; rc2/3→Admit 가능한 원래 상태 |
| agent 명령 Consumed | ConsumeCommand, `owner_input.rs:95-137` |
| original/ticket 수용→first-key | Admit→OwnerAccept→Submit, `launch.rs:161-163`, `owner_input.rs:222-283`, 기존 owner arm/physical writer |
| positive NotSent | ObserveNotSent→RearmKnownUnsent, 기존 `effect.rs:122-139` proof. absence/시간만으로 NotSent를 만들지 않음 |
| native 수용/완료 | NativeAccept/ObserveAccepted/CompleteAgent, `reliability.rs:74-155`, `agent_executor.rs:406-443`. response 분류/Discord transport는 밖 |
| provider 오류·timeout | ProviderFailure / Expire→CancelUnsentAtDeadline 또는 CloseUncertainAtDeadline, `agent_executor.rs:361-515` |
| boot/periodic recovery | BeginRecovery→RecoverWithdraw→RecoverUnsent(정확한 ACK 뒤 interrupted) 또는 RecoverUncertain; `store.rs:3057-3132`와 기존 actor cancel seam |
| 취소 actor ACK→PG 저장 사이 재시작 | Restart stutter가 cancelled/queue0을 보존. 기존 `queued_head/v2/control.rs:242-264`의 commit ACK가 먼저 |
| 세션 teardown/reset/kill | SharedSessionPreserved+TeardownChannel mutant. 정확한 owned process/tmux 정리·legacy persistent API는 이 모델 밖, 새 shared 파괴 권한0 |
| logger·relay·thread 재사용·UI·알림 TTL | 표시 boolean 외 범위 밖. 기존 owner/lease/outbox 권위·source/currentness 계약을 모델 GREEN으로 대체하지 않음 |

## 수정 전 증거 승계와 r2 실제 결과

r1 모델 SHA256 `8d475a68a8206231797e2618384fbcc4581afefba28761deb61c66637dd263b4`의 실제 로그는 `i8117/sc-out/<cfg>-recovery-final/`에 보존했다. before 표는 그 실행 결과를 승계한다. 소스가 바뀐 r2의 GREEN은 승계하지 않고 **18cfg를 같은 r2 source로 전부 새로 실행**했다.

| cfg군 | r1 실제 states/steps·결과 | r2에서 달라진 범위 |
|---|---|---|
| agent off 3개 | 각49/127 GREEN | owner가 기한/복구를 모르는 Submit 경합·Consumed·실패 종류 포함 |
| script | 15/33 GREEN | 비0 종료·script_agent·판정 제외 포함 |
| BeforeRetry / BeforeHeld | 99/254 duplicate RED / 17/43 liveness RED | 두 before 반례 유지 |
| NoDeadlineCancel / NoRecoveryCancel | 51/130, 53/133 late RED | TerminalLeavesNothingQueued로 더 이른 반례 추가 |
| ResendUnknown / DropHeld | 116/298 duplicate RED / 51/131 silent RED | 늦은 native 수용에 따른 late RED 추가 / silent 유지 |

| cfg | r2 states / steps | 실제 결과 |
|---|---:|---|
| `RoutineRunBeforeHeld.cfg` | 26 / 61 | EveryRunTerminates RED |
| `RoutineRunBeforeRetry.cfg` | 98 / 276 | NoDuplicateTurn, SuccessfulAgentExactlyOne RED |
| `RoutineRunCancelAfterKey.cfg` | 62 / 167 | NoLateDispatch RED |
| `RoutineRunChannel.cfg` | 58 / 158 | GREEN |
| `RoutineRunChannelPrejudge.cfg` | 196 / 524 | GREEN |
| `RoutineRunChannelTeardown.cfg` | 86 / 217 | SharedSessionPreserved RED |
| `RoutineRunConsumedAsAgent.cfg` | 58 / 158 | SuccessfulAgentExactlyOne RED |
| `RoutineRunDropHeld.cfg` | 60 / 162 | NoSilentLoss RED |
| `RoutineRunHeadless.cfg` | 58 / 158 | GREEN |
| `RoutineRunHeadlessPrejudge.cfg` | 196 / 524 | GREEN |
| `RoutineRunNoDeadlineCancel.cfg` | 60 / 161 | TerminalLeavesNothingQueued, NoLateDispatch RED |
| `RoutineRunNoRecoveryCancel.cfg` | 62 / 164 | TerminalLeavesNothingQueued, NoLateDispatch RED |
| `RoutineRunResendUnknown.cfg` | 148 / 423 | NoDuplicateTurn, NoLateDispatch, SuccessfulAgentExactlyOne RED |
| `RoutineRunScript.cfg` | 25 / 53 | GREEN |
| `RoutineRunScriptAgent.cfg` | 66 / 173 | ScriptHasNoAgentTurn RED |
| `RoutineRunScriptPrejudgeIgnored.cfg` | 25 / 53 | GREEN |
| `RoutineRunThread.cfg` | 58 / 158 | GREEN |
| `RoutineRunThreadPrejudge.cfg` | 196 / 524 | GREEN |

총 **8 GREEN, 10 기대 RED**, 규칙 누락·0-test PASS 없음. 지정 CLI 각 cfg 약1.9~4.8초, 전체 30분 한계 안. 공식 `scripts/ci/statecharts_tlc.py`도 동일 바이트의 18cfg만 별도 root로 선택하여 fairness precheck·safety·각 temporal property를 실행했다. **18/18 expected set 일치, exit0, 총53.6초**다. 다른 수백 cfg를 검증했다고 주장하지 않는다.

r2 원시 증거: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8117/sc-out-r2/<cfg>/report.md`, `tlc.out`, `chart.mmd`. 집계 `results.json`, 공식 checker 출력 `official-checker.stdout.log`(선택 목록 이후 stdout), exact-byte checker 입력 `ci-root/` 및 실행 메타데이터 `execution-record.json`. Runtime/API/Discord/키 호출0, Rust 재현/뮤턴트/실행 시험 NOT_RUN.

## 반례의 최단 경로와 코드 대조

- BeforeRetry: 등록→수용→native1→시작 확인→실패→새 턴 retry→native2, 7step. agent 성공1은 완료까지9step. main `agent_executor.rs:376-398,664-685,581-597`, `store.rs:2395-2415`가 기존 재실행 정책을 가진다.
- BeforeHeld: 등록→기한, 2step 뒤 영원히 보류. `owner_input.rs:316-347`과 `agent_executor.rs:346-347,446-448`이 observer/may_retry를 기다리며 timeout 전 continue, `store.rs:1114-1126`이 lease를 연장한다. 새 모델도 이 반례를 fairness로 없애지 않았다.
- CancelAfterKey: 등록→수용→기한→**기한 뒤 owner Submit**→잘못된 cancelled 종결→늦은 NativeAccept, 6step NoLateDispatch. `attempt.rs:103-115`의 MayHaveSent 뒤 `effect.rs:151-152`는 unproven NotSent를 WriteUncertain으로 둔다. mutant는 이 구분을 없앤 것이다. recovery 중 Submit도 같은 source에서 탐색한다.
- NoDeadlineCancel/NoRecoveryCancel: 원본 등록→기한/복구→취소 없는 terminal은3step TerminalLeavesNothingQueued, 이어 늦게 실행되면4step NoLateDispatch. 기존 `queued_head/v2/control.rs:242-264`의 exact cancellation commit ACK가 빠진 형태다. boot 복원 `discord/runtime_bootstrap/queued_recovery.rs:48-51`·`discord/queue_io/input_owner/recovery.rs:15-25`와 run 종결 권위 사이의 대조이며 운영 interleaving 재현은 아니다.
- ResendUnknown: 불확실 원본 재전송 뒤 native2가5step NoDuplicateTurn, 성공1은7step. 재전송된 waiting row만 취소해도 원래 key가 남아7step NoLateDispatch를 추가로 깬다. positive NotSent 없이 재전송하지 않아야 한다.
- DropHeld: 등록→기록/표시 없는 폐기2step NoSilentLoss. 진단 Discord 출력을 없애도 run 저장·조치 표시를 없애면 안 된다.
- ChannelTeardown: JS skip→shared cleanup2step SharedSessionPreserved. `agent_executor.rs:825-828`의 no-op을 신규 channel에도 유지한다는 가드를 제거한 mutant다.
- ScriptAgent: script→Admit→OwnerAccept→Submit3step ScriptHasNoAgentTurn. `runtime.rs:675-829`의 JS/agent 분기와 신규 script validator가 막아야 하는 경로다.
- ConsumedAsAgent: ConsumeCommand1step 성공 agent0 오분류. main `owner_input.rs:108-132`의 기존 agent/succeeded 형태를 새 outcome_kind로 분리해야 함을 검출한다.

모든 코드 대조는 읽기 전용 file:line mapping이다. 실제 Rust call-site mutant RED를 실행한 증거가 아니며 구현 조각의 수정 전 RED/수정 후 GREEN을 대신하지 않는다.

## 공정성·생략·재현

Spec은 실제 적용된 WF(Expire)·WF(Finish)·WF(RecoveryFinish)를 쓴다. BeginRecovery는 optional 외부 사건으로 강제하지 않는다. RecoveryFinish는 recovering일 때만 활성이다. actor/PG가 영구 정지해도 종결된다는 주장이 아니라, 기존 tick과 exact commit이 계속 가능한 회복 환경을 전제로 한다. BeforeHeld는 복구를 선택하지 않을 수 있고 liveness RED가 남는다.

Restart·Done·AwaitRecoveryCommit stutter와 반복 BeginRecovery 진입선(none/input_pending/accepted 이외)을 view에서 숨겼다. core first-key 경합과 모든 위반 상태는 남겼다. CLI가 같은 라벨 안의 전이를 합친다. off agent 차트18화면/36화살표, prejudge 차트21/51, script9/13이다. `InputIdentity`, `CurrentSource`, exact host/incarnation, 실제 physical writer guard, Discord 표면/전달 lease, shell stdout/PGID cleanup, 세션 생성/재사용 세부는 모델 밖이며 #8114·구현 시험이 검증한다.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/routine-run/RoutineRun.tla \
  --config docs/statecharts/routine-run/RoutineRunChannel.cfg \
  --view docs/statecharts/routine-run/routine-run.view.mjs \
  --out /tmp/i8117-r2-new
```

[공통 agent](routine-run.chart.mmd)·[script](routine-run-script.chart.mmd)·[사전 판정](routine-run-prejudge.chart.mmd), [first-key 취소 mutant](routine-run-cancel-after-key.chart.mmd)·[shared cleanup mutant](routine-run-channel-teardown.chart.mmd)·[script agent mutant](routine-run-script-agent.chart.mmd)·[Consumed mutant](routine-run-consumed-as-agent.chart.mmd), [before retry](routine-run-before-retry.chart.mmd)·[before Held](routine-run-before-held.chart.mmd)·[unknown 재전송](routine-run-resend-unknown.chart.mmd)·[기한 철회 생략](routine-run-no-deadline-cancel.chart.mmd)·[복구 철회 생략](routine-run-no-recovery-cancel.chart.mmd)·[조용한 폐기](routine-run-drop-held.chart.mmd).

최종 모델 SHA256 `1afd1bc11f42c7909d7490e1b719096c53f8eaf526309f0b1d5df26d8800b5b3`. expected.tsv의18행과 실제 위반 집합이 일치한다.

## r2b 사용자 결정: 여러 run 사이의 실패 에스컬레이션

per-run exact1 모델의 소스와18cfg는 변경하지 않았다. 연속 실패 N/N+1과 별도 Announce 사고 대응 original은 [별도 모델 보고서](routine-failure-escalation.report.md)에서 검사한다. 원 routine 재실행·두 번째 routine 턴으로 합치지 않으며 Unknown의 eventual remediation 효과를 보장하지 않는다.

후속 PJ는 #8127 공통 gate를 pin하며, **키 없음 ≡ PrejudgeEnabled=FALSE**다. Off(no_key)는 판정 CLI·네트워크 호출0이며 NoAnswer와 구별한다. 기존 per-run 모델 소스/cfg는 변경하지 않는다.

## PR3b D: mailbox 공유·surface 별 actor retire·시작 보류·primary fallback

코드 기준 `061b3d4cc6df3368122146b666db0d63d4593234` 위 PR3b 조각, 2026-11-14 KST. 기존 `RoutineRun.tla` 를 갱신했다(새 모듈 없음). 갱신 전 사본의 SHA256 은 위 `1afd1bc1…`, 갱신 후는 `ccc0ad4cef5787b0af2069f90f4a2ac52f9548b0710fa79d82c52440e4f1f587` 이다. 기존 18cfg 의 기대 위반 집합은 그대로이고 4cfg 를 더해 22cfg 가 expected.tsv 와 일치한다(공식 checker, GREEN 8 + 기대 RED 14).

추가한 상태와 action:

- `scope`: run 의 surface. headless·channel 은 `shared_primary`, thread 는 `owned_thread`/`owned_dm`(구형 동적 DM) 중 하나, script 는 없음.
- `actor`·`RetireActor`: 턴이 끝난 뒤 idle 이고 queue 가 빈 actor 를 내린다. owned surface 에서만 가능하다.
- `hArrived`·`hQueued`·`hDone`, `HumanArrive`·`HumanProcess`: headless run 과 같은 mailbox 에 도착하는 사람 입력 둘. routine 턴이 mailbox 를 쥔 동안(`RoutineHolds`) 대기하고 run 이 닫히면 처리된다.
- `humanBusy`·`DeferStart`·`HumanFinish`: 사람이 먼저 일하는 동안 routine 은 Admit 하지 못하고 시작 보류를 두 번까지 쓴 뒤 `primary_busy` 로 실패한다.
- `threadFault`·`ThreadRecover`: thread 해석의 일시·영구 실패. 일시 실패는 같은 thread 로 보류 뒤 `thread_unavailable`, 어느 쪽도 primary 로 가지 않는다.

| 규칙 | 뜻 |
|---|---|
| `PrimaryActorPreserved` | shared primary surface 의 actor 는 idle·빈 queue 에서도 바뀌지 않는다 |
| `NoPrimaryFallback` | thread 해석 실패가 primary 에서의 실행으로 바뀌지 않는다 |
| `HumanInputsConserved` | 사람 입력은 유실·중복 없이 각 한 번 처리되거나 대기 중이다 |
| `HumanInputsEventuallyProcessed` (liveness) | headless 진행 중 도착한 사람 입력은 결국 처리된다 |
| `OwnedSurfaceRetires` (liveness) | routine 소유 thread·DM 의 actor 는 턴이 끝나면 내려간다(positive control) |

| cfg | 뜻 | 결과 |
|---|---|---|
| `RoutineRunHeadless`·`RoutineRunThread`·`RoutineRunChannel` | 수정 후 | GREEN(새 규칙 5개 포함) |
| `RoutineRunHeadlessRetireShared` | 수정 전: process 턴이 surface 를 가리지 않고 actor 를 내림 | RED `PrimaryActorPreserved` |
| `RoutineRunThreadPrimaryFallback` | 현재 main: thread 준비 실패 시 primary 로 실행 | RED `NoPrimaryFallback` |
| `RoutineRunHeadlessStartedRetry` | 현재 main: 제출 뒤 provider 실패에 새 턴 | RED `NoDuplicateTurn`·`SuccessfulAgentExactlyOne` |
| `RoutineRunHeadlessNoRelease` | 닫힌 run 이 mailbox 를 놓지 않음(liveness 가 공허하지 않음을 보이는 mutant) | RED `HumanInputsEventuallyProcessed` |

모델 밖: 완료 timeout 의 기준 시각(claim 대 실제 시작)은 `Expire` 한 걸음으로 추상화돼 있어 구별하지 못한다. `primary_busy` 종결의 provider spawn 0 은 `FailBeforeSubmit` 이 `input = "none"` 에서만 가능해 모델에서는 자명하고, 구현 시험이 본다. `HumanInputsConserved` 를 깨는 mutant 는 두지 않았다(queue 가 남은 actor 를 내리지 않는 기존 가드는 이 조각에서 바뀌지 않는다).

## PR3b-2: per-run 세션 행 종료, 구현이 thread GREEN cfg 로 옴

코드 기준 `8752805029e9dfded84791b0a9a79b6a22d511a3` 위 PR3b-2 조각, 2026-11-14 KST. 갱신 전 SHA256 `ccc0ad4c…`, 갱신 후 `8f0b7404a01ef767b10aeddb88c75aee770e1eba9392e94d10c0512799c1e7a7`. 24cfg 가 expected.tsv 와 일치한다(공식 checker, GREEN 8 + 기대 RED 16).

- null-mode 실패 전이(fallback 0·5xx 보류·제출 뒤 재실행 0·`primary_busy` 종결)는 앞 조각에서 목표 동작으로 넣은 그대로다. 이 조각에서 구현이 `RoutineRunThreadPrimaryFallback`·`RoutineRunBeforeRetry`(RED) 쪽에서 `RoutineRunThread`(GREEN) 쪽으로 왔다.
- 더한 것: headless run 의 세션 행 `runRow`(none → active → removed)와 `primaryRow`, action `CloseRunRow`. 턴의 완료 경로가 run 자신의 행만 종료·삭제한다. 재시작으로 `interrupted` 가 된 run 은 완료 경로에 닿지 않아 행이 남는다(stale 세션 GC 몫, 구현 그대로).

| 규칙 | 뜻 |
|---|---|
| `PrimaryRowKept` | run 의 행을 닫을 때 primary 행은 그대로다 |
| `RunRowRemoved` (liveness) | 끝난 run(interrupted 제외)의 행은 결국 지워진다 |

| cfg | 뜻 | 결과 |
|---|---|---|
| `RoutineRunHeadless` | 수정 후 | GREEN |
| `RoutineRunHeadlessNoRowClose` | 수정 전: 완료가 행을 닫지 않아 `turn_active` 로 남음 | liveness RED `RunRowRemoved` |
| `RoutineRunHeadlessWideRowClose` | 닫는 대상에 primary 행이 섞임 | RED `PrimaryRowKept` |

