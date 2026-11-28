# 재시작 뒤 원본 ticket 정산 — 모델 보고서

기준 코드 `bd0c385be24ca25a10725127dcd77eba1274d4fa`(S2)와 merge한 main `826a01da0e`. 설계는 [design-6](hook-transcript-custody.design.md)이다(S3a 한정).
`HookTranscriptRefresh.tla`(source refresh와 첫 pin)는 그대로 두고, 그 뒤 단계인 "재시작 뒤 boot input이 된 원본 ticket이 어떻게 끝나는가"를 `HookTranscriptCustody.tla`로 모델링했다.
design-6은 design-5 리뷰 두 개가 요구한 상태 구분을 더한다: completion 진입점, membership receipt 미확정, 현재 실행과 원본 역사적 공급 권한, host별 종료 판독, Settle 재진입. design-5의 S3a 규칙은 `S3a5`로 남겨 수정 전 비교에 쓴다.

## 무엇을 모델링했나

옛 실행 N1의 keyed ticket 하나가 재시작 뒤 boot input으로 남아 있고, 채널의 boot gate는 그 ticket이 settled가 될 때까지 닫혀 있다.

- `Rules`: `S2`는 기준 코드, `D2`·`D3`는 design-2·3 본문, `S3a4`는 design-4의 S3a, `S3a5`는 design-5의 S3a, `S3a`는 design-6의 S3a, `Full`은 design-6 S3a + design-4 S3b.
- `entry` (design-6): 저장 결과가 어느 진입점으로 completion에 들어오는지. `restore`는 boot restore(stat 없이 읽음), `retry`는 settle_saved·retry job(exact stat 뒤). `Entries`에서 고른다.
- `receipt` (design-6): durable membership receipt가 있는지. `Receipts`에서 처음 값을 고르고, `WriterReceipt`(writer)와 `ScanReceipt`(finalizer scan)가 만든다. `Settle`·`RawEOFLive`·`HistRawEOF`는 receipt를 요구한다.
- `host` (design-6): N1의 host와 종료 판독 가능성. `herdr`, `tmuxReboot`(재부팅 뒤), `tmuxDead`(dead pane), `tmuxUnknown`(spawn 기록 없음). `Hosts`에서 고른다.
- `fx` (design-6): completion 부작용 실행 횟수. `CleanupDefer`가 Settle 재진입을 만든다(공정성 없음).
- `Mut`: guard 하나를 끈다. 정상은 `none`.
- `scen`: N1이 재시작 전에 남긴 것. 완전한 증거(`complete`), 마지막 capture 뒤의 closer(`closerLate`), 개행 없이 끝난 온전한 closer(`rawCloser`), 끝나지 않은 턴(`killed`), 저장 barrier(`barrierStored`), 저장 결과(`outcomeSaved`·`outcomeLegacy`).
- `dev`, `file`: 장치 번호가 같은지, transcript가 그대로인지(`ok`), 잠시 stat되지 않는지(`late`), 끝내 stat되지 않는지(`gone`), 다른 inode·prefix인지(`changed`). 제품은 `late`와 `gone`을 구별하지 못한다. `S3a4`는 둘 다 확정으로 보고(design-4의 Gone 분류), `S3a`는 둘 다 기다린다.
- `approved`: 파일 승인(B). 같은 dev면 참이고, dev가 바뀌면 `Approvals`에서 고른 처음 값(이미 계보가 있음)이거나 N2 입력·`EndRefresh`로 생긴다.
- `writer`, `otherBad`: 그 ticket의 파일이 Changed이거나 같은 채널의 다른 source cursor가 reopen에 실패하면(`Others`에서 고름) writer가 멈춘다(`WriterHalt`, 공정). S2·D2·D3는 boot debt 구동이 writer 수명에 묶인다. `S3a4`는 구동은 finalizer지만 파일을 읽는 정산이 writer에 남는다. `S3a`·`Full`은 그것까지 finalizer의 source scan(`EndScan`)이 한다.
- `Starts`: `fresh`는 N1이 현재인 채 시작, `superseded`는 설치 전에 이미 N2가 현재이고 종료 기록이 없는 채 시작.
- `Switches`: 참이면 ticket이 N1의 앞 source에 있다.
- `cutKnown`: 종료 기록에 `end_len`이 있는지. launch 때 파일이 온전하면 참이다.

