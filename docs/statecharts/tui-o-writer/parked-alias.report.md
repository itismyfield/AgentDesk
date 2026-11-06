# WriterParkedAlias — park된 A0와 raw old A1의 다음 fresh bind

구판 구현 SHA: **`34822f3bf14c35907b440dbe8c81f62ad167e2ba`**. `StableOldReference=FALSE`는 이 코드의 old/new/parent 공통 bind_name 경로다. TRUE는 `review-2-pro.out.md` P1-1의 좁은 old-reference 수리를 구현한 **`1e2763de57d7932e1ae2585b9951169232c730d8`**를 모델링한다. 수리 main 기준은 `0a2996a8c537234c1cdc8da8c0ce67a06f0f600c`다. 구판34822와 수리 구현을 구별하며 중간 merge commit을 수리 검증 SHA로 쓰지 않았다.

이 모델은 기존 WriterWake/WriterRebound가 이미 정규화된 source 하나로 접었던 **저장 A0(session 빈 값)와 raw A1(session A)의 차이**를 추가한다. source identity가 정상적으로 확정된 뒤 park하고 다음 fresh source로 이동할 때의 경계다. 실제 family cursor가 이 이름 이력인지 운영 상태를 조사하거나 단정하지 않았다.

## 결과

| config | TLC state / step | chart state / arrow | 결과 |
|---|---:|---:|---|
| WriterParkedAliasMain | 66 / 69 | 18 / 18 | NoHaltForConfirmedOld, HaltRepeatZero, FirstReplyExactlyOnce RED |
| WriterParkedAliasFixed | 64 / 64 | 15 / 14 | 9 invariant·NoDeadlock·FirstReplyExactlyOnce GREEN |

공식 statechart 도구를 실제 실행해 구판 exit1/신판 exit0을 확인했다. 이어 공식 `scripts/ci/statecharts_tlc.py`를 지정 root copy에서 **기존8+신규2=10cfg** 모두 실행해 **10/10 expected 일치, 22.6초, exit0**이다. 전체 저장소의 다른 모델까지 실행한 결과로 주장하지 않는다.

원시 로그: `logs/repair-3-statechart-main-ready.log`, `logs/repair-3-statechart-fixed-ready.log`, `logs/repair-3-statechart-ci-metadata.log`. 원시 TLC: `sc-out-repair-3/{main,fixed}-ready/`. source copy 및 각 SHA는 `sc-out-repair-3/ci-metadata-source-sha256.json`이다.

## 독자의 규칙

| 규칙 | 검사하는 결과 |
|---|---|
| FirstReplyExactlyOnce (PROPERTY) | 정상 확정 old alias의 입력/Startup bind 뒤 첫 답변이 결국 1회 게시되고 두 번째는 없다 |
| FirstReplyAtMostOnce | 모든 상태에서 posted<=1 |
| FirstReplyUsesFreshSource | 답변은 fresh B가 검증·attach되고 cp3이며 Startup인 상태에서만 게시된다 |
| NoHaltForConfirmedOld | 정상 확정 old alias 때문에 새 입력 뒤 halt가 한 번이라도 생기지 않는다 |
| HaltRepeatZero | 정상 fresh bind 뒤 owner 재호스트 반복은 0이다 |
| ParkedBytesUnchanged | A0 cursor의 retired/frontier/prefix hash와 retained spool 바이트는 모든 전이에서 그대로다 |
| NarrowOldOnly | reopen 생략은 parked이고 유일하며 적용 prefix가 같은 raw→stored 대응을 확정한 old 역할에만 허용된다 |
| StoredNameNeverDuplicated | 저장 이름 A0를 유지하고 A1 이름의 중복 cursor를 만들지 않는다 |
| ParkedNewStillHalts | parked A를 new target으로 재bind하면 파일 복귀 후 strict prefix가 통과해도 C6 halt를 유지한다 |
| MissingFreshTargetHalts | old 참조는 통과해도 새 B 파일이 Missing이면 cp2/POST0/ready=false로 halt한다 |

첫 답변은 invariant의 at-most-once와 PROPERTY의 eventual-once를 함께 검사한다. 구판은 first reply가 전혀 없는 상태라 단순 posted<=1만으로는 잡히지 않으며 liveness RED를 따로 남겼다.

## 원본 이름과 실제 전이

정상 사례의 prefix는 파일이 존재할 때 `Bound(A0)` seq1, `Bound(old=A0,new=A1)` seq2를 적용한 상태다. cursor는 A0 한 개, cp2다. A0/A1은 같은 파일의 저장/filled 이름이며 alias 대응은 적용된 durable prefix로 확정했다.

그 뒤 A가 Missing이고 세션이 없어 A0를 park·ready한다. 입력이 승인되면 모델의 `QueuedBound`는 **`[old|->"A1", new|->"B", parent|->"none", cause|->"Startup"]`**다. B는 정상 사례에서 존재한다. 구판은 old A1을 A0로 loose 대응한 뒤 A0를 다시 open하려다 실패한다. checkpoint는 2이고 B cursor/POST가 없다. 재호스트해도 같은 seq3을 재적용해 같은 오류가 난다.

