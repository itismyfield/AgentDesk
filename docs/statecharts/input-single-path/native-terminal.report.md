# Native terminal boundary 모델 결과

Code baseline: `200ef5a37f2926f01f7dbcda70ccdb192285b8b8`.
[설계와 진입점 전수표](native-terminal.design.md), [수동 계약 상태도](native-terminal.chart.mmd), [모델](NativeInputTerminal.tla).
이 레그는 빌드 없는 설계다. Rust/PG/Windows/clippy 시험을 실행하지 않았다.

## 기존 모델의 정밀화

`NativeInputTerminal`은 기존 #8148 `NativeInputPreparation`을 EXTENDS한다. 그 모델을 수정하지 않고, B source ACK·AnyKey·durable membership 뒤의 같은 phase/role/pin/source/result 상태 위에 두 행동 `InterruptTerminalRow`, `RawEOFCloser`를 정밀화했다. canonical pre-key 준비/권한 모델 전체를 다시 검증한 결과가 아니다. 새 `TerminalSpec`만 cfg에서 선택한다. 기존 cfg/expected 결과는 변경하지 않았다.

| 모델 값 | 코드/설계 대응 |
|---|---|
| phase=Unknown, prepared/AnyKey=true, source=Bound, role=Primary | B 구현 이후 시작; 실제 source/pin/receipt 생산 자체는 앞선 모델·Rust 증거의 범위 |
| strict/whole, InterruptTerminalRow | canonical strict terminator 우선, native.rs Prompt→barrier 전에 terminal 분류 |
| captureCursor=1, terminalEnd=2 | 실제 offset 1/2라는 뜻이 아니라 complete C와 raw EOF L의 서로 다른 경계 |
| scope=Exact/ForeignNonce/ForeignSource/ForeignKey | original full ticket·source·execution nonce와 domain port 대조 |
| barrier / barrierRowMatches / OverridePermitted | 원본 stored barrier를 보존하며 exact strict interrupt supersession만 같은 결과 CAS에서 인정. UUID/range/foreign_hashes·원본 digest 일치를 하나의 비교 값으로 추상화 |
| ended, RawEOFCloser | exact ExecutionEnd::Ended와 `[C,L)` strict whole JSON proof의 conjunction |
| version/proofVersion/commitVersion, MetadataRace/Reobserve | observed/current EOF fingerprint·digest·등록 stamp 변화 재검증; 외부 파일과 history를 한 atomic write로 가정하지 않음. StoredStampRace는 저장된 interrupt 후보 뒤에도 같은 경합을 허용 |
| proofBarrierMatch / BarrierRace / BarrierOK | 후보가 관측한 barrier identity와 결과 CAS 시점의 현재 stored barrier digest를 분리. CAS는 현재 값만 비교 |
| proofKind / committedKind | 후보와 durable 결과 증거를 분리. raw terminal은 membership/본문을 만들지 않음 |
| ExistingControl(ControlKind), ExistingEndNotice | 기존 exact Cancel/Terminate/clear/stop 또는 E/no-terminal notice ACK/결과 경계. 새 권한을 부여하는 행동이 아님 |
| FlushTerminalResult → CleanupTerminalRow | 결과 durable 뒤 기존 row/actor CAS cleanup. Control/native 결과가 두 번 commit되지 않음 |
| CrashAtBoundary / BootRetry / readable / RepairRead | 결과 전 후보는 다시 검증, 결과 뒤 저장 proof로 cleanup; 기존 Source 가독성 관문은 유지 |

모델의 current-version guard는 fresh 관측 간 불일치를 거절하는 추상화다. 파일의 모든 미래 변경까지 잠그는 증명이 아니다. 과거 captured prefix와 membership의 신뢰는 기존 O capture 계약을 전제로 하고, 새 leaf가 전체 prefix를 재독한다고 주장하지 않는다. 제어의 정확한 actor/nonce CAS와 follower 권한 전체는 기존 모델/구현의 의무다. 이 정밀화는 primary row의 terminal 경계에 한정한다.

## 규칙

- `NoTerminalEvidenceFreeDelete`: row 삭제에는 검증된 native terminal 또는 이미 허용된 exact control/E-notice 정책 증거가 있어야 한다. raw JSON/E만으로 성공을 만들지 않는다.
- `NoPartialTerminalRead`: native proof로 쓰는 tail은 whole/strict다. invalid tail의 기존 notice 정책 결과를 Native Executed로 부르지 않는다.
- `NoForeignTerminalBarrier`: strict interrupt를 foreign Prompt로 만들어 settle을 막지 않는다. genuine foreign Prompt guard를 삭제하는 규칙이 아니다.
- `NoForeignBarrierBypass`: 현재 interrupt와 다른 genuine stored barrier를 supersede하지 않음.
- `NoForeignTerminalProof`, `NoStaleTerminalCommit`: 다른 원본/nonce/source 또는 current snapshot과 맞지 않는 proof 거절.
- `NoCaptureGuardWidening`: EOF proof가 일반 complete-line cursor를 전진시키지 않는다.
- `NoCleanupBeforeDurable`, `AtMostOneTerminalResult`: durable 결과 전 삭제 0, 제어 경합 포함 결과 CAS 1회.
- `ClosedExecutionEventuallySettles`: matching scope의 확인된 closed execution row는 기존 poll/boot/IO 회복/notice ACK 공정성 아래 결국 settle한다. 영구 unread는 기대 RED다. Foreign scope cfg는 권한 safety를 확인하며 잘못된 scope의 row 삭제 진행성을 약속하지 않는다.

