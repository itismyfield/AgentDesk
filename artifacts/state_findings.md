# 상태 불변식 실패

속성·모델 테스트(`tests/state_properties_runner.rs`, `--features state_properties`)가 재현한 실패만 적는다. 한 번 전체 통과 예산은 약 7.9초이고, 같은 바이너리를 여러 번 돌려 합계 약 1분 분량의 모델 실행에서 아래 8건이 같은 최소 시퀀스로 반복됐다. 메일함 중복은 랜덤 시퀀스와 고정 시퀀스 둘 다에서 나왔다.

통과한 모델: 배달 임대 상태기계(48사례, 길이 1–24), 임대 경합 48라운드×8스레드, 최종화 임의 시퀀스 24사례, 동시 터미널 24×8, 메일함 임의 시퀀스 중 중복 예약에 닿기 전의 사례, 칸반 자유 경로·리듀서 워크, 캠페인 명령 재생, 세션 호스트 투표·가드, 큐 prefix 80사례, advisory lock 16×6.

## 1. 빈 배달 범위도 임대를 잡는다

- 위치: `src/services/discord/delivery_lease_cell.rs:694` (`try_acquire`)
- 최소 시퀀스: `try_acquire(key, Sink, start=11, end=0, deadline=10)`
- 관찰: `true`, 셀은 `Leased`
- 기대: `[start, end)` 가 비거나 뒤집히면 `false` 이고 셀은 `Unleased`
- 원인: CAS 전에 `start < end` 를 보지 않는다. 빈 범위가 채널의 유일한 배달 권을 차지한다.
- 고침: `start >= end` 이면 태그를 바꾸기 전에 `false` 를 반환한다.

## 2. 식별자가 반쪽인 id-0 턴이 한 임대 키로 합쳐진다

- 위치: `src/services/discord/delivery_lease_key.rs:91` (disambiguator 한쪽이 없으면 둘 다 버리는 분기, 반환은 116행 근처)
- 최소 시퀀스: 같은 채널·generation 에서 `new(..., user_msg_id=0, started_at="turn-a", offset=None)` 와 `started_at="turn-b"`
- 관찰: 두 키가 같다. `turn_started_at` 과 `turn_start_offset` 이 모두 `None`
- 기대: `started_at` 이 다르면 임대 신원도 다르다. 한쪽 키로 잡은 임대를 다른 턴이 commit 할 수 없어야 한다.
- 원인: id-0 이고 `started_at` 또는 offset 중 하나라도 없으면 레거시 축퇴 키 `(channel, generation, 0)` 로 떨어진다. 서로 다른 턴이 한 임대를 공유한다.
- 고침: 있는 식별자는 키에 남긴다. 둘 다 없을 때만 축퇴 키를 쓴다.

## 3. 최종화 장부는 프로세스를 넘기면 같은 턴을 다시 소유한다

- 위치: `src/services/discord/turn_finalizer.rs:616` (액터 로컬 `HashMap` 장부). 계약은 321행: `Finalized` 는 이 제출이 사후 정리를 소유한다는 뜻. 실제 반환은 `src/services/discord/turn_finalizer/finalize.rs:357`
- 최소 시퀀스: `register_start` → `Complete` → 액터 drop → 새 `TurnFinalizer` 에 같은 키로 `Complete`
- 관찰: 첫 결과 `Finalized`, 재시작 뒤에도 `Finalized`
- 기대: 두 번째는 `AlreadyFinalized`. 재시작이 같은 턴의 정리 소유권을 다시 주지 않는다.
- 원인: exactly-once 장부가 액터 메모리에만 있다. 에피소드가 없는 제출은 `claim_episode` 가 `Ok(None)` 이라 새 액터가 고아 엔트리를 만들고 다시 `Finalized` 를 돌려준다.
- 고침: 최종화된 `(channel, generation, user_msg_id)` 를 디스크나 DB에 남기고, 재시작 후 첫 제출은 그 기록을 보면 `AlreadyFinalized` 를 반환한다.

## 4. 현재 세대가 아닌 터미널은 기록 없이 버려진다

- 위치: `src/services/discord/turn_finalizer/terminal_handler.rs:18`
- 최소 시퀀스: 부트 `current_generation = 0`, `TurnKey { user_msg_id: 15, generation: 1 }` 에 `register_start` 없이 `Complete`
- 관찰: `AlreadyFinalized`. 장부에 엔트리가 생기지 않는다.
- 기대: 그 키의 유일한 터미널이면 고아 최종화가 한 번 일어나고, 같은 키의 재제출만 `AlreadyFinalized` 다.
- 원인: 에피소드가 없고 generation 이 프로세스 세대와 다르면, 장부에 맞는 엔트리가 없을 때 25행에서 바로 `AlreadyFinalized` 를 반환한다. 호출자는 이미 끝난 턴으로 보고 아무것도 하지 않는다.
- 고침: 매칭 엔트리가 없고 같은 채널·세대에 살아있는 턴도 없으면 현재 세대와 같이 고아 최종화로 떨어뜨린다. 이미 최종화된 엔트리만 `AlreadyFinalized` 로 둔다.

