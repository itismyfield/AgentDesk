DESIGN_DONE base=2239d6c475517b1b219f4a2bcbe0b3bb8470c4bd cmds=abort-release(released,releasing) est=5/+255

# #5340 i5340_home_escape_cli 설계 r1 — channel-home 탈출 CLI

- 기준: worktree `/Users/itismyfield/adk-lanes/adk-escape-cli`, HEAD = base = `2239d6c475517b1b219f4a2bcbe0b3bb8470c4bd`(`git rev-parse HEAD`로 확인). 소스·커밋·cargo·운영 접근은 하지 않았다(읽기 전용 설계). 운영 GET도 하지 않았다. 이 설계에 live 값이 필요하지 않다.
- 입력: twomac 회신 `design-twomac-r1.out.md` R2(229~277행), §7 승인표(1263~1290행), 시나리오별 롤백 절(S3 730행, S4 774행 등).
- 결론 요약
  - **지원 1 — `abort-release` Released 갈래**: Released → row 제거. 설계 회신의 후속 코드안과 같다. 단, 회신의 "target=local gateway" 조건은 데이터 모델상 성립할 수 없어서 바꿨다([설계 이탈-1], §2.1).
  - **지원 2 — `abort-release` Releasing 갈래**: Releasing → row 제거. 실행 노드가 holder(=gateway)이고 exact epoch일 때만 허용한다. drain 완료와 경합해도 row CAS가 한쪽만 이기게 한다. 안전 근거는 §2.2에 있다. 맥이 거절하면 이 갈래(SQL 한 갈래와 시험 2개)만 빼면 된다.
  - **미지원 — Orphaned 탈출**: PG만으로는 옛 holder 노드의 turn·O ledger·pane 상태를 증명할 수 없다. 운영 절차는 "twomac 중 force 금지, Orphaned가 되면 실측 중단 후 사용자·맥 결정"이다(§2.3).
  - 새 플래그·원장·방어 장치·테이블·migration은 만들지 않는다. 기존 `o_channel_homes` row에 조건부 DELETE 1문(기존 `remove_reclaimed`와 같은 방식)만 더한다. `queue_enqueue.rs`·`injected_inputs.rs`·`lock_inflight_state_path`는 건드리지 않는다.

---

## 1. 상태 전이 전수표 (base 2239d6c47)

스키마 제약(`migrations/postgres/0133_o_channel_homes.sql:16-28`):
- holder는 Releasing·Worker·Reclaiming에서만 NOT NULL이다. holder가 있으면 renewed_at도 NOT NULL이다.
- Worker는 target IS NULL이다. Orphaned의 target은 자유다. 나머지 상태는 target NOT NULL이다.
- holder ≠ target이다.
- epoch는 `o_channel_home_epochs` sequence에서 받으므로 한 채널의 생애 안에서 다시 나오지 않는다(`src/db/o_channel_homes.rs:1-3`).

### 1.1 쓰기(만들기·바꾸기·지우기)

| # | 전이 | 진입점(호출 경로) | DB 함수·CAS 조건 | 새 epoch |
|---|---|---|---|---|
| W1 | 없음 → Releasing(holder=G, target=H) | CLI `delegate` `src/cli/channel_home.rs:27-34,78-90,160-171` | `delegate` `o_channel_homes.rs:253-276`: `INSERT … WHERE NOT EXISTS row … ON CONFLICT DO NOTHING` | 예 E0 |
| W2 | Releasing → Released(holder NULL, target=H 유지) | G watch `channel_home_boot.rs:663-675` → `drain_round` `channel_home_drain.rs:133-208`(200행) | `finish_release`→`leave` `o_channel_homes.rs:280-313`: `state='releasing' AND holder=$G AND epoch=$E0` | 예 E1 |
| W3 | Released → Worker(holder=H, target NULL) | H watch `channel_home_boot.rs:646-658`(`releases_here && port.restored()`) | `adopt` `o_channel_homes.rs:326-344`: `state='released' AND target=$H AND epoch=$E1` | 아니오(E1 유지) |
| W4 | Releasing·Worker·Reclaiming 갱신(renewed_at만) | lease `channel_home.rs:930-950`(936), drain resume `channel_home_drain.rs:115-121` | `renew` `o_channel_homes.rs:392-418`: `holder=$h AND epoch=$e AND state IN (releasing,worker,reclaiming)` | 아니오 |
| W5 | Worker → Reclaiming(holder=H 유지, target=G) | CLI `reclaim` `src/cli/channel_home.rs:173-190` | `begin_reclaim` `o_channel_homes.rs:347-365`: `state='worker' AND epoch=$E` | 아니오 |
| W6 | Reclaiming → Reclaimed(holder NULL, target=G) | H watch → `drain_round`(201행) | `finish_reclaim` `o_channel_homes.rs:315-323` | 예 |
| W7 | Reclaimed → 없음 | G watch `channel_home_boot.rs:659-661` → `give_back` `:576-598`(writer join 뒤) → `finish_return` `channel_home_drain.rs:235-246` | `remove_reclaimed` `o_channel_homes.rs:368-389`: `DELETE … state='reclaimed' AND target=$G AND epoch=$E`, 1행일 때만 Applied | — |
| W8 | holder 상태(Releasing·Worker·Reclaiming) → Orphaned(holder NULL, target 보존, detail="operator force") | CLI `force` `src/cli/channel_home.rs:192-203`. **Orphaned를 만드는 유일한 생산 경로**(grep `'orphaned'`·`Orphaned` 결과 다른 생산 쓰기 없음) | `force_orphan` `o_channel_homes.rs:422-455`: `epoch=$E AND holder IS NOT NULL AND NOW()-renewed_at > F(200s)` | 예 |
| W9(신규) | Released → 없음 / Releasing → 없음 | CLI `abort-release` | §2 SQL | — |