환경 동작 가운데 **후속 N2의 launch, N2의 입력, N2가 행을 차지함, N1의 죽음에는 공정성을 주지 않았다.** 채널 gate가 닫혀 있으면 새 턴 시작이 기다리므로 N2가 뜬다고 가정할 수 없기 때문이다.
약한 공정성은 writer capture, writer 정지, host의 종료 판독, 정산·정리 단계, N2가 뜬 뒤의 idle, 살아 있는 N1의 closer, 잠시 없던 파일의 복귀에만 준다.

모델 가정 두 가지: N2 launch는 transcript를 읽을 수 있을 때만 일어난다(`file # "late"`). `superseded` 시작은 `rawCloser`를 빼고 본다(설계 §8의 남은 위험).

## 규칙

"검사 범위"는 모델이 실제로 보는 것이다. 이름보다 좁은 부분은 제품 시험이 맡는다(설계 §7.3).

| 규칙 | 종류 | 뜻 | 검사 범위 |
|---|---|---|---|
| IdleDoesNotExpand | 불변식 | 결과 없이 행이 정리되는 것은 N1 자신의 idle 또는 N1의 종료로만 | `clearedBy # "n2idle"` |
| NoSilentLoss | 불변식 | 원본 내용이 돌아올 수 있는 동안에는 아무것도 포기하지 않는다 | completion 건너뜀 ⇒ `file = "changed"` 또는 restore 진입(S2 동등). closer 있는 턴의 `Unverified` 정산 ⇒ `file = "changed"` |
| IdleNotExpandedCrossDev | 불변식 | N2 idle 정리는 장치가 같을 때만(S2의 기존 공백을 넓히지 않음) | `clearedBy = "n2idle" => dev = "same"` |
| NoSuffixAttribution | 불변식 | Executed는 N1이 직접 쓴 closer로만 | `outcome = "Executed" => fileCloser` |
| RejectUnverifiedFile | 불변식 | 새 결과는 같은 파일, 같은 장치 또는 승인된 장치 계보에서만 | 새 결과 시점의 `file = "ok" ∧ (dev = "same" ∨ approved)`. hash 내용은 보지 않는다 |
| EndProofOwned | 불변식 | 종료 기록은 실제로 끝난 실행만 가리킨다 | `endRec => n1 = "dead"` |
| ReleaseNeedsEnd | 불변식 | N1의 종료로 한 정리에는 이 ticket의 source에 적용되는 종료 기록이 있다 | `EndApplies` |
| NoLostAnswer | 불변식 | closer를 읽을 수 있는 턴은 결과 없이 정산되지 않는다 | 파일이 온전하고 closer(raw 포함)가 있으면 결과 없는 정산 금지 |
| CompletionBeforeCleanup | 불변식 | 저장된 Executed의 행은 completion 단계가 끝난 뒤에만 정리된다 | 같은 이름 |
| SkipOnlyWhenTerminal | 불변식 | completion을 포기하는 것은 파일이 사라졌거나 바뀌었거나, seal 없는 결과가 장치 변경을 만났거나, restore 진입(S2 동등)일 때뿐 | 미리 분류된 `file` 값. ENOENT 뒤 복귀는 보지 않는다 |
| OneCompletionEffect (design-6) | 불변식 | completion 부작용은 ticket마다 한 번 | `fx <= 1`. 부작용 도중 process 종료는 모델에 없다 |
| SuccessorUntouched | 불변식 | N1 정리가 N2의 행을 지우지 않는다 | 같은 이름 |
| ExactTarget | 불변식 | 다른 target의 ticket은 정산되지 않는다 | 추상 ticket 하나(`t2`). key·channel·namespace별 비교는 보지 않는다 |
| OneOutcome | 불변식 | 새 outcome 기록은 많아야 한 번 | Registry outcome 쓰기 횟수. completion 부작용은 세지 않는다 |
| SavedOutcomePreserved | 불변식 | 저장된 결과는 바뀌지 않는다 | 같은 이름 |
| BootEventuallyReady | liveness | runtime이 뜬다 | |
| ChannelEventuallyReady | liveness | 채널의 boot gate가 결국 열린다 | |
| EligibleEventuallySettles | liveness | N1이 닫은 턴은 transcript를 읽을 수 있으면 Executed로 정산된다 | |
| SameDevBarrierStillClears | liveness | 같은 장치의 저장 barrier는 후속이 idle이면 지금처럼 정리된다 | |
| SameDevKilledStillClears | liveness | 같은 장치에서 N1이 턴 중간에 끝나고 N2 입력 뒤 idle이면 지금처럼 정리된다 | |

