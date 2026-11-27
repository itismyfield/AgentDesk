# 재시작 뒤 원본 ticket 정산 — 모델 보고서

기준 코드 `bd0c385be24ca25a10725127dcd77eba1274d4fa`. 설계는 [design-3](hook-transcript-custody.design.md)이다.
`HookTranscriptRefresh.tla`(source refresh와 첫 pin)는 그대로 두고, 그 뒤 단계인 "재시작 뒤 boot input이 된 원본 ticket이 어떻게 끝나는가"를 `HookTranscriptCustody.tla`로 따로 모델링했다.

## 무엇을 모델링했나

옛 실행 N1의 keyed ticket 하나가 재시작 뒤 boot input으로 남아 있고, 채널의 boot gate는 그 ticket이 settled가 될 때까지 닫혀 있다.

- `Rules`: `S2`는 기준 코드, `D2`는 design-2 본문, `D3`는 design-3.
- `Mut`: D3의 guard 하나를 끈다. 정상은 `none`.
- `scen`: N1이 재시작 전에 남긴 것. 완전한 증거(`complete`), 마지막 capture 뒤의 closer(`closerLate`), 끝나지 않은 턴(`killed`), 저장 barrier(`barrierStored`), 저장 결과(`outcomeSaved`·`outcomeLegacy`).
- `dev`, `file`: 장치 번호가 같은지, transcript가 그대로인지(`ok`), 잠시 없는지(`late`), 사라졌는지(`gone`), 바뀌었는지(`changed`).
- `n1`: 앱만 재시작했으면 살아 있을 수 있다. 장치가 바뀌었거나 파일이 온전하지 않으면 죽은 것만 본다.

환경 동작 가운데 **후속 N2의 launch, N2의 입력, N1의 죽음에는 공정성을 주지 않았다.** 채널 gate가 닫혀 있으면 새 턴 시작이 기다리므로 N2가 뜬다고 가정할 수 없기 때문이다.
약한 공정성은 writer capture, host의 종료 판독, 정산·정리 단계, N2가 뜬 뒤의 idle, 살아 있는 N1의 closer, 잠시 없던 파일의 복귀에만 준다.

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| IdleDoesNotExpand | 불변식 | 결과 없이 행이 정리되는 것은 N1 자신의 idle 또는 N1의 종료로만 |
| NoSuffixAttribution | 불변식 | Executed는 N1이 직접 쓴 closer로만 |
| RejectUnverifiedFile | 불변식 | 새 결과는 prefix가 인증된 transcript에서만 |
| EndProofOwned | 불변식 | 종료 기록은 실제로 끝난 실행만 가리킨다 |
| ReleaseNeedsEnd | 불변식 | N1의 종료로 한 정리에는 종료 기록이 있다 |
| NoLostAnswer | 불변식 | closer를 읽을 수 있는 턴은 결과 없이 정산되지 않는다 |
| CompletionBeforeCleanup | 불변식 | 저장된 Executed의 행은 completion 단계가 끝난 뒤에만 정리된다 |
| SkipOnlyWhenTerminal | 불변식 | completion 기록을 포기하는 것은 파일이 사라졌거나 바뀌었거나, seal 없는 결과가 장치 변경을 만났을 때뿐 |
| SuccessorUntouched | 불변식 | N1 정리가 N2의 행을 지우지 않는다 |
| ExactTarget | 불변식 | 다른 target의 ticket은 정산되지 않는다 |
| OneOutcome | 불변식 | 새 outcome 기록은 많아야 한 번 |
| SavedOutcomePreserved | 불변식 | 저장된 결과는 바뀌지 않는다 |
| BootEventuallyReady | liveness | runtime이 뜬다 |
| ChannelEventuallyReady | liveness | 채널의 boot gate가 결국 열린다 |
| EligibleEventuallySettles | liveness | N1이 닫은 턴은 transcript를 읽을 수 있으면 Executed로 정산된다 |
| SameDevBarrierStillClears | liveness | 같은 장치의 저장 barrier는 후속이 idle이면 지금처럼 정리된다 |

