# Outbox transaction: S4-13·S4-14 설계

코드 SHA `5c3ed37cddc80036fa8a274419b0219f49926704`. production 변경 0, cargo/PG/소스 스캔 시험 0이다. 이 보고서는 설계 모델의 실행 결과이며 제안 구현의 시험 결과가 아니다.

`sc=not-run(statechart-cli-missing)`은 브리프에서 지정한 statechart 생성/시각화 실행 상태다. WSL에 `~/.claude/skills/statechart/scripts/statechart`와 `/home/adk/skills/statechart`가 없으며 Node는 v18.19.1(README 요구 23.6+)이다. **검증 엔진은 사용 가능해서 repository의 `scripts/ci/statecharts_tlc.py`로 plain TLC 9/9 expected outcomes를 확인했다.** statechart wrapper 실행·생성 Mermaid PASS로 바꿔 쓰지 않는다. view 소스는 Mac 실행용으로 준비했으며 `.chart.mmd`는 생성하지 않았다.

모델 작성/실행은 2026-11-12 09:12~09:31 KST에 끝냈다(30분 이내). Java 21.0.13, TLC 1.7.4 jar SHA256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`. 원시 로그는 `/home/adk/adk/lane-runs/i7353/logs/design-s41314-1/`에 있다. 설정 파일·운영 DB·runtime 데이터는 열지 않았다.

## 기존 모델과 범위

README 색인 및 [outbound-port 보고서](../outbound-port/outbound-port.report.md), `OutboundPort.tla`, `OutboundCallerFlow.tla`를 먼저 읽었다. 기존 모델은 intent→reservation→nonce→HTTP post→receipt 및 caller fallback/관측/deduper 소유를 다룬다. durable message_outbox DB transaction은 그 보고서의 명시적 범위 밖이다. 따라서 HTTP/nonce/receipt 전이는 복제/수정하지 않고 새 DB transaction 모델을 추가했다. committed row 이후 송신과 재전송은 기존 outbound/TUI 모델과 이어진다.

이번 범위는 canonical autoqueue 완료의 SQL UPDATE→slot 해제→notice INSERT→post-INSERT metadata→commit/rollback 및 그 전후 restart다. `system` 완료, `lifecycle_notifier`, `headless_turn`, configured-agent/unknown/padded, exact, replay hold, circuit의 관측 동치류를 [전수표](entrypoints.md)에서 가져왔다. no-entry/no-target는 canonical finalizer의 무공지 예외다. provider는 실제 enum의 Claude/Codex 두 대표값, config snapshot 유무/source agent 충돌, task tag Some/None, producer host 초기화 전/후, 최대 1회 설정 변경·1회 canonical restart를 사용한다. 같은 task의 tag가 다른 task 때문에 바뀐다는 가정은 없다.

Current는 기존 DB orchestration이다. Service는 slot release 뒤 `ReturnPreparation`에서 private tx borrow를 서비스에 반환하고 서비스가 관측/INSERT를 조립한다. 이 추가 행동이 실제로 model state/guard에 반영돼 있으며 Boundary 이름만 바꾼 두 cfg가 아니다. 이후 원래 관측 행동을 유지한다. 새 준비 API의 컴파일·실제 Rust 구현은 아직 없으므로 formal refinement/구현 동등성 증명으로 부르지 않는다.

## 규칙별 결과

`Current.cfg`, `Service.cfg`는 아래 13개 invariant와 deadlock 검사를 모두 통과했다. Current는 12,888 generated / 7,682 distinct / queue 0, Service는 13,166 generated / 7,844 distinct / queue 0이다. source 검증·default 관측과 INSERT 사이의 설정 변경을 허용한다. 검사하는 것은 **원래 관측 지점의 값으로 INSERT한다**는 규칙이며 INSERT 시점의 최신 설정과 같아야 한다는 새 규칙이 아니다.

|규칙|현재/제안|의미·코드 대조|
|---|---|---|
|TypeOK|PASS / PASS|전수표에서 가져온 작은 값 범위|
|AtomicCompletion|PASS / PASS|canonical 완료·slot·notice가 함께 commit. `src/db/auto_queue/runs.rs:440,:493,:498`→`src/services/message_outbox.rs:860`의 같은 tx|
|RollbackHasNoNewNotice|PASS / PASS|미커밋 실패/restart면 새 notice 0; 이전에 커밋한 row는 지우지 않음|
|CommittedNoticeExactlyOnce|PASS / PASS|canonical 완료, entry와 target이 있는 단일 distinct target의 notice 정확히 1. terminal 재확인 `runs.rs:416~418`이 완료 뒤 restart를 막음|
|SameTransaction|PASS / PASS|모델이 선언한 owner tx/connection과 실행 대상 INSERT tx/connection ID 관계만 확인. Rust/PG 연결 증명은 아님|
|PreparationKeepsBorrow|PASS / PASS|Service 준비 객체가 살아 있는 동안 active tx/동일 tx id 유지; 구현은 private borrow와 PG로 검증 예정|
|SameProviderConnection|PASS / PASS|모델이 선언한 expected tx/connection과 실제 SELECT 대상 ID의 관계 확인. exact의 pre-tx connection은 별도 ID이며 실제 backend PID 증명은 아님|
|SourceDecisionUsesObservation|PASS / PASS|각 V 지점의 관측값을 사용. TTL/persistent의 tx 안 재검증을 pre-tx 결과로 대체하지 않음(:438/:551, :242/:275)|
|ProviderObservationPointPreserved|PASS / PASS|각 row의 live default를 원래 위치에서 읽음. `session_notice.rs:99`→`agent_channels.rs:18~20,:59`|
|InsertUsesObservedDesignation|PASS / PASS|INSERT의 provider hint는 관측한 행들의 합의값/불일치 None. 관측 뒤 reload는 소급하지 않음|
|CensusObservationPointPreserved|PASS / PASS|INSERT 뒤 config snapshot/task-local 관측. source membership 변경과 snapshot 없음 모두 포함|
|ProducerObservationPreserved|PASS / PASS|Some census 분기에서만 host/boot stamp 관측, 저장용 값을 유지. `legacy_body_census/mod.rs:154~164`|
|CensusFollowsInsert|PASS / PASS|실제 새 row가 생긴 뒤 C; `message_outbox.rs:877`, 다른 경로 :313/:413/:600/:693/:951|
|NoDeadlock|PASS / PASS|bounded safety graph에서만 확인. 진행성/외부 lock fairness를 주장하지 않음|

Savepoint begin 오류는 outer 실패로 전파한다(`body_census.rs:113`). UPDATE 오류+savepoint rollback 성공은 metadata 없음으로 notice commit이 가능하다(:119~129). rollback/release 오류가 outer를 실패시키는 결과도 포함했다(:123/:130). 모든 optional metadata 실패를 notice 실패로 바꾸거나, 반대로 begin Err를 성공으로 바꾸는 설계가 아니다. 모델의 commit/rollback은 DB 결과 분기를 추상화하며 모든 PostgreSQL/sqlx 내부 오류 조합의 증명은 아니다.

## cfg 결과와 코드로 대조한 반례

|cfg|expected broken|TLC 실제 broken|결과|
|---|---|---|---|
|ForeignTx|SameTransaction|동일|expected RED|
|ForeignProviderConnection|SameProviderConnection|동일|expected RED|
|Current|없음|없음|GREEN|
|Service|없음|없음|GREEN|
|PreSource|SourceDecisionUsesObservation|동일|expected RED|
|Preobserve|ProviderObservationPointPreserved, InsertUsesObservedDesignation|동일|expected RED|
|SplitInsert|AtomicCompletion, CommittedNoticeExactlyOnce, RollbackHasNoNewNotice, SameTransaction|동일|expected RED|
|EarlyCensus|CensusObservationPointPreserved|동일|expected RED|
|FreshTask|CensusObservationPointPreserved|동일|expected RED|

검사기는 각 실패 invariant를 제외하고 다시 실행해 전체 broken 집합을 수집한다. 최초 원시 TLC trace만으로 다른 규칙도 실패한다고 추정하지 않았다. P2 보강 뒤 checker rc 0, 9 cfg, 20 TLC safety runs, 12.2초. 개별 원시 TLC는 positive rc 0, negative rc 12다. 보강 뒤 원시 로그는 `/home/adk/adk/lane-runs/i7353/logs/s13-1/`의 `p2-checker.log`, `tlc-*.log`, `tlc-commands.json`이다. 모델의 tx/connection ID 관계 검출 결과이며 실제 Rust/PG 연결 증명은 아니다.

반례는 **잘못된 이동의 negative control**이며 현재 코드 버그 5건이라는 뜻이 아니다. 각 trace를 코드로 다음처럼 대조했다.

1. **PreSource, 6 states:** Start→Validate(known=true, pool :438)→Begin(:480)→설정에서 agent 제거→ValidateInner. main은 :551→source_registry :206→settings/content :493에서 다시 읽어 거절한다. 변형은 앞의 true를 재사용해 허용한다. source 동적 허용을 조기에 봉인하는 안이 동등하지 않다는 것을 코드로 확인했다. 수용 시험은 두 V 사이 reload barrier다.
2. **Preobserve, 10 states:** Start에서 default를 미리 캡처→Begin→완료(:440)→slot(:493)→ReturnPreparation(제안만)→Validate(:848)→bindings SELECT(session_notice :78)→default reload→ObserveProvider1. main은 SQL 후 :99→agent_channels :18~20에서 새 default를 읽지만 변형은 옛 값을 쓴다. 두 row 사이 reload의 불일치 None도 같은 관측 계약이다. 수용 시험은 D 전/행 사이/D 뒤 reload barrier다.
3. **SplitInsert, 11 states:** Begin→완료→slot→(제안 borrow 반환)→V→bindings→각 D→notice가 별도 tx에 먼저 durable. main :875는 원래 tx이므로 outer rollback이면 row가 없다. 변형은 outer 미커밋인데 notice가 보이고, restart/retry가 같은 no-dedupe notice를 다시 만들 수 있다. 최초 trace의 AtomicCompletion과 검사기 재실행에서 나온 나머지 두 규칙을 각각 구별했다. 수용 시험은 INSERT/metadata 실패 시 완료·slot·notice 전체 rollback 및 commit 전/후 restart다.
4. **EarlyCensus, 13 states:** 시작 전 C 봉인→Begin/완료/slot/V/D→INSERT(:860)→config snapshot/agent membership 변경→C. main은 :877→body_census :8→source_registry :150~153에서 그때의 값으로 None/NonBody를 고른다. 변형만 옛 값으로 분류한다. 실제 원시 trace가 고른 필드는 snapshot availability였으며 source-id 충돌도 bounded graph에 포함된다. 수용 시험은 INSERT 뒤 C 전 snapshot install/removal 및 source 충돌 barrier다.
5. **FreshTask, 7 states:** headless Validate→Begin→ineligible display 생략→INSERT→C. main은 `body_census.rs:16`에서 현재 future의 CONTEXT(`raw_output.rs:25~33`)를 clone한다. 변형은 새 task의 None을 써 태그를 잃는다. 별도 task로 C를 옮기면 동등하지 않음을 코드로 확인했다. 수용 시험은 Some tag를 가진 실제 public enqueue와 None 대조, 중첩 task scope 복원이다.

원시 trace의 순서는 TLC action label과 main statement 좌표를 매칭한 것이다. 현재 코드에서 이 negative action이 실행된다는 주장을 하지 않는다. 아직 cargo/PG RED를 실행하지 않았으며, 위 수용 시험은 구현 레그에서 main GREEN/잘못된 이관 mutant RED로 확인해야 한다.

## 전수표·입력·관측 범위

[entrypoints.md](entrypoints.md)는 INSERT 8곳, production 호출 75행, DTO 생성 30곳 및 tx/입력 작성자/관측 leaf의 전수표다. 호출 이름만 보지 않고 `enqueue` alias(:105), replay private helper, circuit stage, 운영자 redrive, generic direct-send validator를 추가 검색했다. fixture와 선언은 production 수에 넣지 않았다. canonical 완료의 DB caller 재귀 폐쇄는 DB 8 + 상위 20파일이며 선택 설계가 이 조립을 서비스 child로 옮기는 비용을 포함한다.

|값 동치류|전수표의 코드 근거|모델/시험 분담|
|---|---|---|
|static system/lifecycle, dynamic exact known/unknown/padded|source_registry :201~214, settings/content :493~502|boolean 허용과 phase는 모델, 실제 runtime/org fallback 우선순위·전체 label/mask는 동등성 시험|
|eligible completion/lifecycle, attachment/ineligible, raw session/reason|session_notice :150~166; identity :47~59; message_outbox :402/:915|모델은 대표 eligible/ineligible, malformed/legacy/raw bytes는 PG/순수 시험|
|provider SQL와 각 legacy-primary default|session_notice :78~110, agent_channels :36~59|두 대표 row/default를 모델; 0-row locator/ambiguous/malformed/전체 provider enum은 동등성 시험|
|config snapshot Some/None·source id 충돌|source_registry :150~153|동시 변경은 모델, 실제 Arc/config 설치는 PG barrier 시험|
|headless task tag Some/None·producer host/boot|raw_output :25~44, legacy_body_census :154~164|같은 future clone/재시작을 모델; JSON bytes와 실제 task scope는 시험|
|canonical run no entry/no target/terminal|runs :416~418/:496~498, targets :121~122|단일 distinct target 투영을 모델; 다중 target 오류 원자성은 PG|

## 범위 밖과 남은 기존 한계

- 다중 target fan-out, 다른 run들의 operator end batch, 전체 run lock ordering/SQL predicate, 일반 producer의 임의 재호출/TTL/exact 충돌 알고리즘, commit 응답 유실은 이 작은 모델의 exactly-once 증명 범위가 아니다. INSERT 포트 이동 때 SQL·partial index·dedupe·conflict predicate를 유지하고 기존 PG/새 동등성 시험으로 검증한다.
- 특히 operator end는 `cancel_run/terminalize_tx.rs:49~56`에서 후보를 읽고 advisory를 얻은 뒤 :114~120에서 조건부 UPDATE하며, :175~178은 **batch rows_affected>0**일 때 후보 run 전부에 안내한다. 이는 canonical `runs.rs:416/:440/:498`의 per-run 성공 guard와 다르다. 이 모델의 canonical exactly-once를 그 batch로 확대하지 않는다. 후보가 lock 대기 동안 terminal이 되는 다중 run 경계는 별도 기존 경로 후속이며 이번 설계에서 per-run 새 guard/삭제/권한을 추가하지 않는다. operator end의 entry 0 공지도 기존대로 유지한다.
- SQL snapshot과 runtime config, census snapshot, producer stamp 사이에는 전역 원자적 snapshot이 없다. 관측 뒤 설정 변화는 소급하지 않는다. metadata UPDATE/savepoint 실패로 저장이 빠지면 기존 delivery_tag UnknownBody fallback을 유지한다(`body_census.rs:23~100`). 이것을 NonBody/실행 허가/삭제 준비 PASS로 바꾸지 않는다.
- replay hold 경로에는 원래 display/census가 없다. 이 경로의 private proof/ExactAttempt/LocalSnapshot 발급·incarnation·CAS는 S4-15이며 이 모델에 새 권한을 넣지 않는다.
- `PreparedCompletion`은 제안 API다. borrow가 살아 있는 동안의 tx 일치를 모델링했지만 drop 후 잘못 commit하는 모든 Rust 프로그램을 타입만으로 금지했다고 주장하지 않는다. 실제 caller의 Err 전파/rollback과 다른 tx 적용 불가를 구현 검증으로 넘긴다.
- fairness/진행성/HTTP 송신·nonce/운영 설정값/실제 운영 DB 상태는 미검증이다. TLC GREEN을 코드 실행/전체 이슈 완료로 표시하지 않는다.

## 실행 재현

WSL에서 실행한 검증(현재 폴더의 local expected.tsv는 전체 root manifest와 같은 7행의 scoped view다):

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=/home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar
python3 scripts/ci/statecharts_tlc.py --root docs/statecharts/outbox-transaction --jobs 2 --timeout 180
```

