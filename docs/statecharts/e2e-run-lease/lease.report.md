# Run lease · test instance FD 수명 검증

2026-11-13 KST, L0. **현재 production lease를 수정하지 않았다.** 아래 GREEN은 제안한 고정 inode flock/상속 FD/전송 journal 계약의 유한 모델 및 임시 파일 OS 실험이다. 현재 제품의 lease 안전성을 GREEN으로 승인하지 않는다.

## L0 확장 (2026-11-17)

코드 기준 `origin/main=1161162a18c87a0514f53dd7ae5dac3c830149cb`. 새 모델을 만들지 않고 이 `LeaseLifetime.tla`를 확장했다. prod 코드 변경 0, production lease 변경 0.
수용 규칙과 모델 규칙의 대응:

| 수용 규칙 | 모델 규칙 | 원인 뮤턴트(RED) |
| --- | --- | --- |
| 채널당 실행 writer 정확히 하나 | `NoDualWriter`·`NoDualGateway` | `duplicate_child`, `non_atomic` |
| TTL 만료 전 탈취 0 | `NoHolderTheft` | `ttl_takeover` |
| 부모 crash 뒤 lease 회수 | **새** `NoOrphanOwner`(owner면 그 실행의 FD가 하나 이상 열려 있다 = 마지막 FD가 닫히면 커널이 회수) + `NextRequestProgress` | **새** `crash_no_reclaim`(parent/child crash가 마지막 FD를 닫아도 owner를 비우지 않음) |
| FD 누락 시 실행 0 | `WriterHasInheritedFd` | `missing_fd` |
| (liveness) lease를 쥔 실행이 결국 끝나거나 회수 | **새** `HeldEventuallyEnds == \A r: (owner = r) ~> (owner # r)` | `live-leak_fd`(금지된 helper가 FD를 쥐어 회수되지 않음) |

`HeldEventuallyEnds`는 `FairSpec`(no-crash, startup·send·ACK·settlement·finish·parent release weak fairness)에서만 주장한다. crash가 있는 `Spec`에는 Start/Finish fairness가 없어 Held에 영원히 머무는 실행을 배제할 수 없다. crash 뒤 회수는 safety `NoOrphanOwner`와 조건부 liveness `NextRequestProgress`로 본다. liveness가 실제로 돌았다는 근거는 공통 runner의 실행 수다(`run-live-none` 4회 = safety 1 + PROPERTY 3, `HeldEventuallyEnds` 포함). 같은 속성이 `live-leak_fd`에서 RED라서 공허하게 참이 아니다.

바꾼 cfg와 결과(공통 `scripts/ci/statecharts_tlc.py` 판정, expected.tsv와 정확 일치):

| config | 변경 | safety/deadlock 위반 | liveness 위반 |
| --- | --- | --- | --- |
| run-none.cfg | INVARIANTS에 `NoOrphanOwner` | - | - |
| instance-none.cfg | 같음 | - | - |
| run-live-none.cfg | `NoOrphanOwner`, PROPERTY `HeldEventuallyEnds` | - | - |
| instance-live-none.cfg | 같음 | - | - |
| run-live-leak_fd.cfg | PROPERTY `HeldEventuallyEnds` | - | HeldEventuallyEnds,NextRequestStarts |
| instance-live-leak_fd.cfg | 같음 | - | HeldEventuallyEnds,NextRequestStarts |
| run-crash_no_reclaim.cfg | 새 뮤턴트 | NoOrphanOwner | NextRequestProgress |
| instance-crash_no_reclaim.cfg | 새 뮤턴트 | NoOrphanOwner | NextRequestProgress |

나머지 17 cfg는 바이트 변경 없이 같은 결과를 유지한다(전체 25/25 일치). `check_models.py`는 정확한 cfg 집합에 `crash_no_reclaim`을 더했고, 범위 필터가 `admission/` 하위 모델 행까지 세어 기반에서 이미 실패하던 결함을 top-level cfg만 보도록 고쳤다.

값 범위는 원인 가정이 아니라 아래 진입점 전수표에서 왔다. `git grep -n -E 'flock|LOCK_|pass_fds|set_inheritable|O_CLOEXEC|CLOEXEC|fstat|st_ino|os\.close|unlink|ttl|TTL' -- scripts/dev_instance scripts/e2e/tui_relay/lease.py`(시험 제외) 96행을 모델 동작별로 묶었다. #8124 시험 인스턴스의 FD 수명은 `scripts/dev_instance/`에 있고, run lease(`Kind=run`)는 S1에서 `scripts/e2e/continuous/`에 생겼고(아래 S1 절), 전송 journal은 아직 코드가 없는 설계 좌표다.

| 모델 동작 | 코드 좌표(실제) | 비고 |
| --- | --- | --- |
| `Acquire` 원자 flock | `scripts/dev_instance/lease.py:66`, `scripts/dev_instance/keeper.py:48`, `scripts/dev_instance/host.py:36` | `LOCK_EX\|LOCK_NB`, 실패는 busy. TTL 분기 없음 |
| 고정 inode witness | `scripts/dev_instance/lease.py:31-32`, `:45-52`, `scripts/dev_instance/keeper.py:47` | fstat device/inode·부모 디렉터리 identity 대조 |
| `Start` FD 상속 | `scripts/dev_instance/lifecycle.py:247`, `:310` | `close_fds=True, pass_fds=(lease fd,)`. `:310`은 선택적 verification FD 하나를 더 넘긴다 |
| keeper spawn(lease 전달 아님) | `scripts/dev_instance/keeper.py:134` | 제어 socketpair의 child FD만 상속한다. lease FD는 넘기지 않는다 |
| 자기 FD close만(`Finish`·`ReleaseParent`·crash) | `scripts/dev_instance/lease.py:87`, `:119` | lock leaf unlink 없음(`keeper.py:82`의 unlink는 marker 파일) |
| `ttl_takeover`·`non_atomic` 원인 | `scripts/e2e/tui_relay/lease.py:28`, `:35`, `:50` | 현재 일반 하네스 lease의 TTL·unlink. S1에서 재사용하지 않음(§6) |
| `Kind=run` channel lease | `scripts/e2e/continuous/channel_lock.py:80`(S1) | 위 S1 전수표 |
| `Prepare`/journal·`Reconcile` | 설계 좌표: 같은 §6 mutation-pending 기록 | 코드 없음 |

## S1 확장 (2026-11-18)

코드 기준 `origin/main=ffa099d4f4a6a68c26171fb464f7b5edffca2e5f`. 이 모델에 continuous runner(`scripts/e2e/continuous/`)의 off·예산 reservation·cursor 전진을 더했다. 새 설정과 뮤턴트는 `Mut`로만 켜므로 기존 25 cfg는 바이트 그대로이고 결과도 expected.tsv와 같다. `run-none`·`run-live-none`에는 새 불변식 4개를 더해 정상 GREEN이 그것까지 확인한다.

