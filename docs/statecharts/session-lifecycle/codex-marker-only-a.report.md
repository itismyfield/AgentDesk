# Codex marker-only A-admission r6 상태 모델

코드 정본 SHA: `e9549f721d7a9910a52a19a5b9101c48fda27386`. 설계/모델만 변경하며 운영 Rust 코드는 변경하지 않았다. 최종 `HerdrSession.tla` SHA256: `1a1f6f1c08bdf89d9889fd6b338c18c476c63c322100d317f01632869a928e2f`. [전체 설계](codex-marker-only-a.design.md), [생성 차트](codex-marker-only-a.chart.mmd), [재생성 view](codex-marker-only-a.view.mjs).

## 결정과 모델 범위

A의 DB 호출은 **반환되는 호출**로 추상화한다. fence 아래 DB await는 허용한다. DB 무응답 상한, 절대 deadline, CAS timeout/Indeterminate, late CAS commit의 새 행동은 모델 밖이다. 새 timeout·재grant/새 nonce 경로를 도입하지 않는다. 기존 일반 Herdr 모델의 `RecordPaneFails`에 남은 Indeterminate 주석은 기존 pane-record 실패이며 r5의 새 Pending CAS timeout 행동이 아니다.

현재 코드 정본에는 r4 A 행동만 있다. r5 설계 head `2904cbaf71a545f3de7cd39ede998f7dab356fd4`의 `D*` 행동과 `HerdrHintADb{Before,Fixed,NoTimeout}.cfg`는 비교용으로 외부 증거 폴더에 추출·실행했으며 r6의 추적 파일에는 포함하지 않는다. 따라서 이 base에 없던 r5 cfg를 삭제했다고 주장하지 않는다.

`AIntent → ARecheck → 두 unlink → ACommit → AOutbox → AAdmission`과 controller continuation·최대 세 번의 반환 오류 정산을 유지한다. `ALegacyProgress`는 정상 Legacy/Missing 및 예산 안에서 끝난 일시 오류에 대해 **원래 입력의 delivered**를 요구한다. 명시 거절을 전달 성공으로 세지 않는다. 새 `a-progress-two` cfg는 intent/partial-unlink/receipt/boot 오류가 두 번 반환된 뒤 회복하는 경계를 검사한다. 세 번의 실패가 먼저 끝내는 경우는 보존·통지 정산이며 무제한 자동 재시도나 나중의 자동 전달 보장은 아니다.

A-fence의 typed rollout witness(`src/services/tmux_common/runtime_hint_fence.rs:82`, `:93`, `:101`, `:214`)를 반영했다. path·session·execution nonce·proof seq·선언된 source dev/ino·실제 source dev/ino를 비교한다. rollout marker 저장 inode와 cursor는 실행 identity에서 제외한다. 일반 runtime hint의 파일 identity·owner/path snapshot 비교는 유지한다. `AExactOwned`는 mutant의 판정 조건과 독립적으로 삭제 오라클을 계산한다.

## cfg 전/후 결과

정본 r4 base의 기존 A cfg/expected 행 8개는 변경하지 않았다. 실제 비교는 base 파일을 외부 `model-before/`에 고정하고 같은 Java/TLC로 수행했다. GREEN은 모든 규칙이 성립한다는 뜻이며 expected-RED는 잘못된 정책을 탐지했다는 뜻이다.

