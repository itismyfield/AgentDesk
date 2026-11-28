# 재시작 뒤 원본 ticket 정산 — 모델 보고서

기준 코드 `bd0c385be24ca25a10725127dcd77eba1274d4fa`. 설계는 [design-4](hook-transcript-custody.design.md)이다.
`HookTranscriptRefresh.tla`(source refresh와 첫 pin)는 그대로 두고, 그 뒤 단계인 "재시작 뒤 boot input이 된 원본 ticket이 어떻게 끝나는가"를 `HookTranscriptCustody.tla`로 모델링했다.
design-4에서 모델을 다시 썼다. design-3 리뷰가 짚은 다섯 경로(파일 승인, writer 수명, 이미 N2인 처음 상태, 같은 nonce의 source 전환, 개행 없는 closer)를 동작과 처음 상태로 넣고 S3a 규칙을 더했다.

## 무엇을 모델링했나

옛 실행 N1의 keyed ticket 하나가 재시작 뒤 boot input으로 남아 있고, 채널의 boot gate는 그 ticket이 settled가 될 때까지 닫혀 있다.

- `Rules`: `S2`는 기준 코드, `D2`·`D3`는 design-2·3 본문, `S3a`는 design-4의 첫 단계, `Full`은 S3a+S3b.
- `Mut`: guard 하나를 끈다. 정상은 `none`.
- `scen`: N1이 재시작 전에 남긴 것. 완전한 증거(`complete`), 마지막 capture 뒤의 closer(`closerLate`), 개행 없이 끝난 온전한 closer(`rawCloser`), 끝나지 않은 턴(`killed`), 저장 barrier(`barrierStored`), 저장 결과(`outcomeSaved`·`outcomeLegacy`).
- `dev`, `file`: 장치 번호가 같은지, transcript가 그대로인지(`ok`), 잠시 없는지(`late`), 사라졌는지(`gone`), 바뀌었는지(`changed`).
- `approved`: 파일 승인(B). 같은 dev면 참이고, dev가 바뀌면 `Approvals`에서 고른 처음 값(이미 계보가 있음)이거나 N2 입력·`EndRefresh`로 생긴다.
- `writer`: `file = "changed"`이면 writer가 멈춘다(`WriterHalt`, 공정). S2·D2·D3는 정산이 writer 수명에 묶이고 S3a·Full은 묶이지 않는다.
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
| IdleNotExpandedCrossDev | 불변식 | N2 idle 정리는 장치가 같을 때만(S2의 기존 공백을 넓히지 않음) | `clearedBy = "n2idle" => dev = "same"` |
| NoSuffixAttribution | 불변식 | Executed는 N1이 직접 쓴 closer로만 | `outcome = "Executed" => fileCloser` |
| RejectUnverifiedFile | 불변식 | 새 결과는 같은 파일, 같은 장치 또는 승인된 장치 계보에서만 | 새 결과 시점의 `file = "ok" ∧ (dev = "same" ∨ approved)`. hash 내용은 보지 않는다 |
| EndProofOwned | 불변식 | 종료 기록은 실제로 끝난 실행만 가리킨다 | `endRec => n1 = "dead"` |
| ReleaseNeedsEnd | 불변식 | N1의 종료로 한 정리에는 이 ticket의 source에 적용되는 종료 기록이 있다 | `EndApplies` |
| NoLostAnswer | 불변식 | closer를 읽을 수 있는 턴은 결과 없이 정산되지 않는다 | 파일이 온전하고 closer(raw 포함)가 있으면 결과 없는 정산 금지 |
| CompletionBeforeCleanup | 불변식 | 저장된 Executed의 행은 completion 단계가 끝난 뒤에만 정리된다 | 같은 이름 |
| SkipOnlyWhenTerminal | 불변식 | completion을 포기하는 것은 파일이 사라졌거나 바뀌었거나, seal 없는 결과가 장치 변경을 만났을 때뿐 | 미리 분류된 `file` 값. ENOENT 뒤 복귀는 보지 않는다 |
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

TLC 1.7.4, 설정 59개, 전체 실행 약 5분 30초(statechart 도구, 설정별 차트 포함). CI 검사기(`statecharts_tlc.py --jobs 4`)로 59/59 통과, 163.5초. `-`는 깨진 규칙이 없다는 뜻이다.

### S3a 범위와 비회귀