Orphaned를 소비하거나 바꾸는 생산 쓰기는 base에 없다. 그래서 Orphaned는 현재 막다른 상태다(R2 사실 확인).

### 1.2 읽기·판정(상태 소비자)

| # | 소비자 | file:line | 상태별 동작 |
|---|---|---|---|
| R1 | CLI `status` | `src/cli/channel_home.rs:119-135` | 모든 row와 open intake(current/other_epoch/unrouted)를 보여 준다 |
| R2 | boot 등록 | `channel_home_boot.rs:396-472`, `named` `:475-484`, `foreign_rows` `:487-492` | row가 이 노드를 holder나 target으로 지목하면 gate를 등록하고 watch를 시작한다. 그 밖의 row는 foreign이라 boot writer에서 뺀다(`start_boot` `:201-222`) |
| R3 | watch 루프 | `channel_home_boot.rs:604-686` | `Ok(None)`(:624-630): unmount, lease 종료, gateway writer `ensure`, candidate가 아니면 종료. foreign(:631-642): lease 종료, writer join, unmount. 지목됨(:643-680): target이면 adopt 또는 give_back, holder면 renew와 drain |
| R4 | gateway writer 보장 | `ChannelWriters::ensure` `channel_home_boot.rs:255-286` → `Supervisor::start` `home_supervisor.rs:183-275` | 등록된 home이 있으면 거절한다. 기존 bundle이 있으면 **stop_and_join 후 build**한다(:236-241). Stuck이면 Blocked가 되고 writer를 새로 만들지 않는다 |
| R5 | intake 라우터 | `intake_router_hook.rs:135-141`, `route_to_home` `:165-172` | Worker에서만 holder로 라우팅하고 row epoch를 찍는다. 그 밖의 상태는 전부 `InTransition`으로 차단한다. row가 없으면 gateway 규칙을 따른다 |
| R6 | intake claim fence | `src/db/intake_outbox/worker_status.rs:87-93` | `home_epoch IS NULL`은 row가 없을 때만 claim된다. home_epoch가 있으면 `state='worker' AND holder=target AND epoch=home_epoch`일 때만 claim된다 |
| R7 | drain intake 판정 | `channel_home_drain.rs:100-111` → `activation_rows` `src/db/o_channel_activation.rs:34-61` | 채널의 open intake 전체(`'pending','claimed','accepted','spawned','dispatched'`)가 0이어야 떠난다 |
| R8 | ingress 관측 | `channel_ingress.rs:173-176,218-229`, `ingress_shadow/worker.rs:102` | Worker이고 fresh lease일 때만 ingress 후보가 된다. 그 밖의 상태는 `HomeNotTakingIngress` |
| R9 | gateway stop 전달 | `session_forwarding/home_stop.rs:176-189,335-339` | Worker만 전달한다. 그 밖의 상태는 `home_in_transition`. row가 없으면 Legacy |
| R10 | health | `health_api/runtime_profile.rs:9-11` → `channel_home::health` `channel_home.rs:799-823` | **프로세스 메모리의 gate만** 보여 준다(intake_open·draining·lost·blocker). row 존재 여부는 R1로만 판정한다 |
| R11 | boot 판독 보조 | `channel_home::boot_home` `channel_home.rs:875-881` | 없음 → Gateway, row, Unreadable |
| R12 | 런타임 배선 | `runtime_bootstrap.rs:347,413,442` → `start_channel_homes` `:580-621`, `herdr_runs_here` `:623-627`, `legacy_reset` `:631-667` | 스위치가 꺼져 있으면 아무것도 읽지 않는다. candidate는 Herdr endpoint가 local인 선택 채널이다. Releasing drain의 source reset은 endpoint가 local이면 no-op이다 |

### 1.3 상태별 판정표 (holder·writer·intake)

| 상태 | holder 프로세스 | G(gateway) writer | H(target·worker) writer | 새 intake | 기존 open intake claim |
|---|---|---|---|---|---|
| 없음 | — | gateway 규칙 | 없음 | gateway 규칙 | NULL epoch만 |
| Releasing(E0) | G가 renew·drain | G의 home gate 아래(drain 중) | H에 gate만 있다. endpoint가 G-local이면 writer 없음(R3 제약) | 차단(R5) | NULL epoch는 **claim 불가**(R6), E0은 생기지 않는다 |
| Released(E1) | 없음 | G watch의 foreign 분기가 join한다(:639). G를 재부팅해도 boot에서 빠진다 | H가 떠 있으면 gate만 있고 restored 뒤 adopt한다 | 차단 | claim 불가 |
| Worker(E1) | H | 없음(foreign) | H delegated writer | holder로 라우팅, E1 | E1 행만 |
| Reclaiming | H drain | 없음, G에 gate | H | 차단 | claim 불가 |
| Reclaimed | 없음 | G give_back: H join 확인 뒤 제거하고 writer 재개 | twomac R5: mini 먼저 재기동 | 차단 | claim 불가 |
| Orphaned | 없음. 옛 holder는 F 이상 무응답(살아 있을 수도 있다) | 없음(foreign이거나 target이면 gate만) | 옛 holder H면 delegated writer가 Lost gate를 들고 남는다[U] | 차단 | 전부 claim 불가 |