design-2 모델의 `OwnIdleProgresses`는 뺐다. N1이 죽은 운영 경로에서는 전제가 거짓이라 아무것도 검사하지 않는다. 그 자리를 `ChannelEventuallyReady`와 `SameDevBarrierStillClears`가 맡는다.

## 결과

TLC 1.7.4, 설정 25개, 전체 실행 약 1분 40초. `-`는 깨진 규칙이 없다는 뜻이다.

### 수정 전과 후

| 설정 | 범위 | 상태 수 | 깨진 불변식 | 깨진 liveness |
|---|---|---|---|---|
| `S2` | 전체 | 752 | IdleDoesNotExpand, SkipOnlyWhenTerminal | BootEventuallyReady, ChannelEventuallyReady, EligibleEventuallySettles |
| `D2` | 전체 | 539 | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears |
| `Design` | 전체 | 1,093 | - | - |
| `KilledS2` | 장치 변경, 턴 중간 종료 | 7 | - | ChannelEventuallyReady |
| `KilledD2` | 같음 | 7 | - | ChannelEventuallyReady |
| `KilledDesign` | 같음 | 22 | - | - |
| `BarrierS2` | 같은 장치, 저장 barrier | 11 | IdleDoesNotExpand | ChannelEventuallyReady |
| `BarrierD2` | 같음 | 7 | - | ChannelEventuallyReady, SameDevBarrierStillClears |
| `BarrierDesign` | 같음 | 29 | - | - |

"전체"는 `scen` 6가지 × `dev` 2가지 × `file` 4가지 × `n1` 2가지다.

차트: 턴 중간 종료 [전](hook-transcript-custody-killed-before.chart.mmd) · [후](hook-transcript-custody-killed-after.chart.mmd), 저장 barrier [전](hook-transcript-custody-barrier-before.chart.mmd) · [후](hook-transcript-custody-barrier-after.chart.mmd).
전체 설정은 상태가 500개를 넘어 차트가 없다. 규칙 검사는 전부 했다.

### 변형

모두 D3에 guard 하나를 끈 것이고 범위는 전체다.

| 변형 | 끄는 것 | 깨진 불변식 | 깨진 liveness |
|---|---|---|---|
| `NoBootProver` | 살아 있는 runtime의 종료 확인(launch 때만 기록) | - | ChannelEventuallyReady |
| `NoLaunchRecord` | launch 전 종료 기록 | - | ChannelEventuallyReady, EligibleEventuallySettles, SameDevBarrierStillClears |
| `EndWithoutProof` | 종료 확인의 host 판독 | EndProofOwned, NoLostAnswer | EligibleEventuallySettles |
| `NoEndProof` | 정산의 종료 기록 요구 | ReleaseNeedsEnd | - |
| `NoFinalCapture` | 원본 범위 확정 대기 | NoLostAnswer | EligibleEventuallySettles |
| `NoEndCut` | 종료 길이까지 원본 범위 연장 | NoLostAnswer | EligibleEventuallySettles |
| `DropEvidence` | "범위 안에 증거 없음" 조건 | NoLostAnswer | EligibleEventuallySettles |
| `IdleBorrow` | idle의 target 일치 | IdleDoesNotExpand | - |
| `SuffixScope` | scoped 입력 | NoSuffixAttribution | - |
| `NoPrefixCheck` | prefix 인증 | RejectUnverifiedFile | - |
| `PendingForever` | 확정 실패에서 completion 종료 | - | ChannelEventuallyReady |
| `SkipTransient` | 확정·일시 실패 구분 | SkipOnlyWhenTerminal | - |
| `VoidCompletion` | completion 반환값 gate | CompletionBeforeCleanup | - |
| `NoRowCAS` | 원본 row CAS | SuccessorUntouched | - |
| `AnyTarget` | 종료 기록의 target 비교 | ExactTarget | - |
| `ReaperExact` | 두 번째 boot gate의 구조 검증 | - | BootEventuallyReady, ChannelEventuallyReady |

`NoEndCut`은 리뷰가 제안한 최소 수리(원본 범위를 N2 시작에서 얼림)와 같다. gate는 열리지만 closer가 마지막 capture 뒤에 있고 N2가 먼저 뜨면 그 턴을 결과 없이 정산한다. design-3은 종료 길이를 같이 기록해 이 경우도 Executed로 정산한다.