| cfg | base 실행 | r6 실행 | 판정 |
|---|---|---|---|
| HerdrHintAFixed | 2 runs, GREEN | 2 runs, GREEN | 동일 |
| HerdrHintALegacyBefore | AProgress RED | AProgress RED | Legacy를 다시 Wait에 두면 진행 불가 |
| HerdrHintAMissingBefore | GREEN | GREEN | Missing 대조군 |
| HerdrHintANoFrontCheck | ANoStaleGrant RED | ANoStaleGrant RED | 다른 front grant 금지 |
| HerdrHintANoOutboxDiscovery | AProgress RED | AProgress RED | outbox-only discovery 필요 |
| HerdrHintARetryFixed | 2 runs, GREEN | 2 runs, GREEN | 110 distinct, depth 19 |
| HerdrHintARetryNoController | AProgress RED | AProgress RED | partial 오류 뒤 callback 소실 탐지 |
| HerdrHintARetryNoBudget | AProgress RED | AProgress RED | 영구 오류 무한 반복 탐지 |
| HerdrHintAIdentityFixed (신규) | 해당 없음 | 3 runs, GREEN | 10 case, 282 generated / 236 distinct, depth 15 |
| HerdrHintAIdentityUnchecked (신규 mutant) | 해당 없음 | safety exit 12 두 번; 4 runs | ANoForeignMutation·ANoForeignExecution RED |
| HerdrHintALegacyProgress (신규) | 해당 없음 | 3 runs, GREEN | 11 case, 311 generated / 249 distinct, depth 24 |
| r5 HerdrHintADbBefore | 비교용 재실행: DGuardBound·DWaitProgress RED | r6에서 제외 | 3 runs, safety 12 / liveness 13 |
| r5 HerdrHintADbFixed | 비교용 재실행: GREEN | r6에서 제외 | 2 runs, 1934 distinct |
| r5 HerdrHintADbNoTimeout | 비교용 재실행: DGuardBound·DWaitProgress RED | r6에서 제외 | 3 runs, safety 12 / liveness 13 |

최종 묶음은 **28 cfg / 71 TLC 실행, 기대 불일치 0, 실행 오류 0**이다. r4 A before 8 cfg/17회, r6 HerdrSession 종속 전체 17 cfg/46회, r5 비교 3 cfg/8회다. property 실행 전에 공식 checker의 fairness precheck를 적용했다. `WF_vars(AHintNext)`를 실제 사용하며 DB 응답 fairness로 무응답을 숨기지 않는다.

새 fixed cfg들은 8 invariants(`ANoForeignMutation`, `ANoDuplicateEffects`, `ANoUnprovedGrant`, `ANoStaleGrant`, `ANoSilentLoss`, `ANoEarlyUnlink`, `ANoForeignExecution`, `ARetryBounded`)와 `AProgress`/`ALegacyProgress`를 검사한다. 독립 positive state witness 19개는 typed 8필드 변경 후 거절, cursor/storage republish 후 전달, 정상·오류 회복·crash·redelivery·경합의 grant/create/prompt=1 상태를 확인한다.

## 반례 경로와 한계

- `ANoForeignMutation`: 같은 실행 identity를 확인해야 다른 실행 파일을 삭제하지 않는다. identity 비교 제거 시 `observe → intent → recheck → nonce 교체 → unlink_canonical → unlink_alias`의 6번째 상태에서 위반한다.
- `ANoForeignExecution`: 다른 실행 증거로 새 grant를 만들지 않는다. identity 비교 제거 시 `observe → intent → recheck → 두 unlink → commit → outbox → wake → admit → nonce 교체 → grant → create`의 12번째 상태에서 위반한다.
- `AProgress`(LegacyBefore): hint 정리 후에도 Legacy의 typed 증거를 소비하지 않으면 `admit → wait`에 머문다.
- `AProgress`(NoController): 첫 unlink 성공·두 번째 unlink 오류 뒤 `retry_wait`에 남고 이어갈 callback이 없다.
- `AProgress`(NoBudget): 영구 unlink 오류에서 `retry_wait → recover → unlink`를 무한 반복한다.
- `ANoStaleGrant`: front 확인을 제거하면 기존 receipt가 교체된 front를 grant한다. NoOutboxDiscovery는 boot의 outbox-only 한 상태에서 진행할 수 없다.

이 반례는 구현 예정 A 경로의 mutant 계약 검사다. 실제 A-admission이 아직 없으므로 Rust 경로의 RED→GREEN을 실행했다고 주장하지 않는다. typed identity의 기존 코드 대조군은 `runtime_hint_fence.rs:973`, `:1026`에 있으며 이번 설계 레그에서 cargo를 실행하지 않았다. 구현에서는 실제 owner 진입점 시험과 비교 제거 mutant로 재현해야 한다.