## 결과

TLC 1.7.4, 설정 95개. CI 검사기(`statecharts_tlc.py --jobs 5 --shard-count 3`)로 이 모듈을 포함해 바뀐 설정 274개가 기대값과 같다. `-`는 깨진 규칙이 없다는 뜻이다.

### S3a 범위: 수정 전과 후

같은 모집단(처음 상태 fresh, source 전환 없음, 승인 있음, 파일 ok·late·changed, 진입점 둘, receipt 유무, host는 herdr·tmuxReboot·tmuxDead, seal 없는 결과 제외)을 design-5와 design-6 S3a에 돌렸다.

| 설정 | 규칙 | 깨진 불변식 | 깨진 liveness |
|---|---|---|---|
| `S3a5` | design-5 S3a | IdleDoesNotExpand, **OneCompletionEffect** | **ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears** |
| `S3a` | design-6 S3a | IdleDoesNotExpand | - |

`IdleDoesNotExpand`는 S2의 N2 idle 정리를 남긴 기존 공백이다(`IdleNotExpandedCrossDev`는 통과).

### 리뷰가 요구한 경로 (같은 모집단, 세 규칙)

| 경로 | 모집단 | S2 | S3a5 (수정 전) | S3a (수정 후) |
|---|---|---|---|---|
| Restore | 저장 결과, restore 진입, 파일 ok·late·gone·changed | OneCompletionEffect · Channel | OneCompletionEffect · **Channel** | - · - |
| Receipt | receipt 없이 시작, 다른 source 실패 없음 | OneCompletionEffect · Channel, Eligible | OneCompletionEffect · **Channel, Eligible** | - · - |
| Historic | 설치 전 이미 N2, 같은 dev | OneCompletionEffect · - | - · **Channel, Eligible** | - · - |
| Tmux | host tmuxReboot·tmuxDead | IdleDoesNotExpand, OneCompletionEffect · Boot, Channel, Eligible | IdleDoesNotExpand, OneCompletionEffect · **Channel, Eligible** | IdleDoesNotExpand · - |
| Reentry | 저장 결과, Settle 재진입 | **OneCompletionEffect** · - | **OneCompletionEffect** · - | - · - |
| TmuxUnknown | host tmuxUnknown | IdleDoesNotExpand, OneCompletionEffect · Boot, Channel, Eligible | (설정 없음) | IdleDoesNotExpand · Channel, Eligible |

(`·` 앞은 불변식, 뒤는 liveness.)

- Restore 반례(S3a5, 4단계): 저장 결과 + restore 진입 + 파일이 stat되지 않음 → N2가 뜨고 idle이 돼도 "결과 저장됨, 본문 읽기 대기 (파일 없음)"에서 멈춘다. S2는 stat 없이 읽고 실패하면 경고 뒤 진행한다. 수정 후에는 seal 없는 결과의 dev 변경도 restore 진입이면 S2처럼 읽는다.
- Receipt 반례(S3a5, 6단계): receipt 없음, 다른 source 복구 실패 → writer 정지 → N2 launch(종료 기록) → scan이 범위를 확정하지만 receipt를 만들지 않아 `Settle`이 막힌다.
- Historic 반례(S3a5, 2단계): 처음부터 N2가 현재, N1 closer 있음 → N2 입력 → N2 idle에서 멈춘다. design-5 writer는 현재 실행만 공급하고, 종료 기록이 없어 scan도 없다. S2는 writer가 원본 Bound로 공급한다.
- Reentry 반례(S2·S3a5, 3단계): completion 기록 → 정리 지연(`CleanupDefer`) → 다시 completion. 부작용이 두 번 돈다.
- TmuxUnknown의 S3a는 종료 기록 없이 S2 출구만 가진다. 같은 모집단의 S2보다 나빠지지 않는다(Boot 통과).

