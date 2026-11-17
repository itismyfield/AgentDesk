# API Herdr 종료·출력 연결 — 설계 D3

기준 main `d40c06eb47a358373a740b8596144640a3b8ba63`, lane base `2c359680117efb2e898c50a26524cf570213b1c0`. 선택은 **G**, 기존 bound channel의 실제 Discord delivery presentation과 기존 O 본문 소유권이다. [설계 §2 N08–N14/M06 및 §4](../../api-thread-ingress.design.md)가 코드 좌표·값 범위의 정본이다. Rust 생산 변경은 이 커밋에 없다.

TIME start=2026-11-16T04:53:55+09:00 / end=2026-11-16T05:08:20+09:00 / elapsed=14m25s / budget=30m. census 고정부터 최종 모델 실행까지이며 초과0이다.

`ThreadIngress`/`ThreadIngressRace`의 first-key 모델은 그대로 유지한다. 그 모델의 추상 `HPost`를 실제 Herdr terminal 수락의 증거로 쓰지 않는다. 새 `ThreadIngressTerminal`은 **T1 admission과 first-key가 연결되어 이미 채택된 original 두 개**부터 시작하는 종료 동반 모델이다. main 공개 API가 이미 Herdr start를 허용한다는 뜻이 아니다. 현재 main gateway/terminal 계약을 이 지점에 붙이면 종료 liveness RED이며, 실제 HTTP 초안에서 발견한 반례와 같다.

## 관측과 전이

실제 HTTP 초안은 native paste+Enter1 뒤 `ClaudeTuiTerminalDone`을 받았지만, main의 direct=false 조건 때문에 actor AuthorityPinned→AuthorityLost로 반환했다. 원시 `http-after-v2.log:512,841` 및 구현 중단 보고서는 lane-runs/i8125에 보관한다. 별도 TLC trace `tlc-d3-before-trace.log`(exit13)는 queued→running→terminal→published[1]=1→rejected→Stuttering, token=1/inflight=1/두 번째 입력 queued를 실제 출력했다. 이 커밋으로 그 Rust 실행이 GREEN으로 바뀌었다고 주장하지 않는다.

모델의 `Start`→`NativeTerminal`은 기존 exact owner/driver가 한 번 제출하고 canonical native source에 terminal/body를 남기는 포트다. `Admit`는 direct 능력과 current actor를 요구하며, main headless는 rejected에서 멈춘다. O의 `Publish`는 admission과 독립적이므로 **답을 게시했어도 live actor 종료가 실패하는 경우**를 포함한다.

G는 실제 전달 수단을 제공하여 `Admit`를 통과한다. `RecordReceipt`는 terminal receipt의 성공 포트이며 Discord 게시 ACK가 아니다. O source custody가 남으면 O 게시 전에도 기존 bridge는 receipt→guarded `ClearInflight`→captured allocation `End`를 할 수 있다. O는 canonical native source와 자기 writer checkpoint에서 결국 답을 게시한다. `o_pending::capture`의 best-effort 성공을 durable ACK로 가정하지 않는다. 비 O bridge 제어군은 실제 게시 후에만 receipt/clear한다. T1 Herdr 수용 cfg는 O-owned이며 비 O cfg는 기존 전달 포트 비교용이다.

`WakeNext`는 기존 completion/deferred idle kickoff 포트다. machine인 두 번째 입력은 첫 original의 종료·정리 후에만 `Start`한다. `Restart`는 durable terminal admission 이후 한 번만 들어가 prior-process settlement를 투영한다. 새 process의 token 공백은 inflight 공백이 아니므로 다음 입력을 선점할 수 없다. `LateCleanup`은 이전 original의 늦은 정리/Busy probe가 다음 original의 token/inflight를 지우지 못하는 exact ownership 포트다.

## 검사하는 규칙과 공정성

- `ExactlyOneReply`와 `EachReply`: accepted original마다 논리 답 게시 ≤1 및 결국 1. status/progress 메시지와 긴 답의 분할 chunk를 별도 답으로 세지 않는다. HTTP fixture는 단일 최종 본문으로 실제 게시1을 확인한다.
- `NoSilentLoss`: original custody, 게시 결과, 또는 receipt가 있는 O canonical source custody가 남는다. 끝난 실행의 inflight만이 유일한 출력 보관소라고 가정하지 않는다.
- `NoResidueAfterEnd`: 해당 original이 done이면 그 token/inflight는 없고 terminal receipt는 있다. 다음 original의 token은 허용한다.
- `TerminalRequiresPresenter`: 기존 direct gate를 보존하며 실제 전달 수단 없는 headless actor에 종료 능력을 추가하지 않는다.
- `SourcePreserved`: iMessage 사람 original과 generic API machine original의 provenance가 그대로다. transport 선택은 사람 분류를 바꾸지 않는다.
- `LiveCustody`: running/terminal의 same-incarnation token·inflight를 늦은 이전 정리가 침범하지 못한다.
- `EachEnds`, `NextProgress`, `EventuallyDrained`: 두 채택 original이 결국 끝나고 다음 입력이 실행되며 마지막 token/inflight가 0이다.

