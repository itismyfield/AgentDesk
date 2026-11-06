# WriterRebound — 현재 원본 판정과 park 안전성

구현 소스 참조: `2b07eed6f3a2a4ce9292456b1483d1fd9e5ef170` (main `a2ab3f26df7059ecf13d9ae9f7fcffa418560089` 위 구현). 최종 검증 결과는 구현 레인 보고서를 따른다.


코드 기준 SHA: `7a0c819e127964befd6bc0d022c5d4194d669c9c`. 구판은 이 base 위의 impl-1 birth 비교 작업 트리, 신판은 독립 설계 리뷰 `design-2-review.out.md`의 「구현 조각 계약 (델타)」를 모델링한다. 최종 Rust 코드의 검증 결과는 구현 레인 보고서에 기록하며 모델의 GREEN으로 대체하지 않는다.

외부 최소 반례 모델을 이 저장소 위치로 옮기고 6 invariant와 중단 경계를 추가했다. 모든 cfg는 같은 모델을 사용하며 `LogCurrent=FALSE`는 최초 link seq/outgoing successor, TRUE는 전체 log의 마지막 유효 Bound를 기준으로 한다.

## 결과

| cfg | 모델 상태 / step | 차트 상태 / 화살표 | 결과 |
|---|---:|---:|---|
| WriterReboundBirth | 207 / 219 | 20 / 23 | 아래 5규칙 RED, 보존·NoDeadlock GREEN |
| WriterReboundCurrent | 239 / 254 | 17 / 22 | 6규칙과 NoDeadlock 전부 GREEN |

구판의 예상 RED는 `ActiveLossVisible`, `CurrentNeverHistorical`, `UnknownNeedsAbsence`, `ReadyAfterFollow`, `ParkedReboundHalts`다. 첫 번째 원본 소실 반례를 그대로 유지했으며 다른 네 가지 경계도 검사한다. 신판은 모두 안전성 검사다. 응답 전달 liveness는 기존 WriterWake의 scope이며 이 모델에는 추가하지 않았다.


새 두 cfg와 기존 WriterWake 6 cfg를 포함한 **scoped CI checker 8/8 expected 일치, 총 17.8초**를 확인했다. 공식 checker `scripts/ci/statecharts_tlc.py --root <lane>/sc-out-impl-2/ci-complete-input`를 사용했으며 이 root에는 저장소의 해당 8 cfg/TLA와 expected.tsv 해당 행을 그대로 복사했다. 전체 저장소의 다른 statechart까지 실행한 결과로 주장하지 않는다. 원시 로그 `sc-out-impl-2/ci.complete.run.log`, 복사 원본 SHA 목록 `ci-complete-source-sha256.json`에 보존했다.

## 지킬 규칙

| invariant | 독자가 확인하는 규칙 |
|---|---|
| ActiveLossVisible | resume 판정·필요한 follow가 끝나 현재 원본이 Missing이고 producer가 Present/Unknown이면 그 원본은 park되지 않으며 halt와 alarm을 남기고 ready=false다 |
| CurrentNeverHistorical | 해당 resume이 읽은 마지막 유효 Bound target은 link birth·successor·rotation 복원 순서 때문에 historical이 되지 않는다 |
| UnknownNeedsAbsence | 로그를 확정할 수 없어 Unknown이면 park에 producer_absent=true가 필요하다 |
| ReadyAfterFollow | 지금 적용할 수 있는 유효 Bound가 미적용인 상태에서 ready를 내보내지 않는다. 새 source attach/reopen 실패는 ready 없이 halt한다 |
| ParkedReboundHalts | 판정 뒤 parked 원본으로 bind되거나 그 원본의 boundary refinement가 적용되면 running/ready로 남지 않는다 |
| ParkPreservesEvidence | park는 cursor의 retired·frontier·prefix hash와 retained spool 바이트를 바꾸지 않는다 |

판정 뒤 새 bind/refinement가 오면 이전 resume 관측은 더 이상 current 판정의 근거가 아니다. `decided/viewValid`를 내리고 C6의 halt 규칙으로 넘긴다. 이때 parked 메모리 bool을 억지로 지우는 전이는 없다. 다음 host에서 다시 reopen/판정한다.