### 옛 규칙에 생긴 차이

- `OneCompletionEffect`는 design-6에서만 latch가 있으므로, 옛 규칙(S2·D2·D3·S3a4)의 설정 다수에서 새로 깨진다.
- S2 모델의 retry 진입을 exact stat 뒤에서 기다리도록 바로잡았다(`discord/turn_finalizer/native_supply.rs:664-675`). 건너뜀은 restore 진입에서만 일어나고, 그것은 S2 동등으로 허용된다. 그래서 `S2`·`GoneS2`·`LegacyDevS2`는 `NoSilentLoss`(`S2`는 `SkipOnlyWhenTerminal`도)가 더는 깨지지 않는다.
- `NoEndCut`(Full 변형)은 `ChannelEventuallyReady`가 더 깨진다. cut이 없으면 인계가 없고 scan receipt도 없기 때문이다.

### 수정 전과 후 (바뀐 설정 전부)

"전"은 design-5 모델의 기대값(73개, `s3d6-expected-before.tsv`로 보관)이고 "후"는 design-6 모델이다. 기존 설정 파일에는 `Entries = {"retry"} Receipts = {TRUE} Hosts = {"herdr"}`와 `OneCompletionEffect`가 더해졌다. 옛 모델에 없던 설정은 "(없음)"이다.