모든 cfg가 canonical `NoKeyBeforePrepare`, `PrimaryPinPreserved`, `NoSilentLoss`도 유지한다. 이들이 준비 이후 상태에만 적용됨을 전제 표에 명시했다.

## cfg별 결과

CI checker가 **35/35 cfg의 예상 broken-rule 집합과 일치**, unexpected 0. **20 GREEN / 15 expected RED**이며 35개가 모두 GREEN이라는 뜻이 아니다. 마지막 4개는 저장된 supersession의 결과 CAS 재검증(등록 stamp·현재 barrier)을 검사하려고 구현 조각에서 더했다. Safety와 temporal PROPERTY를 runner가 분리해서 검사했고 fairness precheck도 수행했다. Before/negative cfg의 expected RED를 PASS로 숨기지 않는다.

| cfg | 판정 | 실제 깨진 규칙(예상과 일치) |
|---|---|---|
| `NativeTerminalBeforeInterrupt.cfg` | RED | NoForeignTerminalBarrier, ClosedExecutionEventuallySettles |
| `NativeTerminalBeforeRaw.cfg` | RED | ClosedExecutionEventuallySettles |
| `NativeTerminalFixedInterrupt.cfg` | GREEN | 없음 |
| `NativeTerminalFixedRaw.cfg` | GREEN | 없음 |
| `NativeTerminalFixedPartial.cfg` | GREEN | 없음 |
| `NativeTerminalFixedMention.cfg` | GREEN | 없음 |
| `NativeTerminalFixedBootBefore.cfg` | GREEN | 없음 |
| `NativeTerminalFixedBootAfter.cfg` | GREEN | 없음 |
| `NativeTerminalFixedRetryIO.cfg` | GREEN | 없음 |
| `NativeTerminalFixedStampRace.cfg` | GREEN | 없음 |
| `NativeTerminalFixedForeignNonce.cfg` | GREEN | 없음 |
| `NativeTerminalFixedForeignSource.cfg` | GREEN | 없음 |
| `NativeTerminalFixedForeignKey.cfg` | GREEN | 없음 |
| `NativeTerminalFixedCancel.cfg` | GREEN | 없음 |
| `NativeTerminalFixedTerminate.cfg` | GREEN | 없음 |
| `NativeTerminalFixedClear.cfg` | GREEN | 없음 |
| `NativeTerminalFixedStop.cfg` | GREEN | 없음 |
| `NativeTerminalNoEndGuard.cfg` | RED | NoTerminalEvidenceFreeDelete |
| `NativeTerminalAllowPartial.cfg` | RED | NoPartialTerminalRead, NoTerminalEvidenceFreeDelete |
| `NativeTerminalNoLineage.cfg` | RED | NoForeignTerminalProof, NoTerminalEvidenceFreeDelete |
| `NativeTerminalNoStampRecheck.cfg` | RED | NoStaleTerminalCommit |
| `NativeTerminalAdvanceCapture.cfg` | RED | NoCaptureGuardWidening |
| `NativeTerminalNoDurableOrdering.cfg` | RED | NoCleanupBeforeDurable |
| `NativeTerminalNoBootRetry.cfg` | RED | ClosedExecutionEventuallySettles |
| `NativeTerminalPermanentUnread.cfg` | RED | ClosedExecutionEventuallySettles |
| `NativeTerminalFixedSavedMissing.cfg` | GREEN | 없음 |
| `NativeTerminalPermanentSavedUnread.cfg` | RED | ClosedExecutionEventuallySettles |
| `NativeTerminalFixedStoredInterrupt.cfg` | GREEN | 없음 |
| `NativeTerminalFixedStoredOtherBarrier.cfg` | GREEN | 없음 |
| `NativeTerminalNoStoredOverride.cfg` | RED | NoForeignTerminalBarrier, ClosedExecutionEventuallySettles |
| `NativeTerminalBareStoredOverride.cfg` | RED | NoForeignBarrierBypass, NoTerminalEvidenceFreeDelete |
| `NativeTerminalFixedStoredStampRace.cfg` | GREEN | 없음 |
| `NativeTerminalNoStoredStampRecheck.cfg` | RED | NoStaleTerminalCommit |
| `NativeTerminalFixedStoredBarrierRace.cfg` | GREEN | 없음 |
| `NativeTerminalNoStoredBarrierRecheck.cfg` | RED | NoForeignBarrierBypass, NoTerminalEvidenceFreeDelete |
- 모델 SHA256: `ac8bc9780b6520bd1432f4fdf2d0577f0a2b504585b5f904e7dccb72d7076bda`.
- 구현 조각의 35 cfg는 `python3 scripts/ci/statecharts_tlc.py --base 200ef5a37f --jobs 4`로 PASS 35/35(25.0초)다. 아래는 설계 레그의 실행 기록이다.
- 설계 레그: 동일 소스/31 cfg를 격리한 root로 `python3 scripts/ci/statecharts_tlc.py --root <model-check> --jar /home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar --jobs 2 --report-json <model-check.json>` 실행, rc0, 56.12초. 각 cfg limit 300초 안이다.
- 원시 로그 root: `/home/adk/adk/lane-runs/i8148/logs/termdelta-1/`. CI 전체 출력 `model-check.log`, 결과/시간/coverage `model-check.json`, direct TLC 로그·반례 `tlc/`, SHA/개별 rc/state 수 `tlc-results.json`.
- `statechart` wrapper는 WSL에 없어 실행하지 않았다. 이 chart는 수동 계약 그림이며 TLC로 생성한 상태 그래프로 표시하지 않는다. 모델 검증은 Java/TLC 1.7.4와 저장소의 CI checker로 수행했다.

