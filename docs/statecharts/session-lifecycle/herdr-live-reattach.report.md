# #8280 dcserver 부팅 후 첫 Herdr 입력의 상태 차트

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`. 기존 `HerdrMissingBound`에 `AttachSpec`을 추가했다. 역사 `Spec`은 추가 변수의 값을 고정한다. 변수와 상태의 근거는 [설계 진입점 전수표](../../design/i8280/live-session-reattach.design.md#진입점-전수표)에 있다.

| 기존 모델 | 이번 갱신에 넘긴 계약 |
|---|---|
| restart-a · `HerdrRestart` | End 이후 resume offset 캡처와 boot에서 같은 경계 복원, 일반 Codex fresh 유지 |
| restart-b-ports · b1 · b2a · b2b · `HerdrRestartOwner` | owner grant와 frozen followers, clear epoch/효과 permit, cache→gate 해제→wake, seed는 자기 successor에만 합류 |
| herdr-clear-launch · `HerdrClearLaunch` | clear fence가 옛 selector를 차단하며 검증된 후계만 fence 해제 |
| root-replaced-policy · preclaim · `HerdrRootRecovery` | 옛 root는 입력 대상이 될 수 없고, preclaim 회복 후 기다리던 같은 원본을 깨움 |
| missing-bound · `HerdrMissingBound` | exact Missing과 Unknown 분리, 원래 front 보존. 이 모델에 부팅 source와 첫 입력 판정 단계를 추가 |

## 상태와 검증 범위

| 값/상태 | 근거 |
|---|---|
| 시작 `down`, reconcile, 독립 source scan, 첫 payload의 `await/queued` | E01–E06, E19–E22, E47. restart_pending과 일반 재시작은 새 프로세스의 동일 부팅 경계로 투영한다. 입력이 일반 reconcile 및 reader scan 전후에 도착할 수 있다. |
| `idle/busy/dead/rowOnly`, 후발 생존·종료 증거 | E06–E09, E26–E28, E41, E48–E51. rowOnly는 Pending/위치 부재/읽기 실패의 안전한 보류를 나타낸다. 현 cfg에서 판독이 나중에 가능해진다. |
| scan mirror, 확인한 current owner, route `none/existing/ended` | E09–E15, E22–E28. mirror 발행과 입력 owner 확정은 독립이다. Before는 cache miss 사고 경로이며 초기화를 이미 끝낸 별도 채널의 cache hit는 투영 밖이다. |
| `changed/deadWrite`, 전달 0/1/2 | E29, E38–E41, E48–E51. 한 원본이 첫 키 직전에 pane을 잃을 수 있다. 2는 결산된 원본을 다시 제출하는 변형이다. |
| `banner/notice/reinjected` | E23, E30–E32, E50, E52의 별개 출력. 죽은 pane 대조군에는 기존 native source와 직전 대화가 있다. |

`NoLiveRestore`는 기존 실행으로 이어 쓸 때 배너·복구 안내·재주입 0을 검사한다. `EndedResumeShowsRecovery`는 확인된 종료 뒤 새 프로세스로 실제 resume하면 세 효과가 모두 있는지 검사한다. `NoDeadPaneWrite`는 죽었거나 판정되지 않은 pane으로 쓰기를 시도하지 못하게 한다. `ExactlyOnce`는 두 번째 전달과 전달 0건으로 끝난 결산을 모두 막는다. `FirstInputDelivered`는 첫 원본이 큐에 들어가면 결국 1회 전달로 끝나는지 확인한다.

기존 Busy turn이 계속 작동하면 후속 원본을 안전한 입력 경계까지 보류한다. 모델의 Busy 진행은 그 턴이 Idle에 도달하는 경우다. 이미 허가된 Busy steer와 provider별 composer는 기존 구현 계약을 따른다. Weak fairness는 부팅, 관찰 가능해진 증거, wake 이후 재구동, 확인된 종료의 resume와 submit을 진행시킨다. 영구 socket 장애, native resume가 불가능한 세션, 계속 Busy인 pane, Enter 여부가 영원히 불명인 경우, clear/stop의 명시적 폐기, 복수 메시지/채널 및 물리 I/O와 DB 간 원자성은 이 투영 밖이다. 이 경계는 설계에서 기존 큐·제어/hold 계약으로 다룬다.

## 조각 1의 제출 횟수 보강

실제 기존·후계 제출 전이가 전달 횟수를 누적한다. 확정 receipt와 재구동 시도는 별개 상태다. Duplicate는 확정 receipt를 무시해 정상 제출 전이에 다시 들어간다. 같은 재구동에서 기존 덮어쓰기 계수만 되살린 진단 모델은 GREEN이었다. 누적 계수에서는 두 번째 제출 직후 ExactlyOnce가 깨진다. 기존 cfg 위반 집합은 동일하다.

## 새 cfg의 수정 전과 제안·대조 결과

| cfg | 비교 의미 | TLC 결과 | 상태/step | 차트와 반례 |
|---|---|---|---:|---|
| Before | 기존 cache miss의 잘못된 분류 | RED `NoLiveRestore` | 90 / 151 | [차트](herdr-live-reattach-Before.chart.mmd) · [4단계](herdr-live-reattach-Before.report.md) |
| After | 네 시작 경우, 부팅/intake 양 순서 | GREEN | 90 / 151 | [차트](herdr-live-reattach-After.chart.mmd) · [TLC](herdr-live-reattach-After.report.md) |
| AfterStale | 판정 뒤 첫 key 전 죽음 | GREEN | 26 / 42 | [차트](herdr-live-reattach-AfterStale.chart.mmd) · [TLC](herdr-live-reattach-AfterStale.report.md) |
| NoRecheck | AfterStale의 첫 key 재검사 제거 | RED `NoDeadPaneWrite`, `ExactlyOnce`, `FirstInputDelivered` | 15 / 24 | [차트](herdr-live-reattach-NoRecheck.chart.mmd) · [6·6·7단계](herdr-live-reattach-NoRecheck.report.md) |
| NoWake | 증거 확인 뒤 기존 front wake 제거 | RED `FirstInputDelivered` | 64 / 100 | [차트](herdr-live-reattach-NoWake.chart.mmd) · [6단계 정지](herdr-live-reattach-NoWake.report.md) |
| Duplicate | 확정 receipt를 무시해 정상 제출 전이에 한 번 더 진입 | RED `ExactlyOnce` | 103 / 169 | [차트](herdr-live-reattach-Duplicate.chart.mmd) · [7단계](herdr-live-reattach-Duplicate.report.md) |
| TrustRow | 행만 보고 제출 | RED `NoDeadPaneWrite`, `ExactlyOnce`, `FirstInputDelivered` | 67 / 112 | [차트](herdr-live-reattach-TrustRow.chart.mmd) · [5·5·6단계](herdr-live-reattach-TrustRow.report.md) |
| NoRecoveryEffects | 종료 후 실제 resume의 표시를 끔 | RED `EndedResumeShowsRecovery` | 90 / 151 | [차트](herdr-live-reattach-NoRecoveryEffects.chart.mmd) · [5단계](herdr-live-reattach-NoRecoveryEffects.report.md) |

Before의 최단 경로를 소스와 대조했다. (1) E02의 새 runtime 부팅, (2) E19–E20에서 메시지를 원래 입력으로 보관, (3) E04의 reconcile 완료와 queue kickoff, (4) E22–E23에서 cache miss에 selector 복원을 먼저 준비하고 E27에서 살아 있는 Bound pane을 사용한다. E30–E32에서 복원 신호가 만들어질 수 있다. #8280에서 확인된 작업 중 사례는 같은 원인의 Busy 분기다. 복구 안내의 실제 emitter와 세 효과의 상호 순서는 이 모델에서 하나로 단정하지 않는다. 나머지 RED는 데이터 유실·중복·회귀를 탐지하는 설계 대조군이며 실제 운영에서 재현되었다는 뜻이 아니다. E2E 재생 절차와 경계는 설계의 수용 시험에 있다.

Mermaid view는 같은 라벨 안에서의 reconcile/reader scan, 중복되지 않는 등록 플래그 변화와 결산 유지의 self-loop를 그리지 않는다. TLC는 해당 상태와 전이를 검사한다.

## 기존 cfg의 수정 전후

기준 모듈 27 cfg에서 98회, 갱신 모듈과 새 cfg 35개에서 121회 checker 실행. 두 checker 모두 exit 0. 아래 결과는 위반 규칙의 집합이다. 대시는 모든 검사 통과를 뜻한다.

| 기존 cfg (HerdrMissingBound 접두사) | 이전 모듈 | 갱신 모듈 |
|---|---|---|
| BeforeEffectActive | - | - |
| BeforeEffectIdle | InputResolves, WatchResolves | InputResolves, WatchResolves |
| BeforeInput | InputResolves, MissingRetired, NoIdleHeld | InputResolves, MissingRetired, NoIdleHeld |
| BeforeNoticeRetry | InputResolves, NoticeOnce, ReasonDelivered | InputResolves, NoticeOnce, ReasonDelivered |
| BeforeRestart | InputResolves, MissingRetired, NoIdleHeld | InputResolves, MissingRetired, NoIdleHeld |
| BeforeRestored | InputResolves, MissingRetired, NoIdleHeld | InputResolves, MissingRetired, NoIdleHeld |
| BeforeUnknown | InputResolves, NoIdleHeld | InputResolves, NoIdleHeld |
| FixedAlive | - | - |
| FixedCancel | - | - |
| FixedClear | - | - |
| FixedEffectActive | - | - |
| FixedEffectIdle | - | - |
| FixedEffectSuccessor | - | - |
| FixedEffectUnknown | - | - |
| FixedInput | - | - |
| FixedLaunchFailure | - | - |
| FixedNonceRace | - | - |
| FixedNoticeRetry | - | - |
| FixedReboot | - | - |
| FixedRepairFailure | - | - |
| FixedRestart | - | - |
| FixedRestored | - | - |
| FixedRunning | - | - |
| FixedStop | - | - |
| FixedTerminate | - | - |
| FixedUnknown | - | - |
| RetireOnlyLaunchFailure | InputResolves, NoRetainedRefusal | InputResolves, NoRetainedRefusal |

## 재현

저장소 루트에서 실행한다. 다른 cfg를 재현하려면 접미사와 출력 경로를 함께 바꾼다.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/session-lifecycle
~/.claude/skills/statechart/scripts/statechart HerdrMissingBound.tla \
  --config HerdrMissingBoundAttachAfter.cfg --view herdr-live-reattach.view.mjs \
  --out /tmp/sc-i8280-after
```