개별 trace 명령은 logs의 `tlc-commands.json`/`run_tlc.py`에 있다. 두 positive cfg는 모든 상태 탐색 완료/queue 0, negative cfg는 invariant violation exit 12다. `expected.tsv`는 그 전체 broken 집합만 기록하며 기존 모델의 기대 실패를 늘리지 않았다.

Mac에서 지정 wrapper/시각화 실행은 아래와 같이 남긴다(WSL에서 실행한 것으로 세지 않음):

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
cd docs/statecharts/outbox-transaction
~/.claude/skills/statechart/scripts/statechart OutboxTransaction.tla --config OutboxTransactionService.cfg --view outbox-transaction.view.mjs --out /tmp/sc-outbox-transaction
```

상세 설계는 `/home/adk/adk/lane-runs/i7353/design-s41314-1.md`. S13 + S14a~d 5조각, prod 합집합 60파일, 순증 예산 +1340/보정 +476. 실제 구현·테스트·독립 리뷰는 후속 레그다.

## S13 착수 전 P2 보강

기준 `1e94361e442b07c21a1f09c720fae15c98bf3f6e`의 리뷰 P2 두 건을 반영했다. 전수표의 pool 호출 23행을 callee의 pool 시그니처/실제 begin에 대조하여 caller tx 없음과 callee 소유를 구별했다. reactivation의 done/non-done 분기와 per-entry 별도 tx도 적었다.

ownerTx/ownerConnection, provider/insert 실행 target, 실행 결과에 기록하는 tx/connection ID를 분리했다. foreign-tx는 다른 INSERT tx/connection(9), foreign-provider-connection은 다른 SELECT connection(9)을 선택한다. SameTransaction/SameProviderConnection이 각각 RED다. split-insert도 이제 SameTransaction이 함께 RED다. 이는 모델이 선언한 연결 관계의 검출력 보강이며 실제 Rust/PG의 connection 증거가 아니다.

보강 전 7 cfg는 Current/Service GREEN, 5 expected RED였고 SameTransaction은 split-insert에서 GREEN이었다. 보강 후 9 cfg는 Current/Service GREEN, 기존 5 + foreign 2 expected RED이며 split-insert의 SameTransaction도 RED다. 모든 cfg를 plain TLC로 재실행했고 `logs/s13-1/tlc-*.log`에 raw trace/상태 수, `tlc-commands.json`에 명령/exit를 남겼다. 전체 broken 집합은 `p2-checker.log`로 확인했다. 앞 절의 상태 수·시간·trace state 개수는 보강 전 설계 실행의 기록이다.