---

## 2. 명령 계약 — `agentdesk channel-home abort-release <channel> --expected-epoch <E>`

공통 전제(기존 `execute` 규칙 재사용, `src/cli/channel_home.rs:61-110`):
- 변경 명령이므로 `runtime.channel_home_delegation_enabled == Some(true)`가 아니면 DB 접속 전에 거절한다.
- `cluster.instance_id`가 비어 있으면 DB 접속 전에 거절한다(`reclaim`과 같은 분기).
- `--expected-epoch`는 필수이고 운영자가 `status` 출력에서 옮겨 적는다. epoch는 sequence에서 나오므로 생애마다 유일하다. 따라서 exact epoch가 곧 "운영자가 본 그 row 그 상태"를 고정한다. 단 adopt(W3)와 begin_reclaim(W5)은 epoch를 유지하므로 상태 조건은 SQL에 따로 둔다.
- 실행 노드는 항상 **원래 gateway G**다(`local` = G의 instance_id).

### 2.0 원자 CAS (DB 함수 `abort_release`, `o_channel_homes.rs`에 추가)

```sql
DELETE FROM o_channel_homes h
 WHERE h.channel_id = $1 AND h.epoch = $2
   AND (   (h.state = 'released'  AND h.holder IS NULL AND h.target <> $3)
        OR (h.state = 'releasing' AND h.holder = $3))
   AND NOT EXISTS (SELECT 1 FROM intake_outbox io
                    WHERE io.channel_id = $1
                      AND io.status IN (<INTAKE_OUTBOX_OPEN_STATUSES_SQL>))
 RETURNING state, target, epoch
```

- open intake 상태 목록은 기존 상수 `INTAKE_OUTBOX_OPEN_STATUSES_SQL`을 그대로 쓴다. 상태 목록 고정 검사 `intake_outbox_open_status.rs:262-295`가 이 상수 사용을 강제한다. 문자열을 직접 쓰면 ratchet에 걸린다.
- 반환형은 `HomeWrite<ChannelHome의 부분 또는 HomeState>`다. `fetch_optional`이 PK 아래 0행 또는 1행을 돌려준다.
- **영향 행이 1이 아닐 때**: 0행이면 `Stale`로 보고 아무것도 바꾸지 않은 채 비0 종료한다. CLI는 거절 사유를 붙이려고 `read_home`과 `open_intake_by_epoch`를 한 번 읽는다. 이 읽기는 이름표용일 뿐 row를 바꾸지 않는다(기존 `force_orphan` `:446-454`와 같은 방식). 사유는 다음 중 하나다.
  - `no home row`
  - `row is now <state> at epoch <e>, not <E>`(stale epoch)
  - `state <s> is not released or releasing`(Worker면 reclaim 안내, Orphaned면 미지원 안내)
  - `releasing holder is <h>; run on the holder`
  - `released target is this node; run on the gateway`
  - `open intake current/other_epoch/unrouted = a/b/c`

  재시도하지 않는다. PK가 있으므로 2행 이상은 일어날 수 없다. 혹시 생기면 sqlx Err로 처리해 비0 종료한다.
- 경합 의미(READ COMMITTED에서 행 잠금 대기 뒤 PG가 WHERE를 갱신된 행에 다시 평가한다): adopt·finish_release·renew·force가 먼저 커밋하면 상태나 epoch가 바뀌어 DELETE는 0행이 된다. DELETE가 먼저 커밋하면 그쪽 UPDATE가 0행(`Stale`)이 된다. 둘 다 이기는 경우는 없다(기존 `race_renewal_and_force`와 같은 증명 방식).
- NOT EXISTS는 문장 스냅샷 기준이다. 경합으로 새 open intake가 끼어들 수 있는 경로는 두 상태 모두 R5 차단 때문에 "delegate 전에 '행 없음'을 읽은 늦은 라우터의 NULL epoch 삽입"뿐이다. 그 행은 DELETE 뒤 row가 없으므로 R6에 따라 원래 target이 claim한다. 유실도 이중 소유도 생기지 않는다.

출력(성공): `{"run_on": local, "aborted": {"state": "...", "epoch": E, "target": "..."}, "next": "restart this gateway"}`. `planned()`·`row_view()` 모양은 그대로 둔다.

### 2.1 Released 갈래 (twomac R2 후속 코드안)