수리 모델은 이 **old graph predecessor 참조만** 이미 확정된 A0로 취급하고 reopen하지 않는다. B의 이름·원본 검증은 별도 단계이며 통과 후 cp3/B cursor/POST1을 만든다. 중복 bind/refresh는 POST를 늘리지 않는다. 같은 store의 process restart 뒤 A0 park를 복원하고 Posted를 보존해 추가 POST0이다.

8개 사례를 탐색한다: 정상 confirmed old, B Missing, prefix에 없는 최초 alias, 다른 session alias(A1을 확정했는데 A2를 old로 요구), 모호한 stored 후보, parent 역할, parked new target, 파일이 돌아온 parked new target. 후자의 strict prefix 성공도 C6를 우회하지 않는다. new/parent 전체를 named event로 바꾸는 행동이나 임의 loose alias 승격을 넣지 않았다.

## 구판 반례와 코드 단계 대조

다음 좌표는 모두 **구판 `34822f3bf14c35907b440dbe8c81f62ad167e2ba`의 Git object**를 읽어 고정했다. 현재 수리 작업 트리의 이동한 행 번호를 뜻하지 않는다.

| 단계 | 모델 / 관측 | 구판 코드 근거 |
|---|---|---|
| 0 | 저장 cursor A0, session 빈 값 | Claude handoff `claude.rs:1989` 및 runtime binding `runtime_binding.rs:799`의 정상 생산 경로; review가 source identity 이력을 확인 |
| 1 | Bound(A0), cp1 | `writer/rotation.rs:478`의 bind 적용 후 `:485`의 checkpoint 전진 |
| 2 | 파일 존재 중 A0→A1 실제 이름 확정, cursor는 A0 한 개, cp2 | `writer/renumbered.rs:214`의 bind_name, `:221` prefix reopen, `:222` confirm; 기존 `source_identity_tests.rs:95`~`:117`이 cursor1/cp2의 지원 상태를 검사 |
| 3 | A pathname Missing | 모델 환경 전이. 이미 캡처한 cursor/spool은 남는다 |
| 4 | named A0 park, ready | `writer/renumbered.rs:239`의 적용 prefix rebuild와 `:245` named mapping; `rotation/readers.rs:52` Current, `:451` park 검사, `:477` parked=true; `actor.rs:348` follow 성공 뒤 `:363`/`:364` clear/ready |
| 5 | raw Bound(old=A1,new=B,Startup) | producer의 `binding_events.rs:867`~`:878`은 pane.current.clone()을 old로 사용. `rotation.rs:478`~`:480`은 raw old를 bind에 전달 |
| 6 | old A1의 저장 A0 reopen NotFound, cp2, POST0 | `rotation.rs:585`~`:588`에서 old도 bind_name 사용; `renumbered.rs:219`~`:223`의 loose stored cursor reopen 실패. B attach `rotation.rs:689`와 cp 이동 `:485` 전에 반환 |
| 7 | actor 끝나고 owner cleanup 재호스트 | `writer/host.rs:809`~`:814` 비재시도 halt 종료, `discord/runtime_bootstrap/owner_cleanup.rs:87`/`:89` 재호스팅; active host의 중복 claim 방지는 `host.rs:197` |
| 8 이후 | A0 다시 park, 같은 seq3 old reopen 반복 | `renumbered.rs:264`의 rebuild는 cp 이하 이름 대응을 복원하지만 다음 raw old의 bind_name은 여전히 reopen을 요구한다. cp2가 그대로라 follow가 seq3을 다시 처리 |

- **NoHaltForConfirmedOld:** 최단 6단계, 위 0→6. 현재 old alias 때문에 첫 fresh bind가 실패한다.
- **HaltRepeatZero:** 최단 7단계, 위 0→7. 정상 park host가 새 bind 뒤 죽어 owner 재호스트가 시작된다.
- **FirstReplyExactlyOnce:** TLC의 15단계 반례는 같은 cp2/old reopen 실패/rehost loop에 들어가 B를 붙이지 못하고 POST0으로 남는다. 횟수 표시는 유한 그래프를 위해 halt3/rehost2에서 포화하지만 실제 재시도 전이를 막지 않아 loop 자체는 계속된다.

모델 담당은 cargo 금지 범위라 Rust 시험/뮤턴트를 실행하지 않았다. 위는 코드 단계 대조와 모델 RED/GREEN이다. 부모의 조합 시험 RED는 아래 절에서 실제 rc와 연결하며, 수리 GREEN·old helper 복원 mutant의 최종 증거는 부모가 별도로 연결한다. 수리 모델 GREEN을 구현 검증 PASS로 바꾸어 쓰지 않는다.


## 수리 구현과 모델 대응