## 상태와 동작

source는 A/B/C 세 개이고 초기 adopted A는 link가 없다. B/C의 birth seq와 outgoing successor는 실제 수명처럼 일반 rebind에서 새 순서로 바꾸지 않는다. log는 Bound의 순서와 Pending/Resolved 행을 가진 작은 유계 기록이며 source 이름은 이미 저장 이름으로 정규화된 기호다.

14개 시나리오 × producer Present/Unknown/Absent를 탐색한다: A→B→A, A→B→A→C, rotation 저장 뒤 cp 저장 전 중단, oldless 다른 pane의 cursor, 미적용 새 bind(원본 있음/없음), unresolved Pending suffix, 늦은 Resolved 뒤에도 마지막 Bound가 C인 기록, 읽기 실패, gap, checkpoint 없음, 유효 Bound 없음, parked 재bind, parked refinement. A→B→A→C는 A/B/C 각각의 소실을 따로 탐색한다.

`SaveRebindRotation`과 `CommitCheckpoint`는 분리되어 있다. `InterruptAfterRotation`은 B→A(seq3) rotation이 저장됐지만 cp=2인 채 resume을 시작한다. 그 상태의 full log는 A1/B2/A3이며 current는 여전히 A다. 이것이 cp prefix만 보거나 outgoing successor를 current 증거로 쓰면 안 되는 이유다.

새 current는 cursor가 없어도 된다. 새 B log만 durable하고 cp=1이면 A를 historical로 park한 뒤 `FollowBeforeReady`가 B를 attach/open한다. B Missing이면 alarm+halt만 내며 ready는 한 번도 내지 않는다. 기존 cursor를 같은 view 안에서 다시 apply하는 것은 무조건 새 reopen으로 바꾸지 않는다.

`resolveB` 행은 seq2 Pending Bound의 target을 B로 채울 뿐 독립적인 최신 bind가 아니다. 그래서 A1/Pending2/C3/Resolved(B)4의 current는 C다. unresolved Pending suffix가 있으면 뒤의 C를 선택하지 않고 Unknown이다.

**C4의 Pending 경계:** 기존 follow는 해결되지 않은 Pending에서 wait_resolution 후 Ok로 기다린다. 이 동작은 바꾸지 않는다. 여기서 '미적용 bind'는 follow가 지금 적용할 수 있는 유효 Bound를 말한다. unresolved Pending 뒤의 Bound는 아직 적용 가능하지 않으며 이 모델의 RequiredThrough도 그 장벽 앞에서 멈춘다. Pending 해소를 기다리는 동안의 binding-pending debt와 입력 보류는 모델 밖이다. 이를 없애거나 무조건 halt하는 새 정책을 넣지 않았다.

## 구판의 최단 반례와 코드 대조

- **ActiveLossVisible / CurrentNeverHistorical, 5단계:** 0 A 채택 → 1 B bind → 2 A 재bind rotation 저장 → 3 checkpoint 저장 → 4 A Missing → 5 현재 A park/ready. impl-1 `rotation.rs:628`은 기존 A의 link를 만들지 않고, `:681/:707`은 successor A 제거/B→A만 남긴다. impl-1의 `source_superseded` helper(구 행 953)의 birth 비교가 A0<B2를 historical로 오판한다. 실제 Rust `rebound_adopted_latest_missing_source_without_absence_stays_halted`는 `accepts=true halted=[]`로 RED였으며 원시 증거는 구현 레인의 `logs/rebound-reachable-summary.json`과 해당 시험 log다.
- **UnknownNeedsAbsence, 3단계:** 0 A 채택 → 1 unknown current가 되는 log 준비 → 2 A Missing → 3 producer Unknown인데 park. impl-1 reopen 후보/park guard `rotation/readers.rs:111/:460`은 durable birth 비교만으로 absence=false를 우회한다. 추가 시험 6·7의 RED/GREEN이 실제 입력·log 오류 해석을 확인해야 한다.
- **ReadyAfterFollow, 3단계:** 0 A 채택 → 1 cp 뒤 새 bind → 2 A Missing → 3 bind 적용 전에 ready. impl-1 actor `actor.rs:362`가 resumed=true를 게시하고 이후 read_sources/follow가 적용된다. 추가 시험 4·5와 Mf가 이 순서를 실제 검증한다.
- **ParkedReboundHalts, 4단계:** 0 A 채택 → 1 B current → 2 A Missing → 3 A park/ready → 4 tail 뒤 A bind인데 계속 running. impl-1 `rotation.rs:728`의 기존 non-retired cursor 분기는 Ok로 끝나 reopened capture를 만들지 않는다. 추가 시험 8과 Mg가 실제 halt/다음 host reopen을 확인한다.