진입점 전수표(`git grep -n -E 'flock|pass_fds|enabled|budget\.|save_cursor|_abandon_orphans' -- scripts/e2e/continuous`, 시험 제외):

| 진입점 | 모델 동작 | 코드 좌표 | 모델 값 |
| --- | --- | --- | --- |
| `tick`(launchd) off | `Acquire`가 `Enabled`일 때만 | `runner.py:172` | off면 잠금·디렉터리·상태 IO 0 |
| `tick` 잠금 | `Acquire`(원자 nonblocking flock) | `channel_lock.py:91`, 키 `:26` | guild+정렬 parent+bot 집합, TTL·unlink 없음 |
| `tick` 고아 정리 | `NoOrphanOwner` 뒤 다음 보유자 | `runner.py:185` | 잠금 보유자가 본 RUNNING 기록은 ABANDONED |
| `tick` 예산 | `Reserve`/`Defer` | `budget.py:21`, `runner.py:134` | turn·token·time 한도나 예약값이 null이면 거절. crash 뒤 예약 유지 |
| `tick` 실행 | `Start`(FD 상속) | `channel_lock.py:66` | `close_fds=True`, `pass_fds=(lease fd,)`, device/inode witness |
| `tick` cursor | `Finish`의 `cursor' = seen + 1` | `runner.py:166` | 끝난 실행만 한 칸. crash·defer는 그대로 |
| `hold`(수동 E2E) | `Acquire`·`Start` | `runner.py:193` | 같은 채널 lock. enabled·예산과 무관 |
| `submit`(배포 inbox) | 모델 밖(요청 파일) | `inbox.py:22` | 비밀 없는 키 4개, request id당 1회, 최신만 실행 |

| 수용 규칙 | 모델 규칙 | 정상 cfg | 원인 뮤턴트(RED) |
| --- | --- | --- | --- |
| off일 때 runner 실행 0 | **새** `OffNoRun` | **새** `run-off` GREEN | **새** `run-ignore_off` |
| 예약 없이 provider 턴 0 | **새** `RunHasReservation` | `run-none`·**새** `run-budget_one` GREEN | **새** `run-skip_reserve` |
| 일일 예산 초과 0 | **새** `WithinBudget` | 같음 | **새** `run-over_budget` |
| cursor는 끝난 실행마다 한 칸 | **새** `CursorCountsFinishedRuns` | 같음 | **새** `run-advance_on_crash` |
| 동시 writer 0·crash 뒤 고아 owner 0·보유 lease 종료 | 기존 `NoDualWriter`·`NoOrphanOwner`·`HeldEventuallyEnds` | 기존 정상 cfg | 기존 뮤턴트 |

전/후(공통 `check_cfg`, 기존 25 cfg 결과 불변):

| config | 수정 전 모델 | 수정 후 |
| --- | --- | --- |
| run-off.cfg | SANY 실패(`OffNoRun` 없음) | - / - |
| run-budget_one.cfg | SANY 실패 | - / - |
| run-ignore_off.cfg | SANY 실패 | OffNoRun / - |
| run-skip_reserve.cfg | SANY 실패 | RunHasReservation / - |
| run-over_budget.cfg | SANY 실패 | WithinBudget / - |
| run-advance_on_crash.cfg | SANY 실패 | CursorCountsFinishedRuns / - |
| run-none.cfg, run-live-none.cfg | SANY 실패(새 불변식 없음) | - / - |

`non_atomic` 뮤턴트에서는 두 실행이 같은 cursor를 읽어 한 칸을 잃는다. 그 cfg는 새 불변식을 싣지 않아 기대집합이 바뀌지 않는다. 배포 inbox·due 경계는 산술·파일 계약이라 모델 대신 시험(`scripts/e2e/tests/test_continuous_runner.py`)이 맡는다.

## S2 확장 (2026-11-18)

S1 head `405140a73683540b2453bf3c69bd1307abd3d31d`의 리뷰 P2 네 경계를 기존
`LeaseLifetime.tla`의 트랜잭션 분기로 추가했다. 기존 lease 31 cfg는 바이트를 바꾸지 않았고
기대 위반 집합도 유지했다. 새 `tx` record는 한 배포 요청, 부족한 첫 예산 창과 사용 가능한 다음 창,
턴 한도 1, 최대 두 번째 턴 시작 시도, 각 저장 경계의 process crash 최대 1회를 탐색한다.
`TransactionMode`가 켜진 cfg만 이 분기를 실행한다. 이 격리는 파일 트랜잭션 수명과 커널 FD 수명을
각각 전수 탐색하려는 경계이며 둘을 하나의 원자 저장으로 가정하는 뜻이 아니다.

| 코드 경계 | 모델 행동 | 지키는 규칙 | 기존 의미/원인 뮤턴트 |
| --- | --- | --- | --- |
| `budget.configured`, runner `_preflight` | `TxAcquire` 앞 enabled·known guard | `OffOrNullNoWrites`: off·null이면 lock·request·cursor·result·status·outbox 무변경 | `tx-old-off`, `tx-old-null` |
| `inbox.pending`, 예산 거절 | `TxDefer` → `TxNextDay` → 재획득 | `DeferredRequestKept`: 실행 전 보류는 소비가 아님 | `tx-consume-deferred` |
| `budget.TurnGuard`, runner `_execute` | `TxBeginTurn`, `TxUsage` | `UnknownNeverZero`, `EveryTurnWithinBudget`: unknown 보존·각 native 턴 시작 전 상한 검사 | `tx-unknown-zero`, `tx-between-only` |
| runner `_record` → `_apply_completed` | `TxComplete` → `TxApply`, 각 사이 `TxCrash` | `CursorRequiresDurable`, `ProjectionExactlyOnce`: 정본 완료 전 cursor 금지·멱등 적용 | `tx-apply-first`, `tx-double-apply` |
| runner 예약 전 RUNNING crash | `TxCrash`(Held) → `TxRecover`(Deferred) | `UnreservedCrashPreservesRequest`: 예약 전이면 실제 입력 0이며 요청을 보존 | `tx-consume-deferred` |
| runner 예약 후 고아 복구 | `TxReserve` → `TxCrash` → `TxRecover` | `ReservedOrphanPausesProvider`: 예약 유지·ABANDONED·provider pause | `tx-orphan-no-pause` |
| runner `_abandon_orphans` 완료 정본 복구 | `TxRecover` → `TxApply` | `CompletedResultNeverRegresses`, `CompletedProjectionRecovers` | `tx-result-regress`, `tx-no-recover` |
| summary·status 및 outbox 발행 | `TxPublish`, `TxStatusFirst` | `StatusRequiresDurable`, `CompletedPublicationRecovers`: 정본에서 발행하며 crash 뒤 회복 | `tx-status-first` |
| `outbox.dispatch` 전송 실패 | `TxNotifyFails` | `FailedNotifyRemainsPending`: 재시도 실패도 pending 보존 | `tx-alert-drop` |