## 반례와 코드

코드 확인은 기준 SHA의 소스를 읽은 것이다. 설계 단계라 실행으로 재현하지 않았다. 재현은 설계 §6.3의 제품 RED가 맡는다.

| 설정·규칙 | 반례 경로 | 코드 |
|---|---|---|
| S2 · IdleDoesNotExpand | 저장 barrier → N2가 뜸 → N2가 idle → N2의 idle로 정리 | 확인. idle 통지는 현재 binding의 target으로 나가고(`tui_o/writer/rotation/owe.rs:728-767`), 최신 Bound가 origin인지만 본 뒤(`discord/turn_finalizer/native_supply.rs:403-415`) barrier가 있고 outcome이 없는 boot input을 target 비교 없이 모두 돈다(`:444-453`). 정리는 `native_restore.rs:122-201` |
| S2 · BootEventuallyReady | 장치 변경 + 저장 barrier → 부팅 거절 | 확인. `boot_reaper.rs:83-92` → `boot_custody.rs:435-443` → `history/v2/native.rs:229-275`(`:266`) → `:158-176` → `native_barrier.rs:18-21,41-54` → `history/v2.rs:21-33`의 exact dev·inode 비교. 실패하면 `runtime_bootstrap/native_input.rs:42-57`이 `Refused` |
| S2 · SkipOnlyWhenTerminal | 저장 결과 + 파일 잠시 없음 → completion 기록 건너뜀 → 정리 | 확인. 읽기 오류는 종류와 무관하게 경고만 남기고(`native_completion.rs:72-79`), 반환값 없이 cleanup 메시지를 보낸다(`actor_state.rs:347-363`) |
| S2 · EligibleEventuallySettles | 장치 변경 + 완전한 증거 → 정산 안 됨 | 확인. 새 증거는 exact stat을 요구한다(`native_supply.rs:786`, `native_restore.rs:86-88`) |
| S2·D2 · ChannelEventuallyReady | 턴 중간 종료 → N2가 떠도, 입력이 와도, idle이 돼도 그대로 | 확인. gate는 모든 boot input의 settled(`native_prepare.rs:355-360`), settled가 되는 길은 Settle과 Abstain뿐(`native_restore.rs:493-509,528-536`). D2는 design-2 §2.4·§3.3이 suffix barrier와 N2 idle admission을 닫는다 |
| S2 · ChannelEventuallyReady (N2 없음) | 저장 barrier → 아무 일도 없음 | 확인. gate가 닫혀 있으면 턴 시작이 기다린다(`router/message_handler/provider_dispatch/owner_input.rs:770-773`). N2가 뜰 다른 계기가 없으면 S2의 출구도 열리지 않는다 |
| D2 · EligibleEventuallySettles | closer가 마지막 capture 뒤 → N2가 먼저 뜸 → 범위 밖 | design-2 §2.1·§2.2 본문(원본 범위는 N1이 현재 실행일 때만 늘어남) |
| D2 · SameDevBarrierStillClears | 같은 장치 + 저장 barrier → N2 idle → 그대로 | design-2 §3.3 본문(N2 idle은 N1 admission을 만들지 않음). S2에서는 위 첫 행의 경로로 풀린다 |

## 모델이 다루지 않는 것

- hash·byte 변조, FD와 path의 경합, Registry fingerprint, launch와 marker 게시의 실제 순서.
- Herdr와 tmux의 차이. 모델의 "host가 N1 종료를 확인"은 두 host의 판독을 하나로 본 것이다.
- ticket 여러 개, 후속 실행 여러 개, 종료 기록 16건 상한.
- 살아 있는 actor가 있어야 하는 제어 경로(Cancel·Terminate·clear·purge·Stop).
- host 판독이 끝내 답하지 않는 경우. 모델은 답한다고 가정한다.

## 다시 돌리기

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/tui-o-writer
~/.claude/skills/statechart/scripts/statechart HookTranscriptCustody.tla \
  --config HookTranscriptCustodyDesign.cfg --view hook-transcript-custody.view.mjs --out /tmp/sc-custody
```
