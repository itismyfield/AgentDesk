# Intake readiness: 같은 잠금·같은 관측·같은 transaction

코드 기준 SHA: `b20a366c8812348f14a9e08a7439cb8df6cfb09a` (S4-05 이동 전). 제안은 그 동작을 DB child의 private PreparedSweep와 서비스 조립으로 나누는 #7353 묶음7 계약이다. S4-18의300초 상수 이동은 상태·단위가 바뀌지 않아 이 모델 범위 밖이다.

구현 대조 SHA: `ca0e458dd759a456b7dcb1009c0872c7d6bf8e0b`. DB의 `src/db/intake_outbox/pre_accept_sweep.rs`가 잠긴 transaction·원본·후보를 소유하고 `src/services/cluster/pre_accept_sweep.rs`가 같은 잠금 안의 live 관측과 commit을 조립한다. 아래 진입점 좌표는 이동 전 base이며, 구현 레그 보고서는 이동 후 좌표를 함께 제시한다.

실행: 2026-11-12 05:06 KST. `statechart`(TLC 1.7.4, JAVA=/opt/homebrew/opt/openjdk/bin/java), 단일 worker. Current와 Proposed는 전체 bounds에서 GREEN이며 두 결함 변형만 아래 규칙을 깨뜨렸다. 모델은 실제 SQL/Rust 시험이 아니며, PG 경계 시험·뮤턴트 실행 결과는 구현 레그 보고서가 맡는다.

## 진입점 전수표와 모델 값 범위

좌표는 위 고정 SHA의 소스다. 실제 유일 production sweep 호출을 `rg -n sweep_failed_pre_accept_once src -g '*.rs'`로 확인했다. 나머지는 cfg(test) 호출이며 기존 이름의 시험 전용 재수출로 보존한다.

|진입점/관측|코드 좌표|모델 값·행동|
|---|---|---|
|leader maintenance가 sweep 호출|`src/services/cluster/node_registry.rs:260,275`|Begin. stale-node/attachment maintenance 뒤 sweep 시작이며 그 앞선 작업은 범위 밖|
|source 후보 잠금·fallback|`src/db/intake_outbox.rs:250,265,333,367`|LockSource; empty/source의 두 결과. 기존 `SKIP LOCKED`·source family 최신/terminal/home_epoch 필터는 후보 존재 입력으로 추상화|
|family MAX·예산·조기 commit|`src/db/intake_outbox.rs:380,389,390`|CheckFamilyBudget; empty/budget/retry. Empty(:377), budget(:390) commit은 기존 정상 종료|
|forwarded owner fence|`src/db/intake_outbox.rs:396,410`|fenced=TRUE/FALSE, LockOwner. 기존 비소유 경로는 owner lock 없음|
|worker 공유 잠금·현 후보 행|`src/db/intake_outbox.rs:421,475`|LockWorker. source→(해당할 때 owner)→worker 순서. offline SQL 제외는 scenario=offline에서 candidate 없음|
|requirements 파싱|`src/db/intake_outbox.rs:477`|ParseRequirements; parse-error는 rollback. 상세 JSON parser/unsupported identifier는 기존 단위·PG 시험 범위|
|현재 인증 profile|`src/db/intake_outbox.rs:481`; `src/services/cluster/readiness.rs:126`|ObserveAuthProfile, profile TRUE/FALSE, ChangeAuthProfile. provider/channel/agent bindings의 선택 결과만 추상화|
|requirements→attachment→declared readiness 단락 평가|`src/db/intake_outbox.rs:489,490,492`|ObserveForwardingAndTime→Eligible. parser 성공 이후 requirements/attachment 세부 계산은 기존 순서대로 보존한다는 구현 전제이며 이 모델은 구체 predicate 구현을 증명하지 않음|
|requirements 시간 관측|`src/services/cluster/execution_requirements.rs:49,53`|시각에 따른 현재 적격/비적격이라는 유한 결과. 실제 timestamp/Duration 계산은 PG·단위 시험|
|modern probe·forwarding cache/TTL|`src/services/cluster/readiness.rs:147,158,162,170`; `src/services/session_forwarding/probe.rs:60,65,68,74`|forwarding ready/unknown/expired, ExpireForwarding. modern unknown와 offline은 retry0|
|legacy protocol의 현재 호환 분기|`src/services/cluster/readiness.rs:148,153`|scenario=legacy는 현재 eligible=true bypass를 유지. legacy를 새 unknown 거부로 바꾸는 규칙을 모델에 발명하지 않음|
|첫 적격 node의 ID|`src/db/intake_outbox.rs:486,499`|malformed-id는 첫 predicate가 통과해도 NoCapableTarget. 다음 node를 새로 고르는 find_map 변화 금지. 실제 SQL instance_id는 문자열이며 이 값은 선택 함수의 합성 JSON 경계를 모델링|
|no-target 갱신·INSERT·최종 commit|`src/db/intake_outbox.rs:501,505,512,557`|Apply, Commit. 같은 tx=1을 사용하고 준비 객체는 한 번 소비. tx=2는 조기 종료 결함 변형만 생성|

