이 WriterWake는 historical 여부를 이미 올바르게 판정된 입력으로 추상화한다. 최초 link seq와 실제 current를 비교하는 구현의 안전성은 [WriterRebound](rebound.report.md)가 별도로 검사한다. superseded 입력은 델타 계약의 `Current::Known(c) && c != source`를 뜻하며, link birth seq 자체가 historical 증거라는 뜻이 아니다.

구현 소스 참조: `2b07eed6f3a2a4ce9292456b1483d1fd9e5ef170` (main `a2ab3f26df7059ecf13d9ae9f7fcffa418560089` 위 구현). 최종 검증 결과는 구현 레인 보고서를 따른다.


# WriterWake — 소실 원본 park와 입력 재개

코드 기준 SHA: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. `ParkRule=FALSE`는 이 SHA의 missing resume 실패를 모델링하며, `TRUE`는 #7845 park-and-run 계약을 적용한다. 조사 모델 `WriterWake`를 옮겨 입력 승인을 새 세션 생성의 선행 조건으로 고쳤다. 이 모델은 코드 실행 시험을 대신하지 않는다.

## 상태와 계약

- 초기 상태는 `retired=false`인 옛 cursor의 원본이 이미 사라지고 새 host가 resume을 시작한 시점이다. family 설정에는 EOF witness나 successor가 없다.
- `presence`는 absent/present/unknown, `loss`는 typed Missing/replaced/permission denied/shrunk를 나타낸다. probe 실패는 Unknown으로 접는다.
- `drain`은 아직 캡처해야 할 tail, `unsealed`는 아직 완결되지 않은 derivation이다. 이 두 상태와 기존 spool의 **게시 의무**는 다르다. `oldDebt=1`인 retained spool은 park 뒤에도 replay된 채 남고, 실행 중인 actor의 `DeliverRetainedSpool`이 전달한다. `RetainedSpoolDelivered`가 이 진행을 확인한다.
- latest source는 typed Missing, drain 없음, producer absent, 전체 replay 뒤 unsealed 없음일 때만 park한다. historical source는 Current로 확인되면 producer presence와 무관하게 park하며 drain은 없어야 한다. reopen의 NotFound와 typed Missing의 일치 여부는 `loss="missing"` 한 값으로 접었다.
- park는 메모리 플래그이며 actor는 running/ready다. cursor retired·spool·GC를 변경하는 전이는 모델에 없으며, 실제 바이트 보존은 T1/M4/M9가 확인한다.
- 입력은 `waiting → held → accepted → launched → done` 순서다. `AdmitInput`은 running에서만 가능하다. 새 세션과 새 원본은 승인 후 첫 prompt를 처리한 뒤에만 생성된다. Startup/Owed{0}와 durable Posted의 세부 저장은 기존 구현 계약으로 추상화한다.
- `Restart`는 한 번만 허용한다. 메모리 park/readiness를 리셋하고 durable successor/new source/Posted/옛 spool 의무는 유지한다. 재시작 전·후 어느 시점에도 old missing cursor를 다시 park할 수 있어야 한다.
- stale halt가 있는 초기 상태와 없는 초기 상태를 모두 탐색한다. halt clear는 성공한 `ResumePark`와 같은 전이에서만 일어난다. 비재시도 halt에는 clear 전이가 없다.

## 진입점 전수표

아래 줄 번호는 모두 기준 SHA의 값이다. gateway만 뜬다는 가정 없이 실제 시작·주기 작업 호출부를 확인했다. 호출부별 lease/owner 권한은 host에 들어오기 전 이미 승인된 전제로 두며, 각 경로가 같은 resume 판단으로 수렴하는지를 모델링한다.