`TxComplete`는 PASS 또는 FAIL 결과를 내구 기록하고, cursor/request projection과 summary/status/outbox는
별도 행동이다. `TxApply`는 정본에 저장한 목표 cursor를 대입하므로 crash 복구로 다시 적용해도
+1을 중복하지 않는다. crash가 완료 기록 전에 일어나면 ABANDONED이며 cursor는 그대로이고,
완료 기록 뒤면 원 결과를 유지하여 누락 projection과 발행만 복구한다. 예약 전 RUNNING 기록의 crash는
실제 입력이 시작되지 않았음을 알 수 있어 요청을 DEFERRED로 남긴다. 예약 후 고아는 예약을 유지하고
사용량을 unknown으로 보아 provider를 pause한다.
`CompletedProjectionRecovers`와 `CompletedPublicationRecovers`는 `TxAdvance` weak fairness 아래 검증한다.
예약 후 고아도 ABANDONED 정본을 저장하고 summary/status/outbox 발행 생존성을 검증한다.
전송 성공은 가정하지 않으며 알림 실패의 영구 pending은 허용한다.

새 cfg 15개는 정상 `tx-good`·`tx-off`·`tx-null` 3 GREEN과 원인 뮤턴트 12 RED다.
기존 31개와 합쳐 **46/46 기대 집합 일치(9 GREEN + 37 expected RED)**를 요구한다.
원인 뮤턴트는 `Mut` 값만 다르고 모델 소스는 모든 cfg에서 같다. 실제 Python의 수정 전 RED와
GREEN·바이트 복원 뮤턴트는 구현 보고서가 정본이다. 새 모델에 예전 불변식 이름이 없어서 나는
SANY 오류를 행동 RED로 세지 않았다.

상태 차트는 [트랜잭션 수명](run-tx-good.chart.mmd)에 있다. 정상 모델 135 상태·338 step을
22 표시 상태·35 화살표로 그리며, 같은 표시 상태를 유지하는 185 step(usage 판독·FD 해제·
pending 알림의 전송 실패 등)은 숨긴다. 병렬 region으로 합치면 불가능한 조합을 그릴 수 있어
공식 생성기가 parent 상태 차트로 전환했다.
파일 전원 손실·다중 provider 합산·실제 usage parser·실제 native API 전송·알림 transport·outbox
동시 dispatcher 및 신규 요청으로 supersede되는 정책은 이 유계 모델 밖이며 Python 시험이 맡는다.
off/null 무변경을 우선하므로 완료 projection 뒤 다음 bundle의 예약값이 null이면 복구는 valid config를
기다린다. 설정은 모델 상수라 이 지연은 liveness 약속에 넣지 않았다. 프로세스 startup을 포함한
absolute monotonic deadline의 실제 시계·입출력 경계는 Python 시험이 맡는다.

재현:

```sh
python3 -B docs/statecharts/e2e-run-lease/check_models.py --self-test-selection
python3 -B docs/statecharts/e2e-run-lease/check_models.py --java /opt/homebrew/opt/openjdk/bin/java --jar /Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar --out /tmp/i8120-s2-tlc-recheck
```

statechart 스킬의 공식 생성기로 만든 차트 원본·TLC 출력은 같은 logs 아래 `s2-model-contract-chart/`이며,
view는 `s2-model-view.mjs`다. 원시 로그·matrix JSON은 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s2-model-contract-final/`,
직접 TLC 반례는 같은 logs 아래 `s2-model-contract-raw/`에 있다. 모델의 고정 입력 SHA256·상태 수와 짧은
반례는 구현 보고서의 증거 목록에도 기록한다.

## S3 소비자 사상 (2026-11-18)

S2 head `593399f0880b4991cc13dc265d454ceaf073eaee` 위에 소비자 관측 분기를 추가했다.
기존 46 cfg의 바이트와 기대 위반 집합은 유지했다. `tx.consumer`는 한 번 수신한 입력 snapshot과
읽기 결과, 독립 transport의 확인 여부를 담으며, 기존 lease·예산·cursor·실행 상태를 바꾸지 않는다.
`ConsumerIsPure`가 이를 검사한다. 새 분기는 21개 snapshot 종류 중 하나를 선택하고 형식 정규화 →
공유 판정 → 표시 순서로 진행한다. transport는 확인 성공 또는 한도 소진 실패를 선택한다.

| 입력 경계 | 모델의 표시 상태 | 실제 소비자 형식 |
| --- | --- | --- |
| 활성 supervisor, 첫 완료 전 | `NO_DATA` | result=NO_DATA |
| 명시적 off | `DISABLED` | result=DISABLED |
| interval+max_run+grace를 지난 완료 | `STALE` | result=UNKNOWN reason=STALE |
| coordinator와 다른 배포 SHA | `STALE_HEAD` | result=UNKNOWN reason=STALE_HEAD |
| schema·future 시각·heartbeat 누락·host 불가 | `UNKNOWN` | result=UNKNOWN와 해당 reason |
| 0건·skip·fixture·단언 누락·usage 불명·driver schema 미지원·필수 qualifier 미확정 | `UNEVALUABLE` | 비PASS와 해당 reason |
| 기능 FAIL에 malformed usage 동반 | `FAIL` | FAIL 증거 유지, usage는 unknown 정규화 |
| 모든 근거 충족, 의미 있는 nullable control 포함 | `PASS` | result=PASS |

`ConsumerStateMapping`·`NoGroundlessPass`·`NoOldHeadGreen`으로 빈 실행·미실행·옛 head·미지원 형식이
초록으로 바뀌지 않음을 확인한다. `usage-unknown`은 마지막 occurrence의 aggregate 숫자는 있어도
구성 counter를 확정하지 못한 경우이며, `driver-schema`는 continuous schema1과 별개의 raw schema2
검사다. `qualifier-unknown`은 필수 origin/surface/operation의 null·빈 값을 추상화하고,
`nullable` control은 busy_basis/placement의 허용된 null을 유지한다.

리뷰 P2-4는 `MalformedUsageCompletes`·`FunctionalFailRetained`로 정규화 뒤 예외 없이 완료되고
기존 FAIL을 유지하는 경계를, P2-5는 `DeferredHeartbeatIsLive`로 유효한 한도를 모두 소진해
새 driver 입력을 보류해도 살아 있는 supervisor의 heartbeat가 갱신되는 경계를 표현한다.
null 한도·예약이나 off에서 상태를 쓰지 않는 기존 `OffOrNullNoWrites` 규칙은 그대로다.

`PendingEventuallySentOrShown`은 **enabled이고 알림 snapshot을 읽을 수 있는 fair 소비자**의 계약이다.
transport가 확인하지 못하고 재시도 한도가 소진돼도 pending을 상태 표시로 노출한다.
새 `consumer-hide-pending`이 이 생존성을 위반하므로 공허하게 참이 아니다. off는 무상태 IO를
우선하며 새 알림 읽기·전송 actor나 추가 생존성을 요구하지 않는다. 외부 mirror에 이미 보존된
pending metadata는 읽을 수 있으나, 자료를 얻을 수 없는 off/host-unavailable 경우까지 전송을
가정하지 않는다. `ConsumerDispatch`는 기존 supervisor transport의 외부 사건이며 소비자의 쓰기
권한을 늘리는 동작이 아니다.

새 12 cfg = 정상 `consumer-good` 1 GREEN + 원인 뮤턴트 11 RED다. 정상 cfg가 21 snapshot 전체를
탐색하며 각 뮤턴트의 해당 snapshot도 같은 입력으로 포함한다. 전체는 **58/58 기대집합 일치
(10 GREEN + 48 expected RED)**다. 실제 Python 수정 전 RED/GREEN은 구현 보고서가 정본이며,
새 모델 이름이 S2 모델에 없어 생기는 SANY 오류를 행동 RED로 세지 않는다. 뮤턴트는 cfg의 Mut만
바꾸며 모델 소스는 같다. 원시 전체 위반 집합은 `expected.tsv`와 정확히 일치해야 한다.

정상 차트는 [소비자 상태 사상](run-consumer-good.chart.mmd)에 있다. 실제 시각의 수치 경계,
복수 오류가 동시에 있는 모든 우선순위 조합, mirror 파일 IO·head 복원·CLI subprocess, transport의
실제 수신자·재시도 간격과 credential은 Python fixture/배선 시험이 맡는다. snapshot은 불변이며
여러 파일 사이의 읽기 경쟁과 새 완료가 덮어쓰는 경계도 Python fixture가 맡는다.

원시 증거는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s3-model-first/`,
`s3-model-raw/`, `s3-model-chart/`와 `s3-model-selection.log`에 있다.
외부 요약 `report-s3-model-summary.md`에 입력 SHA256·전체 위반 집합·실행 환경을 기록한다.
재현 명령은 S2와 같고 새로운 out 경로를 사용한다. 기존 46 cfg의 결과도 새 모델에서 다시 검사했다.

