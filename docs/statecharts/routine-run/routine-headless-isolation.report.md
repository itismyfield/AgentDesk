# headless run 의 per-run 격리 컨텍스트 모델 (#8117 PR3a D0)

코드 기준 `03c3c5e2bfa1ca99d1e304c0e9dd92fdb8717268` 위 PR3a 조각, 2026-11-14 KST. 한 headless routine run 이 에이전트 primary 채널에서 시작·완료·재시작 복구되는 동안 **primary 컨텍스트가 바뀌지 않는다**는 규칙만 모델링했다. 실행 단위의 exactly-once 는 `RoutineRun`, 연속 실패 대응은 `RoutineFailureEscalation` 이 계속 맡고 두 모델의 source 는 이 조각에서 바뀌지 않았다.

## 모델 경계

- `primary` 는 세션 id·history·uploads·clear boundary·binding·memory 표식·`cleared`·in-memory 존재 여부를 한 레코드로 묶은 값이다. Init 이 `cleared`/`present` 를 비결정으로 고르고, 모델 안에서 primary 를 바꿀 수 있는 주체는 headless run 뿐이다. primary 자신의 턴은 넣지 않았다(mailbox 공유·양방향 대기는 PR3b D).
- `evidence` 는 launch metadata 의 판독 결과다: `ok`, `missing`(per-run 키를 만들 label 없음), `unreadable`(표식 판독 불가). `ok` 가 아니면 run 은 실행 0으로 거절된다.
- `pin` 은 시작 때 turn row 에 저장되는 격리 표식이다. 완료·provider 재시도·재시작 복구는 현재 정의(`definition`)가 아니라 이 pin 을 읽는다. `PatchDefinition` 은 실행 중 정의를 thread 로 바꾼다.
- `Complete` 는 완료 postlude, `Settle` 은 transcript 증거 유무에 따른 run 종결(증거가 없으면 기한에서 failed)이다. `Restart`→`Recover` 는 프로세스 재시작과 boot 복구다.
- 공정성: Start·Refuse·Answer·Complete·Settle·Recover 는 약한 공정성. Restart·ProviderRetry·PatchDefinition 은 선택 사건이다.

## 규칙

| 규칙 | 뜻 |
|---|---|
| `PrimaryUnchanged` | primary 세션·history·uploads·boundary·binding·memory 는 run 시작 전 값 그대로다 |
| `PerRunKeyOnly` | run 이 세션 행을 쓰는 키는 per-run 키뿐이다 |
| `NoPrimaryFallback` | 증거가 없거나 판독이 실패하면 실행 0 |
| `IsolationPinned` | 시작한 run 은 per-run pin 을 갖는다 |
| `NoRerunAfterRestart` | 재시작 뒤 Started run 재실행 0 |
| `EveryRunTerminates` (liveness) | 모든 run 은 종결한다 |
| `FinalizedRunSucceeds` (liveness) | 완료 postlude 까지 간 run 은 succeeded 로 종결한다 |

## 12cfg 결과

공식 checker `scripts/ci/statecharts_tlc.py --base 03c3c5e2`(tla2tools 1.7.4) 로 실행했다. GREEN 1, 기대 RED 11, expected.tsv 와 전부 일치.

| cfg | Policy | 깨지는 규칙 |
|---|---|---|
| `RoutineHeadlessIsolation` | `isolated`(수정 후) | 없음. 116 distinct states |
| `…Before` | `shared_path`(수정 전: 표식 없이 기존 fresh 경로) | PrimaryUnchanged·PerRunKeyOnly·NoPrimaryFallback·IsolationPinned |
| `…SharedClear` | 시작이 channel clear boundary·provider 세션 clear 를 수행 | PrimaryUnchanged |
| `…SharedTake` | 시작이 primary uploads 소비·`cleared` reset·세션 생성 | PrimaryUnchanged |
| `…SharedWriteback` | 완료가 session id·history 를 primary 에 기록 | PrimaryUnchanged |
| `…MemoryEffect` | 완료가 primary 의 memory 표식을 올림 | PrimaryUnchanged |
| `…RetryClear` | provider 재시도가 primary provider 세션을 clear | PrimaryUnchanged |
| `…RecoverySeed` | 복구가 run 의 provider 세션으로 primary 를 seed | PrimaryUnchanged |
| `…PrimaryKeyFallback` | label 없는 run 을 primary 키로 실행 | PrimaryUnchanged·PerRunKeyOnly·NoPrimaryFallback·IsolationPinned |
| `…PlanReadsPrimary` | 격리 시작 + 완료 계획이 primary 세션을 읽음 | FinalizedRunSucceeds (liveness) |
| `…RerunStarted` | 복구가 Started run 을 다시 실행 | NoRerunAfterRestart |
| `…Repin` | 완료·복구가 pin 대신 현재 정의를 읽음 | PrimaryUnchanged·FinalizedRunSucceeds |

`PlanReadsPrimary` 가 이 조각의 liveness RED→GREEN 이다. 시작만 격리하면 primary 의 `cleared` 를 reset 하지 않고 세션도 만들지 않으므로, 완료 계획이 primary 세션을 읽는 한 `cleared` 이거나 세션이 없는 채널에서 run 의 transcript 가 기록되지 않아 run 이 기한 실패로 끝난다. 격리 run 의 transcript 판단을 primary 세션에서 떼어 내면 GREEN 이다.

## 모델 밖

- mailbox 공유와 사람 입력의 양방향 대기, `process_mailbox` retire scope(PR3b D).
- Started 뒤 history retry·fallback 금지(PR3b D). 모델의 `ProviderRetry` 는 같은 턴 안의 세션 reset 만 뜻한다.
- 세션 행의 identity 종류. 구현은 per-run 키의 상태 기록에 채널 소유가 아닌 기존 격리 identity 를 쓴다. 모델은 "per-run 키에만 쓴다"까지만 본다.
- 실제 provider process·Discord 전달·PG 저장. 구현 시험이 맡는다.