- 전제
  - 상태 released, holder IS NULL(스키마가 보장하고 SQL에서도 확인), exact epoch E1, target ≠ local, open intake 3값 모두 0(문장 안에서 원자 확인).
  - **[설계 이탈-1]** 회신의 "target=local gateway"는 성립할 수 없다. Released의 target은 위임 대상 H다(`delegate`가 `to==local`을 거절 `cli/channel_home.rs:85-89`하고 `leave`는 target을 보존 `:288-293`). 그대로 구현하면 어떤 Released도 지울 수 없다. 그래서 "실행 노드가 gateway(local)이고 target이 아니다"로 바꿨다. Released row에는 옛 holder가 남지 않는다(holder NULL, detail 미기록). 그래서 "실행 노드가 진짜 원래 gateway"라는 점은 운영자 전제로 남긴다.
- writer 부재 확인(CLI가 원격 노드를 검증할 수 없으므로 운영자 체크리스트다. CLI는 성공 출력에 체크리스트를 다시 찍지 않고 런북에만 둔다)
  1. G: Released에 들어간 뒤 G watch의 foreign 분기가 writer를 join한다(`channel_home_boot.rs:631-642`). G loopback `/api/health/detail`에서 `channel_homes`에 C가 없고 C writer가 hosted가 아님을 확인한다. writer hosted 필드 이름은 [U].
  2. H: H 프로세스가 내려가 있거나, 양 노드 endpoint를 G로 되돌린 뒤 H를 재기동해 H health가 `endpoint local=false`이고 `channel_homes`에 C가 없음을 확인한다. H가 adopt 전이면 H에는 Lost gate뿐이고 delegated writer는 그 gate 아래에서 POST할 수 없다[U: writer 쪽 판정 경로 미확인]. 운영 전제는 "H writer 없음"으로 더 강하게 둔다.
- 경합: H adopt(W3)와는 §2.0대로 한쪽만 이긴다. adopt가 이기면 Worker(E1)가 되고 abort는 `state worker`로 거절된다. 이 경우 정상 reclaim으로 돌아간다(twomac S4 롤백 "Worker면 reclaim만").
- 성공 뒤 운영자 순서
  1. `status`에서 C row가 없음을 확인한다.
  2. G를 fenced restart(`restart-dcserver`)한다. boot가 row 없음을 읽어 gateway writer를 boot에서 연다. twomac R5에 따라 writer 시작은 boot에 고정되므로 in-process `ensure`만 믿지 않는다.
  3. G health에서 writer와 intake를 확인하고 짧은 입력을 정확히 한 번 처리하는지 본다.
  4. H는 endpoint가 G인 설정을 유지한다.
  5. delegation switch off는 row가 없음을 다시 확인한 뒤에만 한다(§7 금지 목록).

### 2.2 Releasing 갈래 (판정: 안전한 계약 있음, 조건부 지원)

- 전제
  - 상태 releasing, holder = local(G에서 실행), exact epoch E0, open intake 3값 모두 0.
  - endpoint가 G-local이다(twomac R3의 forward 전제). 그러면 source reset이 no-op이고 H에 delegated writer가 없다.
  - H health가 `endpoint local=false`다.
- 왜 안전한가
  1. Releasing에서는 소유자가 G 하나다. H는 Released 전에는 adopt할 수 없으므로(W3 조건) gate만 등록하고 아무것도 소유하지 않는다. row를 지우면 위임 전의 "row 없음 → gateway 규칙"으로 돌아간다. 돌아간 소유자도 같은 G다. 이중 소유가 생기지 않는다.
  2. G가 살아 있으면 G watch가 다음 회차(≤ `RENEW_EVERY` 5s)에 `Ok(None)` 분기로 들어간다. 이때 gate unregister, lease 종료, `ensure` 순서로 진행한다. `Supervisor::start`가 home gate 아래 writer를 **join한 뒤에** gateway writer를 build한다(`home_supervisor.rs:236-241`). Stuck이면 Blocked로 멈추고 writer를 만들지 않는다. 같은 프로세스 안에서 writer가 둘이 될 수 없다. lease는 다음 renew가 Stale이 되어 끝난다(`channel_home.rs:937-939`, 기존 시험 `channel_home_tests.rs:174-177`이 "row가 사라지면 lease 종료"를 이미 증명한다).
  3. G가 내려가 있으면 다음 boot가 row 없음을 읽어 처음부터 gateway 규칙으로 뜬다.
  4. drain이 이미 `close()`까지 갔어도 남은 POST는 writer join이 끝을 기다린다. 아직 보내지 않은 O 조각은 G 자신의 ledger에 남아 gateway writer가 이어 보낸다. 노드가 바뀌지 않으므로 다른 노드 ledger에 갇히는 유실이 없다.
  5. 그 ≤5s 동안 G gate는 닫혀 있어 `intake_held_channels`(`channel_home.rs:826`)가 C의 claim을 미룬다. 지연일 뿐 유실은 아니다.
- **drain 완료와 abort가 겹칠 때**: `finish_release`는 `state='releasing' AND holder=G AND epoch=E0`이고 abort는 `state='releasing' AND holder=G AND epoch=E0`이다. 같은 행 잠금에서 직렬화된다.
  - finish_release가 먼저면 Released(E1)가 된다. abort는 `row is now released at epoch E1`로 거절된다. 운영자는 §2.1 전제를 확인한 뒤 `--expected-epoch E1`로 다시 판단한다(자동 재시도 금지).
  - abort가 먼저면 `finish_release`가 Stale이 되어 `DrainStep::NotDraining`(`channel_home_drain.rs:205`)으로 끝나고, watch가 다음 회차에 `Ok(None)`으로 정리한다.