## 만료된 옛 head 결과 (2026-11-19)

S3 리뷰가 넘긴 우선순위 공백을 모델에 넣었다. 다른 배포 head의 결과가 freshness 한도까지 지나면
소비자는 `STALE`이 아니라 `STALE_HEAD`를 유지한다(`result=UNKNOWN reason=STALE_HEAD stale=1`).
감시자는 `STALE_HEAD`만 경보로 삼으므로, 만료가 불일치 표식을 지우면 새 배포의 결과가 없다는 경보가 사라진다.

새 snapshot `old-head-stale`과 뮤턴트 `consumer-stale-masks-head`(만료가 head 불일치를 덮는 수정 전 판정)를
추가했다. 뮤턴트는 `ConsumerStateMapping`을 위반한다. `NoOldHeadGreen`은 두 옛 head snapshot 모두에 적용한다.
정상 cfg는 22 snapshot, 161 상태/361 step을 탐색하며 차트의 상태·화살표는 그대로다(만료된 옛 head도
"다른 배포 head" 상태로 표시). 전체는 59 cfg = 10 GREEN + 49 expected RED다. 기존 58 cfg의 바이트는 그대로다.
같은 head의 만료 결과는 계속 `STALE`이며, 두 조합의 실제 시각 경계는 Python 소비자 시험이 맡는다.

## 관측 불가 분류와 핵심 묶음 주기 (2026-11-19)

릴리스 호스트에서 상시 실행을 켜면서 두 행동을 모델에 넣었다.

**관측 불가는 FAIL이 아니다.** 소비자 snapshot에 `unobserved`(조회 전송 타임아웃, 교체된 서버 로그)를 추가했다.
표시는 `UNEVALUABLE`이고 알림 pending을 남긴다. 새 규칙 `UnobservedNeverFail`과 뮤턴트
`consumer-unobserved-fail`(관측 실패를 기능 FAIL로 세는 판정)을 더했다. 뮤턴트는 `ConsumerStateMapping`과
`UnobservedNeverFail`을 위반한다. 정상 cfg는 23 snapshot, 차트의 상태·화살표는 그대로다.

**핵심 묶음 주기.** 새 모델 [CoreSchedule](schedule/CoreSchedule.tla)은 supervisor가 배포 head와 주기 칸마다 넣는
요청을 다룬다. 배포 시작·끝(끝에서 새 head의 배포 요청 제출), 시간 경과, 다른 묶음 결과가 최신 summary를 바꾸는
경우, 요청 제출, 실행 시작·완료·crash가 행동이다.

| 규칙 | 뜻 | 깨는 뮤턴트 |
| --- | --- | --- |
| `OneCoreRunPerHeadAndSlot` | 같은 head·같은 칸의 주기 실행 기록은 많아야 하나 | `tick_id`: 요청 이름을 tick마다 새로 지음 |
| `NoStartDuringDeploy` | 배포 lock이 잡힌 동안 실행을 시작하지 않음 | `ignore_deploy` |
| `DeployRequestKeepsItsRun` | 같은 head의 주기 요청이 기다리던 배포 요청을 밀어내지 않음 | `ignore_waiting` |
| `StartedRunIsRecorded`(생존성) | 시작한 실행은 결국 결과 또는 abandoned 기록을 남김 | `drop_on_crash` |

정상 1 GREEN + 뮤턴트 4 RED, 기대 위반 집합과 일치한다. 한도는 head 2, 칸 2, 제출 4회다. 실제 시각·예산 보류·
채널 lease는 기존 LeaseLifetime 모델과 Python 시험이 맡는다. 실행 도중 배포가 시작되는 경우(그 반대 방향)는
막지 않으며, 이때 배포 스모크가 셀을 idle로 보지 못해 건너뛸 수 있다.

전체는 65 cfg = 11 GREEN + 54 expected RED다.

## 코드 기준과 범위

코드 감사 기준 `origin/main=79bebdd4eaa29b289bf88412825baa7d70a6ac77`, 레인 기반 `21c80274`다.