## 반례를 코드에 대응시킨 결과

1. BeforeInterrupt: own membership 준비 → strict interrupt row → Prompt barrier가 먼저 true → native close 결과/cleanup 불가. 실제 PG ledger의 membership 640..1001/interrupt 1001..1207 및 native.rs:452,563,635와 일치한다.
2. BeforeRaw: complete cursor C → execution Ended → strict whole tail C..L이 capture/decoder에 전달되지 않음 → 결과 None/row 잔존. capture.rs:384,403과 native.rs:491, source_evidence의 through 관문에 대응한다. 이전 HTTP raw trial은 O 답 게시 후 row 잔존을 실제 관측했으며 private seed 자체를 덤프한 증거는 아니었다.
3. AllowPartial/NoEndGuard/AdvanceCapture: 완전하지 않은 tail 또는 미확인 E를 proof로 만들거나 capture cursor를 L로 늘리면 각각 안전 규칙이 깨진다. 이를 고치는 guard는 설계의 strict whole JSON + exact Ended + 별도 terminal range다.
4. NoLineage/NoStampRecheck: foreign scope 또는 재관측된 version 변화 뒤 old proof를 쓰면 RED. 실제 구현에서는 full ticket/Source/nonce와 observed/current stamp가 담당한다.
5. NoBootRetry/NoDurableOrdering: 후보 소실 뒤 복구를 부르지 않으면 liveness RED, 결과 flush 전에 지우면 safety RED. 결과 전/후 boot cfg 및 파일 가독성 회복 cfg는 GREEN이다.
6. NoStoredOverride: 이미 저장된 wrong barrier가 fresh strict 분류 뒤에도 계속 candidate/result를 막는 RED다. FixedStoredInterrupt는 barrier를 지우지 않고 exact Cancelled supersession으로 GREEN. FixedStoredOtherBarrier는 다른 barrier를 native 결과로 넘기지 않고 기존 boundary ACK 정책만 허용한다. BareStoredOverride는 matching 검사를 지워 NoForeignBarrierBypass/삭제 규칙 RED다.
7. FixedStoredStampRace/NoStoredStampRecheck: 저장된 barrier로 supersession 후보를 만든 뒤 등록 stamp가 바뀌면 옛 proof를 버리고 재관측해 Cancelled에 도달한다. 결과 CAS의 stamp 재검증을 빼면 NoStaleTerminalCommit RED다. 앞선 31개는 저장된 interrupt cfg의 version이 0으로 고정돼 이 누락을 잡지 못했다.
8. FixedStoredBarrierRace/NoStoredBarrierRecheck: 후보 생성 뒤 현재 stored barrier가 달라지면 native 결과는 0이고 기존 boundary ACK 정책만 남는다. 결과 CAS가 후보 시점의 일치 판정을 재사용하면 NoForeignBarrierBypass/삭제 규칙 RED다.
9. PermanentUnread/PermanentSavedUnread: 실행이 닫혔어도 source를 영구히 읽지 못하면 진행성이 보장되지 않는다. 기존 Source::validate를 완화해 GREEN으로 만들지 않았고, 진단 가능한 debt 한계를 그대로 남겼다.

## 크기와 실행 계획

보정 추정은 [설계의 분할 표](native-terminal.design.md#크기와-리뷰-조각-분할) 정본이다. stored-barrier 복구까지 포함한 raw +330~600에 실측 배율 1.84~3.45를 적용하고 기존 +828을 합하면 +1436~2898이므로 PLAN_READY다. 이 레그에서는 문서·모델만 commit하고, T1(B 확정), T2(interrupt+stored barrier), T3(EOF proof 전체 경로)를 별도 review base에서 측정한다. 두 결함은 #8148 착지 전에 모두 해소해야 한다.