## 5. 빼낸 헤드를 다시 enqueue 하면 예약과 큐에 동시에 남는다

- 위치: `src/services/turn_orchestrator/queue_enqueue.rs:25` (큐 벡터만 보고 `SourceIdAlreadyQueued`). 예약은 `pending_user_dispatch` 인데 `src/services/turn_orchestrator/claim_observation.rs:164` `enqueue_refusal` 도 그 예약을 보지 않는다.
- 최소 시퀀스: `Enqueue(6)` → `Take` → `Enqueue(6)`
- 관찰: 두 번째 enqueue 가 `enqueued=true`. 스냅샷에서 6 이 `pending_user_dispatch` 이면서 큐에도 있다.
- 기대: 두 번째는 거절(`SourceIdPendingOrActive`). 한 입력은 예약 또는 큐 중 한곳에만 있다.
- 원인: `Take` 는 헤드를 큐에서 빼고 디스패치 예약만 남긴다. 그 다음 enqueue 는 큐에 같은 id 가 없으므로 통과하고, 예약된 헤드와 새 큐 항목이 둘 다 나중에 디스패치될 수 있다.
- 고침: enqueue 거절에 `pending_user_dispatch` 와 `pending_user_dispatch_source_ids` 를 포함한다. 그 id 의 재진입은 예약이 풀리기 전에는 `SourceIdPendingOrActive` 다.

## 6. 소진된 품질 예산이 다음 사이클에서 다시 센다

- 위치: `src/services/campaign_execution/failure_budget.rs:20`
- 최소 시퀀스: `quality_repairs = 0`, `record_quality_failure(cycle=0)` 다음 `record_quality_failure(cycle=1)`
- 관찰: 첫 호출 `Exhausted`, `quality_failures = 1`, `disposition = Failed`. 둘째 호출이 다시 세어 `quality_failures = 2`
- 기대: 노드가 `Failed` 가 된 뒤의 호출은 `AlreadyCounted` 이고 카운트는 1 에 남는다.
- 원인: 소진 분기(25행)는 `repair_cycles` 를 올리지 않는다. 카운트 키는 `(series, cycle)` 이라 더 큰 cycle 은 20행 가드를 통과한다.
- 고침: `disposition == Failed` 이면 즉시 `AlreadyCounted`. 또는 소진 때 `repair_cycles` 를 더 이상 새 cycle 이 통과하지 못하게 올린다.

## 7. idempotency 응답 기록이 첫 응답을 덮어쓴다

- 위치: `src/db/idempotency.rs:230`
- 최소 시퀀스: 같은 `(scope, key, fingerprint)` 로 `claim` → `record_response(200, {"ok": 0})` → `record_response(500, {"ok": "replaced"})` → `claim`
- 관찰: 재생 status `500`, body `{"ok":"replaced"}`. 동시 `claim` 자체는 소유자 1명으로 유지됐다.
- 기대: 첫 기록(200, `{"ok": 0}`)이 재생된다. 늦은 두 번째 `record_response` 는 no-op.
- 원인: UPDATE 가 `response_status IS NULL` 을 조건으로 하지 않는다. 완료된 행도 다시 쓴다.
- 고침: `WHERE scope=$3 AND key=$4 AND response_status IS NULL`. 이미 기록이 있으면 `Ok` 이되 본문은 바꾸지 않는다.

## 8. dispatch outbox claim 이 살아있는 소유자를 빼앗는다

- 위치: `src/db/dispatches/outbox/claim.rs:74`
- 최소 시퀀스: pending 행을 `select_pending_dispatch_outbox_claim_candidates_pg` + `mark_dispatch_outbox_claimed_pg(owner=worker-0)` 로 커밋한 뒤, 다른 트랜잭션에서 `mark_dispatch_outbox_claimed_pg(owner=thief)`
- 관찰: 두 번째 mark 가 `Ok(claimed_at)`. `SKIP LOCKED` 로 고른 첫 승자는 한 명이었다.
- 기대: 이미 `processing` 이고 다른 `claim_owner` 가 잡은 행의 mark 는 실패한다.
- 원인: UPDATE 의 WHERE 가 `id` 뿐이다. 상태·소유자 CAS 가 없다. 재시도 SQL(`schedule_outbox_retry_pg`)은 소유자와 `claimed_at` 을 맞추는데 mark 는 그렇지 않다.
- 고침: `AND status = 'pending' AND (claim_owner IS NULL OR claim_owner = $2)` 를 넣고, 0행이면 에러를 반환한다.