| 현재 main 좌표 | 관측·경계 |
| --- | --- |
| `scripts/e2e/tui_relay/lease.py:15` | cell 별칭별 경로이며 실제 parent 쌍 단일 resource가 아님 |
| `scripts/e2e/tui_relay/lease.py:34` | read 후 TTL 비교. live holder의 커널 배타권 확인 없음 |
| `scripts/e2e/tui_relay/lease.py:41` | O_TRUNC open, read와 원자적 획득이 아님 |
| `scripts/e2e/tui_relay/lease.py:44` | owner 쓰기 뒤 FD가 닫힌 후 context를 실행함 |
| `scripts/e2e/tui_relay/lease.py:50` | 이전 context가 후계 파일도 unconditional unlink할 수 있음 |
| `scripts/e2e/run_tui_relay.py:5416` | 현재 일반 하네스의 cell lease 진입 |
| `scripts/e2e/post_deploy_turn_smoke.py:428` | 현재 reset 경로의 같은 lease 사용 |

이 기존 결함은 그대로 남겨 후속 S1의 시험 전용 lease 구현과 분리했다. 운영 run/설정/프로세스/DB에 쓰지 않았고 새 kill·unlink·TTL 회수 권한을 추가하지 않았다. 제안 정본은 `docs/e2e-continuous.design.md` §6과 `docs/test-infra.design.md` §4.1이다.

`Kind=run`은 parent 쌍 입력 writer, `Kind=instance`는 bot-set Gateway child다. 같은 FD 수명 메커니즘을 각각 독립 실행하며 **instance lifetime lock이 run channel lock을 대체하지 않는다**. 한 모델은 resource 하나와 요청 A/B 각 1회만 다룬다. instance 실행은 네트워크 전송을 생략하고, run 실행은 durable pending→전송→늦은 수락→ACK/settlement→해제를 추가한다. 두 요청은 TTL/스케줄 횟수를 표현하기 위한 임의 운영 상한이 아니라 최소 경쟁 반례다.

## 불변식·생존성

- `NoDualWriter` / `NoDualGateway`: Running·Prepared·Draining의 활성 child≤1. 동일 FD를 두 child에 넘기는 뮤턴트도 RED다.
- `NoHolderTheft`: parent·driver·금지된 helper 중 FD를 가진 주체의 owner가 바뀌거나 사라지지 않는다.
- `WriterHasInheritedFd`: 실행 허용 시 driver가 FD를 직접 보유한다. startup의 실제 inode/instance witness 검증 구현은 I3a/S1에 남는다.
- `UncertainSendJournaled` / `NoNewWriterDuringUncertainty`: 미확정 전송은 내구 기록으로 남고 후계는 PausedUncertain으로 대기한다. ACK 불명 상태의 늦은 서버 수락은 새 writer와 겹치지 않는다.
- `NoSendAfterRelease` / `NoForeignCleanup`: 해제 뒤 옛 실행의 새 send와 외부 cleanup 권한은 없다. 이미 보낸 HTTP의 늦은 **수락**은 새 send와 구분한다.
- `NextRequestProgress`: clean(미완결 전송 없음·실제 처리 종료)과 **의도한 parent+driver FD가 모두 없음**이 이후 안정적으로 유지되면, weak fairness 하에 각 Pending 요청이 결국 Held로 진행한다. 이 속성은 Running 진입까지 주장하지 않는다. 정상 모델에는 helper holder가 없다. `leak_fd`는 금지된 helper가 남아 **의도한 수명이 끝난 뒤에도** 진행을 막는 반례다. 이를 ‘helper까지 포함한 모든 holder 종료’ 증명이라고 부르지 않는다.
- `NextRequestStarts`: 별도 `*-live-*.cfg`에서 `CrashEnabled=FALSE`, startup/resume/send/finish/release와 실제 ACK/수락/settlement/reconcile가 weakly fair라는 추가 가정하에 각 요청이 Running에 도달한다. 영원한 provider 장애·ACK 유실·임의 crash 하에서 진행을 보장하지 않는다.

미완결 기록 작성 직전/직후 crash, parent/driver 각각의 crash, owner 진단 기록이 없어도 FD가 남는 경우, ACK 전에 parent/driver가 모두 종료되는 경우를 탐색한다. Prepare 뒤 전송 전에 둘 다 죽어 정확한 미전송 witness가 없는 경우 기록을 자동 삭제하지 않으므로 pause가 영구 지속될 수 있다. 조건부 liveness 전제가 성립하지 않는 정직한 불확정 상태다. receipt 본문은 배타권 결정에 쓰지 않아 옛 owner receipt의 생존 추정/TTL이 권위를 갖지 않는다.

## S0 r2: 공통 CI 등록과 전체 위반 집합

2026-11-13 17:22 KST, 기반 `a5caed86eaf2f8a00faa6dba66b57b6d7c4a8264`. 공통 `scripts/ci/statecharts_tlc.py`와 `.github/workflows/statecharts-tlc.yml` 경로에 L0 **23 cfg를 빠짐없이 등록**했다. `docs/statecharts/expected.tsv`의 셋째 열은 전체 invariant/deadlock 위반, 넷째 열은 전체 liveness 위반이다. 이 절이 CI 기대값 정본이며 아래 최초 실행의 ‘첫 위반’ 표는 조사 이력이다.

수정 전 같은 공통 CLI의 `--base HEAD --head HEAD`는 `NO_TARGETS`와 함께 23개의 `config not listed in expected.tsv`를 출력하고 **exit 1 RED**였다. 첫 위반에서 멈추는 이전 개별 실행만으로는 누락된 `NoDualGateway`·`NoDualWriter`·`NoNewWriterDuringUncertainty`가 기대값에서 빠지므로 이를 CI 결과로 재사용하지 않았다.

공통 runner가 위반 safety rule을 제거하며 재실행한 뒤 각 PROPERTY를 **한 개씩** 검사하는 원래 로직으로 전체 집합을 실측했다. fairness precheck도 그대로 적용했다. source/model 분류·선택·판정에는 수정을 가하지 않았고, 외부 `run_common_logged.py`는 공통 `main`에 같은 CLI 인자를 전달하며 `run_java` 반환 원문만 보존했다. 지정 모델만 실행하기 위해 저장소 밖 scoped root에 TLA/cfg 원본 23개와 해당 expected 23행을 복사했고, 원본과 복사본의 바이트 동일성을 확인했다.