| 설정 | 전: 깨진 불변식 | 전: 깨진 liveness | 후: 깨진 불변식 | 후: 깨진 liveness | 같음 |
|---|---|---|---|---|---|
| `AnyTarget` | ExactTarget | - | ExactTarget | - | 예 |
| `BarrierD2` | - | ChannelEventuallyReady, SameDevBarrierStillClears | - | ChannelEventuallyReady, SameDevBarrierStillClears | 예 |
| `BarrierD3` | - | - | - | - | 예 |
| `BarrierDesign` | - | - | - | - | 예 |
| `BarrierS2` | IdleDoesNotExpand | ChannelEventuallyReady | IdleDoesNotExpand | ChannelEventuallyReady | 예 |
| `BarrierS3a` | IdleDoesNotExpand | - | IdleDoesNotExpand | - | 예 |
| `D2` | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 아니오 |
| `D3` | NoLostAnswer, NoSilentLoss | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | NoLostAnswer, NoSilentLoss, OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 아니오 |
| `Design` | - | - | - | - | 예 |
| `DropEvidence` | NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | 예 |
| `EndWithoutProof` | EndProofOwned, NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | EndProofOwned, NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | 예 |
| `GoneS2` | NoSilentLoss | BootEventuallyReady, ChannelEventuallyReady | - | BootEventuallyReady, ChannelEventuallyReady | 아니오 |
| `GoneS3a` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `HaltedD3` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `HaltedDesign` | - | - | - | - | 예 |
| `HaltedS2` | - | BootEventuallyReady, ChannelEventuallyReady | - | BootEventuallyReady, ChannelEventuallyReady | 예 |
| `HaltedS3a` | - | - | - | - | 예 |
| `HistoricS2` | (없음) | (없음) | OneCompletionEffect | - | 아니오 |
| `HistoricS3a` | (없음) | (없음) | - | - | 아니오 |
| `HistoricS3a5` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `IdleBorrow` | IdleDoesNotExpand, IdleNotExpandedCrossDev | - | IdleDoesNotExpand, IdleNotExpandedCrossDev | - | 예 |
| `KilledD2` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `KilledD3` | - | - | - | - | 예 |
| `KilledDesign` | - | - | - | - | 예 |
| `KilledS2` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `KilledS3a` | - | - | - | - | 예 |
| `LatestSourceOnly` | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 예 |
| `LegacyDevS2` | NoSilentLoss | ChannelEventuallyReady | OneCompletionEffect | ChannelEventuallyReady | 아니오 |
| `LegacyDevS3a` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `NoApproval` | RejectUnverifiedFile | - | RejectUnverifiedFile | - | 예 |
| `NoBootProver` | - | ChannelEventuallyReady, EligibleEventuallySettles | - | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `NoEndCut` | NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | NoLostAnswer, NoSilentLoss | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `NoEndProof` | ReleaseNeedsEnd | - | ReleaseNeedsEnd | - | 예 |
| `NoEndRefresh` | - | ChannelEventuallyReady, EligibleEventuallySettles | - | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `NoFinalCapture` | NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | 예 |
| `NoHistRaw` | - | ChannelEventuallyReady, EligibleEventuallySettles | - | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `NoLaunchRecord` | - | ChannelEventuallyReady, EligibleEventuallySettles | - | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `NoN2D3` | - | ChannelEventuallyReady, EligibleEventuallySettles | OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `NoN2Design` | - | - | - | - | 예 |
| `NoN2S2` | - | ChannelEventuallyReady, EligibleEventuallySettles | - | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `NoN2S3a` | - | ChannelEventuallyReady, EligibleEventuallySettles | - | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `NoPrefixCheck` | RejectUnverifiedFile | - | RejectUnverifiedFile | - | 예 |
| `NoRowCAS` | SuccessorUntouched | - | SuccessorUntouched | - | 예 |
| `NoSupersededProof` | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 예 |
| `OtherHaltS2` | IdleDoesNotExpand | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears | IdleDoesNotExpand, OneCompletionEffect | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears | 아니오 |
| `OtherHaltS3a` | IdleDoesNotExpand | - | IdleDoesNotExpand | - | 예 |
| `OtherHaltS3a4` | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears | IdleDoesNotExpand, OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears | 아니오 |
| `PendingForever` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `RawCloserD3` | NoLostAnswer, NoSilentLoss | EligibleEventuallySettles | NoLostAnswer, NoSilentLoss, OneCompletionEffect | EligibleEventuallySettles | 아니오 |
| `RawCloserDesign` | - | - | - | - | 예 |
| `RawCloserS2` | - | ChannelEventuallyReady, EligibleEventuallySettles | OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `RawCloserS3a` | - | - | - | - | 예 |
| `ReaperExact` | - | BootEventuallyReady, ChannelEventuallyReady | - | BootEventuallyReady, ChannelEventuallyReady | 예 |
| `ReceiptS2` | (없음) | (없음) | OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `ReceiptS3a` | (없음) | (없음) | - | - | 아니오 |
| `ReceiptS3a5` | (없음) | (없음) | OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `ReentryS2` | (없음) | (없음) | OneCompletionEffect | - | 아니오 |
| `ReentryS3a` | (없음) | (없음) | - | - | 아니오 |
| `ReentryS3a5` | (없음) | (없음) | OneCompletionEffect | - | 아니오 |
| `RestoreS2` | (없음) | (없음) | OneCompletionEffect | ChannelEventuallyReady | 아니오 |
| `RestoreS3a` | (없음) | (없음) | - | - | 아니오 |
| `RestoreS3a5` | (없음) | (없음) | OneCompletionEffect | ChannelEventuallyReady | 아니오 |
| `S2` | IdleDoesNotExpand, NoSilentLoss, SkipOnlyWhenTerminal | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | IdleDoesNotExpand, OneCompletionEffect | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `S3a` | IdleDoesNotExpand | - | IdleDoesNotExpand | - | 예 |
| `S3a4` | IdleDoesNotExpand, NoSilentLoss, SkipOnlyWhenTerminal | - | IdleDoesNotExpand, NoSilentLoss, OneCompletionEffect, SkipOnlyWhenTerminal | - | 아니오 |
| `S3a5` | (없음) | (없음) | IdleDoesNotExpand, OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears | 아니오 |
| `S3aAll` | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `S3aCrossIdle` | IdleDoesNotExpand, IdleNotExpandedCrossDev | ChannelEventuallyReady, EligibleEventuallySettles | IdleDoesNotExpand, IdleNotExpandedCrossDev | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `S3aEnoentTerminal` | IdleDoesNotExpand, NoSilentLoss, SkipOnlyWhenTerminal | - | IdleDoesNotExpand, NoSilentLoss, SkipOnlyWhenTerminal | - | 예 |
| `S3aLegacy` | - | - | - | - | 예 |
| `S3aNoLatch` | (없음) | (없음) | OneCompletionEffect | - | 아니오 |
| `S3aRestoreAwait` | (없음) | (없음) | - | ChannelEventuallyReady | 아니오 |
| `S3aScanInWriter` | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles, SameDevKilledStillClears | 예 |
| `S3aScanNoReceipt` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `S3aTmuxLiveOnly` | (없음) | (없음) | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `SkipTransient` | NoSilentLoss, SkipOnlyWhenTerminal | - | NoSilentLoss, SkipOnlyWhenTerminal | - | 예 |
| `SuffixScope` | NoSuffixAttribution, RejectUnverifiedFile | - | NoSuffixAttribution, RejectUnverifiedFile | - | 예 |
| `SupersededD3` | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 아니오 |
| `SupersededDesign` | - | - | - | - | 예 |
| `SupersededS2` | IdleDoesNotExpand | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | IdleDoesNotExpand, OneCompletionEffect | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `SupersededS3a` | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | 예 |
| `SwitchedD3` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `SwitchedDesign` | - | - | - | - | 예 |
| `SwitchedS2` | - | BootEventuallyReady, ChannelEventuallyReady | - | BootEventuallyReady, ChannelEventuallyReady | 예 |
| `SwitchedS3a` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |
| `TmuxS2` | (없음) | (없음) | IdleDoesNotExpand, OneCompletionEffect | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `TmuxS3a` | (없음) | (없음) | IdleDoesNotExpand | - | 아니오 |
| `TmuxS3a5` | (없음) | (없음) | IdleDoesNotExpand, OneCompletionEffect | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `TmuxUnknownS2` | (없음) | (없음) | IdleDoesNotExpand, OneCompletionEffect | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `TmuxUnknownS3a` | (없음) | (없음) | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `TransientS2` | IdleDoesNotExpand, NoSilentLoss, SkipOnlyWhenTerminal | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | IdleDoesNotExpand, OneCompletionEffect | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | 아니오 |
| `TransientS3a` | IdleDoesNotExpand | - | IdleDoesNotExpand | - | 예 |
| `TransientS3a4` | IdleDoesNotExpand, NoSilentLoss, SkipOnlyWhenTerminal | - | IdleDoesNotExpand, NoSilentLoss, OneCompletionEffect, SkipOnlyWhenTerminal | - | 아니오 |
| `VoidCompletion` | CompletionBeforeCleanup | - | CompletionBeforeCleanup | - | 예 |
| `WriterRetry` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 예 |