| 설정 | 범위 | 상태 수 | 깨진 불변식 | 깨진 liveness |
|---|---|---|---|---|
| `S2` | 전체 | 3,046 | IdleDoesNotExpand, SkipOnlyWhenTerminal | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles |
| `S3a` | S3a 범위: `Starts = fresh`, `Switches = FALSE`, `Approvals = TRUE` | 1,496 | IdleDoesNotExpand | - |
| `S3aAll` | 전체 | 4,592 | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles |
| `Design` | 전체, S3a+S3b | 6,090 | - | - |

- S3a 범위에서 `SameDevBarrierStillClears`·`SameDevKilledStillClears`·`ChannelEventuallyReady`를 포함한 liveness 5개가 모두 통과한다.
- S3a의 `IdleDoesNotExpand`는 S2의 N2 idle 정리를 남겨 둔 것이다. `IdleNotExpandedCrossDev`는 통과한다. `S3aCrossIdle` 변형이 장치가 바뀐 경우까지 허용하면 그 규칙이 깨진다.
- `S3aAll`에서 깨지는 규칙은 모두 `S2`에서도 깨진다.

"전체"는 `scen` 7가지 × `dev` 2가지 × `file` 4가지 × `n1` 2가지 × `Starts` 2가지 × `Switches` 2가지 × `Approvals` 2가지에서 모순되는 조합을 뺀 것이다.

### P1 경로

| 경로 | S2 | D3 | S3a | Design |
|---|---|---|---|---|
| NoN2 (dev 변경, N1 중간 종료, 승인 없음) | Channel, Eligible | Channel, Eligible | Channel, Eligible | - |
| Halted (파일 바뀜, writer 정지) | Boot, Channel | Channel | - | - |
| Superseded (설치 전 이미 N2) | IdleDoesNotExpand · Boot, Channel, Eligible | Channel, Eligible, SameDevBarrier, SameDevKilled | IdleDoesNotExpand · Channel, Eligible | - |
| Switched (앞 source의 ticket) | Boot, Channel | Channel | Channel | - |
| RawCloser (개행 없는 closer) | Channel, Eligible | NoLostAnswer · Eligible | - | - |

(`·` 앞은 불변식, 뒤는 liveness. 이름은 줄였다.) S3a가 깨는 규칙은 같은 행의 S2도 깬다.

### 수정 전과 후

"전"은 design-3 모델(설정 25개, `s3d4-expected-before.tsv`로 보관)의 기대값이고, "후"는 design-4 모델이다. 모델을 다시 써서 설정 파일은 모두 바뀌었다. 옛 `Design`은 D3 규칙이었고 새 모델에서는 `D3`라는 이름으로 남았다. 옛 모델에 없던 설정은 "(없음)"이다.