- 남는 부작용(넓히지 않음): endpoint가 G-local이 아니었다면 drain의 `legacy_reset`이 이미 Legacy 세션을 리셋했을 수 있다(`runtime_bootstrap.rs:637-665`). abort는 이것을 되돌리지 않는다. 입력 유실은 아니지만 대화 문맥이 사라진다. twomac R3은 endpoint G-local을 유지하므로 이 실측에서는 일어나지 않는다.
- **[U-1] 넓히지 않은 기존 결함 후보**: Releasing 동안 채널에 남은 `pending` NULL epoch intake는 R6에 따라 claim될 수 없다. 그런데 R7 drain은 open intake 0을 기다린다. 그래서 `open_intake` blocker로 Releasing이 영구히 멈출 수 있다고 읽힌다. 다른 claim 경로가 있는지는 확인하지 못했다. 이 경우 row를 지우면 그 행은 원래 target의 claim으로 돌아간다(위임 전 동작 복원). 그래도 r1은 브리프의 "open intake 0"을 그대로 지켜 **거절**한다. "Releasing 갈래에서 unrouted>0 허용" 여부는 맥 결정 후보로 남긴다. 그때까지 런북은 "open_intake blocker면 abort 거절, 사용자·맥 결정"이다.
- 성공 뒤 운영자 순서: `status`로 row 없음을 확인한다. G health에서 C gate 사라짐을 확인한다(≤10s). G를 fenced restart해 boot 상태로 다시 고정한다. 짧은 입력 한 번을 G에서 처리한다.

### 2.3 Orphaned 탈출 (판정: 이번 조각 미지원)

- 생기는 경로는 W8(CLI `force`) 하나다. 그때 상태는 다음과 같다.
  - 옛 holder의 lease가 F=200s 넘게 무응답이었다(`channel_home.rs:24-27`). holder gate는 H=20s 뒤 스스로 닫혔고 마지막 POST는 60s delivery abort로 끝났다는 것이 F의 설계 근거다.
  - row는 holder NULL, 새 epoch, target 보존(Releasing에서 왔으면 H, Worker에서 왔으면 NULL, Reclaiming에서 왔으면 G), detail="operator force"다.
  - 옛 holder 프로세스는 살아 있을 수 있다. 다시 연결되면 renew가 Stale이 되어 gate를 닫는다. delegated writer는 watch가 멈추지 않으므로 Lost gate를 들고 남는다[U].
- 제거·gateway 복귀가 안전하지 않은 이유
  1. Released와 달리 Orphaned는 "drain이 turn·owed·open intake·POST 0을 증명하고 떠났다"는 증거가 없는 상태다. 그 증거는 옛 holder 노드의 메모리, Herdr pane, `o_store/<C>/ledger.jsonl`(노드 로컬)에 있다. PG의 CAS만으로는 확인할 수 없다.
  2. Worker나 Reclaiming에서 온 Orphaned(옛 holder H)에서 row를 지우고 G로 돌리는 경우를 보자. H ledger에 남은 owed 조각은 G가 보낼 수 없어 **출력 유실**이 된다. H pane에서 아직 도는 turn이 있으면 G의 새 turn과 **이중 실행**이 된다. 옛 epoch로 찍힌 open intake는 row가 없어지면 영구히 claim될 수 없다(R6).
  3. Releasing에서 온 Orphaned(옛 holder가 G)는 §2.2와 같은 이유로 안전하다. 그러나 row에 옛 holder가 남지 않는다. 그래서 "Orphaned, target≠local"만으로는 Releasing에서 온 것과 다른 경로를 구분할 수 없다. Reclaiming에서 온 것은 target=G라 구분되지만 Worker에서 온 것(target NULL)과 그 밖의 변형까지 증명할 근거가 없다.
- 운영 절차
  - twomac 동안 `force`를 쓰지 않는다(이미 §7 금지 목록, S10·S12 롤백 "force 금지"). 그러면 Orphaned에 도달할 수 없다.
  - 만약 생기면 실측을 중단하고, 옛 holder를 정지하고, ledger·pane 증거를 보존한 뒤 사용자·맥이 따로 수리를 결정한다. CLI는 `abort-release`에서 `state orphaned is not released or releasing; unsupported`로 거절한다.
- 후속 후보(이번 범위 밖, 넓히지 않음): `force_orphan`이 기존 `detail` 열에 옛 holder·옛 state를 기록하게 바꾼다(테이블·migration 없음). 그러면 "Releasing에서 왔고 옛 holder=local"인 Orphaned를 §2.2와 같은 증명으로 지울 수 있다. Worker·Reclaiming에서 온 것은 노드 로컬 증거가 필요해서 PG 전용 CLI로는 지원할 수 없다.

---

## 3. 전수표 행별 이중 소유·이중 writer·입력 유실 근거 (abort-release 기준)

