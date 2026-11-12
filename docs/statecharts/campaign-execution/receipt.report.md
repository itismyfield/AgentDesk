# Campaign command receipt and transaction boundary

코드 기준: `0051e898e4e43012c232c244a521c5a280765fb8` (S4-16 merge 직후). 2026-11-12 KST 설계 조사. production 코드·DB schema 변경, cargo 및 Rust 시험 실행은 없다.

기존 `CampaignVerification.tla`의 실행 영수증은 host journal의 start/result 증거다. DB의 `server_campaign_receipts` 명령 영수증과 transaction commit을 뜻하지 않는다. 같은 모델 파일에 `ReceiptSpec` 모드를 추가해 이 경계를 검사했다. 기존 17개 cfg는 `receiptTx = "inactive"`를 유지하므로 verification 전이와 기존 expected 결과를 바꾸지 않는다. 새 모드는 기존 verification 변수를 고정하고 순수 kernel 호출을 `RDecide`로 추상화한다. 따라서 이 결과는 모든 kernel 전이를 transaction과 직접 곱한 상태 공간의 증명이 아니다. 기존 cfg가 kernel 규칙을, 새 cfg가 그 주위 receipt/lock/commit 규칙을 검사한다.

## 상태 차트

[그림과 Mermaid 원문](receipt.chart.mmd), [진입점 전수표](receipt-entrypoints.md), [전체 정상 실행](receipt-full.tool.report.md), [chart 실행](receipt.tool.report.md), [조기 관측 부정 대조](receipt-early-facts.tool.report.md), [분리 write 부정 대조](receipt-split-write.tool.report.md).

|규칙|뜻|정상/Chart|EarlyFacts|SplitWrite|
|---|---|---|---|---|
|ReceiptEffectAtMostOnce|같은 입력이 만든 durable 변경 효과는 최대 1|PASS|PASS|FAIL, 13단계|
|ReceiptAtomicEffects|changed 영수증이 commit되면 묶음 효과 1, 그 전에는 0; 다른 outcome은 0|PASS|PASS|FAIL, 6단계|
|ReceiptRollbackHasNoEffect|rollback된 해당 시도가 남긴 durable 효과 0|PASS|PASS|FAIL, 7단계|
|ReceiptDecisionUsesLockedFacts|결정에 쓴 사실은 해당 lock 보유 중 관측했고 결정 시점에도 같다|PASS|FAIL, 5단계|PASS|
|ReceiptSingleLock|receipt 조회부터 replay/commit까지 한 caller만 캠페인 lock을 보유|PASS|PASS|PASS|
|NoDeadlock|정상 종료를 포함한 모델의 명시적 다음 단계가 있음|PASS|PASS|PASS|
|ReceiptCallsComplete (PROPERTY)|계속 실행 가능한 caller와 재접속이 진행하면 두 요청 모두 종료|PASS|PASS|PASS|

정상 full cfg는 **844 distinct / 2,872 generated / depth 20**, Chart cfg는 **86 distinct / 265 generated / 264 steps / depth 15**다. full은 chart 상한 500을 넘어 Mermaid 파일을 만들지 않았고, Chart는 crash 횟수만 0으로 줄여 같은 두 caller와 rollback/commit/replay 흐름을 13 chart states / 29 arrows로 그렸다. full cfg가 restart를 검사한다. 두 mutant는 현재 main의 결함을 뜻하지 않는다. 아래 코드 대조 결과 코드로 확인된 결함 0, 새 버그 수용 시험으로 넘긴 반례 0이다.

`effects` 1은 한 변경 명령의 state/history/event/evidence 묶음을 의미하며 SQL 행 개수나 외부 프로세스 실행 횟수가 아니다. 성공했어도 `unchanged`인 결정은 효과 0이다. 두 caller가 모두 rollback한 실행도 효과 0이 정상이다. "정확히 1"은 **changed outcome의 성공한 commit**에만 적용하며 요청이 반드시 성공한다는 뜻은 아니다. receipt는 rejected/NotFound/no-op도 저장한다. ReceiptCallsComplete는 commit 또는 rollback으로 요청이 끝난다는 성질이며, 결국 성공하거나 영수증이 반드시 생긴다는 성질이 아니다.

Liveness는 `WF(RWork(c))`를 caller별로, `WF(RRestart)`를 재접속에 둔다. crash는 최대 1회로 유한하며, 무한 DB 실패·영구 lock holder·연결 단절의 회복을 보장하지 않는다. 이는 서비스가 새 재시도 loop를 제공한다는 가정도 아니다. 외부 실행의 영구 멈춤은 기존 verification cfg의 별도 PROPERTY가 맡는다.

### 값 범위와 진입점