| 실제 진입점 | 기준 코드 좌표 | 모델 값/행동 |
|---|---|---|
| 최초 gateway | `src/services/discord/runtime_bootstrap.rs:423` → `o_writer_host.rs:416` → `host.rs:411` | Init → ResumePark/ResumeHalt |
| REST execution worker | `src/services/discord/runtime_bootstrap.rs:339` → `o_writer_host.rs:639` | Init → ResumePark/ResumeHalt |
| confirmed standby execution | `src/services/discord/runtime_bootstrap.rs:394` → `o_writer_host.rs:639` | Init → ResumePark/ResumeHalt |
| owner_cleanup gateway 재호스팅 | `src/services/discord/runtime_bootstrap/owner_cleanup.rs:87` → `o_writer_host.rs:662` | HostEnds → PeriodicBoot → ResumePark/ResumeHalt |
| owner_cleanup execution 재호스팅 | `src/services/discord/runtime_bootstrap/owner_cleanup.rs:89` → `o_writer_host.rs:639` | HostEnds → PeriodicBoot → ResumePark/ResumeHalt |
| 공통 host/actor 진입 | `src/services/tui_o/writer/host.rs:524`, `:765`, `:824`, `:854` → `actor.rs:340` | ResumePark/ResumeHalt |
| process 재시작 | 위 최초 진입점 + store restore `rotation/readers.rs:17` | Restart 후 동일 resume |
| 비재시도 halt 종료 | `src/services/tui_o/writer/host.rs:801` | ResumeHalt → HostEnds |
| 입력 readiness | `src/services/tui_o/cutover/intake_route.rs:126`, `:140`; `writer/actor.rs:357` | held → AdmitInput |
| fresh 세션·Startup | `src/services/claude/tui_session_launch/prepared.rs:97`, `:118`; `tui_prompt_dedupe/binding_events.rs:969` | NewSessionSource |
| retained spool replay | `src/services/tui_o/writer/rotation/readers.rs:85`, `:212` | oldDebt 보존 → DeliverRetainedSpool |

## 규칙과 결과

7개 safety 규칙과 NoDeadlock을 검사한다. `ParkOnlySafe`는 잘못된 park 금지, `ActiveLossVisible`은 실제 capture/derivation 또는 active loss halt 보존, `ReadyHasNoStaleHalt`는 성공 resume 후 stale halt 해소, `FirstReplyOnce`/`FirstReplyNeedsAdmission`은 입력 승인 뒤 응답 1회, `NoQuietReboot`/`ParkedKeepsHost`는 세션 없는 반복 host 생성 금지다.

| cfg | 상태 / step | safety 결과 | NewSourceGetsFirstReply | WaitingInputGetsFirstReply | RetainedSpoolDelivered |
|---|---:|---|---|---|---|
| WriterWakeMain | 14 / 35 | RED: NoQuietReboot | GREEN, 새 source가 열리지 않아 vacuous | RED, 7-step 무한 반복 | GREEN, running에 도달하지 않아 vacuous |
| WriterWakePark | 34 / 92 | GREEN | GREEN | GREEN | GREEN |
| WriterWakeSupersededMain | 7 / 14 | GREEN | RED, 6-step 무한 반복 | GREEN, 초기 입력이 이미 launched여서 vacuous | GREEN, running에 도달하지 않아 vacuous |
| WriterWakeSupersededPark | 14 / 36 | GREEN | GREEN | GREEN, 초기 입력이 이미 launched여서 vacuous | GREEN |
| WriterWakeActiveLoss | 28 / 70 | GREEN: Present/Unknown은 halt 유지 | 미검사 | 미검사 | 미검사 |
| WriterWakeUnsafeLoss | 244 / 617 | GREEN: Replaced/denied/Shrunk, drain, latest unsealed은 halt 유지 | 미검사 | 미검사 | 미검사 |

수정 전/후 liveness의 유효한 비교는 family의 `WaitingInputGetsFirstReply` RED→GREEN, 새 source가 이미 존재하는 superseded의 `NewSourceGetsFirstReply` RED→GREEN이다. negative control은 의도적으로 영구 halt를 허용하므로 liveness를 요구하지 않는다. Main에서 vacuous인 속성을 회복 증거로 세지 않는다.

TLC CI checker는 새 6개 cfg를 실제 실행해 expected.tsv와 일치했다: **6/6, 총 15.1초**, 가장 긴 cfg 4.1초. 300초/cfg 예산 안이다. statechart 실행은 Main/SupersededMain exit=1이 예상 결과이며 나머지는 exit=0이다.

## 반례와 코드 대조

