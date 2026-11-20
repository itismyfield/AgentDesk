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

## 조각 2의 동일 원본 제출

`OwnerAttachSpec`은 E19–E29와 E51의 admission ticket, Busy/Idle grant, front와 frozen followers 예약, watch/wake, 실제 제출과 receipt를 연결한다. `shellHeld`는 셸이 foreground를 되찾았을 때 기록된 provider가 여전히 살아 있거나 판독할 수 없는 경우다(E07–E09). 제출이 확인되기까지는 실행 행만 있거나 준비 상태를 읽을 수 없는 원본과 같은 방식으로 보류한다. 첫 키 전 사망·stop·reset은 효과를 막고 기존 owner가 다시 판정하도록 한다. 제어가 원본을 명시적으로 폐기하는 경우는 이 전달 liveness의 범위 밖이다.

`SameOriginal`은 같은 원본만 예약·결산하는지 확인한다. `BeforeFirstKey`는 사망·stop·reset이 먼저 온 예약에서 물리 효과를 막는지 확인한다. `OwnerExactlyOnce`는 확인된 제출과 응답 불명의 제출을 모두 센다. `OwnerDelivers`의 fairness는 Held가 풀릴 증거와 AckUnknown을 결산할 receipt가 결국 관찰되는 경우에 적용한다. boot와 첫 intake의 상대 순서는 고정하지 않는다. 조각 2의 production 진입점은 기존 분기를 사용한다. 아래 표는 활성화 전에 준비한 owner 경로의 모델 결과다.

| cfg (HerdrMissingBoundOwner 접두사) | 결과 | 상태 / 전이 | 산출 |
|---|---|---:|---|
| After | GREEN | 203 / 397 | [차트](herdr-reattach-owner-After.chart.mmd) · [결과](herdr-reattach-owner-After.report.md) |
| Death | GREEN | 359 / 664 | [차트](herdr-reattach-owner-Death.chart.mmd) · [결과](herdr-reattach-owner-Death.report.md) |
| Stop | GREEN | 333 / 625 | [차트](herdr-reattach-owner-Stop.chart.mmd) · [결과](herdr-reattach-owner-Stop.report.md) |
| Reset | GREEN | 333 / 625 | [차트](herdr-reattach-owner-Reset.chart.mmd) · [결과](herdr-reattach-owner-Reset.report.md) |
| NoWake | RED `OwnerDelivers` | 93 / 165 | [차트](herdr-reattach-owner-NoWake.chart.mmd) · [6단계](herdr-reattach-owner-NoWake.report.md) |
| NoFirstKey | RED `BeforeFirstKey` | 251 / 487 | [차트](herdr-reattach-owner-NoFirstKey.chart.mmd) · [7단계](herdr-reattach-owner-NoFirstKey.report.md) |
| ResendUnknown | RED `OwnerExactlyOnce`, `OwnerDelivers` | 323 / 685 | [차트](herdr-reattach-owner-ResendUnknown.chart.mmd) · [8·11단계](herdr-reattach-owner-ResendUnknown.report.md) |

## 이름 기반 재부착과 평소 입력 규칙

`ParitySpec`은 이름으로 바뀐 pane 위치를 선택하는 경우와 기존 정밀 판정으로 내려가는 경우를 나눈다. 유일한 이름의 nonce가 DB와 같고 agent pane이 살아 있는 경우에만 이름 경로가 위치를 갱신한다. 재부착은 watcher와 registry를 연결하고, 작업 중이면 진행 턴 lease와 Busy grant를 인수한다. 이어 같은 원본을 평소 Busy 주입 또는 유휴 제출로 보낸다. 살아 있지만 Missing을 증명하지 못한 실행도 같은 경로를 사용한다. 평소 composer가 입력을 막으면 그 경계가 풀릴 때까지 같은 claim을 보존한다.

`NoLiveDiscard`는 살아 있는 pane에 보낸 원본의 폐기를 막고, `OwnerExactlyOnce`는 응답 불명까지 제출 횟수에 포함한다. `NamedOnlyCurrent`는 복수 이름과 nonce 불일치가 이름 경로를 통과하는지, `BusyAuthority`는 Busy grant에 진행 턴 lease가 있는지 검사한다. `NoCompetingClaim`은 같은 채널의 새 headless claim이 기존 Busy lease를 앞지르는 경우를 검출한다. `ParityResolves`는 결국 1회 제출과 답 게시 또는 종료 증명을 거친 거절에 도달하는지 확인한다. 영구 관찰 장애나 영구 composer 차단, 영구적으로 찾을 수 없는 receipt는 이 fairness 범위 밖이다. 중단·reset은 기존 Owner cfg에서 검증한다.