차트: boot restore [S3a5](hook-transcript-custody-restore-before.chart.mmd) · [S3a](hook-transcript-custody-restore-after.chart.mmd), 설치 전 N2 [S3a5](hook-transcript-custody-historic-before.chart.mmd) · [S3a](hook-transcript-custody-historic-after.chart.mmd), Settle 재진입 [S3a5](hook-transcript-custody-reentry-before.chart.mmd) · [S3a](hook-transcript-custody-reentry-after.chart.mmd), receipt [S3a5](hook-transcript-custody-receipt-before.chart.mmd). `ReceiptS3a`는 994 상태라 차트 상한(500)을 넘어 그리지 않았다. design-4·5의 차트는 그대로 둔다.

### 변형

design-6 변형 넷(`S3a` 기준)은 각자 겨냥한 규칙을 새로 깬다.

| 변형 | 끄는 것 | 깨진 규칙 |
|---|---|---|
| `S3aRestoreAwait` | restore 진입에 design-5의 Await gate | **ChannelEventuallyReady** |
| `S3aScanNoReceipt` | scan의 receipt 생산 | **ChannelEventuallyReady, EligibleEventuallySettles** |
| `S3aTmuxLiveOnly` | tmux 종료 판독에 live pane 요구 | IdleDoesNotExpand · **ChannelEventuallyReady, EligibleEventuallySettles** |
| `S3aNoLatch` | durable completion latch | **OneCompletionEffect** |

