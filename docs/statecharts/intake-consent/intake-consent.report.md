# Intake consent authority — r2 구현 경계 검증

모델 head: `df492f9412` (production 변경 전). 모델 기준 production은
`08e2336e47986d70d0cbe95e56215858377aed5c`이며 이번 검증의 선행 병합은
`79b6b405f144552229b4bef5bba16e5b5762ee8a`다.

2026-11-11 17:41–17:43 KST에 Mac에서 저장소 검사기를 실행했다.
기존 9 cfg가 모두 선택되어 기대한 invariant 집합과 정확히 일치했다(exit 0,
64.0초). Current·Proposed는 GREEN, D1Public·ScopeOpen은 효과 뒤 rollback과
무권한 발급 규칙이 RED다. 모든 cfg에서 두 PROPERTY는 깨지지 않았다.
기존 `SC_RUN=mac-needed` 예측을 이 실제 결과가 대체한다.

## 실행과 실제 결과

```sh
export TLA2TOOLS=$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py \
  --base 79b6b405f144552229b4bef5bba16e5b5762ee8a --head df492f9412 --jobs 3
```

당시 `--head HEAD`로 실행했으며 HEAD는 위 모델 head였다. 검사기는 head diff로
cfg를 고르고 실제 모델 내용은 working tree에서 읽는다. 이후 Rust 구현은 모델
전이를 바꾸지 않는다. cfg별 제한 300초, worker 1, fp 0, MaxClaims=2다.

| cfg | 실제 깨진 invariant | 공식 검사 시간 | 전수 distinct states |
|---|---|---:|---:|
| Current | 없음 | 17.6s | 15,482 |
| D1Public | NoUnauthorizedIssue, NoEffectsThenRollback | 43.4s | 67,364 |
| Proposed | 없음 | 11.6s | 15,922 |
| Forge | NoUnauthorizedIssue | 37.3s | 74,126 |
| Reuse | ProofConsumedOnce | 14.9s | 29,234 |
| Stale | NoStaleProofWrite | 9.1s | 16,946 |
| Bookkeeping | RollbackExactAndBookkeeping | 19.2s | 15,922 |
| OwnerOnly | NoStaleProofWrite | 12.6s | 15,998 |
| ScopeOpen | NoUnauthorizedIssue, NoEffectsThenRollback | 15.4s | 29,096 |

전수 상태 수는 별도 TypeOK-only TLC 실행에서 queue=0까지 탐색한 값이다.
RED cfg를 첫 위반에서 멈춘 상태 수와 혼동하지 않는다. 위 집합 판정은 공식
검사기가 위반 규칙을 하나씩 제거하고 재실행한 결과이며, TypeOK-only 결과로
안전성을 주장하지 않는다. ProofEventuallySettles·TransactionEventuallyEnds는
공식 검사기가 각 PROPERTY를 따로 검사했다.

## 같은 프로세스의 pre-accept rearm

기존 Restart→ReclaimPreAccept 외에 다음 행동을 추가했다. 새 cfg는 필요하지
않으며 기존 9 cfg의 Next에 모두 포함된다.

- `SweepPreAccept(i)`: 같은 epoch의 claimed row를 pending으로 돌리고 이전
  worker의 pc를 claimed로 남긴다. `worker_status.rs:394–414` stamped sweep은
  실행 중 worker를 중단시키지 않으며, `Change::from` :226–233은 Rearm을
  claimed에만 허용한다.
- `CancelPreAccept(i)`: 같은 상태 전이 뒤 해당 worker를 done으로 만든다.
  `intake_worker.rs:310`와 :529–545가 반환 뒤 executor에 진입하지 않는 경로다.
- 기존 Claim은 process token을 재사용할 수 있고, 기존 AdvanceStatus는
  serial이 아닌 실제 stamp/status로 비교한다. 가상의 새 DB fence를 넣지 않았다.

탐색용 임시 invariant `NoOldWorkerResume`를 저장소 밖 모델 사본에만 추가했다.
`epoch=0 /\ a[1].pc="accepted" /\ a[2].pc="claimed" /\ same claim` 상태를
금지하는 이 규칙은 의도대로 exit 12였다. 실제 최단 trace 5개 상태:
Initial → Claim(1,worker) → SweepPreAccept(1) → Claim(2,worker) →
AdvanceStatus(1,claimed,accepted). 두 claim_owner는 같은 token이고 restart는 없다.
따라서 P2가 지적한 ABA 도달 상태를 실제로 탐색한다. 이후 spawned CAS의
status='accepted' 경쟁에서는 한 worker만 성공하며, official 9 cfg의 기존
효과·rollback·소비 규칙에는 새 위반이 없다.