| 설정 | 전: 깨진 불변식 | 전: 깨진 liveness | 후: 깨진 불변식 | 후: 깨진 liveness | 후 상태 수 |
|---|---|---|---|---|---|
| `AnyTarget` | ExactTarget | - | ExactTarget | - | 6,090 |
| `BarrierD2` | - | ChannelEventuallyReady, SameDevBarrierStillClears | - | ChannelEventuallyReady, SameDevBarrierStillClears | 7 |
| `BarrierD3` | (없음) | (없음) | - | - | 29 |
| `BarrierDesign` | - | - | - | - | 29 |
| `BarrierS2` | IdleDoesNotExpand | ChannelEventuallyReady | IdleDoesNotExpand | ChannelEventuallyReady | 11 |
| `BarrierS3a` | (없음) | (없음) | IdleDoesNotExpand | - | 37 |
| `D2` | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 2,142 |
| `D3` | (없음) | (없음) | NoLostAnswer | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 5,362 |
| `Design` | - | - | - | - | 6,090 |
| `DropEvidence` | NoLostAnswer | EligibleEventuallySettles | NoLostAnswer | EligibleEventuallySettles | 6,220 |
| `EndWithoutProof` | EndProofOwned, NoLostAnswer | EligibleEventuallySettles | EndProofOwned, NoLostAnswer | EligibleEventuallySettles | 6,226 |
| `HaltedD3` | (없음) | (없음) | - | ChannelEventuallyReady | 108 |
| `HaltedDesign` | (없음) | (없음) | - | - | 108 |
| `HaltedS2` | (없음) | (없음) | - | BootEventuallyReady, ChannelEventuallyReady | 58 |
| `HaltedS3a` | (없음) | (없음) | - | - | 108 |
| `IdleBorrow` | IdleDoesNotExpand | - | IdleDoesNotExpand, IdleNotExpandedCrossDev | - | 6,358 |
| `KilledD2` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 7 |
| `KilledD3` | (없음) | (없음) | - | - | 22 |
| `KilledDesign` | - | - | - | - | 22 |
| `KilledS2` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 7 |
| `KilledS3a` | (없음) | (없음) | - | - | 22 |
| `LatestSourceOnly` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 5,584 |
| `NoApproval` | (없음) | (없음) | RejectUnverifiedFile | - | 6,240 |
| `NoBootProver` | - | ChannelEventuallyReady | - | ChannelEventuallyReady, EligibleEventuallySettles | 5,588 |
| `NoEndCut` | NoLostAnswer | EligibleEventuallySettles | NoLostAnswer | EligibleEventuallySettles | 6,158 |
| `NoEndProof` | ReleaseNeedsEnd | - | ReleaseNeedsEnd | - | 6,572 |
| `NoFinalCapture` | NoLostAnswer | EligibleEventuallySettles | NoLostAnswer | EligibleEventuallySettles | 6,204 |
| `NoHistRaw` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles | 6,006 |
| `NoLaunchRecord` | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears | - | ChannelEventuallyReady, EligibleEventuallySettles | 8,074 |
| `NoN2D3` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles | 40 |
| `NoN2Design` | (없음) | (없음) | - | - | 64 |
| `NoN2S2` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles | 14 |
| `NoN2S3a` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles | 40 |
| `NoPrefixCheck` | RejectUnverifiedFile | - | RejectUnverifiedFile | - | 6,594 |
| `NoRowCAS` | SuccessorUntouched | - | SuccessorUntouched | - | 6,090 |
| `NoSupersededProof` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 5,042 |
| `PendingForever` | - | ChannelEventuallyReady | - | ChannelEventuallyReady | 4,970 |
| `RawCloserD3` | (없음) | (없음) | NoLostAnswer | EligibleEventuallySettles | 89 |
| `RawCloserDesign` | (없음) | (없음) | - | - | 117 |
| `RawCloserS2` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles | 35 |
| `RawCloserS3a` | (없음) | (없음) | - | - | 117 |
| `ReaperExact` | - | BootEventuallyReady, ChannelEventuallyReady | - | BootEventuallyReady, ChannelEventuallyReady | 5,538 |
| `S2` | IdleDoesNotExpand, SkipOnlyWhenTerminal | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | IdleDoesNotExpand, SkipOnlyWhenTerminal | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | 3,046 |
| `S3a` | (없음) | (없음) | IdleDoesNotExpand | - | 1,496 |
| `S3aAll` | (없음) | (없음) | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | 4,592 |
| `S3aCrossIdle` | (없음) | (없음) | IdleDoesNotExpand, IdleNotExpandedCrossDev | ChannelEventuallyReady, EligibleEventuallySettles | 4,620 |
| `SkipTransient` | SkipOnlyWhenTerminal | - | SkipOnlyWhenTerminal | - | 6,546 |
| `SuffixScope` | NoSuffixAttribution | - | NoSuffixAttribution, RejectUnverifiedFile | - | 6,216 |
| `SupersededD3` | (없음) | (없음) | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears | 54 |
| `SupersededDesign` | (없음) | (없음) | - | - | 126 |
| `SupersededS2` | (없음) | (없음) | IdleDoesNotExpand | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles | 44 |
| `SupersededS3a` | (없음) | (없음) | IdleDoesNotExpand | ChannelEventuallyReady, EligibleEventuallySettles | 48 |
| `SwitchedD3` | (없음) | (없음) | - | ChannelEventuallyReady | 30 |
| `SwitchedDesign` | (없음) | (없음) | - | - | 51 |
| `SwitchedS2` | (없음) | (없음) | - | BootEventuallyReady, ChannelEventuallyReady | 8 |
| `SwitchedS3a` | (없음) | (없음) | - | ChannelEventuallyReady | 30 |
| `VoidCompletion` | CompletionBeforeCleanup | - | CompletionBeforeCleanup | - | 7,212 |
| `WriterRetry` | (없음) | (없음) | - | ChannelEventuallyReady | 6,090 |