등록 후 공통 CLI **23/23 기대집합 정확 일치, exit 0, ERROR 0, skip 0**. **TLC 85회 + fairness precheck 23회**, `--jobs 2 --timeout 300`, 총 **45.5초**, cfg당 최대 **6.2초**다. 4개 정상 cfg는 전체 safety/liveness가 GREEN이고 19개 뮤턴트는 기대한 전체 위반 집합이 RED다. ‘RED가 발생했다’만으로 PASS하지 않고 각 cfg의 누락/추가 위반이 모두 없는지 공통 runner가 대조했다.

유계 조건은 resource 1개, 요청 `A/B` 각 1회, 유한 phase/FD/전송 상태이며 모델/cfg를 바꾸거나 `CONSTRAINT`·`-depth`·simulation으로 탐색을 잘라 통과시키지 않았다. run 모델의 네트워크 상태는 단일 전송으로 순서화하며 instance 모델은 전송을 생략한다. `Spec`은 획득 WF를, `FairSpec`은 no-crash cfg에서 startup/ACK/settlement/종료의 WF를 추가한다. `NextRequestProgress`는 clean과 의도한 parent+driver FD 비보유의 안정적 유지에만 조건부이고, `NextRequestStarts`는 no-crash+fair 환경에서만 주장한다. helper FD leak은 금지한 helper가 의도한 수명 뒤 진행을 막는 반례다. production 구현·실제 Discord E2E 완료를 뜻하지 않는다.

| config | 기대하는 전체 safety/deadlock 위반 | 기대하는 전체 liveness 위반 |
| --- | --- | --- |
| instance-duplicate_child.cfg | NoDualGateway,NoDualWriter | - |
| instance-foreign_cleanup.cfg | NoForeignCleanup | - |
| instance-leak_fd.cfg | - | NextRequestProgress |
| instance-live-leak_fd.cfg | - | NextRequestStarts |
| instance-live-none.cfg | - | - |
| instance-missing_fd.cfg | NoDualGateway,NoDualWriter,WriterHasInheritedFd | - |
| instance-non_atomic.cfg | NoDualGateway,NoDualWriter,NoHolderTheft | - |
| instance-none.cfg | - | - |
| instance-parent_unlock.cfg | NoDualGateway,NoDualWriter,NoHolderTheft | - |
| instance-ttl_takeover.cfg | NoDualGateway,NoDualWriter,NoHolderTheft | - |
| run-clear_on_idle.cfg | UncertainSendJournaled | - |
| run-duplicate_child.cfg | NoDualGateway,NoDualWriter | - |
| run-foreign_cleanup.cfg | NoForeignCleanup | - |
| run-leak_fd.cfg | - | NextRequestProgress |
| run-live-leak_fd.cfg | - | NextRequestStarts |
| run-live-none.cfg | - | - |
| run-missing_fd.cfg | NoDualGateway,NoDualWriter,NoNewWriterDuringUncertainty,WriterHasInheritedFd | - |
| run-no_pending.cfg | UncertainSendJournaled | - |
| run-non_atomic.cfg | NoDualGateway,NoDualWriter,NoHolderTheft,NoNewWriterDuringUncertainty | - |
| run-none.cfg | - | - |
| run-parent_unlock.cfg | NoDualGateway,NoDualWriter,NoHolderTheft,NoNewWriterDuringUncertainty | - |
| run-send_after_release.cfg | NoSendAfterRelease | - |
| run-ttl_takeover.cfg | NoDualGateway,NoDualWriter,NoHolderTheft,NoNewWriterDuringUncertainty | - |