design-5의 변형(`S3aEnoentTerminal`, `S3aScanInWriter`)과 design-4의 변형 23개(`Full` 기준)는 각자 겨냥한 규칙에서 그대로 깨진다.

## 반례와 코드

코드 확인은 기준 SHA의 소스를 읽은 것이다. 설계 단계라 실행으로 재현하지 않았다. 재현은 설계 §7.3의 제품 RED가 맡는다.

| 설정·규칙 | 반례 경로 | 코드 |
|---|---|---|
| S2 · IdleDoesNotExpand | 저장 barrier → N2가 뜸 → N2 idle → N2의 idle로 정리 | 확인. idle 통지는 현재 binding의 target으로 나가고(`tui_o/writer/rotation/owe.rs:728-767`), boot input을 target 비교 없이 돈다(`discord/turn_finalizer/native_supply.rs:403-415,444-453`). S3a·BarrierS3a도 같은 경로(3단계)이고 장치가 같을 때만이다 |
| S2 · BootEventuallyReady | 장치 변경 + 저장 barrier → 부팅 거절 | 확인. `boot_reaper.rs:83-92` → `boot_custody.rs:435-443` → `history/v2/native.rs:229-275` → `history/v2.rs:21-33`의 exact dev·inode 비교 |
| D3 · NoLostAnswer (RawCloser) | 개행 없는 closer → N1 종료 → N2 launch(종료 기록) → writer capture(범위 확정, closer 없음) → 결과 없이 정산 | 확인. partial 유지 `tui_o/shadow/capture.rs:389-419`, 현재 raw EOF는 row가 N1일 때만 `discord/turn_finalizer/native_terminal.rs:122-129` |
| D3 · ChannelEventuallyReady (Halted) | 장치·파일 바뀜 → N2 launch(길이 모름) → writer 정지 → 그 뒤 아무 것도 정산을 부르지 않음 | 확인. `tui_o/writer/rotation/readers.rs:121-138`, `tui_o/writer/actor.rs:367-370,441-453`, `tui_o/writer/host.rs:701-712`, `tui_o/writer/host/missing.rs:67-68` |
| S3a · ChannelEventuallyReady (NoN2) | 장치 변경, 승인 없음 → N2가 뜨지만 입력 없음 → idle → 그대로 | 확인. 계보 생산자는 `user_prompt_submit`·`stop` hook뿐(`tui_o/writer/rotation/source_refresh.rs:3-19`). S3b `EndRefresh`가 닫는다 |
| S3a · ChannelEventuallyReady (Superseded) | 처음부터 N2가 현재, 종료 기록 없음 → N2 idle → 그대로 | 확인. launch가 row를 N2로 바꾼다(`herdr_launch.rs:492-509,548-568`). S3b `superseded`가 닫는다 |
| S3a · ChannelEventuallyReady (Switched) | 앞 source ticket → 종료 기록(최신 source) → 범위 확정 → N2 idle → 그대로 | 확인. 같은 nonce의 source 전환 `tui_prompt_dedupe/binding_events.rs:116-126`. S3b의 source 확장이 닫는다 |
| S3a4 · NoSilentLoss (Transient) | 저장 결과 + 파일 잠시 stat되지 않음 → completion 건너뜀 → 파일 복귀 | 확인. S2는 exact stat 검사가 job을 막는다(`discord/turn_finalizer/native_restore.rs:73-92`, `tui_input/human_frame/history/v2.rs:21-35`, `discord/turn_finalizer/native_supply.rs:664-675`). design-4는 그 앞단을 구조 검증으로 낮췄다 |
| S3a4 · ChannelEventuallyReady (OtherHalt) | 다른 cursor 실패 → writer 정지 → 이 source는 정상이나 범위 안 귀속을 할 주체가 없음 | 확인. `tui_o/writer/rotation/readers.rs:75-95,121-138`, `tui_o/writer/actor.rs:367-370,441-453`. design-5는 `Projection::reopen`(`tui_o/writer/input_facts/native.rs:249-275`)으로 source를 따로 읽는다 |
| S3a5 · ChannelEventuallyReady (Restore) | restore 진입 + 파일이 stat되지 않음 → 영구 대기 | 확인. S2 restore는 `refreshed` 없이 `prepare_completion`(`discord/turn_finalizer/native_restore.rs:418-430`), 읽기 실패는 경고 뒤 진행(`discord/turn_finalizer/native_completion.rs:74-81`) |
| S3a5 · ChannelEventuallyReady (Receipt) | receipt 없음 → scan이 범위 확정 → `Settle` 불가 | 확인. Executed 기록은 durable receipt를 요구(`tui_input/human_frame/history/v2/native.rs:618-632`). S2 writer는 receipt 먼저(`tui_o/writer/rotation/owe.rs:616-651`) |
| S3a5 · ChannelEventuallyReady (Historic) | 처음부터 N2 → writer 공급 없음 → 영구 대기 | 확인. S2 writer는 원본 Bound로 공급(`tui_o/writer/rotation/owe.rs:425-457`, `discord/turn_finalizer/native_supply.rs:109-159`) |
| S3a5 · ChannelEventuallyReady (Tmux) | tmux 재부팅·dead pane → 종료 판독 불가 | 확인. `vacant`는 실시간 capture를 요구(`discord/commands/control/clear_teardown/clear_end_record.rs:73-101,133-138`). dead pane은 Present이고 respawn(`claude/tui_session_launch.rs:156-168`) |
| S2·S3a5 · OneCompletionEffect (Reentry) | completion → 정리 지연 → 다시 completion | 확인. Settle이 `discord/turn_finalizer/native_restore.rs:317-327`·`:338`에서 정리 없이 돌아오고, 다음 진입이 `prepare_completion`을 다시 부른다. 부작용은 `discord/turn_finalizer/native_completion.rs:215-283` |
| S2·D2 · ChannelEventuallyReady | 턴 중간 종료 → N2가 떠도, 입력이 와도, idle이 돼도 그대로(장치 변경) | 확인. gate는 모든 boot input의 settled(`native_prepare.rs:355-360`) |