## 규칙과 범위

- `LockOrder`: owner/worker lock은 source lock 뒤이며 owner-fenced일 때 worker는 owner lock 뒤다.
- `ApplyUsesLockedTransaction`: child INSERT는 잠금이 유지된 원래 transaction에서만 한다.
- `NoStaleSnapshot`: 적용에 사용한 profile/forwarding 관측은 worker lock 시점보다 오래된 version이 아니다. version은 모델의 관측 순서 표식이며 production nonce/계약을 추가하지 않는다.
- `ModernUnknownAndOfflineFailClosed`: modern unknown evidence/forwarding 또는 SQL offline 후보로 child를 만들지 않는다. legacy bypass는 명시적으로 보존한다.
- `FirstMalformedIdDoesNotSkipAhead`: 첫 적격 node의 비문자열 ID에서 다른 후보를 찾지 않는다.
- `PreparationConsumedOnce`: 제안 객체의 단일 소비. Rust private 필드·move 및 다른 tx/target 주입 불가는 별도 컴파일/시험 증거가 필요하다.
- `OutcomeMatchesObservedInputs`: Current/Proposed 모두 같은 관측 결과를 같은 retry/no-target 결과로 보낸다.
- `TransactionEventuallyEnds`: 해당 추상 DB 작업들이 돌아온다는 weak fairness 아래 empty/budget/parse-error/retry/no-target 모두 종료한다. 실제 네트워크/DB 작업이 무한히 반환하지 않는 장애까지 보장하지 않는다.

환경은 잠금과 독립적인 profile 변경 또는 forwarding 만료를 한 번 일으킬 수 있다. 하나의 변경으로 잘못된 선관측을 드러내기에 충분하다. **정상 관측이 끝난 뒤 live 값이 바뀌는 것은 소급해 stale 위반으로 보지 않는다.** 두 관측을 하나의 시점으로 합쳐 transaction 전 snapshot을 정당화하지 않는다.

Current와 Proposed의 observable Next 단계는 같다. Boundary가 바꾸는 것은 proposed에서만 prepared=owned/spent 및 소비 횟수를 기록하고 Apply 때 owned를 요구한다는 점이다. 두 cfg는 같은 전체 scenario/owner bounds에서 동일602개 상태·동일 결과 규칙을 통과한다. 후속 비교에서 완전한 state graph를 남기려고 plain TLC를 Current/Proposed 각1회 더 실행했다(exit0/0). 원 statechart raw는 report와 tlc.out만 남겨 state/edge 비교에 불충분했으므로 `-dump dot,actionlabels graph.dot`로 전체 DOT를 새로 보존했다. `prepared`와 `spends` 두 private 필드만 제거한 투영에서 **상태602개, 초기 상태16개, action label을 포함한 edge794개가 정확히 같다**. tx·잠금·pc·관측 값/version·결과를 모두 비교에 남겼고 투영 충돌0, 양방향 state/initial/edge 차집합0이다. 모델 source는 같고 cfg 차이는 Boundary 하나뿐임도 검사했다. 이 결과는 주어진 유한 bounds의 모델 graph 동형이며 실제 Rust/SQL bisimulation의 증명이 아니다. 스크립트는 `logs/b7-1/statechart/equivalence.py`, DOT·SHA256·비교 결과는 `sc-b7/equivalence-{current,proposed}/graph.dot`와 `sc-b7/equivalence.json`에 있다.