| 행 | 근거 한 줄 |
|---|---|
| W1 delegate | abort 뒤 row가 없으므로 다시 delegate하면 새 epoch로 새 생애가 된다(sequence). 옛 E0·E1과 섞이지 않는다 |
| W2 finish_release | 같은 행·같은 (state, holder, epoch) 조건의 CAS라 abort와 둘 다 이길 수 없다. 진 쪽은 Stale이고 row는 바뀌지 않는다 |
| W3 adopt | `state='released' AND epoch=E1`에서 abort와 직렬화된다. abort가 이기면 adopt할 row가 없어 H는 소유자가 되지 못한다. adopt가 이기면 abort가 거절된다 |
| W4 renew | DELETE 뒤 renew는 Stale이 되어 lease가 끝나고 gate가 닫힌다(기존 시험이 증명). gate를 다시 여는 쓰기가 없다 |
| W5 begin_reclaim | Worker에만 적용된다. abort는 Worker를 거절하므로 두 명령의 대상이 겹치지 않는다 |
| W6·W7 reclaim 쪽 | abort는 Reclaiming·Reclaimed를 거절한다. 기존 `remove_reclaimed`(writer join 뒤 제거)를 대신하지 않는다 |
| W8 force | abort는 Orphaned를 거절한다. abort가 먼저 지우면 force는 `no holder at that epoch`(Stale)가 된다 |
| R1 status | 읽기만 한다. abort 전에 epoch와 open intake를 옮겨 적는 출처다 |
| R2 boot | abort 뒤 boot는 row 없음을 읽는다. G는 gateway writer를 열고 H는 C를 등록하지 않는다(endpoint가 G이면 candidate도 아님) |
| R3 watch | G: `Ok(None)`에서 unmount 뒤 `ensure`(join 후 start). H: `Ok(None)`에서 unmount, delegated writer는 건드리지 않는다. 그래서 "H writer 없음"을 운영 전제로 둔다 |
| R4 ensure·Supervisor | 한 프로세스 한 채널에 slot 하나, 옛 bundle을 join한 뒤 build, Stuck이면 Blocked(writer 없음)라 이중 writer가 생기지 않는다 |
| R5 router | Releasing·Released에서는 새 intake가 차단되어 abort 판정 중에 epoch가 찍힌 intake가 생기지 않는다. abort 뒤에는 gateway 규칙을 따른다 |
| R6 claim fence | 남은 open intake가 0일 때만 지운다(문장 안에서 원자 확인). 늦게 들어온 NULL epoch 행은 row가 없어지면 원래 target이 claim한다. 갇히는 행이 없다 |
| R7 drain | Releasing 갈래에서 drain이 진행 중이어도 finish_release와 CAS로 직렬화되고, 진 쪽 drain은 `NotDraining`으로 끝난다 |
| R8 ingress 관측 | 읽기만 한다. row가 없으면 `NoHomeAuthority`가 되어 home ingress 후보가 사라진다(gateway 경로) |
| R9 home_stop | row가 없으면 Legacy stop이 되어 gateway가 직접 처리한다. 다른 노드로 전달하지 않는다 |
| R10 health | 메모리 gate만 보여 준다. abort 성공은 R1로 판정하고 health의 C 사라짐은 보조 증거다 |
| R12 bootstrap | 바꾸지 않는다. G 재시작 뒤 boot 경로가 그대로 gateway를 복귀시킨다 |

---

## 4. 시험 계획

모든 PG 시험은 `TestPostgresDb`로 돌리고 `pg_throwaway.sh cargo test --lib <필터> -- --test-threads=1`로 실제 실행한다. `NOT_RUN`은 `PG_THROWAWAY_UNAVAILABLE`일 때만 쓴다.

| 시험(예정 이름) | 파일 | 지키는 계약 한 줄 | 3-1 근거(회귀, 기존 공백, 경계) |
|---|---|---|---|
| T1 `abort_release_removes_only_a_released_row_at_its_exact_epoch_from_the_gateway_pg` | `src/db/o_channel_homes_tests.rs` | Released(E1)는 gateway가 exact epoch로만 지울 수 있다. 다른 epoch(E1±1), Worker(adopt 뒤), Reclaiming, Reclaimed, Orphaned, target 노드 실행은 모두 Stale이고 row가 그대로다(기존 `refused` 헬퍼) | 조건 완화 회귀를 잡는다. 기존 시험에는 삭제 경로가 없다. 경계는 DB 함수다 |
| T2 `abort_release_takes_a_releasing_row_only_from_its_holder_pg` | 같은 파일 | Releasing(E0)은 holder=local이고 exact epoch일 때만 지워진다. holder가 아닌 노드나 다른 epoch는 Stale이다 | holder 조건 회귀를 잡는다. 경계는 DB 함수다 |
| T3 `abort_release_refuses_while_any_intake_is_open_pg` | 같은 파일 | open intake current·other_epoch·unrouted가 각각 1개만 있어도 Stale이고, 그 행을 지우면 같은 호출이 Applied가 된다(같은 row로 거절 사유를 고정) | 입력 유실 회귀를 잡는다. 경계는 DB 함수다 |
| T4 `an_abort_and_an_adopt_racing_on_one_released_row_never_both_win_pg` | 같은 파일 | 기존 `lock_waiters`·잠금 차단 방식으로 두 순서를 돌린다. adopt가 먼저면 Worker가 남고 abort는 Stale, abort가 먼저면 row가 없고 adopt는 Stale이다 | 동시 경합. 기존 race는 renew와 force뿐이다 |
| T5 `abort_release_runs_from_the_cli_and_says_why_it_refused_pg` | `src/cli/channel_home_tests.rs` | `execute`(생산 진입)에서 성공하면 `run_on`과 state가 나온다. stale epoch나 open intake로 거절하면 메시지에 현재 state·epoch·open intake가 나오고 row는 그대로다 | CLI 배선과 거절 이름표. 경계는 생산 진입점이다 |
| T5b(기존 시험 확장) `mutations_are_refused_before_any_database_access_while_the_switch_is…` | 같은 파일 | 스위치가 꺼져 있으면 `AbortRelease`도 DB 접속 전에 거절한다(`connect`가 호출되지 않음) | 새 시험이 아니라 기존 명령 목록에 한 줄 추가. 3-3 dormant |
| T6 `a_releasing_holder_whose_row_is_aborted_returns_the_channel_to_the_gateway_rules_pg` | `src/services/cluster/channel_home_boot_tests.rs` | gw가 Releasing holder로 boot하고 drain을 turn_running으로 막는다. `abort_release` 뒤 gate unregister, lease 종료, 라우팅·intake hold가 위임 전과 같다. 기존 `a_target_holds_without_a_lease_until_its_row_is_gone_pg`의 holder 쪽 짝이다 | §2.2 안전 근거 2의 실제 watch 경계. 기존 시험은 target 쪽만 본다 |