1. `NoQuietReboot`: **0 resume → 1 missing halt → 2 host 종료 → 3 owner_cleanup 재부팅**. 기준 `rotation/readers.rs:109-122`는 unretired legacy missing을 halt로 반환한다. `host.rs:801-806`이 host를 끝내고 `owner_cleanup.rs:87/89`가 재호스팅하므로 코드로 확인된다. T1/T1b가 다섯 번 refresh 후 generation 1을 검사한다.
2. `WaitingInputGetsFirstReply`: 위 종료 상태에서 **3 입력 도착 → 4 재부팅 → 5 missing halt → 6 종료 → 7 재부팅**이 반복된다. `intake_route.rs:126-145`의 unchanged gate가 ready가 아닌 채널을 Hold하므로 코드로 확인된다. T2가 실제 placement/claimed → fresh launch → startup bind → Discord POST를 검사한다.
3. `NewSourceGetsFirstReply`: durable 새 source/successor가 있어도 **0 resume → 1 옛 missing halt → 2 종료 → 3 재부팅 → 4 halt → 5 종료 → 6 재부팅**이다. `rotation/readers.rs:58-63`는 retained cursor부터 열고 `:109-122`에서 실패하므로 새 event를 follow하기 전에 끝난다. T3/T5가 superseded park와 재시작 뒤 POST 추가 0을 검사한다.

코드 대조는 baseline의 실패 경로를 확인한 것이며, 구현의 실제 RED/GREEN 및 뮤턴트 결과는 레인 구현 보고서의 시험 증거가 담당한다.

## 범위와 가정

- 기존 spool 게시 성공에는 공정성 가정이 있다. Discord/lease가 영원히 실패하지 않고, enabled action은 결국 실행된다고 둔다. 재시도 횟수·HTTP backoff·receipt 쓰기는 모델 밖이며 T2의 실제 mock POST/Posted 단언이 확인한다.
- 첫 bind의 Startup 원인은 모델의 명시적 가정이다. Unknown을 Startup으로 승격하는 행동은 넣지 않았다. 실제 binding cause 경로는 T2/M8로 검증해야 한다.
- EOF witness/ReaderConfig cleanup은 별도 bool로 만들지 않았다. 모든 family case가 witness 없이 park 가능해야 하며 증명된 cleanup-retire는 기존 경로다. 두 구현 갈래의 우선순위는 T7이 확인한다.
- presence superseded 표본과 최대 5초 재표본은 unknown에 접지 않았다. 최종 publication 결과만 모델링하며 첫 host 안 재표본 수렴은 T8이 확인한다.
- runtime에서 살아 있던 FD의 tail 회수는 기존 문제이며 이번 범위 밖이다. 모델은 원본 소실 뒤 새 resume부터 시작한다. Present/Unknown의 반복 호스팅 억제도 이번 수리 범위 밖이며 negative control이 그대로 허용한다.
- cursor/spool 바이트·GC·source 재등장 prefix 검증은 모델 밖이다. 이 변화의 보존 의무와 직접 관계가 있으므로 T1/T4e/M4/M9의 코드 시험이 필수다.
- one process restart와 one input/source만 유계로 탐색한다. 세션이 무한히 교체되는 상태 공간을 증명하지 않는다. 중복 bind와 refresh는 상태를 바꾸지 않는 notification으로 접고 durable Posted 가드가 두 번째 게시를 막는다.
- 차트는 oldDebt 전달, 같은 화면 상태 안의 입력 도착, stutter/중복 notification을 일부 생략한다. 모델과 TLC liveness에는 모두 남아 있다. park 차트는 12 states/20 arrows이며 14개의 같은 label step과 38개의 stutter/중복 step을 그리지 않는다.

## 산출물과 재실행

각 cfg와 같은 이름의 결과는 다음 Mermaid/report 쌍이다: `writer-main`, `writer-park`, `writer-superseded-main`, `writer-superseded-park`, `writer-active-loss`, `writer-unsafe-loss`. 원시 TLC 로그는 구현 레인의 `sc-out-impl/{main,park,superseded-main,superseded-park,active-loss,unsafe-loss}-complete/`와 `ci.complete.run.log`에 보존한다.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
  docs/statecharts/tui-o-writer/WriterWake.tla \
  --config docs/statecharts/tui-o-writer/WriterWakePark.cfg \
  --view docs/statecharts/tui-o-writer/writer.view.mjs \
  --out /tmp/i7845-writer-park-new
python3 scripts/ci/statecharts_tlc.py
```

`--out`은 존재하지 않는 새 폴더를 쓴다. 다른 cfg도 같은 명령의 config/output만 바꿔 재실행한다. `expected.tsv`에는 Main의 두 위반과 SupersededMain의 liveness 위반을 예상 위반으로 등록했다.