공정성은 실제 native terminal 발생, 건강한 O writer/transport의 게시 성공, DB receipt/guarded clear, completion wake와 다음 owner start 포트에 대한 weak fairness다. 무한 네트워크 장애·writer 장애·DB 장애가 저절로 성공한다는 보장은 아니다. 그런 때 custody/visible hold를 유지하는 구현 시험이 필요하다. first-key 전 Busy·Idle 전환/driver 재검사, machine Busy 합류 금지와 effect 뒤 terminal 전 불명 crash는 기존 P2 모델/HTTP 수용의 담당이다. 이 동반 모델의 restart는 **이미 admitted된 terminal**에 한정한다.

## 수정 전/후 cfg 전수표

아래 11개 cfg.example 각각을 실제 scratch cfg로 materialize하고 공식 `scripts/ci/statecharts_tlc.py`로 실행했다. before는 같은 모델에서 Contract만 headless로 바꾸며 기대 header/manifest만 그 계약에 맞춘다. 본문/Mutation/BodyOwner/CanRestart는 동일하다. After는 커밋 source bytes 그대로다. `Before`와 `HeadlessAuthority`는 원래 headless이므로 양쪽 바이트가 동일한 제어군이다.

`L` = EachReply, EachEnds, NextProgress, EventuallyDrained 네 PROPERTY 위반. `-`는 모든 INVARIANT/PROPERTY 성립이다. suffix 앞의 공통 이름은 `ThreadIngressTerminal`, 뒤는 `.cfg.example`이다.

| suffix | main terminal 계약 before: actual broken | 선택 G/제어 after: actual broken |
|---|---|---|
| Before | L | L |
| G | L | - |
| GRestart | L | - |
| BridgeControl | L | - |
| DoublePublish | ExactlyOneReply + L | ExactlyOneReply |
| DropCustody | NoSilentLoss + L | NoSilentLoss |
| KeepInflight | L | NoResidueAfterEnd + L |
| NoWake | L | L |
| HeadlessAuthority | TerminalRequiresPresenter | TerminalRequiresPresenter |
| Reclassify | SourcePreserved + L | SourcePreserved |
| StaleCleanup | L | LiveCustody, EachEnds, EventuallyDrained |

HeadlessAuthority는 기존 direct 계약을 우회하는 변형이다. 선택 G의 capability 불변식을 깨며, 별도 계약을 설계하면 A가 항상 불가능하다는 증명은 아니다. DoublePublish의 after에서는 O가 독립적으로 종료/다음 입력을 진행하므로 liveness는 성립하지만 exactly-once가 깨진다. 안전성과 진행을 각각 검사한 이유다.

before 11/11 기대 RED. after 종료 cfg **3 GREEN + 8 기대 RED = 11**, 그중 현재 계약 제어1 + 원인 제거 mutant7이며 mutant7/7 RED. 기존 ingress/P2 39cfg도 함께 실행하여 합계 **25 GREEN + 25 기대 RED = 50cfg**다. 기대값 불일치·도구 오류·skip·미선택 0이다. 바뀐 cfg는 종료 11개뿐이며 기존 두 모델/39cfg는 변경0이다.

## 재현과 증거

lane raw logs는 `/home/adk/adk/lane-runs/i8125/logs/`에 보관한다.

- before: `check-d3-model.py <scratch> before`, `tlc-d3-before.log`, `model-d3-before/expected.tsv` 및 source-manifest.json.
- after 전체: `check-d3-all-models.py <scratch>`, `tlc-d3-verified-env.log`, `model-d3-verified-env/expected.tsv` 및 `model-d3-byte-compare.json`.
- 두 스크립트는 cfg header의 EXPECTED_SAFETY/EXPECTED_LIVENESS를 manifest로 변환한 다음 `python3 scripts/ci/statecharts_tlc.py --root <scratch> --jar <tla2tools.jar> --jobs 3`를 호출한다. source hash, scratch hash, actual 선택 cfg 이름을 기록한다.
- jar: `/home/adk/adk/lane-runs/i8119/logs/s1/statechart/tla2tools.jar`(1.7.4). `statechart_index.py`로 색인을 생성하고 `--check`를 실행한다.
- 탐색 중 v1/v2 로그는 기대값/모델 진단 기록이며 최종 PASS 증거가 아니다. 최종 검증은 verified 로그와 source byte 비교만 사용한다.

`.cfg.example`은 CI 자동 등록 파일이 아니다. expected.tsv를 저장소에서 바꾸거나 CI가 이미 선택했다고 주장하지 않는다. Rust 생산 mutant, 실제 HTTP G GREEN, same-GUID DB 재시작 및 O 게시/다음 입력 경합은 다음 구현 레그의 필수 수용이다. 모델 RED가 생산 mutant RED를 대신하지 않는다. 운영 iMessage 실행·운영 상태 변경은 0이다.