필수 뮤턴트(env 스위치 하나 `ADK_ESCAPE_MUTANT`, 테스트 바이너리를 한 번 빌드하고 뮤턴트별로 실행한다. 스위치 코드는 검증 로그로만 남기고 커밋하지 않는다):

| 뮤턴트 | 바꾸는 것 | 죽여야 할 시험 |
|---|---|---|
| `abort_ignores_epoch` | `h.epoch = $2` 제거 | T1(E1±1), T2, T4 |
| `abort_ignores_open_intake` | NOT EXISTS 제거 | T3 3건, T5 |
| `abort_any_state` | 상태 갈래를 `TRUE`로 완화 | T1(Worker·Reclaiming·Reclaimed·Orphaned), T4(adopt가 먼저인 순서에서 둘 다 이김) |
| `abort_releasing_any_holder` | `h.holder = $3` 제거 | T2 |
| `abort_released_on_target` | `h.target <> $3` 제거 | T1(target 노드 실행) |

3-2 자체 공격 항목(구현 때): open intake 상태 목록을 문자열로 쓰는 변형은 기존 ratchet이 막는다. epoch를 i64로 파싱할 때 음수·0은 스키마상 행이 없어 Stale이 된다. `--expected-epoch` 누락은 clap이 필수 인자로 거절한다. holder·target 비교는 `local`을 trim한 값을 쓴다(기존 `execute` :74-75).

예상 크기(`pr_cap_prod.py` 미실행, 추정):

| 파일 | 내용 | 순증 |
|---|---|---|
| `src/db/o_channel_homes.rs` | `abort_release` 함수와 doc | +35 |
| `src/cli/channel_home.rs` | 서브커맨드, execute 분기, 거절 이름표, 모듈 doc 1~2행·clap about 수정 | +50 |
| `src/db/o_channel_homes_tests.rs` | T1~T4 | +110 |
| `src/cli/channel_home_tests.rs` | T5, T5b | +35 |
| `src/services/cluster/channel_home_boot_tests.rs` | T6 | +45 |
| 생성 문서 | `generate_cli_reference.py` 산출 CLI 문서 1개(경로 [U]) | — |

합계는 코드 5파일 약 +255(생산 약 +85, 시험 약 +170)다. 상한(30파일, +1500) 안이므로 분할하지 않는다. lib 시험 manifest 갱신이 필요하다.

---

## 5. twomac 런북 대응표

| twomac 단계·상태 | 기존 정상 복귀(회신) | 이 CLI의 역할 |
|---|---|---|
| S0~S2(row 없음) | 설정 원복 후 재시작 | 해당 없음 |
| S3 Releasing이 120s 넘게 유지(blocker가 turn_running·owed·posts·source_reset·not_held) | G와 endpoint를 유지하고 G 재기동으로 drain 계속 | **2차 수단**: G 재기동 뒤에도 blocker가 남고 사용자 승인이 있으면 G에서 `abort-release C --expected-epoch E0`, 그다음 G fenced restart. blocker가 `open_intake`면 CLI가 거절하므로 중단하고 사용자·맥 결정([U-1]) |
| S3 Released, target 부팅 불가 | S4 target 부팅 복구가 먼저 | **"exact Released emergency repair"(§7 표, 손 SQL)를 대체**: endpoint G를 유지하고 H 정지(또는 writer 없음 확인) 뒤 G에서 `abort-release C --expected-epoch E1`, 그다음 G fenced restart. §7의 별도 사용자 승인은 그대로 필요하다 |
| S4 row가 아직 Released(H adopt 실패) | 회신: endpoint G 원복, H 재시작으로 writer 부재 확인, 승인된 Released 복구 | 같은 순서에서 "승인된 Released 복구" = `abort-release --expected-epoch E1`. 이어서 G fenced restart |
| S4 이후 Worker | 정상 reclaim만 | 해당 없음. CLI가 `state worker`로 거절한다(경합으로 adopt가 먼저 된 경우 포함) |
| S5~S13(Worker) | reclaim 또는 동일 config 재기동, force 금지 | 해당 없음 |
| S10·S12 등에서 Orphaned 발생(force를 썼을 때만) | 자동 복구 금지, 실측 중단 | 해당 없음. CLI 미지원(§2.3), 사용자·맥 별도 결정 |
| S14 Reclaiming | H를 살려 drain 완료 | 해당 없음(거절) |
| S15 Reclaimed | mini 먼저 writer join, gateway가 `remove_reclaimed` | 해당 없음(거절). 기존 경로를 유지한다 |
| 종료 정리(delegation switch off) | row 없음을 다시 확인한 뒤에만 | CLI도 스위치가 켜져 있어야 동작한다. abort는 반드시 switch off 전에 한다 |