수리 source `1e2763de57d7932e1ae2585b9951169232c730d8`를 Git object로 읽어 대응을 확인했다. `writer/rotation.rs:585`~`:587`은 old만 `bind_old`로 분리하고 parent/new는 `:588`~`:590`의 기존 bind_name을 유지한다. `rotation.rs:712`의 bind_old는 parked·stored empty/raw filled 조건(`:717`~`:720`), 읽힌 prefix의 cp/tail·seq 검증(`:726`~`:730`), cp 이하 Names 재구축과 같은 session/stored 대응(`:734`~`:742`)을 모두 확인한 경우만 저장 이름을 반환한다. 그 밖은 `:749`의 기존 bind_name이다. 이는 모델의 old 역할·confirmed·unique·같은 alias 대응·parked guard와 일치한다. new B 검증과 parked new C6를 통째로 우회하지 않는다.

부모의 실제 수정 전 RED는 `logs/red-repair-3-summary.json` 선택9 중 semantic fail5/control PASS4/ignored0이다. source proof `logs/repair-3/red-source-proof.json`의 head141759는 main 병합 뒤에도 review34822의 해당 모듈 바이트가 같은 구판이며 수리 SHA가 아니다(`repair-3-meta.json`). P1 시험 `parked_confirmed_empty_session_old_alias_posts_fresh_reply_once_across_restart`는 실제 `park_alias_tests.rs:234`에서 cp `Some(2)` != `Some(3)`으로 rc101이었다. 즉 반례표 0~6의 이름 확정/park/새 bind/이전 reopen 실패까지 실제 시험과 연결됐다. 추가 재호스트 반복의 단계는 구판 host/owner_cleanup 코드 대조 및 TLC loop이며 Rust에서 반복 횟수를 따로 검증했다고 주장하지 않는다. binary `bin/red-repair-3-libtest`, SHA256 `33c11af44c25ef950806c81137f7e1e09ebe533a02a5523fcdcd7130ea300416`. 실제 수리 GREEN·뮤턴트·최종 docs-only head는 부모 구현 보고서의 별도 실행 증거가 담당한다.

## 가정과 한계

- 정상 사례에는 새 입력이 도착하고 B 파일/Startup 기록이 생기며 내부 단계와 게시가 공정하게 진행된다는 가정이 있다. `WF_vars(Next)`는 이 한 건의 준비된 시나리오를 끝까지 진행시킨다. 입력 없는 idle 채널의 자발적 응답을 요구하지 않는다. 무한한 Discord 장애·lease 거부·I/O 실패를 모델링하지 않았다.
- absent=true, typed Missing+NotFound, !retired, drain 없음, replay 뒤 unsealed 없음을 고정한다. 이 수리의 범위는 old 이름 대응이며 나머지 park 조건을 완화하지 않는다.
- alias의 path/inode/session 비교와 durable prefix parsing은 확정 대응/유일성/역할 predicate로 추상화한다. ambiguous 사례는 helper의 보수적 경계 검사이며 실제 actor가 더 앞의 unique-store 검사에서 거절할 수도 있다. 이 모델을 모호 store의 정상 운영 도달성 증거로 쓰지 않는다.
- EvidenceA는 실제 저장값을 대표하는 명시 상태다. 모델 전이가 바꾸지 않음을 확인하지만 실제 파일 바이트/GC 호출의 부재는 Rust byte oracle가 확인해야 한다. B의 새 spool 쓰기는 A의 보존 의무와 별개다.
- 기존 WriterWake/Rebound는 raw/stored alias를 같은 이름으로 접었다. 해당 8cfg의 GREEN이 이 P1 조합을 증명하지 않았고, 이번 모델이 그 차원을 추가한다.
- `review-2-pro.out.md`의 P2-1 notice 저장 후 stop, P2-2 cleanup-retire I/O latch, P2-3 cp>tail 절단은 이 모델에 넣지 않았다. 이 결과를 그 세 경계의 PASS로 세지 않는다.
- view는 negative guard 준비의 내부 36단계를 같은 화면 상태로 접고 terminal Rest 10개(구판)/8개(신판)를 숨긴다. TLC에는 모두 있다. 버그 path와 loop의 카운터 상태는 별도로 유지해 서로 다른 위반 상태를 합치지 않았다.

## 산출물·재실행

- [구판 차트](parked-alias-main.chart.mmd), [구판 report](parked-alias-main.report.md)
- [수리 차트](parked-alias-fixed.chart.mmd), [수리 report](parked-alias-fixed.report.md)
- 모델 SHA256: `5be3c06aede8d71150d830146f91b55c41a05a750f20f8fd0b2e37dc1b7d0bd2`
- view SHA256: `a3dcc56ff16d94ca3b7bf38eee85889605e5d740bd8f9a71ccbd2723d406435d`
- CI log SHA256: `6244ca956a54ea26b9ec294379704fda1049dea371582218278eb03b1df41c46`

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
  docs/statecharts/tui-o-writer/WriterParkedAlias.tla \
  --config docs/statecharts/tui-o-writer/WriterParkedAliasFixed.cfg \
  --view docs/statecharts/tui-o-writer/parked-alias.view.mjs \
  --out /tmp/i7845-parked-alias-fixed-new
```

구판은 WriterParkedAliasMain.cfg와 새 output 폴더를 쓴다. expected.tsv에는 구판의 두 safety/한 liveness RED를 그대로 등록했다. 모델을 GREEN으로 만들기 위해 safety/liveness 규칙을 빼거나 expected를 실패에 맞춰 넓히지 않았다.