전체 호출자·인접 경로는 [전수표](receipt-entrypoints.md)에 있다. 모델 값은 다음 코드 경계에서만 가져왔다.

|값 또는 행동|file:line 근거|모델 대응|
|---|---|---|
|두 동시 caller, 같은 campaign/key/input|`src/db/server_campaigns/storage_tests.rs:150,153,235,239`|RC={1,2}; 실제 caller 개수 상한이라는 뜻은 아님|
|begin→key/run lock|[base의 mod.rs 142·166·172·177행](https://github.com/itismyfield/AgentDesk/blob/0051e898e4e43012c232c244a521c5a280765fb8/src/db/server_campaigns/mod.rs#L142)|RBegin→RAcquire. INSERT ON CONFLICT의 대기와 두 row lock을 한 직렬화 경계로 추상화|
|receipt absent / same fingerprint / different fingerprint|`mod.rs:181,190,193`|absent는 facts로, same은 replay로. different는 kernel 이전의 rollback/error 가지로 추상화; fingerprint bytes 자체는 모델링하지 않음|
|state joined projection와 BeginReview stored seal|`mod.rs:207,210,213,216`; `read.rs:42,85`; `evidence_store.rs:70,78,88`|RReadFacts가 lock 안 version summary를 관측. missing/mismatch seal의 StaleBinding은 rejected outcome으로 추상화|
|create/command 결정|`mod.rs:218,220,227,236`; `control.rs:252,286`|RDecide. create/command의 concrete command kind는 boundary에서 opaque|
|Changed / unchanged / rejected / NotFound|`mod.rs:238,249,250`; `control.rs:732`; `write.rs:57`|outcome의 4값. phase/quality budget의 구체 전이는 기존 kernel cfg·Rust 시험의 별도 책임|
|state/history/event write와 EvidenceWrite|`mod.rs:240,242,244`; `write.rs:48,66,106,130`|RPersist가 pending만 갱신, DB durable 값은 그대로|
|renderer/receipt INSERT/commit|`mod.rs:252,253,144`|RRender→RWriteReceipt→RCommit. receipt와 변경 묶음이 한 번에 durable이 됨|
|명시 rollback·renderer/receipt 실패|`storage_tests.rs:398,413,415,426,428`|RRollback. 대기/쓰기/receipt 단계의 오류는 commit 없이 종료|
|재연결·응답 loss 뒤 replay|`storage_tests.rs:94,162,167`; `mod.rs:181`; `read.rs:76`|RCrash는 pending을 버리고 durable receipt/effects를 보존, RRestart 뒤 같은 입력을 재시도|

`receiptTx.saved`는 동일 입력의 단일 receipt를 표현한다. 단일성은 캠페인 직렬화와 `migrations/postgres/0161_server_campaigns.sql:99`의 기존 receipt primary key를 전제로 한다. 모델이 SQL schema/트랜잭션 엔진을 증명하는 것은 아니다. `snapshot`은 revision/control_version/event_sequence 및 seal 사실의 currentness를 나타내는 한 version summary다. `observedLocked`는 lock 이전 관측을 재사용하지 않았다는 출처를 기록한다.

### 부정 대조의 단계별 코드 대조

EarlyFacts의 가장 짧은 경로:

|단계|모델 행동|main 코드 대조|
|---|---|---|
|0|새 요청 두 개, receipt 없음|동시 create/execute fixture `storage_tests.rs:150,235`|
|1|lock 전에 사실을 관측|**main에서는 불가능**. begin/identity/fingerprint 뒤 :172/:177 lock이며 state :207와 seal :213은 그 다음|
|2|campaign/run lock 획득|`mod.rs:172,177`|
|3|receipt 없음 확인|`mod.rs:181,189`|
|4|lock 이전 snapshot 유지|**main에서는 불가능**. `read::get_in(tx)`와 `sealed(tx)`를 같은 tx에서 새로 호출|
|5|service 결정 → LockedFacts 위반|호출은 현재 :220/:227, 그리고 관측은 lock 뒤. 따라서 이것은 기각한 설계의 부정 대조이며 main 버그가 아님|

SplitWrite의 공통 prefix 0~5는 begin→lock→receipt 없음→locked facts→decision이며 위 main 순서와 같다. 그 뒤:

|단계|모델 행동|main 코드 대조|
|---|---|---|
|6|receipt commit 전에 state/evidence가 durable이 됨 → AtomicEffects 위반|**main에서는 불가능**. :240 persist와 :244 evidence는 같은 열린 tx이고 commit은 :144 하나뿐|
|7|오류로 rollback했는데 효과가 남음 → RollbackHasNoEffect 위반|**main에서는 불가능**. :252 render와 :253 receipt 실패도 transact의 `?`를 통해 tx rollback. 기존 fixture :388이 이를 검증|
|8~12|두 번째 caller begin→lock→receipt 없음→facts→decision|처음 시도가 rollback됐다면 이 prefix 자체는 가능; main에서는 앞선 효과가 0|
|13|두 번째 durable write로 effects=2 → EffectAtMostOnce 위반|main에는 단계6의 분리 write가 없으므로 중복 효과를 만들 수 없음|

부정 대조가 지킨 것은 **현재 transaction 계약을 분리하면 탐지된다**는 점이다. 원자성 mutant RED를 수정 전 main RED라고 보고하지 않는다. 다음 구현에서 기존 PG 시험을 같은 head에서 통과시키고, 새 service API가 외부 snapshot/임의 tx를 받지 못하도록 리뷰하는 근거로만 사용한다.

### 범위 밖과 이번 변경의 관계

|범위 밖|관계와 남은 검증|
|---|---|
|kernel 모든 command의 실제 데이터·quality budget 수치|순수 결정 본체를 이동하지 않고 소유 유지하므로 기존 17 cfg와 control/stage/verification/failure_budget Rust 동등성 시험이 맡음. 새 모델은 호출의 facts/commit 경계만 검사|
|host start/result journal와 실제 프로세스 중복|기존 CampaignVerification의 verification mode가 담당. DB 명령 receipt와 혼동하지 않음|
|HTTP bytes, fingerprint 충돌, 옛 receipt JSON|형식 변경 없음. saved status/body 재생 및 다른 fingerprint rollback은 기존 storage/http/model 시험이 맡음. 새 모델의 same-input replay는 이 byte-level 동등성의 증명이 아님|
|옛 Run의 default maps/optional execution|restore 경계는 read.rs decode와 domain serde default에 유지. next implementation의 옛 행 fixture 동등성 시험으로 검증|
|다른 key의 상태/CAS 경쟁 및 state/evidence 개별 누락|동일 입력 두 caller에 한정하고 물리적 state/history/evidence를 effects 하나로 묶었다. 서로 다른 명령의 CAS와 각각의 SQL write 원자성은 PG trigger/동시성 시험이 맡음|
|DB statement 중간 오류의 종류와 PostgreSQL isolation 구현|모든 commit 전 phase에서 rollback 가능으로 추상화. 실제 receipt INSERT trigger·renderer failure·run lock 시험을 재사용|
|영구 외부 lock holder/무한 DB 장애/무한 crash|bounded 모델로 회복 SLA를 주장하지 않음; 새로운 timeout·retry·kill 권한을 만들지 않음|
|legacy campaigns의 Paused/Active/Cancelled|별도 테이블/CRUD이며 server_campaign_keys/receipt 상태를 공유하지 않음. 전수표의 legacy 경로와 기존 불변 시험을 유지|
|server campaign Start/Resume/Retry runtime enablement|현재 ExecutionUnavailable(control.rs:335~349); 자동 recovery caller 없음. RRestart는 DB 재접속과 persisted retry 대표이며 runtime을 켜는 모델이 아님|

### 재현 명령과 산출물

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/campaign-execution/CampaignVerification.tla --config docs/statecharts/campaign-execution/CampaignVerificationReceipt.cfg --view docs/statecharts/campaign-execution/receipt.view.mjs --out /tmp/sc-s417-receipt-new
```

다른 cfg는 ReceiptChart / ReceiptEarlyFacts / ReceiptSplitWrite 접미사로 바꾸고 매번 빈 새 out 경로를 사용한다. 정상/Chart exit 0, EarlyFacts/SplitWrite exit 1이 기대값이다. 원시 로그는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/sc-out-s417/{receipt-final,chart-final,early-facts-final,split-write-final}/tlc.out` 및 부정 대조의 `tlc-<rule>.out`에 있다. 각 `.tool.report.md`는 도구 출력을 그대로 보존했다. model report/README는 code SHA와 해석을 덧붙인다.

그림은 한 chart state에 머무는 begin/대기/완료의 일부 단계와 별도 idle step을 생략한다. Chart는 crash 없는 projection이며 full cfg에서 crash/restart를 검사한다. 초기 상태에서 모든 chart state가 도달 가능하고, 각 상태의 진행·rollback 출구를 유지한다. 29개 화살표는 쓰기/renderer/receipt와 rollback 경계를 각각 보여주기 위해 유지했다.

## 전체 cfg 회귀 검증

검증 head: `73dfb0310cd34398fe849eeb05ffd16679d3582a`. 다음 명령은 최종 증거 커밋 전에 실행했다. 이후 변경은 보고서뿐이며 모델·cfg·expected 23개 입력 SHA256은 동일하다.

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --base 0051e898e4 --head HEAD --jobs 2
```

실제 선택은 **기존 17 + 신규 4 = 21 cfg**, 21/21 expected MATCH, exit 0이다. 기존 cfg는 변경하지 않았고 기존 expectation도 수정하지 않았다. 새 PROPERTY에는 이전 ReceiptSpec이 없어 base 실행 결과를 만들지 않았다. 기존 모델의 고정 expectation과 수정 후 결과를 아래에 같이 남긴다.

| cfg | expected to break | broken | seconds |
|---|---|---|---|
| campaign-execution/CampaignVerification.cfg | - | - | 195.3 |
| campaign-execution/CampaignVerificationChart.cfg | - | - | 12.6 |
| campaign-execution/CampaignVerificationClaimKept.cfg | ImmediateRevokes, NoStartWithoutAuthority | ImmediateRevokes, NoStartWithoutAuthority | 274.0 |
| campaign-execution/CampaignVerificationFactsRefused.cfg | DeliveredEventually, StopNowCompletes, UnsealedClosed, VSettles | DeliveredEventually, StopNowCompletes, UnsealedClosed, VSettles | 61.2 |
| campaign-execution/CampaignVerificationFencedStartBefore.cfg | DeliveredEventually, StopFinishCompletes, VSettles | DeliveredEventually, StopFinishCompletes, VSettles | 47.5 |
| campaign-execution/CampaignVerificationFinishDeadBefore.cfg | StopFinishCompletes | StopFinishCompletes | 96.2 |
| campaign-execution/CampaignVerificationIndeterminate.cfg | StopFinishCompletes, StopNowCompletes, VSettles | StopFinishCompletes, StopNowCompletes, VSettles | 86.1 |
| campaign-execution/CampaignVerificationIndeterminateSettled.cfg | - | - | 135.5 |
| campaign-execution/CampaignVerificationInterruptBefore.cfg | StopFinishCompletes | StopFinishCompletes | 92.0 |
| campaign-execution/CampaignVerificationLateLaunch.cfg | - | - | 85.3 |
| campaign-execution/CampaignVerificationLateLaunchBefore.cfg | FinishScopeKept | FinishScopeKept | 93.6 |
| campaign-execution/CampaignVerificationNoRedelivery.cfg | DeliveredEventually, StopFinishCompletes, StopNowCompletes, VSettles | DeliveredEventually, StopFinishCompletes, StopNowCompletes, VSettles | 40.9 |
| campaign-execution/CampaignVerificationNotSentBefore.cfg | NoCancelWhileAuthorized | NoCancelWhileAuthorized | 297.9 |
| campaign-execution/CampaignVerificationOverwrite.cfg | FillOnce | FillOnce | 33.7 |
| campaign-execution/CampaignVerificationQuietSession.cfg | StopFinishCompletes | StopFinishCompletes | 107.6 |
| campaign-execution/CampaignVerificationReceipt.cfg | - | - | 6.2 |
| campaign-execution/CampaignVerificationReceiptChart.cfg | - | - | 4.8 |
| campaign-execution/CampaignVerificationReceiptEarlyFacts.cfg | ReceiptDecisionUsesLockedFacts | ReceiptDecisionUsesLockedFacts | 6.7 |
| campaign-execution/CampaignVerificationReceiptSplitWrite.cfg | ReceiptAtomicEffects, ReceiptEffectAtMostOnce, ReceiptRollbackHasNoEffect | ReceiptAtomicEffects, ReceiptEffectAtMostOnce, ReceiptRollbackHasNoEffect | 10.0 |
| campaign-execution/CampaignVerificationSettleBefore.cfg | StopFinishCompletes, UnsealedClosed, VSettles | StopFinishCompletes, UnsealedClosed, VSettles | 188.3 |
| campaign-execution/CampaignVerificationTwoValues.cfg | - | - | 26.9 |

total 969.5s, 21 configs

원시 checker stdout/stderr: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/logs/s417-statecharts-ci.log`. 신규4의 per-config 시간은 6.2/4.8/6.7/10.0초다. 기존 NotSentBefore는 297.9초로 300초 상한에 가깝고, 전체 로컬 969.5초는 README의 15분 권고보다 길다. PR workflow의 실제 job 제한도 `.github/workflows/statecharts-tlc.yml:33`에서 15분이므로, 이 Mac의 로컬 16.2분은 그 상한을 넘는다. hosted runner에서는 아직 실행하지 않아 시간 통과를 주장하지 않는다. 이 시간 여유는 남은 CI 관측 항목이다. 기존 모델 범위를 줄이거나 timeout/기대 위반을 바꾸지 않았다.