원시 증거: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s0r2/tlc/registration-before.log`(등록누락 RED), `discovery-v2.console.log`와 `discovery-v2/`(전체 집합 발견), **`registered.console.log`와 `registered/`**(최종 23 기대일치). 각 cfg 하위 `fairness.cfg.log`와 `runN.cfg.log`는 공통 runner가 실제 실행한 원문이며 `registered/results.json`에 전체 broken/rules/runs/seconds를 기록했다. 첫 logger 시도에는 인자 위치 오류가 있어 `discovery.console.log`에 남겼으며 검증 증거에는 사용하지 않았다.

모델 SHA256 `68864d182b701d6d446b61480287be9303ac7895d76a5702eb69576882586c64`는 최초 L0와 같다. 공통 runner SHA256 `75d0532ebd0f6b64ee4dd7a0de9ad3e5e8ccb709a12e6daf0f4104ab7369debe`, fairness helper SHA256 `c370fe698546292f4ecd898467d6b84bff0c63b1d842623f8a0d5dc64fc887e9`. cfg 23개도 기반 HEAD 대비 바이트 변경 0이다. Java/TLC/OS 환경은 아래 증거 목록과 같고 cargo·PG·운영 프로세스·Discord 실행 0이다.

정확한 공통 CLI 재현(증거의 scoped root 사용, production root 아님):

```sh
JAVA_TOOL_OPTIONS=-Xmx512m python3 scripts/ci/statecharts_tlc.py --root /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s0r2/tlc/discovery-root --java /opt/homebrew/opt/openjdk@25/libexec/openjdk.jdk/Contents/Home/bin/java --jar /Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar --timeout 300 --jobs 2
```

저장소 CI는 PR diff의 expected.tsv 추가 23행으로 이 cfg들을 선택하고, nightly/manual에서는 다른 등록 모델과 함께 전부 선택한다. CI workflow를 실행·변경하지 않았다.

## 최초 개별 TLC 결과

**23 configs = 정상 4 GREEN + 원인 뮤턴트 19 expected RED, ERROR 0, skip 0.** 정상 run 224 states, instance 20 states; 실제 Running liveness의 fair/no-crash run 49, instance 16 states다. 뮤턴트 RED는 의도된 실패이며 검사 harness는 규칙명과 nonzero exit를 대조해 전체 exit 0이다.

| config | 결과 | 첫 위반 규칙 | distinct states | TLC exit |
| --- | --- | --- | ---: | ---: |
| instance-duplicate_child.cfg | RED | NoDualWriter | 4 | 12 |
| instance-foreign_cleanup.cfg | RED | NoForeignCleanup | 6 | 12 |
| instance-leak_fd.cfg | RED | NextRequestProgress | 24 | 13 |
| instance-live-leak_fd.cfg | RED | NextRequestStarts | 9 | 13 |
| instance-live-none.cfg | GREEN | 없음 | 16 | 0 |
| instance-missing_fd.cfg | RED | WriterHasInheritedFd | 4 | 12 |
| instance-non_atomic.cfg | RED | NoHolderTheft | 19 | 12 |
| instance-none.cfg | GREEN | 없음 | 20 | 0 |
| instance-parent_unlock.cfg | RED | NoHolderTheft | 9 | 12 |
| instance-ttl_takeover.cfg | RED | NoHolderTheft | 12 | 12 |
| run-clear_on_idle.cfg | RED | UncertainSendJournaled | 29 | 12 |
| run-duplicate_child.cfg | RED | NoDualWriter | 4 | 12 |
| run-foreign_cleanup.cfg | RED | NoForeignCleanup | 6 | 12 |
| run-leak_fd.cfg | RED | NextRequestProgress | 152 | 13 |
| run-live-leak_fd.cfg | RED | NextRequestStarts | 25 | 13 |
| run-live-none.cfg | GREEN | 없음 | 49 | 0 |
| run-missing_fd.cfg | RED | WriterHasInheritedFd | 4 | 12 |
| run-no_pending.cfg | RED | UncertainSendJournaled | 16 | 12 |
| run-non_atomic.cfg | RED | NoHolderTheft | 19 | 12 |
| run-none.cfg | GREEN | 없음 | 224 | 0 |
| run-parent_unlock.cfg | RED | NoHolderTheft | 9 | 12 |
| run-send_after_release.cfg | RED | NoSendAfterRelease | 30 | 12 |
| run-ttl_takeover.cfg | RED | NoHolderTheft | 12 | 12 |

수정 전/후는 **현 제품 vs 안전한 제안 모델**의 구별이다. 현재 lease의 TTL·read/O_TRUNC race·옛 unlink는 아래 Python 재생에서 각각 RED가 확인됐다. 제안에서 해당 권한/비원자 획득을 빼면 GREEN이고 같은 원인을 cfg 뮤턴트로 되살리면 RED다. production 수리를 완료했다는 뜻이 아니다.

| 원인 | 원인 재도입 결과 | 제안 기본 모델 |
| --- | --- | --- |
| TTL 경과만으로 live 임대 탈취 | run/instance `ttl_takeover`: NoHolderTheft RED | TTL 권위 없음, GREEN |
| 관측과 획득 분리·동시 writer | run/instance `non_atomic`: NoHolderTheft RED; 상태차트의 후속 경로 NoDualWriter/Gateway RED | 원자 flock 획득, GREEN |
| 부모 crash가 child의 lock까지 해제 | run/instance `parent_unlock`: NoHolderTheft RED | 자기 FD close만, GREEN |
| exec에서 FD 상속 누락을 허용 | run/instance `missing_fd`: WriterHasInheritedFd RED; 후속 부모 crash→두 writer 경로 RED | 상속 FD 보유 후 Ready, GREEN |
| helper로 FD 전파 | acquire liveness와 실제 Running liveness 모두 RED | helper 비보유, GREEN |
| journal 누락/idle snapshot만으로 삭제/해제 후 send | 각 해당 불변식 RED | ACK+settlement 대조까지 pause, GREEN |

TLC raw trace는 State 1부터 센다. 최소 실패 경로는 다음과 같다.

| 뮤턴트 | 깨진 약속 | raw TLC 최소 경로 |
| --- | --- | --- |
| ttl_takeover | 살아 있는 FD holder를 탈취하지 않음 | 1 Init→2 Acquire→3 ExpireDiagnostic→4 Acquire |
| non_atomic | 동시에 입력권을 획득하지 않음 | 1 Init→2 ObserveFree→3 Acquire→4 Acquire |
| parent_unlock | 부모 crash 뒤 child FD의 배타권 유지 | 1 Init→2 Acquire→3 Start→4 ParentCrash |
| missing_fd | driver가 FD 없이 실행하지 않음 | 1 Init→2 Acquire→3 Start |
| duplicate_child | 한 FD group에 live child 하나 | 1 Init→2 Acquire→3 Start(twin) |
| no_pending | 전송 전에 내구 기록 존재 | 1 Init→2 Acquire→3 Start→4 Prepare→5 Send |
| clear_on_idle | 늦은 서버 수락을 idle 하나로 배제하지 않음 | 위 Send까지→6 ClearFromIdleSnapshot |
| send_after_release | 해제된 옛 실행이 새 send하지 않음 | Prepare까지→5 ChildCrash→6 Send |
| foreign_cleanup | 새 외부 cleanup 권한 없음 | 1 Init→2 Acquire→3 ForeignCleanup |
| leak_fd | 의도한 holder 종료 뒤 다음 획득 진행 | 1 Init→2 Acquire→3 Start→4 ParentCrash→5 ChildCrash→stutter |
| live-leak_fd | fair/no-crash에서도 다음 Running 진행 | Acquire→Start→Prepare→Send→ACK→수락→settle→reconcile→Finish→ReleaseParent→stutter |

`statechart`는 첫 위반 뒤에도 규칙을 하나씩 제거해 탐색하므로 mutation chart 보고서의 state 수는 첫 오류에서 멈추는 위 matrix와 다르다. 위 네 필수 뮤턴트의 추가 NoDualWriter/Gateway 최소 경로는 도구 보고서의 0-based 0→5 단계다. TTL은 Acquire→TTL→Start→Acquire→Start, 비원자 획득은 ObserveFree→Acquire→Start→Acquire→Start, 부모 해제/FD 누락은 Acquire→Start→ParentCrash→Acquire→Start다. raw 전체는 evidence의 `chart-compact-*/report.md`와 `tlc-*.out`이다.

## 상태 차트

[run 상태 차트](run-none.chart.mmd)는 11 표시 상태/29 화살표, [instance FD 차트](instance-none.chart.mmd)는 5 표시 상태/7 화살표다. 도구가 같은 표시 상태 안의 단계 471/23개를 생략한다. parent FD만 닫혀도 driver FD가 남는 단계와 ACK 수신처럼 표면 상태를 바꾸지 않는 단계는 raw TLC에서 확인한다. 이 생략은 동일 상태에 머무는 단계이며 안전성 탐색에는 포함된다. 뮤턴트의 전체 차트와 최단 경로는 raw evidence에 보존했고, 위 표는 각 위반의 경로를 별도로 드러낸다.

```mermaid
stateDiagram-v2
  s0: 요청 대기
  s1: 커널 임대 획득
  s2: 이전 실행 종료 #183; 다음 요청 대기
  s3: 상속 FD로 실행 중
  s4: 모든 실행 종료
  [*] --> s0
  s0 --> s1: 커널 임대 획득
  s1 --> s2: 부모 crash #183; 자기 FD만 닫힘
  s1 --> s3: FD 상속 후 실행
  s2 --> s1: 커널 임대 획득
  s3 --> s2: 자식 crash #183; 자기 FD만 닫힘 or 자기 FD 닫고 종료
  s1 --> s4: 부모 crash #183; 자기 FD만 닫힘
  s3 --> s4: 자식 crash #183; 자기 FD만 닫힘 or 자기 FD 닫고 종료