## 모델이 다루지 않는 것

- hash·byte 변조, FD와 path의 경합, Registry fingerprint, launch와 marker 게시의 실제 순서.
- `Retired` 근거별 강도와 host 판독의 세부. 모델의 `host`는 판독 가능 여부(Herdr, tmux 재부팅, dead pane, 판독 불가)만 고른다. pid 재사용과 시작 시각 비교는 제품 시험이 맡는다.
- live completion과 제어 결과(`control_proof`) 진입. 모델의 진입점은 restore와 retry 둘이다.
- 부작용 도중의 process 종료. `fx`는 Settle 재진입만 센다.
- ticket 여러 개, 후속 실행 여러 개, 종료 기록 16건 상한.
- 살아 있는 actor가 있어야 하는 제어 경로(Cancel·Terminate·clear·purge·Stop).
- host 판독이 끝내 답하지 않는 경우. 모델은 답한다고 가정한다.
- 시간. 반응 시간 계약은 설계 §4가 다룬다.

## 다시 돌리기

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/tui-o-writer
~/.claude/skills/statechart/scripts/statechart HookTranscriptCustody.tla \
  --config HookTranscriptCustodyS3a.cfg --view hook-transcript-custody.view.mjs --out /tmp/sc-custody
```

설정 95개의 기대값 대조는 저장소 검사기로 한다. main merge로 다른 모듈의 설정도 바뀌어 274개가 선택되므로 shard로 나눠 돌렸다.

```sh
python3 scripts/ci/statecharts_tlc.py --base 9b0cdbe9c7 --head HEAD --jobs 5 --shard-index 0 --shard-count 3
```