차트: 턴 중간 종료 [S2](hook-transcript-custody-killed-before.chart.mmd) · [S3a](hook-transcript-custody-killed-after.chart.mmd), 저장 barrier [S2](hook-transcript-custody-barrier-before.chart.mmd) · [S3a](hook-transcript-custody-barrier-after.chart.mmd), 개행 없는 closer [D3](hook-transcript-custody-rawcloser-before.chart.mmd) · [S3a](hook-transcript-custody-rawcloser-after.chart.mmd), writer 정지 [D3](hook-transcript-custody-halted-before.chart.mmd) · [S3a](hook-transcript-custody-halted-after.chart.mmd).
전체 설정은 상태가 500개를 넘어 차트가 없다. 규칙 검사는 전부 했다.

### 변형

변형은 `Full`에 guard 하나를 끈 것이고(`S3aCrossIdle`만 S3a), 범위는 전체다. 모두 기대한 규칙에서 깨진다.

| 변형 | 끄는 것 | 깨진 규칙 |
|---|---|---|
| `NoBootProver` | 살아 있는 runtime의 종료 확인(launch 때만 기록) | ChannelEventuallyReady, EligibleEventuallySettles |
| `NoLaunchRecord` | launch 전 종료 기록 | ChannelEventuallyReady, EligibleEventuallySettles |
| `EndWithoutProof` | 종료 확인의 host 판독 | EndProofOwned, NoLostAnswer, EligibleEventuallySettles |
| `NoEndProof` | 정산의 종료 기록 요구 | ReleaseNeedsEnd |
| `NoFinalCapture` | 범위 확정 대기 | NoLostAnswer, EligibleEventuallySettles |
| `NoEndCut` | 종료 길이까지 범위 연장 | NoLostAnswer, EligibleEventuallySettles |
| `DropEvidence` | "범위 안에 증거 없음" 조건 | NoLostAnswer, EligibleEventuallySettles |
| `IdleBorrow` | idle의 target 일치 | IdleDoesNotExpand, IdleNotExpandedCrossDev |
| `SuffixScope` | scoped 입력 | NoSuffixAttribution, RejectUnverifiedFile |
| `NoPrefixCheck` | prefix 인증 | RejectUnverifiedFile |
| `NoApproval` | 파일 승인(B) 요구 | RejectUnverifiedFile |
| `PendingForever` | 확정 실패에서 completion 종료 | ChannelEventuallyReady |
| `SkipTransient` | 확정·일시 실패 구분 | SkipOnlyWhenTerminal |
| `VoidCompletion` | completion 반환값 gate | CompletionBeforeCleanup |
| `NoRowCAS` | 원본 row CAS | SuccessorUntouched |
| `AnyTarget` | 종료 기록의 target 비교 | ExactTarget |
| `ReaperExact` | 두 번째 boot gate의 구조 검증 | BootEventuallyReady, ChannelEventuallyReady |
| `WriterRetry` | boot debt 구동의 writer 분리(P1-2) | ChannelEventuallyReady |
| `NoHistRaw` | 역사적 raw EOF(P1-5) | ChannelEventuallyReady, EligibleEventuallySettles |
| `NoEndRefresh` | N2 없는 B 생산(P1-1) | ChannelEventuallyReady, EligibleEventuallySettles |
| `NoSupersededProof` | 후속 실행 시작의 종료 증명(P1-3) | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears |
| `LatestSourceOnly` | 종료 증명의 source 확장(P1-4) | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears, SameDevKilledStillClears |
| `S3aCrossIdle` | S3a의 N2 idle 정리를 장치 변경까지 허용 | IdleDoesNotExpand, IdleNotExpandedCrossDev, ChannelEventuallyReady, EligibleEventuallySettles |

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
| S2·D2 · ChannelEventuallyReady | 턴 중간 종료 → N2가 떠도, 입력이 와도, idle이 돼도 그대로(장치 변경) | 확인. gate는 모든 boot input의 settled(`native_prepare.rs:355-360`) |

## 모델이 다루지 않는 것

- hash·byte 변조, FD와 path의 경합, Registry fingerprint, launch와 marker 게시의 실제 순서.
- Herdr와 tmux의 차이, `Retired` 근거별 강도. 모델의 "host가 N1 종료를 확인"은 근거별 판독을 하나로 본 것이다.
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

설정 59개의 기대값 대조는 저장소 검사기로 한다.

```sh
python3 scripts/ci/statecharts_tlc.py --base 7c91beaebf --head HEAD --jobs 4
```