```

## Python OS 재생

`replay_os.py`: **10/10 assertions PASS, 실패 0, skip 0**. 여기서 ‘PASS’는 명시한 관측과 기대가 일치한다는 뜻이며, current 결함 재생의 기대는 안전성 RED다.

- 현재 production `lease.acquire`를 import하고 **임시 디렉터리**에서 TTL=0(같은 만료 분기를 기다림 없이 선택) context A/B의 동시 생존, barrier로 `_read_lease` 둘 다 완료한 뒤 open하는 writer 2개, 옛 context 종료가 후계 경로를 unlink하는 현상을 확인했다. hook은 read 사이의 scheduling만 제어하며 소스 바이트는 바꾸지 않는다.
- 실제 `fcntl.flock(LOCK_EX|LOCK_NB)`은 두 번째 writer를 거부하고 마지막 FD close 뒤 재획득된다.
- 임시 wrapper→child exec에서 동일 device/inode의 FD 상속을 `fstat`으로 확인했다. **자기 wrapper만 SIGKILL(-9)** 한 뒤 child가 살아 있으면 경쟁 획득은 실패한다. child FD close를 pipe로 확인한 뒤 성공한다.
- 상속 누락과 명시 LOCK_UN 뮤턴트는 살아 있는 child에도 경쟁 획득이 성공하는 반례를 실제 OS에서 보였다. 이 프로그램은 누락을 일부러 허용하는 실험 child이며 제품 startup 코드가 아니다.
- production `lease.py` SHA256 전후 동일: `4eb66bc7d9f568b6efe1fb627f3b598892e8524ffe57a71221387036d240aca9`. journal/foreign-cleanup 뮤턴트는 production 구현이 아직 없어 모델 수준이며 실제 Discord 또는 dcserver에서 재생했다고 주장하지 않는다.

## 증거 목록

TLC matrix 원시 로그·JSON: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/l0/matrix-final/` (`results.json` 포함). OS raw: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/l0/os-replay-final.log`, `os-replay-final.json`. matrix 선택 self-test raw: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/l0/selection-check-final.json`.

실행 선택기는 필수 **정확한 23 cfg 이름 집합**을 비교한다. mock **5/5**: 23개 정상 허용; 빈 0개·필수 누락 22개·unknown 단독 1개·unknown 추가 24개 모두 거부. glob 0건의 `all([])`나 축소 matrix가 PASS할 수 없다. 이 보강은 TLC spec/cfg 바이트를 바꾸지 않아 이미 완료된 23개 결과를 재사용했다.

| 입력/실행물 | SHA256 |
| --- | --- |
| LeaseLifetime.tla | `68864d182b701d6d446b61480287be9303ac7895d76a5702eb69576882586c64` |
| check_models.py (초기 first-violation 실행) | `7ee5ac094575b548ed010eb212fea38156d5005138fb4a9fa64a3cbbb91bd30c` |
| check_models.py (r2 공통 adapter) | `892e7640996f113b52c9fd421090d8570b462a10dcd98dd30f93da3d9279681a` |
| replay_os.py | `0ede9764cbd2d2413c6c47b40d0796cf177b0b3d0eeee81206a279deb9290d4d` |
| TLC jar | `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88` |
| Java binary | `8cccd49f6c87c6762b9753f7e7415d15b7043ffa12be57eb9a93b39725ac3273` |

각 cfg SHA256은 matrix JSON에 있다. 뮤턴트는 cfg의 Mut만 달라 **TLA 원본 복원 작업 자체가 없다**. 23 결과 모두 같은 spec SHA256이고 마지막 정상 `run-none`이 모든 run mutation 검사와 같은 바이트를 사용했다. 현재 production 소스도 OS 재생 전후 같았다.

환경: macOS aarch64, Homebrew OpenJDK 25.0.5.0.3, TLC 2.19/tla2tools 1.7.4, Python `3.14.10`, `SHELL=/bin/zsh`, `TMPDIR=/var/folders/7h/1gr2yb5933b36t80rxpj60lr0000gn/T/`. TLC worker=1, heap=512MiB. cargo profile=N/A, cargo/PG/Discord/provider 실제 실행 모두 0. 모델 helper는 임시 pipe와 파일만 쓰고 종료했다. 차트 보조 view는 저장소 밖 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/l0/lease.view.mjs`에 있으며 검증 정본 언어는 TLA+와 Python이다.

재현(기존 Java/TLC 경로 지정, 새 out 디렉터리 사용):

```sh
python3 -B docs/statecharts/e2e-run-lease/check_models.py --self-test-selection
python3 -B docs/statecharts/e2e-run-lease/check_models.py --java /opt/homebrew/opt/openjdk@25/libexec/openjdk.jdk/Contents/Home/bin/java --jar /Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar --out /tmp/i8120-l0-recheck
python3 -B docs/statecharts/e2e-run-lease/replay_os.py --out /tmp/i8120-l0-os-recheck.json
TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk@25/libexec/openjdk.jdk/Contents/Home/bin/java JAVA_TOOL_OPTIONS=-Xmx512m /Users/itismyfield/.codex/skills/statechart/scripts/statechart docs/statecharts/e2e-run-lease/LeaseLifetime.tla --config docs/statecharts/e2e-run-lease/run-none.cfg --view /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/l0/lease.view.mjs --out /tmp/i8120-l0-chart-recheck
```

## 남는 구현 전제

고정 inode와 동일 open-file-description 전달, host 단일성, 정확한 parent/bot-set resource key, 실제 `fstat`/instance witness 검증, startup admission 앞 fail-closed, CLOEXEC 복구, mutation journal의 atomic/durable 쓰기 및 실제 nonce/ACK 대조는 아직 제품 구현으로 증명하지 않았다. 모델은 파일시스템 전원 손실·inode 교체·NFS/SSH 분산 임대·실제 Discord Gateway·provider 내부 상태·receipt 기반 cleanup 알고리즘을 탐색하지 않는다. `foreign_cleanup`은 권한 추가 금지 계약의 mutation assertion이며 PID 소유권 판별 구현의 증명이 아니다. helper FD 전파 금지는 모델과 liveness 반례로 확인했으며 실제 provider/daemon spawn 배선 검증은 #8124 I3a에 남는다.

<!-- doc-path-refs: external /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s3-model- /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s2-model- /tmp/i8120-s2- /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/l0/ /tmp/i8120- -->

자체 `check_models.py`는 정확한 23 cfg 선택과 JSON 증거 보관용 adapter로 유지한다. 기대 집합은 `docs/statecharts/expected.tsv`에서 읽고, 실행·fairness·전체 위반 판정은 `scripts/ci/statecharts_tlc.py`의 `check_cfg`에 위임해 중복 기대값/판정기를 제거했다. `--timeout 300 --jobs 2`는 공통 CI 기본 실행 인자와 같다.