`ANoDuplicateEffects`는 단일 A 제어 pass의 grant/create/prompt ≤1이며 `ALegacyProgress`와 합쳐 해당 성공 case의 정확히 한 번을 확인한다. `ARedeliver`는 wake 횟수만 늘리고 post-grant crash는 `ACrash`에 넣지 않았다. 따라서 **prompt 이후 crash→boot→ack 재관찰의 end-to-end exactly-once 증명은 아니다**. 이 경계는 기존 owner/front claim·Pending reservation·terminal dedup을 이용하는 실제 통합시험 의무다. 새 dedup 권위를 모델만으로 선언하지 않는다.

filesystem bytes/fsync 불확실성, SQL NULL의 실제 decode, 실제 OS lock 순서·Source try-lock·실행기 queued task, history TTL pin·catalog I/O·bootstrap gate 순서는 Rust/PG 검증 대상으로 남는다. `ANotice`는 독립 기존 outbox에 통지가 가능한 경우다. queue와 PG outbox가 모두 영구 불능일 때 notified=true를 주장하지 않는다. DB가 반환되지 않으면 fence가 계속 잡힐 수 있으며 이 기존 위험에 시간 상한을 증명하지 않는다.

## 차트와 재생성

추적 차트는 `HerdrHintAIdentityFixed`의 **236 모델 상태 → 16 차트 상태/17 화살표**를 공식 statechart로 생성했다. 같은 차트 상태 안에 머무는 30개 step(증거 변경 등)은 그리지 않는다. 전체 모델의 retry/crash는 별도 LegacyProgress 차트의 249 상태/17 차트 상태/26 화살표로 확인하며 외부 산출 경로를 아래에 남긴다. 메인 차트가 그 세부 경로까지 그렸다고 읽지 않는다.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
  docs/statecharts/session-lifecycle/HerdrSession.tla \
  --config docs/statecharts/session-lifecycle/HerdrHintAIdentityFixed.cfg \
  --view docs/statecharts/session-lifecycle/codex-marker-only-a.view.mjs \
  --out /tmp/i8057-adm-r6-new
# 커밋 후 현재 base와 비교하면 HerdrSession 종속 cfg가 선택된다.
python3 scripts/ci/statecharts_tlc.py --base e9549f721d7a9910a52a19a5b9101c48fda27386 --head HEAD --jobs 2
```

## 증거 목록

공통 외부 루트는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8057/`다.

- `logs-adm-6/model-checks-r6.log`, `model-results.json`: 최종 28 cfg/71회 결과. `raw-r6/{before,after,r5}/<cfg>/`에 fairness·각 safety/property 원시 stdout·실제 exit·result.json을 보존했다. 앞선 탐색 로그와 구분한다.
- `logs-adm-6/run_model_checks.py`: 저장소 공식 `check_cfg`/`classify`를 import하고 `run_java`의 원시 결과만 보존하는 재현 harness. base expected를 git-show로 고정하며 새 cfg 기대만 추가한다.
- `logs-adm-6/dumps-r6/{HerdrHintAIdentityFixed,HerdrHintALegacyProgress}/states.txt.dump`, `tlc.out`, `tlc.exit`: 최종 모델 전체 state dump와 직접 TLC exit 0.
- `logs-adm-6/model-witnesses.json`, `extract_witnesses.py`: 실제 reachable state 19개·state 번호·SHA256; transient 전달 witness는 failures=2도 확인한다.
- `logs-adm-6/model-artifacts-sha256.json`, `model-environment.json`: 모델/cfg/view/차트 바이트와 환경. Java Homebrew OpenJDK 27.0.1.0.3, jar tla2tools 1.7.4, zsh; cargo profile 해당 없음, cargo/PG 실행 0.
- `sc-out-adm-6-identity-final/{chart.mmd,report.md,tlc.out}`와 `sc-out-adm-6-live-final/{chart.mmd,report.md,tlc.out}`: 공식 statechart 생성물. `logs-adm-6/statechart-{identity,live}-final.log`에 생성 요약이 있다.