범위 밖: SQL query planner·worker/source row payload 전체·다중 family 공정성·정확한 OS clock 단위·config/cache 잠금 내부·DB failure의 모든 조합·이전 leader maintenance의 정리 효과. 이번 변경은 그 동작·권한을 넓히지 않으며 관련 기존 PG와 새 경쟁 fixture가 실제 코드를 검증한다.

## cfg 결과

|cfg|statechart exit|상태 수|깨진 규칙|liveness|
|---|---:|---:|---|---|
|IntakeReadinessCurrent.cfg|0|602|없음|TransactionEventuallyEnds 통과|
|IntakeReadinessProposed.cfg|0|602|없음|동일 통과|
|IntakeReadinessStaleSnapshot.cfg|1(기대)|첫 반례710 / 계속 탐색836|NoStaleSnapshot|위반 규칙 제거 후 통과|
|IntakeReadinessEarlyTxEnd.cfg|1(기대)|첫 반례463 / 계속 탐색662|ApplyUsesLockedTransaction|위반 규칙 제거 후 통과|
|IntakeReadinessChart.cfg|0|276|없음|동일 통과|

statechart는 첫 반례 뒤 해당 invariant를 제거하고 나머지 규칙을 계속 검사하므로 mutant의 두 상태 수를 구분했다. 첫 Current/Proposed의602상태가 chart budget500을 넘으므로 Chart.cfg는 ready/empty/parse-error/malformed-id만 그린다. 전체 unknown/offline/budget/legacy 검사는 Current/Proposed에 남아 있다. Mermaid는12상태·13화살표다. 동일 chart 상태에 머무는132개 세부 step(환경 변화/종료 stutter 등)을 그림에서 생략하며, 모든 상위 단계의 도달성과 나가는 화살표를 남긴다.

별도 CI 동일방식 검사 `scripts/ci/statecharts_tlc.py --root <sc-b7/ci-input> --jobs 1`는5 cfg를 실제 선택했고 safety 재검사 및 property 단독 실행까지12회, 합9.6초, exit0으로 expected.tsv와 일치했다. 이 scratch root는 같은 모델/5cfg를 복사하고 동일5행 expected 목록만 둔 것이다. 전체 저장소 모델을 다시 실행했다는 뜻이 아니다.

## 결함 경로와 실제 코드 대응

**StaleSnapshot / NoStaleSnapshot:** 11단계: tx 시작→잠금 전 profile/cache 읽기→source lock→family budget→owner lock→profile 변경→worker lock→requirements parse→옛 profile 재사용→옛 forwarding 재사용→INSERT. main은 profile 관측이worker lock 뒤(:475→481)이므로 이 반례를 만들지 않는다. 구현의 stale snapshot 원인 복원 뮤턴트가 profile/TTL lock-wait fixture에서 RED가 되는지 별도 확인한다.

**EarlyTxEnd / ApplyUsesLockedTransaction:** 10단계: tx 시작→source lock→budget→owner lock→worker lock→원래 tx 종료→parse→profile→forwarding→다른 tx에서 INSERT. main은 모든 관측과 INSERT 뒤에 commit(:557)하므로 이 경로를 만들지 않는다. 준비 객체가 tx를 소유해야 하는 이유이며 실제 조기 tx 종료 뮤턴트의 backend/lock fixture로 대조한다.

두 경로는 main의 발견된 버그가 아니라 잘못된 리팩터링을 구분하는 모델 변형이다. PG 뮤턴트의 실행 여부·RED를 이 모델 결과로 대신하지 않는다.

## 재현

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/intake-readiness/IntakeReadiness.tla \
  --config docs/statecharts/intake-readiness/IntakeReadinessCurrent.cfg \
  --view docs/statecharts/intake-readiness/intake-readiness.view.mjs \
  --out /tmp/intake-readiness-current-new
```

Proposed/StaleSnapshot/EarlyTxEnd/Chart 각각도 새 output 폴더로 실행한다. StaleSnapshot/EarlyTxEnd만 exit1이 정상이다. 원시 로그는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/sc-b7/{current-r2,proposed-r2,stale-snapshot-r2,early-tx-end-r2,chart-r2}/` 및 `logs/b7-1/statechart/{runs-r2.json,ci-selected.log}`다. r1 실행 후 label 개선용 재실행에서 기존 output 폴더 재사용이 거부되어 exit2였던 시도는 결과에 포함하지 않고 r2 새 폴더로 모두 다시 확인했다.