결과적으로 twomac의 "Released 고립"은 운영 PG를 손으로 고치지 않고 공식 CAS 경로로 빠져나올 수 있다. "Releasing 정지"도 source_reset·turn·owed blocker라면 같은 방식으로 빠져나올 수 있다. Orphaned는 force를 금지해 도달하지 않게 막는다.

---

## 6. 기준 문장 → 설계 대응 (3-4)

| 기준 문장(브리프·회신) | 설계 위치 | 시험 |
|---|---|---|
| 새 off 플래그·원장·방어 장치·테이블·migration 없음 | §2.0 DELETE 1문, 기존 열과 상수만 사용 | — |
| `queue_enqueue.rs`·`injected_inputs.rs` 미사용, `.await` 너머 inflight guard 없음 | CLI→PG만 사용, inflight 경로 없음 | — |
| Released, holder NULL, exact epoch일 때만 제거 | §2.0 released 갈래 | T1, `abort_ignores_epoch`, `abort_any_state` |
| target=local gateway | **[설계 이탈-1]** target≠local로 바꿈(§2.1) | T1(target 노드 실행 거절), `abort_released_on_target` |
| open intake가 하나라도 있으면 거절 | §2.0 NOT EXISTS(원자) | T3, `abort_ignores_open_intake` |
| Orphaned는 계속 거절 | §2.0 상태 갈래에 없음, §2.3 | T1 Orphaned 행 |
| 영향 행 수가 1이 아니면 즉시 중단 | §2.0 Stale → 비0 종료, 재시도 없음 | T1~T3(row 불변), T5 |
| 양 노드 writer 종료 확인 | §2.1 운영자 체크리스트(CLI로 검증 불가) | 운영 증거(health) |
| 이후 gateway 재시작 | §2.1·§2.2 성공 뒤 순서 | — |
| Releasing: holder drain 완료와 abort가 겹칠 때 판정 | §2.2 CAS 직렬화 | T2, T6. finish_release 경합은 T4 방식으로 추가 가능(필요하면 T4에 순서 하나 더) |
| Orphaned: 생기는 경로와 holder·writer 상태 고정 | §1.1 W8, §1.3, §2.3 | 기존 `force_*` 시험 |
| 스위치 off면 DB 전에 거절 | 기존 `execute` :70-73 | T5b |

---

## FILES (맥 승인 대상)

- `src/cli/channel_home.rs`(channel_home*, 맥 소유): `AbortRelease { channel, expected_epoch }` 변형, execute의 instance_id 필수 분기, 거절 이름표. 모듈 doc 1~2행과 clap about은 사실에 맞게 고친다.
- `src/db/o_channel_homes.rs`: `abort_release` DB 함수. §7 표에서 "Released abort CLI 추가 = 맥 승인 대상".
- `src/services/cluster/channel_home_boot_tests.rs`(channel_home*, 맥 소유, 시험만): T6.
- 바꾸지 않는 것: health/**, turn_lifecycle.rs, runtime_bootstrap.rs, restore_inflight.rs, channel_home.rs·channel_home_boot.rs·channel_home_drain.rs 생산 코드, migrations.

## 미확인 [U]

- [U-1] Releasing 중 pending NULL epoch intake가 claim될 수 없어 drain이 영구히 멈추는지. R6·R7로 읽은 추론이고 다른 claim 경로는 확인하지 못했다. Releasing 갈래에서 unrouted>0을 허용할지는 맥 결정 후보다.
- [U-2] adopt 전 Lost gate 아래 H delegated writer가 POST할 수 없다는 writer 쪽 판정 경로를 확인하지 못했다. 그래서 운영 전제를 "H writer 없음"으로 강하게 두었다.
- [U-3] health detail에서 C writer hosted를 보여 주는 정확한 필드 이름.
- [U-4] `generate_cli_reference.py`가 만드는 CLI 문서 경로.
- 크기는 추정이다(`pr_cap_prod.py` 미실행).

## 다음 행동

1. 코디네이터·맥이 판정한다. 결정할 것은 Releasing 갈래 포함 여부, [설계 이탈-1] 수용, [U-1] unrouted 허용 여부다.
2. 승인되면 구현 레인이 FILES 범위로 구현한다(T1~T6과 뮤턴트 5개, `pg_throwaway`로 실제 실행).
3. twomac 런북의 S3·S4 롤백 문구와 §7 "exact Released emergency repair" 행을 `abort-release` 절차로 바꾼다.