코드 대조에서 stamp token이 재사용될 수 있음을 숨기지 않는다.
`worker_status.rs:246–288`은 claimed/accepted 상태와 claim_owner·owner stamp를
검사하며 serial을 검사하지 않는다. 이 모델 확장 때문에 코드에 새 rearm,
cleanup 또는 중단 권한을 추가하지 않았다.

## 실제 P1 trace와 권한 경계

D1Public의 NoEffectsThenRollback singleton 실행은 exit 12, 13개 상태였다:
Claim(sibling) → accepted → spawned → D1RetainReceipt →
EnterExecutorWithoutProof → ExecutorEffectsWithoutProof →
D1ConvertOutsideScope → RollbackMove → LockOwner → ReadForUpdate →
PrepareCAS → Commit. 최종 commit에서 effects=1인 원본이 pending이 된다.

ScopeOpen도 exit 12, 14개 상태였다. 효과 이후 OpenKeyOutsideScope →
EnterLateScope → Take → RollbackMove를 거쳐 같은 rollback이 성립한다.
NoUnauthorizedIssue와 별도로 singleton invariant를 실행하여 효과 뒤 rollback
trace가 앞선 무권한 발급 위반 때문에 가려지지 않게 했다.

이것은 기각된 d1 설계와 key 공개 mutant의 반례다. 실제 Current는 spawned CAS가
bool이고 공개 receipt/변환이 없다. r2도 private key를 요구하는 issuer를 통해
기존 private scope만 발급하므로 sibling의 늦은 authority 취득이 막힌다.
DB exact row 비교는 이미 발생한 효과를 감지하지 않는다. Rust privacy·PG 시험이
이 모델과 실제 구현의 대응을 별도로 확인해야 한다.

## 관측 간극과 범위

ObserveAndMove(TRUE) 뒤 ChangeConsent(FALSE)가 일어나도 CAS/commit은 가능하다.
이는 기존 load/검증 → begin → owner lock → row lock → CAS → commit 순서의
FS 관측 간극이다. Current/Proposed GREEN은 commit 시점 consent 파일의 최신성을
보장하지 않는다. FS/PG cross-store lock이나 재관측을 새로 모델링하지 않았다.

기존 TypeOK, NoUnauthorizedIssue, ProofConsumedOnce, NoStaleProofWrite,
RollbackExactAndBookkeeping, ReleaseOnce, NoEffectsThenRollback을 유지했다.
소비는 linear move/drop이며 SQL mutation 성공 횟수와 같지 않다. 두 진행성 규칙은
각 authority의 Advance와 release에 대한 weak fairness를 전제로 한다.
CHECK_DEADLOCK FALSE만으로 진행성을 주장하지 않는다.

OwnerOnly는 생산 owner transfer가 같은 transaction에서 old open rows를 retire하는
기존 결합을 제거한다. 실제 `owner_record/custody.rs:190–203`, :323–324,
:383–392와 `replay_disposition/write.rs:217–228`은 old spawned row를 unknown으로
바꿔 status CAS를 막는다. 새 active-owner hold query나 cleanup은 없다.

MaxClaims=2, epoch 0/1, 한 번의 drift와 consent 변경을 탐색한다. restart는
in-memory key/receipt/proof를 폐기하고 spawned row를 자동 rearm하지 않는다.
완전한 routing·operator/replay child transaction·FS publication 내부·hash 충돌·
시계 역행·영구 장애는 범위 밖이다. legacy null/half stamp, wire bytes, 실제
private constructor와 API 조합은 Rust/PG 시험의 책임이다. raw SQL·unsafe Rust·
임의 반복 executor 호출에 대한 durable exactly-once ledger를 주장하지 않는다.

## 증거

로그 루트:
`/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/logs/s4-07-3/`.

- `model-check.log`: 공식 9 cfg 선택, 실제 broken 집합, PROPERTY 결과, exit 0.
- `model-evidence.py`, `model-evidence.json`, `model-evidence.log`: 별도 raw TLC
  재현 명령·전수 counts·exit·시간. 저장소 파일은 수정하지 않는다.
- `model-IntakeConsent*-TypeOK.log`: cfg별 전체 생성/distinct/queue와 SANY 결과.
- `model-IntakeConsentD1Public-NoEffectsThenRollback.log`,
  `model-IntakeConsentScopeOpen-NoEffectsThenRollback.log`: 실제 P1 singleton trace.
- `model-IntakeConsentProposed-NoOldWorkerResume.log`: same-process ABA 도달 trace.

`intake-consent.chart.mmd`는 수동 개요도이며 전체 TLC 그래프가 아니다. 내부 lock
전이를 합치고 authority 인스턴스 사이의 교차 경합은 위 실제 trace로 보충한다.