| cfg (HerdrMissingBoundParity 접두사) | 결과 | 상태 / 전이 | 산출 |
|---|---|---:|---|
| After | GREEN | 348 / 393 | [차트](herdr-reattach-parity-After.chart.mmd) · [결과](herdr-reattach-parity-After.report.md) |
| BeforeBusy | RED `NoIdleOnly`, `ParityResolves` | 17 / 18 | [차트](herdr-reattach-parity-BeforeBusy.chart.mmd) · [반례](herdr-reattach-parity-BeforeBusy.report.md) |
| BeforeClaimRace | RED `NoCompetingClaim`, `ParityResolves` | 30 / 38 | [차트](herdr-reattach-parity-BeforeClaimRace.chart.mmd) · [반례](herdr-reattach-parity-BeforeClaimRace.report.md) |
| BeforeUnproven | RED `NoLiveDiscard`, `ParityResolves` | 12 / 12 | [차트](herdr-reattach-parity-BeforeUnproven.chart.mmd) · [반례](herdr-reattach-parity-BeforeUnproven.report.md) |
| ClaimRace | GREEN | 30 / 40 | [차트](herdr-reattach-parity-ClaimRace.chart.mmd) · [결과](herdr-reattach-parity-ClaimRace.report.md) |
| Duplicate | GREEN. 정밀 경로 | 20 / 23 | [차트](herdr-reattach-parity-Duplicate.chart.mmd) · [결과](herdr-reattach-parity-Duplicate.report.md) |
| OtherNonce | GREEN. 정밀 경로 | 20 / 23 | [차트](herdr-reattach-parity-OtherNonce.chart.mmd) · [결과](herdr-reattach-parity-OtherNonce.report.md) |
| TrustName | RED `NamedOnlyCurrent` | 20 / 23 | [차트](herdr-reattach-parity-TrustName.chart.mmd) · [반례](herdr-reattach-parity-TrustName.report.md) |
| ResendUnknown | RED `OwnerExactlyOnce`, `ParityResolves` | 24 / 29 | [차트](herdr-reattach-parity-ResendUnknown.chart.mmd) · [반례](herdr-reattach-parity-ResendUnknown.report.md) |

| 설계 진입점 | 모델 행동과 경계 |
|---|---|
| E01–E05 | `Boot`, `ReconcileIntake`, `FirstInput`. drain 세부 절차는 부팅 경계로 투영. |
| E06–E08, E40 | `ParityAssess`, `ParityObserve`. local canonical 행, 이름과 현재 pane 관찰. |
| E09–E18 | `ParityAttach`. 정확한 source·registry, watcher claim, 진행 턴 인수의 성공 경계. E11–E14의 별도 mirror 수리와 E18의 운영자 수동 선택 세부는 투영 밖. |
| E19–E20, E47 | `FirstInput`, `ReconcileIntake`. 원본·ticket 재사용. catch-up 중복 세부는 기존 `OwnerCatchUp`. |
| E21–E22, E26 | `ParityAssess`, `ParityObserve`, `ParityInputRule`, `ParityComposerClears`. owner의 평소 Idle/Busy 진입, Unproven 보존. |
| E25 | `ParityHeadlessClaim`, `ParityAttach`. 채널 transition 안에서 진행 턴 lease를 먼저 인수하고 같은 채널의 새 headless claim을 거절한다. |
| E23–E24, E30–E33, E52 | 기존 `ChooseRoute`·`ResumeEnded`의 selector와 복원 표시 투영. 살아 있는 경로에서는 `ParityAttach` 직후 복원 문맥·표시를 고르지 않는다. 실제 resume의 후속 효과는 기존 모델에서 검증한다. |
| E27–E29, E44, E51 | `ParityInputRule`, `ParitySubmit`, `ParityObserveReceipt`, `ParityPublishReply`. first-key 중단은 기존 `OwnerInterrupt`·`OwnerRejectBeforeKey`. |
| E34–E38, E46, E48 | 기존 `OwnerInterrupt`·`OwnerRejectBeforeKey`, 이번 `ParityReject`의 증명 경계. 명시적 취소·clear의 운영 세부는 투영 밖. |
| E39, E45, E50 | 기존 `ResumeEnded`. 새 프로세스 resume·표시는 조각 3 경계. |
| E41, E49 | `ParityAssess`의 `endProved`, `ParityReject`. `Unproven`은 `ParityAttach` 또는 `ParityObserve`로 합류. |
| E42–E43 | `FirstInput`, `ParityAttach`의 owner·source 전제. TUI 직접 입력과 consent/O-writer 이관 세부는 투영 밖. |

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