위 코드 좌표는 base+impl-1 작업 트리를 고정한 설명이다. 새 함수 배선과 정확한 최종 행은 구현 레인의 C1~C6 표를 따른다. 모델 담당은 Rust 소스·시험·운영 상태를 수정하거나 Rust 시험을 별도로 실행하지 않았다. ActiveLoss의 기존 Rust RED 외 새 반례의 실제 시험 rc는 구현 레인 최종 증거를 함께 확인해야 한다.

## 범위와 가정

- 이 모델은 safety와 1회 resume/재bind 경계만 탐색한다. 무한히 바뀌는 세션·로그·lease·Discord 전달 성공·native CLI/TUI는 증명하지 않는다.
- typed Missing+NotFound, !retired, drain 없음, full replay 뒤 unsealed 없음을 고정해 조건 4와 ready/C6만 분리한다. unsafe anomaly와 spool 전달 liveness는 기존 WriterWake, 바이트 보존·GC는 Rust T1/M4/M9가 담당한다.
- `ParkPreservesEvidence`는 명시한 저장값이 모든 모델 전이에서 그대로임을 확인한다. 실제 파일의 바이트 불변이나 GC 호출이 없다는 사실을 대신 증명하지 않는다.
- Current의 raw supersede/Rejected 판정과 named SourceId 매핑은 정규화된 target 기호로 추상화했다. 실제 raw parser·renumbered names·same-pane supersede 조건·gapped seq 검출은 Rust Current 시험과 공유 follow 코드가 검증한다. adoption prefix 해석 충돌 규칙은 넣지 않았다.
- 읽기/gap/checkpoint 결함은 classification의 Unknown 입력으로 만든다. read_log/seed의 모든 오류·복구 분기를 재현하지 않는다. absence=true는 host가 current publication인 Idle/no_session으로 확인한 표본이라는 외부 전제다. host probe 호출 자체·timeout·표본 경쟁은 T1/T8/Mh가 확인한다.
- chart는 terminal Rest 54개를 숨기며 실제 TLC에는 남긴다. birth 차트는 같은 규칙을 위반하는 상태만 합쳐 mayBreak 표시 없이 버그 경계를 보존했다. 간선 23/22개로 약 20개 목표를 조금 넘지만 원본 소실·미적용 bind·재bind와 중단 경로를 숨기지 않았다.

## 산출물과 재실행

- [구판 차트](rebound-birth.chart.mmd), [구판 TLC report](rebound-birth.report.md)
- [신판 차트](rebound-current.chart.mmd), [신판 TLC report](rebound-current.report.md)
- 원시 TLC: 구현 레인의 `sc-out-impl-2/{birth,current}-done/`, 실행 log `logs/rebound-{birth,current}-done.log`.
- 이 모델 SHA256: `e4300e622e23b7a2fca297a54932f2a2d80d3ebd028d8a32c8b2f828fa67830b`.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
  docs/statecharts/tui-o-writer/WriterRebound.tla \
  --config docs/statecharts/tui-o-writer/WriterReboundCurrent.cfg \
  --view docs/statecharts/tui-o-writer/rebound.view.mjs \
  --out /tmp/i7845-rebound-current-new
```

구판은 config를 WriterReboundBirth.cfg로 바꾸고 새 output 폴더를 사용한다. exit=1이 예상되는 RED이며 오류 exit=2와 구별한다. CI checker의 expected.tsv는 구판의 5 invariant RED를 정확히 등록한다.
