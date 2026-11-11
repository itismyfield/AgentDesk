# Codex Herdr marker-only A-admission 설계 r6

상태: 구현 전 PLAN, **이 레그의 생산 코드 변경 0**. 코드 기준은 A-fence 착지 후보 `e9549f721d7a9910a52a19a5b9101c48fda27386`(A-fence `4802a3cba3bd3cc4fa8d3b4f5655213fa39403b2` + main 병합)이다. 조사 시 `origin/main`은 `6a5678349833d596c48dc280a9ce2d6f6683657a`다. A-fence가 main에 이미 머지됐다고 주장하지 않는다. r4(`design-h-4.md`)의 A-admission을 기준으로 r5의 비시간제 개선을 반영하며, 이 문서가 커밋본 `codex-marker-only-a.design.md`의 r4를 대체한다. r5는 현 worktree의 선행 커밋이 아니라 별도 설계 입력이다.

원 이슈 #8057 중 Pending 등록부 복구는 main의 `952d58fe8ef7db722142c25aac060c8b7aa999a7`(#8066)에 이미 착지했다. main과 이 head의 `src/services/codex/herdr_turn/first_input.rs` SHA256은 모두 `6b77f60268fa4d7586811e73a20b910a007d89678bd1425c0bef7dbd6861b074`다. 첫 입력 두 경로 `:21,61`이 `restore_pending(:79)`로 합류하고, `:90-105`의 Pending row/owner/nonce/spawn marker 및 빈 registry 복구 검증을 유지한다. 재구현하지 않는다. live/unknown/dead legacy 실행 takeover·종료(B/C)는 계속 범위 밖이며, marker-only A로 원 이슈 전체를 완료했다고 선언하지 않는다.

## 1. 빼는 항목과 남기는 항목

코디네이터의 11-11 20:08 결정은 DB 무응답 상한을 범위 밖으로 뺐다. r4→r5의 P1 감소가 없었고, 새 CAS timeout이 기존 `claude/herdr_turn.rs:690-699`의 String 변환 → `provider_dispatch/start_failure.rs:248`의 Opaque → 재grant/새 nonce로 이어지는 새 위험을 만들었기 때문이다. 실제 재현이 없는 기존 무응답 대기를 이 레그에서 수리하지 않는다.

| r5 항목 | r6 결정 | 근거·배분 |
|---|---|---|
| query 3초 / pass 9초 / notice 1초 / 합계 10초 | 제거 | DB future에 새 timeout/timeout_at을 씌우지 않음 |
| 절대 deadline·clock·취소 타이머·만료된 worker | 제거 | monotonic deadline·시간 필드·새 timer/worker 취소 프로토콜 없음 |
| CAS timeout→Indeterminate·late commit·same-nonce timeout 회복 | 제거 | 기존 CAS 결과/오류 계약 유지. 공통 launched/failure 소비자를 시간제 정책으로 확장하지 않음 |
| r5의 fenced-wait child(신설하지 않음)와 D* 모델/cfg | 새로 도입하지 않음 | 현재 head에 존재하지 않음. 별도 r5 객체를 읽어 전 결과만 재현 |
| 최대 3회 **반환된 오류** 예산과 같은 callback continuation | 유지 | 첫 오류+추가 두 오류. 성공한 중간 단계가 횟수를 초기화하지 않음 |
| v3 공통 Owned read view·전 reader/source fence/history pin | 유지, ① | A 기록 생성 전에 reader를 모두 준비 |
| `v2_channels` 생산 소비자 두 곳 | 유지, ①+② | `runtime_bootstrap/queued_recovery.rs:50` 및 `runtime_bootstrap.rs:39` 모두 배분 |
| structured failed candidate / namespace scan error | 유지 | ① 관측·시험 준비, ② controller/실제 gate 소비 전체 |
| top-level recovery pass id·`catalog_unreadable` stage | 유지 | callback/retry마다 id 생성 금지, read/decode 오류 변화로 알림 증식 금지 |
| reconcile-complete를 discovery/gate 뒤로 | 유지, ② | `runtime_bootstrap.rs:36` 선행 표시 이동, Gateway/worker 호출 경로 함께 연결 |
| once-take carrier·실제 executor에서 fence/증거 재검증 | 유지하되 deadline 필드 제거 | guard를 queued `spawn_blocking` 대기열에 보관하지 않음 |
| r5의 30파일/+1135 단일 admission | 아래 두 조각으로 대체 | 비활성 기반 → 전체 활성화. cleanup만 독립 활성화하지 않음 |
| 작은 child가 debt/증거/전이를 소유 | 유지 | 1,000줄 초과 기존 파일에는 모듈/호출 연결만 |

**대기 계약의 한계:** `herdr_launch.rs:393-398`은 이미 physical fence 아래 DB read를 await하고, `:479`는 Pending CAS를 await한다. `hosted_execution.rs:268-269,600`의 질의에 새 상한을 추가하지 않는다. pool `db/postgres.rs:246`의 acquire_timeout은 query 상한이 아니다. DB가 영원히 응답하지 않으면 같은 physical entry의 입력·clear가 기다릴 수 있고, 이 계획은 그 상한을 보장하지 않는다. 모델은 **하위 I/O가 반환한다**는 전제를 둔다. 다른 physical entry를 전역 직렬화하거나 Source mutex를 쥔 채 DB를 await하는 새 A 경로는 만들지 않는다. 기존 Pending 복구 `first_input.rs:89-104`의 Source 아래 read는 기존 main 경로이며 이번 변경에서 확대하지 않는다.

## 2. 대상과 단일 권위

A 대상은 Codex·로컬 owner·Herdr mapping의 TUI 채널에서 실제 시작을 빚진 **동일 원문/front**다. 일반 사람 입력 성공을 synthetic 회귀의 대체 증거로 쓰지 않는다. 다음을 모두 만족해야 한다.

1. canonical/exact lookup은 Missing 또는 **동일한 Found Legacy**다. Legacy proof는 session row id, provider/token/channel canonical owner, exact key 및 raw SQL NULL 기대값을 포함한다. Unknown/Conflict/alias 불일치, 관찰 중 Pending/Bound/Retired 변경은 A 거절이다.
2. canonical/legacy physical entry에 대응하는 owner 후보가 다른 token/provider까지 정확히 하나다. 현재 token 조회 하나로 유일성을 선언하지 않는다. mapping/alias/DB 후보 전체를 확인할 수 없으면 proof를 발급하지 않는다.
3. tmux exact logical target은 Missing이고 process registry도 없다. Present·dead pane·남은 shell·probe 오류는 부재가 아니다. provider Ended만으로 삭제하지 않는다.
4. inflight/locator·durable runtime·Source binding·host/root/node/owner/nonce가 다른 실행을 가리키지 않는다. 처음부터 Absent인 구형 nonce와 관찰 뒤 소실/변경을 구분한다. 읽기 오류를 Absent로 바꾸지 않는다.
5. 양 runtime-kind 경로는 snapshot이 확인한 regular file의 정확한 `codex_tui` 또는 최초부터 확인한 Absent다. foreign bytes·leaf symlink·부모/entry 교체는 변경 0으로 거절한다.

Known Retired는 A Legacy permit으로 우회하지 않는다. `herdr_launch/runtime_marker.rs:24,66,86-107`의 기존 host/nonce/registry/Missing 조건을 유지한다. A는 확인된 runtime-kind hint만 unlink하며 Source/O cursor/spawn nonce/owner/rollout 파일은 지우지 않는다. broad cleanup·kill·pane close·`/exit` 포트를 갖지 않는다.

#5071 단일 권위 대조의 정본은 `docs/relay-state-contract.md:40-41,481-510`이다. mailbox active-turn lease가 턴 실행의 권위이고 durable inflight row는 파생이다(`turn_orchestrator/episode_identity.rs:68-100`). A receipt는 물리 표식 정리의 증거이며 턴 lease가 아니다. 기존 `queued_head/v2.rs:398,411`의 exact front grant/claim, `input_owner/drive.rs:88-102`의 채널별 직렬 drive, 기존 reservation/Pending CAS·Startup binding을 그대로 소비한다. relay owner·delivery lease·body dedup·배달 경로는 추가하지 않는다. A WakeOutbox는 **owner 재관찰 요청**이며 payload 전송 권위가 아니다.

### A-fence에서 이미 구현된 전제

`tmux_common/runtime_hint_fence.rs:20,118-135`의 key는 `(parent st_dev, parent st_ino, basename bytes)`다. canonical/legacy entry 합집합을 `:410-412`에서 정렬·중복 제거하고 `:430-455`의 고정 sidecar flock을 잠근다. lock 파일을 unlink하지 않는다. `:475-499`가 path set과 reentry를 검사하고, under-fence API에 같은 guard를 넘긴다.

최종 typed rollout identity(`:78,83-112,214-218`)는 path/session, proof의 nonce·seq·dev/ino 및 **실제 source metadata dev/ino**를 포함한다. rollout marker 저장 파일의 inode/mtime와 JSON cursor는 실행 identity가 아니다. `source_snapshot(:39-64)`도 offset을 정규화한다. 다른 sibling(owner/host/hint/generation/nonce)은 기존 file inode+bytes 비교를 유지하고, malformed rollout capture는 strict File fallback이다. r6 모델은 이 차이를 명시적으로 검증한다. A-admission에서 이 구현을 다시 쓰거나 더 넓게 완화하지 않는다.

## 3. 관찰·정리·admission 전이

새 작은 `codex/legacy_hint/evidence.rs`는 순수 증거 비교/관찰을, `codex/legacy_hint.rs`는 hint만의 cleanup을, owner_input의 ② 신설 legacy_hint child는 bounded controller를, queued_head v2의 ② 신설 legacy_hint child는 durable 전이를 소유한다. `owner_input.rs:852` root preclaim 뒤 `:859` start_waits 전에 A prelude를 연결한다.

| 단계 | durable/효과 계약 | 모델 |
|---|---|---|
| observe | 동일 owner/row/paths/front/epoch snapshot. 불명은 hint·입력 효과 0 | AObserve |
| intent | `CleanupIntent(namespace, cleanup-id, identity, front, epoch, row expectation, path snapshots)`를 기존 queue CAS/fsync로 먼저 저장. 원문은 기존 queue가 계속 소유 | AIntent |
| recheck | intent commit 동안 놓은 physical locks를 재취득, DB/owner/Source/files 재관찰 | ARecheck |
| unlink canonical/legacy | 각각 snapshot이 확인한 exact codex_tui만 삭제. 첫 경로 성공+다음 경로 오류는 Partial, 완료가 아님 | AUnlinkCanonical / AUnlinkAlias / APartialUnlink |
| receipt | 양 entry Absent·동일 owner/front/epoch를 재확인한 CleanReceipt commit | ACommit |
| outbox | 안정 event id=`namespace/cleanup-id/front/epoch`의 WakeOutbox CAS. receipt만 있으면 boot가 재생성 | AOutbox |
| wake | owner drive로 at-least-once wake, grant와 구분 | AWake / ARedeliver |
| admission | fresh exact row/private leaf/Source/owner/path/front/epoch 검증 후 **기존 front**만 claim | AAdmission / AGrant |
| launch/prompt | 기존 Pending CAS·host/spawn·Startup/Bound·owned reservation을 통과해 입력 한 번 | ACreate / APrompt |
| ack | 동일 event의 기존 실행 결과 또는 확정 terminal/known-unsent 정산을 확인한 뒤 debt 해소. claim만 보고 ack 금지 | AAck |

unlink 후 receipt 전 crash는 이미 durable인 intent에서 재관찰한다. 첫 snapshot과 다른 실행이 없을 때만 해당 entry의 현재 Absent를 idempotent 완료로 인정한다. 교체된 파일/nonce·읽기 오류를 완료로 간주하지 않는다. partial 실패에서는 같은 path만 이어가고, hint를 복원하거나 다른 파일을 지우지 않는다.

### Legacy private proof와 실제 executor 경계

일반 `activity_now()` 판정은 바꾸지 않는다. `discord/turn_presence/activity/unbound.rs:319-328`의 Found Legacy는 계속 hold다. 공개 `Observed.reason`은 `live.rs:1048`의 `session_identity_unresolved`이며 private `hosted_active_or_unknown` 문자열과 같지 않다. 작은 `live/codex_marker_absence.rs`가 `Reading`의 내부 snapshot에 접근해 **typed LegacyMarkerOnly**와 receipt의 동일 Legacy row/owner/front/epoch/paths를 함께 검증한다. 다른 private hold leaf를 우회하지 않는다. 기존 `local_legacy_absence_for_start(:1084-1101)`는 Idle/no_session 전용이므로 그대로 재사용해 true로 치환하지 않는다.

`start_waits`의 inflight/rest(:699), target(:710), host/config/root/Source 관문을 남긴다. A가 해소하는 차이는 정확히 그 Legacy 행 하나다. fresh proof를 소비할 때마다 같은 front claim을 확인하며 바뀐 epoch/front의 권리를 만들지 않는다.

운반 경로는 `owner_input::start_idle` → `OwnerTurn(:472)` → `StreamingTurn(provider_dispatch.rs:52)` → **실제 CodexHerdrTurn 생성(:613)** → `codex/herdr_turn.rs:65,200` → `herdr_launch.rs:393-398`이다. Clone 가능한 carrier는 `Arc<Mutex<Option<AStartContext>>>`로 **관찰 stamp만** 운반하고 한 번 take한다. deadline/물리 File guard는 담지 않는다. A carrier가 있었는데 None이면 일반 경로로 fallback하거나 새 permit을 만들지 않는다. 일반 생성자 `headless_turn.rs:1068`, `intake_turn.rs:2179`, busy `owner_input.rs:439`, `:1025`의 `..turn`까지 None/동일 carrier 연결을 배분한다. 요청 JSON이나 persisted provenance에 권한 토큰을 넣지 않는다.

실제 executor가 시작한 뒤 기존 Herdr launch가 `:393`에서 fence를 잡고 `:398`에서 row를 읽는다. **`:399`의 Pending 재사용 분기보다 앞, incarnation/prepare보다 앞**에서 A context와 현재 exact claim/owner/front/epoch/부재 증거를 검증하는 작은 child 호출을 넣는다. 단순히 CAS 이전이라고 하면 `codex/herdr_turn.rs:219`의 fingerprint write가 먼저 생기므로 부족하다. 이 gap에서 proof가 stale이면 파일/nonce/CAS/create/prompt 효과 0으로 A known-unsent 정산을 한다. 이후 `:479` Pending CAS → `:489-499` registry/host/spawn/clear는 기존 guard를 계속 쓰고 `:503`에서 놓는다. 중첩 flock은 없다.

기존 `OwnerTurn.refusal`은 `HerdrRefusal` 전용(`owner_input.rs:481,499`)이며 `:926-940`도 confirmed 종류만 소비하므로 A 거절을 그 sink에 얹지 않는다. A carrier는 stamp와 별개인 `Arc<Mutex<Option<AStartResult>>>` 결과 sink를 함께 공유하고, take되는 권리는 stamp 하나뿐이다. executor가 proof를 거절하면 typed `Refused { stage, observed_identity }`를 먼저 쓰고 `claude/herdr_turn.rs:699`의 일반 launched 변환 **전에** `NotSent`로 반환한다. 기존 `start_failure.rs:239-241`이 이를 일반 Opaque commit 없이 돌려보낸다. A controller는 실제 settlement/실행 future가 끝난 뒤(기존 `owner_input.rs:946` answer 경계 뒤) 같은 typed 결과를 한 번 소비해 exact original의 known-unsent/debt를 commit한다. String prefix 자체를 proof나 정산 권위로 삼지 않는다. typed 결과 없이 worker가 끝나거나 carrier take가 중복되면 실행 권한을 복원하지 않고 custody hold로 정산한다. `start_failure.rs:248` Opaque로 바꾸거나 다음 drive에서 새 A carrier를 만들어 같은 원문을 다시 실행하지 않는다. 새로운 CAS timeout/Indeterminate는 없으며 기존 server/create 불명확 결과와 Pending 회복 정책은 변경하지 않는다.

### 반환된 오류·정산·동시성

최초 시도부터 ack까지 parent Handoff/recover future 안에 controller를 유지한다. `input_owner/drive.rs:102`는 이 callback을 await한다. unlink/관찰/commit 오류를 처음부터 일반 Err/START_WAITS로 반환해 continuation을 버리지 않는다. 처음 오류+두 번 추가 오류까지 최대 3회이며, 각 실패 후 physical/Source guard를 놓고 동일 namespace/cleanup-id/front/epoch로 재관찰한다. 성공 단계는 실패 횟수를 되돌리지 않는다. 새 task·sweep·운영 명령·sleep polling은 없다. `commit::recover(:75)`가 original/candidate/제3 상태를 판별하기 전 blind 재삭제하지 않는다.

pre-grant 재관찰 loop 안에 payload/launch를 넣지 않는다. Ready 뒤 기존 owned-input을 한 번 호출하고 ack-only 단계로 간다. ack 실패를 미전송으로 알리거나 payload를 다시 실행하지 않는다. wake/ack 유실은 동일 event 재관찰로만 처리한다.

3회 소진·front/epoch/owner 변경 시 원문은 같은 custody에 남고 **명시 미전송 정산/통지**를 한다. writable queue는 known-unsent retained+notice debt를 한 commit에 기록한다. 이미 취소/정산된 원문은 기존 terminal receipt를 재사용해 두 번째 notice를 만들지 않는다. queue publication이 계속 실패하면 confirmed raw bytes/candidate·원문을 보존하고 hold_front로 grant를 철회한다. 독립 기존 PG non-turn outbox에 notice를 예약할 수 있을 때만 통지 완료로 세며 PG도 불능이면 storage hold/error이지 notified가 아니다. 새 저장소·dedup 권위는 없다.

lock 순서는 **정렬 physical entries → 반환을 기다리는 DB → try Source → 동기 file 비교/변경**이다. Source를 잡은 채 flock/DB를 await하는 새 A 경로, source closure 안에서 mailbox/v2 actor 왕복, global LANES/HELD map mutex를 잡은 채 await는 금지한다. A controller는 mailbox `v2_request` 안에서 장기 I/O를 실행하지 않는다. `drive.rs:72-84`의 채널 lane map과 `tmux_common.rs:78-88` Source map은 lookup 뒤 해제된다. 다른 physical entry의 채널이 계속 진행하는지 유한 barrier 시험으로 검증한다. shared physical alias 채널은 의도적으로 직렬화하며 DB pool 전체 포화/영원한 무응답의 liveness는 보장하지 않는다.

## 4. v3 read view·부팅·history pin

A 최초 intent를 생산하는 commit에서만 version 3으로 승격한다. `v2.rs:155-165`와 `migration.rs:31`의 기존 생산 literal은 ①에서 version 2와 empty A extension을 유지하며 empty 필드는 serialize하지 않는다. v2 unknown-field 무시로 A debt가 사라지는 형식을 만들지 않는다. codec은 version을 먼저 분기하고 v3의 알려진 typed schema/tag/namespace만 허용한다. v2에 A control 필드를 숨겨 넣은 자료·unknown version/record는 명시 오류다. 기존 raw-byte CAS/rename/directory fsync/candidate recovery(`v2/commit.rs:17-105`)를 재사용한다.

①에서는 read view와 **effectful owner adoption**을 분리한다. `v2/sources.rs:47-53`의 생산 restore/adopt는 version3를 명시 read-only 오류로 거절해 원본을 보존한다. 따라서 수동/미래 v3 자료가 있더라도 `input_owner/recovery.rs:15→17→23`의 ensure→tick(commit)→requeue 및 wake가 A debt를 무시하고 진행하지 못한다. ①의 실제 v3 reader/source projection과 TTL pin의 의미시험은 가능하되, v3 owner mutation은 ②에서 이 고정 read-only 가드를 제거하고 controller를 함께 연결할 때만 열린다. feature flag/default-off branch/새 allow는 필요 없다. 이 guard 제거 파일도 ②에 센다.

작은 `v2/a_read_view.rs`가 공통 Owned view와 `has_control_debt`/`references(identity, attempt, receipt)`/owed-delivered 판정을 소유한다. 역사적 `SnapshotV2`·`v2_channels` 이름을 유지해도 실제 v2/v3 공통 read 계약이다. outbox-only 파일도 owner가 정산할 debt이므로 holds_input=true다. 그 사실로 delivered terminal을 새 queued/owed 원문으로 변환하지 않는다.

| reader/경계 (`src/services/` 아래) | ① 연결·② 유지할 계약 |
|---|---|
| `turn_orchestrator/queued_head/compat.rs:5-45` | 공통 Owned decode, queued/attempted/owed 및 intent/receipt/outbox debt의 holds_input |
| `pending_queue_persistence/queued_recovery.rs:35-67,87-91` | outbox-only discovery, owned idle/quarantine 분기 정확히 구분 |
| 같은 파일 `:104-142,145-197` | v3 held sources·catalog. owed와 delivered 분리 |
| `discord/runtime_bootstrap/queued_recovery.rs:50-51` | 생산 소비자 1: ready adoption + ② failed metadata controller |
| `discord/runtime_bootstrap.rs:35-43` | 생산 소비자 2: discovery/gate→mark complete→ready owner wake |
| `discord/queue_io/input_owner/recovery.rs:15-39` | adoption/tick/known-unsent/history/notice 전에 A debt 재관찰; 삭제/무조건 requeue 금지 |
| `queued_head/v2/sources.rs:13-68` | namespace/original identity와 A refs source fence. debt만으로 재발행 금지 |
| `discord/recovery_known_ids.rs:149,166-170`, `catch_up/phase1_sweep.rs:132`, `phase2_sweep.rs:134` | 동일 공통 held-source projection, owed→Queued / delivered→V2Delivered 유지; 이 caller들은 공통 helper 변경으로 소비 |
| `queued_head/v2/origin.rs:127` | 원문 없이 notice/outbox만 있어도 debt 발견, 새 payload 생성 금지 |
| `queued_head.rs:108`, `dispatch_reservation.rs:459-460` | legacy reservation/drain이 v3를 empty/v2로 덮지 않음 |
| `queued_head/snapshot.rs:29`, `pending_queue_persistence/snapshot_io.rs:101` | legacy-only parser는 v3 명시 오류; empty 성공 금지, 기존 간접 호출로 적용 |
| `turn_orchestrator/checked_purge.rs:128,582` | actor-owned exact capture, actorless legacy 오류 보존. purge 권한 확대 없음 |
| `queued_head/v2/projection.rs:162,220`, `discord/runtime_bootstrap/o_writer_host.rs:352` | 공통 holds_input은 outbox-only true/ack 뒤 false. 새 direct edit 없이 공통 projection 소비; host-ready 순환 대기 시험 |
| `queued_head/compat.rs:50,59,70-77` | parse 가능한 non-v1 clear/save 거절 유지. read/parse 실패시 기존 explicit clear 허용 정책까지 수리했다고 주장하지 않음 |
| `queued_head/v2/retained.rs:77-79`, `v2/source_admission.rs:468,618,749` | 직접 terminal TTL·settled bind TTL/prune에서 정확 A refs pin, ack/명시 정산 뒤 기존 만료 재개 |

아래 표의 짧은 경로 `pending_queue_persistence/`, `queued_head/`는 모두 `turn_orchestrator/` 아래다. reader의 public gateway/reexport는 기존 이름·경로를 사용하고 새 불필요한 facade를 추가하지 않는다. exhaustive match 컴파일과 source scan으로 누락을 재확인하며, 다른 caller가 새로 발견되면 파일/순증 견적을 갱신하고 정지선을 먼저 적용한다.

### structured discovery와 완료 표시

`v2_channels`는 현재 `queued_recovery.rs:88`에서 성공 snapshots만 취하며 quarantined/scan_errors를 버린다. 반환형을 `OwnedRecoveryDiscovery { ready, failed, namespace_errors }`로 넓힌다. failed는 `(provider, channel, expected_path, namespace, stage, error)`를 보존한다. numeric filename 확인 후 read/UTF-8/decode/unsupported-schema 오류도 같은 controller에 인계한다. read 불능 파일을 v3라 단정하거나 raw bytes/cleanup-id/원문을 꾸미지 않는다. namespace scan 오류는 채널을 추측하지 않으며 새 channel notice를 만들지 않는다.

②에서는 eligible local Codex Herdr 대상만 기존 owner adoption/migration에 합류한다. 다른 provider·소유 불명 quarantine을 writable queue로 바꾸지 않는다. `owner_recover`의 기존 reexport(`provider_dispatch.rs:28`, `message_handler.rs:77`)는 함수 이름을 유지하며 recovery target을 ready channel/failed metadata로 넓힌다. 새 함수 facade나 reexport를 숨겨서 파일 수를 줄이지 않는다.

notice key는 cleanup-id를 읽었으면 `A-io:v1:SHA256(provider, namespace, cleanup_id, stage)`, 못 읽었으면 `boot-queue-io:v1:SHA256(provider, namespace, channel_id, normalized_expected_path, recovery_pass_id, stage)`다. recovery_pass_id는 **top-level boot recovery 한 번에 하나**이며 두 discovery 소비자와 모든 retry가 공유한다. catalog read/decode 오류는 `catalog_unreadable`로 정규화한다. mtime/inode/raw 오류/랜덤 retry id를 key에 넣지 않는다. 다음 boot는 새 pass이며 실행 nonce/원문 identity를 바꾸지 않는다.

명시 운반 경로는 worker `queued_recovery.rs:19→21`와 Gateway `recovery_flush.rs:48→120`이다. 작은 child의 `RecoveryPass`/gate 결과를 두 호출 사이에서 넘기고 테스트용 wrapper도 실제 경로를 부른다. callback마다 pass를 다시 만들거나 두 번째 discovery에서 id를 다시 만들지 않는다. `complete_active_reconcile`는 typed Complete/Held 결과를 반환한다. 발견/adopt/오류 재관찰이 끝난 뒤에만 `runtime_bootstrap.rs:36`의 mark를 실행하고 owner wake를 한다. Held이면 Gateway의 `recovery_flush.rs:124-132` 성공 로그와 `:160-168` reconcile-complete barrier를 성공으로 처리하지 않는다. `:135`의 두 번째 kickoff와 `:171` restart-report flush도 Complete 분기 뒤에 두어 실패를 복구 완료로 소비하지 않는다. worker `queued_recovery.rs:22-23`의 listener/retry 시작은 같은 Complete 분기로 묶는다. 원문/오류와 readiness hold를 남기며, 기존 unrelated background task를 죽이거나 새 무한 retry task를 만들지 않는다.

부팅 gate는 provider 실행/Startup 완료를 기다리지 않는다. owner 설치와 debt의 custody/오류 분류가 끝났음을 확인한 뒤 intake를 열고 wake한다. outbox ack-only는 provider host-ready가 없어도 실행 결과/terminal을 재관찰할 수 있어야 한다. 그 반대 순서는 `holds_input→legacy_busy→host-ready→owner ack` 순환 hold를 만들므로 실제 outbox-only bootstrap 시험으로 차단한다. namespace를 읽지 못했거나 candidate가 예산 소진 뒤에도 불명인 경우 ready 성공 수만으로 완료 표시하지 않는다.

기존 deliberate skip은 별도다: utility `recovery_flush.rs:35-38`, standby `runtime_bootstrap.rs:471-480`는 기존 skipped-recovery 완료 표시다. 이는 active boot-recovery pass를 의도적으로 생략하는 기존 분기이며 A admission 신규 권한을 주지 않는다. standby도 `:458`에서 full turn 실행을 켜므로 런타임 전체가 A 부적격이라고 선언하지 않는다. standby promotion은 기존 fenced restart로 active 복구에 들어온다. 이 예외와 실제 active worker/Gateway 경로를 source scan·수용시험에서 구분한다.

## 5. 착지 경계와 크기

**① 비활성 기반 → ② 전체 활성화**로 확정한다. ①을 먼저 착지해도 A intent/receipt/outbox 생산·hint unlink·Legacy permit 발급·새 자동 입력 실행은 없다. 운영 feature flag/default-off 분기를 새로 만들지 않는다.

| ① 부위 | 실제 관측 caller 또는 test-gate | 기존 운영 동작 보존 증거/수용시험 |
|---|---|---|
| v3 codec/read view·debt refs | compat::inspect→catalog/holds/origin/source/TTL의 기존 caller, effectful sources::restore의 v3 거절 caller | 현행 v1/v2에서 A extension=None, version2/serialized bytes·disposition·wake 횟수 동일. v3 adoption은 read-only 오류/원본보존 |
| structured discovery | 실제 두 소비자는 `ready`를 기존대로 소비. 오류 metadata는 기존 load_pending_queues/report_scan_errors/quarantine 관측 경로로 전달 | 새 gate·retry·notice·owner recovery 호출 없음. 기존 오류 진단에서 metadata를 실제 읽음; 결과를 버리는 `_`만으로 caller라 하지 않음 |
| 증거 비교·private typed proof 준비 | `codex.rs`·`activity.rs`·`live.rs`의 `#[cfg(test)]` 부모 아래 의미 있는 양/음성 행동시험 | release에는 해당 모듈/permit 생산자가 없음. 새 #[allow]와 경고 baseline 변경 0 |
| pass/stage/event-key·gate 준비 | 운영 decode/관측이 쓰는 정규화만 live; 실제 사용처 없는 event-key/gate reducer는 a_read_view child의 `#[cfg(test)]` | 단지 실행을 켜기 위해 dead production helper를 남기지 않음. test-only support에도 실제 시험 있음 |
| production literals | `v2.rs:155`, `migration.rs:31` 모두 version2+empty | no A record/no bulk rewrite. ① 끝 실제 source scan·byte oracle |

②는 위 test-gate를 실제 caller로 승격하는 변경까지 포함하고 controller/durable 전이/cleanup/typed proof·once-take carrier/두 bootstrap 소비자 오류·완료 순서·정산을 **함께** 켠다. reader나 bootstrap 오류 처리를 나중 조각으로 미루지 않는다. ①만으로 #8057 acceptance 완료 credit은 0이다. ①에서 형식 지원은 추가되지만 현행 생산자가 만드는 v1/v2 입력 영역의 입력·파일·ready/wake 동작은 같다. hand-authored v3 fixture의 해석 지원을 실제 운영 v3 생산/복구가 켜졌다는 뜻으로 쓰지 않는다.

### 파일별 생산 순증 견적

수치는 구현 전 **순 코드 줄 추정**이다. test-gated production-named child도 prod cap에서 보수적으로 센다. 각 표의 prefix는 `src/`다.

| ① 파일 | 순증 | 책임 |
|---|---:|---|
| `services/turn_orchestrator/queued_head/v2_types.rs` | 45 | typed v3 control record의 read schema/empty default |
| `services/turn_orchestrator/queued_head/compat.rs` | 45 | common Owned decode/holds, strict version |
| `services/turn_orchestrator/queued_head.rs` | 4 | legacy meta guard 연결 |
| `services/turn_orchestrator/queued_head/v2.rs` | 4 | empty literal/child 연결 |
| `services/turn_orchestrator/queued_head/v2/sources.rs` | 15 | restore/namespace/source fence |
| `services/turn_orchestrator/queued_head/v2/commit.rs` | 8 | 기존 version 유지 검증/codec 연결 |
| `services/turn_orchestrator/queued_head/v2/origin.rs` | 8 | outbox-only read |
| `services/turn_orchestrator/queued_head/v2/source_admission.rs` | 6 | child refs 호출만 |
| `services/turn_orchestrator/queued_head/v2/retained.rs` | 6 | 직접 TTL pin 호출 |
| `services/turn_orchestrator/queued_head/v2/a_read_view.rs` (신규) | 110 | debt/refs/owed-delivered/schema 본체, 미사용 event-key는 test-gate |
| `services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs` | 55 | structured discovery + 기존 진단 관측 |
| `services/turn_orchestrator/dispatch_reservation.rs` | 4 | legacy drain guard |
| `services/discord/runtime_bootstrap/queued_recovery.rs` | 18 | 소비자1 ready projection/관측·test gate |
| `services/discord/runtime_bootstrap.rs` | 4 | 소비자2 ready projection/관측, 완료 표시 순서 유지 |
| `services/turn_orchestrator/queued_head/v2/migration.rs` | 2 | production literal empty extension |
| `services/discord/turn_presence/activity.rs` | 3 | test-gated typed proof 연결 |
| `services/discord/turn_presence/activity/unbound/live.rs` | 2 | test-gated child 연결 |
| `services/discord/turn_presence/activity/unbound/live/codex_marker_absence.rs` (신규) | 68 | private typed proof verifier, ① test-gate |
| `services/codex/legacy_hint/evidence.rs` (신규) | 125 | 부재/owner/identity 비교, ① test-gate |
| `services/codex/legacy_hint.rs` (신규) | 2 | test-gated evidence child 연결 |
| `services/codex.rs` | 2 | test-gated child 선언 |
| **① 합계** | **536 / 21파일** | |

| ② 파일 | 순증 | 책임 |
|---|---:|---|
| `services/codex/legacy_hint.rs` | 79 | hint-only cleanup·partial 재관찰 및 별도 typed AStartResult/carrier 결과 sink |
| ② 신설 controller child (owner_input의 legacy_hint) | 209 | bounded controller·boot·known-unsent 본체 |
| `services/discord/router/message_handler/provider_dispatch/owner_input.rs` | 13 | prelude/recover/carrier child 연결만 |
| `db/dispatched_sessions/hosted_execution.rs` | 40 | owner 후보 read-only query, timeout/CAS 정책 추가 없음 |
| `services/turn_orchestrator/queued_head/v2.rs` | 14 | durable child 연결 |
| ② 신설 durable child (queued_head v2의 legacy_hint) | 134 | intent/receipt/outbox/ack CAS·정산 |
| `services/discord/queue_io/input_owner/recovery.rs` | 28 | A debt 재관찰·requeue 순서 |
| `services/codex/herdr_turn.rs` | 14 | actual executor carrier take/거절 전달 |
| `services/herdr_launch.rs` | 10 | row read 직후·모든 fresh effect 전 검증 연결 |
| `services/discord/router/message_handler/provider_dispatch.rs` | 22 | carrier와 실제 CodexHerdrTurn 경계 |
| ② 신설 notice child (owner_input의 storage_failure) | 50 | pass/stage dedupe·기존 PG notice, timeout 없음 |
| `services/discord/router/message_handler/headless_turn.rs` | 1 | 일반 carrier None |
| `services/discord/router/message_handler/intake_turn.rs` | 1 | 일반 carrier None |
| `services/discord/runtime_bootstrap/queued_recovery.rs` | 8 | failed controller + pass/gate 운반 |
| `services/discord/runtime_bootstrap.rs` | 4 | gate 결과 소비·완료 순서 변경 |
| `services/discord/turn_presence/activity/unbound/live/codex_marker_absence.rs` | 2 | proof 실제 caller 연결 |
| `services/codex.rs` | 0 | test-gate 해제, 변경 파일에 포함 |
| `services/discord/turn_presence/activity.rs` | 0 | test-gate 해제, 변경 파일에 포함 |
| `services/discord/turn_presence/activity/unbound/live.rs` | 0 | test-gate 해제, 변경 파일에 포함 |
| `services/turn_orchestrator/queued_head/v2/a_read_view.rs` | 0 | event-key 준비 gate 해제, 변경 파일에 포함 |
| `services/discord/runtime_bootstrap/recovery_flush.rs` | 21 | Gateway pass/gate 결과·성공 로그/barrier 분기 연결 |
| `services/turn_orchestrator/queued_head/v2/sources.rs` | 0 | ①의 v3 effectful adoption 거절 해제와 controller 동시 연결 |
| **② 합계** | **650 / 22파일** | gate 줄 삭제와 대체 연결은 순증0으로 보수 상계 |

`pr_cap_prod.py`의 실제 상수는 FILES=30, NET=1500이다. PLAN 정지선은 각 조각 +1200이다. A-fence 원견적 15/+434 → 실측 17/+769의 경험 비율을 **각 조각에 따로** 곱한다: 파일×17/15=1.1333, 순증×769/434=1.77189, 각각 올림.

| 조각 | 원견적 | 경험 보정 전망 | 30파일 여유 | PLAN +1200 여유 | cap +1500 여유 |
|---|---:|---:|---:|---:|---:|
| ① | 21/+536 | **24/+950** | 6 | 250 | 550 |
| ② | 22/+650 | **25/+1152** | 5 | **48** | 348 |

②의 +48은 작다. 독립 점검에서 발견한 v3 effectful adoption guard 해제 파일과 전용 typed 결과 sink +20줄까지 이미 포함한 수치다. 구현 중 API/reexport/추가 caller 때문에 원견적이 커지면 실측과 잔여 예상 합을 즉시 다시 계산한다. 보정 전망이 +1200 또는 30파일에 닿거나 실측이 +1200을 넘기 전에 멈추고 DESIGN_DELTA를 낸다. 기능/reader/오류 처리를 삭제해 맞추지 않는다. ①을 더 작은 책임으로 나눌 수는 있어도 ②의 cleanup·admission·bootstrap 안전 조건을 분리 활성화하지 않는다. 계획표의 두 조각 distinct는 33파일이고 순증 합계는 +1186이다. 전체를 한 리뷰 조각으로 묶지 않는다. 실제 PR cap은 구현 diff에서 다시 재며 조각 파일 수 합계 43을 distinct라고 보고하지 않는다.

### giant 파일 경계

현 head `codex.rs` 3037줄, `provider_dispatch.rs` 1108줄, `owner_input.rs` 1586줄, `discord/turn_presence/activity.rs` 1078줄, `activity/unbound/live.rs` 1107줄, `runtime_bootstrap.rs` 1107줄, `v2/source_admission.rs` 1017줄이다. 이 파일들에는 mod/use/field/호출·gate 연결만 둔다. debt/refs 본체는 `a_read_view.rs`, 증거 본체는 evidence child, controller/notice/durable 본체는 각각 새 child다. source_admission의 prune/expire에 독립 판정 알고리즘을 얹지 않는다. 작은 기존 파일 `herdr_launch.rs`(약 800줄)도 새 책임은 child callback 연결로 제한한다. giant 예외/baseline/allowlist/새 #[allow]는 요구하지 않는다.

## 6. 대상 시험·뮤턴트 계획

아래는 **구현 레그의 수용시험 계획**이며 이 설계 레그에서 실행했다고 주장하지 않는다. ① test-gate support는 의미 있는 행동시험에서 사용하며 빈 helper module을 만들지 않는다. required test id는 paths-filter→job→recipe/filter→실제 ID까지 배선한다. 원인 제거 mutant는 컴파일 성공·관련 assertion 실패로만 RED이며, 원복 후 source SHA256 동일과 GREEN을 확인한다.

| 조각/시험 | 실제 오라클 | 원인 제거 mutant |
|---|---|---|
| ① 구형 v1/v2 의미·생산 | load→inspect→adopt→commit, no debt/version2 및 기존 bytes/disposition/wake 수 동일 | version 무시/v3 debt를 v2에 숨김 |
| ① outbox-only reader | holds_input true, catalog→두 소비자 ready, owed/delivered 정확, payload 복제0 | debt holds 또는 source fence 제거 |
| ① history pin | 24시간 경과 tick·expire/prune에도 정확 refs terminal/bind 보존, ack 뒤 기존 만료 | 직접 TTL pin 제거 |
| ① legacy writer/adoption 음성 | 실제 boot→owner recover가 v3를 read-only로 보존하고 ensure/tick/requeue/submit 효과0. legacy reservation/save/drain/actorless parse도 readable v3 bytes 보존. 기존 unreadable explicit clear 정책은 별도 대조 | effectful restore의 v3 거절 또는 owned legacy 거절 제거 |
| ① structured discovery | numeric filename 뒤 read/UTF8/decode 오류 tuple·namespace scan 오류와 기존 ready projection 동일; 기존 진단 caller 실제 실행 | failed metadata 유실/namespace 오류 empty 처리 |
| ① private proof/evidence | 공개 reason 같은 두 private leaf 중 정확 Legacy만 typed positive; row/owner/nonce/Source/path 교체 negative | reason 문자열 비교 또는 row identity 검사 제거 |
| ② 실제 synthetic Missing/Legacy | 같은 kept 원문으로 cleanup→wake→start→Startup/Bound·hold 해제. grant/create/native prompt 각1, skip0 | owner prelude/typed Legacy 소비 caller 제거 |
| ② 반환 오류 후 진행 | 초기 drive/recover 1회만. unlink/intent/receipt Before·AfterRename 오류1~2회 후 새 입력/wake/reboot 없이 같은 callback 완료 | 첫 오류에서 controller return |
| ② 영구/예산 소진 | 오류3회 뒤 무한 재시도0, raw/candidate/원문 보존, writable retained+notice 또는 storage hold; PG불능을 성공으로 안 셈 | 예산 검사 제거/notice 실패를 성공 취급 |
| ② crash 각 간격 | intent/unlink각각/receipt/outbox/wake/효과 뒤 ack전 crash→boot. 동일 event만 재관찰, native submit1 | intent 선행commit 제거/ack를 claim만으로 완료 |
| ② carrier/executor gap | Clone→take1/doubletake 거절, queued worker에 guard0, row/front/epoch 교체 시 fingerprint/nonce/CAS/create/prompt 모두0, typed Refused1·일반 Opaque commit0·known-unsent commit1 | 실제 Codex 생성 carrier 연결 또는 executor fresh 검증 제거 |
| ② front/epoch·Cancel/Terminate/clear/stop | 모든 경계의 취소·선호변경·owner 교체에서 wrong-front grant0/다른 owner 파일 삭제0. 기존 close/kill 권한 증가0 | exact front 또는 owner 검사 제거 |
| ② bootstrap 전체 | Gateway/worker 실제 wrapper 각각 1회, catalog 불능→회복/소진, pass/stage 동일 key 알림1, Held 성공 로그/barrier0 | 두 소비자 중 한 오류 연결 제거/mark를 discovery 앞으로 |
| ② outbox-only bootstrap | items/head/retained 비고 debt만 있음. owner 설치→완료 gate→wake→ack, holds_input true→false, host-ready 순환 wait0, payload 재발행0 | debt discovery 제거/host-ready 선행 요구 |
| ② finite DB await 격리 | throwaway PG 또는 제어된 반환 future의 시작 barrier 동안 Source try가 가능·다른 physical 채널 진행; release 뒤 같은 입력1 | Source 아래 DB await/전역 map lock 보유 |
| A-fence 승계 회귀 | 두 프로세스 alias lock·부모/leaf 교체·4 publisher·typed rollout 정상재게시/진짜 identity 교체 | 기존 source/witness/fence caller 제거 시 해당 native assertion RED |

설계 모델의 선형 grant/create/prompt 카운터는 효과 뒤 crash 재실행 exactly-once 증거가 아니다. 마지막 두 부류 중 실제 owner/reservation/Startup fixture를 거친 시험이 필수다. DB pending 시험의 barrier는 시험이 직접 풀며 **3초/10초 상한을 주장하거나 timeout mutant를 만들지 않는다**. 운영 DB/store/포트에 접근하지 않고 throwaway fixture만 쓴다.

구현 레그에서 첫 cargo 전 headroom/seed_target을 수행하고 cargo-shim·정해진 debug/strip profile을 사용한다. 변경 파일 clippy warning0(전체 workspace/all-targets/all-features JSON), required ID 선택수=실제 통과수/skip0, source scan, prod cap/giant 감사 및 static prepush를 확인한다. 이 레그는 cargo/Rust/PG/clippy/prepush를 실행하지 않는다.

## 7. 리뷰 집중 항목 초안

| 초점 | 막는 방법·남은 반례 |
|---|---|
| fence 아래 DB await / 교착 / 다른 채널 정지 | global map lock·Source를 await에 안 들고 감, 물리 정렬·동일 guard·actor 밖 I/O. finite barrier 격리시험. 영원한 DB 무응답·공유 pool 포화에는 상한 없음 |
| 비활성 ① dead_code·allow | production read/TTL/discovery의 실제 caller + 미사용 준비는 의미시험 있는 cfg(test). gate 해제 부모·effectful adoption 해제도②파일수. 새 allow/baseline0 |
| 같은 입력 재실행 | exact front/claim·once-take·effect 후 ack-only, generic Opaque로 A 거절 유입금지. 실제 post-prompt crash fixture와 wiring mutant 필수 |
| 다른 owner 실행/파일 삭제 | owner 후보 유일성·raw row expectation·typed nonce/proof/source·정확 hint path, broad cleanup/kill 포트 없음 |
| 부팅 discovery 순서 | 두 v2_channels 소비자 및 Gateway/worker pass 운반, gate 뒤 mark, Held를 성공 log/barrier로 처리0, deliberate utility/standby skip 구분 |
| 경계 시각·경합 | intent/unlink/receipt/admission/queued executor 사이 front/epoch/cancel/owner 변경 및 alias 두 controller 시험 |
| 옛 형식 복원 | v1/v2 no-debt 바이트/의미, strict v3·legacy writer 원본보존. 기존 unreadable explicit clear는 미수리/권한확대0 |
| 문서로만 선언한 guard | A-fence 실제 caller 승계 + actual Codex 생성/launch·bootstrap 연결 제거 mutant가 RED여야 함 |
| 잘못된 0/PASS | 새 cfg case witness와 실제 TLC 상태/규칙 수; Rust/운영은 NOT_RUN. 알림실패/0-test/DB skip은 PASS 아님 |

## 상태 차트

### 결정과 모델 범위

A의 DB 호출은 **반환되는 호출**로 추상화한다. fence 아래 DB await는 허용한다. DB 무응답 상한, 절대 deadline, CAS timeout/Indeterminate, late CAS commit의 새 행동은 모델 밖이다. 새 timeout·재grant/새 nonce 경로를 도입하지 않는다. 기존 일반 Herdr 모델의 `RecordPaneFails`에 남은 Indeterminate 주석은 기존 pane-record 실패이며 r5의 새 Pending CAS timeout 행동이 아니다.

현재 코드 정본에는 r4 A 행동만 있다. r5 설계 head `2904cbaf71a545f3de7cd39ede998f7dab356fd4`의 `D*` 행동과 `HerdrHintADb{Before,Fixed,NoTimeout}.cfg`는 비교용으로 외부 증거 폴더에 추출·실행했으며 r6의 추적 파일에는 포함하지 않는다. 따라서 이 base에 없던 r5 cfg를 삭제했다고 주장하지 않는다.

`AIntent → ARecheck → 두 unlink → ACommit → AOutbox → AAdmission`과 controller continuation·최대 세 번의 반환 오류 정산을 유지한다. `ALegacyProgress`는 정상 Legacy/Missing 및 예산 안에서 끝난 일시 오류에 대해 **원래 입력의 delivered**를 요구한다. 명시 거절을 전달 성공으로 세지 않는다. 새 `a-progress-two` cfg는 intent/partial-unlink/receipt/boot 오류가 두 번 반환된 뒤 회복하는 경계를 검사한다. 세 번의 실패가 먼저 끝내는 경우는 보존·통지 정산이며 무제한 자동 재시도나 나중의 자동 전달 보장은 아니다.

A-fence의 typed rollout witness(`src/services/tmux_common/runtime_hint_fence.rs:82`, `:93`, `:101`, `:214`)를 반영했다. path·session·execution nonce·proof seq·선언된 source dev/ino·실제 source dev/ino를 비교한다. rollout marker 저장 inode와 cursor는 실행 identity에서 제외한다. 일반 runtime hint의 파일 identity·owner/path snapshot 비교는 유지한다. `AExactOwned`는 mutant의 판정 조건과 독립적으로 삭제 오라클을 계산한다.

### cfg 전/후 결과

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

최종 묶음은 **28 cfg / 71 TLC 실행, 기대 불일치 0, 실행 오류 0**이다. r4 A before 8 cfg/17회, r6 HerdrSession 직접 cfg 17개/46회, r5 비교 3 cfg/8회다. 이어 커밋 `3c64ad3da676b39005a5bf2ed7ff5c76e66ff684`에서 공식 선택기를 실행해 PendingRegistration 간접 종속 7개까지 포함한 **24 cfg**, 기대 불일치/오류 0, exit 0을 확인했다(`logs-adm-6/official-head-check.log`, 28.3초). 앞선 17개는 직접 cfg 집합이다. property 실행 전에 공식 checker의 fairness precheck를 적용했다. `WF_vars(AHintNext)`를 실제 사용하며 DB 응답 fairness로 무응답을 숨기지 않는다.

새 fixed cfg들은 8 invariants(`ANoForeignMutation`, `ANoDuplicateEffects`, `ANoUnprovedGrant`, `ANoStaleGrant`, `ANoSilentLoss`, `ANoEarlyUnlink`, `ANoForeignExecution`, `ARetryBounded`)와 `AProgress`/`ALegacyProgress`를 검사한다. 독립 positive state witness 19개는 typed 8필드 변경 후 거절, cursor/storage republish 후 전달, 정상·오류 회복·crash·redelivery·경합의 grant/create/prompt=1 상태를 확인한다.

### 반례 경로와 한계

- `ANoForeignMutation`: 같은 실행 identity를 확인해야 다른 실행 파일을 삭제하지 않는다. identity 비교 제거 시 `observe → intent → recheck → nonce 교체 → unlink_canonical → unlink_alias`의 6번째 상태에서 위반한다.
- `ANoForeignExecution`: 다른 실행 증거로 새 grant를 만들지 않는다. identity 비교 제거 시 `observe → intent → recheck → 두 unlink → commit → outbox → wake → admit → nonce 교체 → grant → create`의 12번째 상태에서 위반한다.
- `AProgress`(LegacyBefore): hint 정리 후에도 Legacy의 typed 증거를 소비하지 않으면 `admit → wait`에 머문다.
- `AProgress`(NoController): 첫 unlink 성공·두 번째 unlink 오류 뒤 `retry_wait`에 남고 이어갈 callback이 없다.
- `AProgress`(NoBudget): 영구 unlink 오류에서 `retry_wait → recover → unlink`를 무한 반복한다.
- `ANoStaleGrant`: front 확인을 제거하면 기존 receipt가 교체된 front를 grant한다. NoOutboxDiscovery는 boot의 outbox-only 한 상태에서 진행할 수 없다.

이 반례는 구현 예정 A 경로의 mutant 계약 검사다. 실제 A-admission이 아직 없으므로 Rust 경로의 RED→GREEN을 실행했다고 주장하지 않는다. typed identity의 기존 코드 대조군은 `runtime_hint_fence.rs:973`, `:1026`에 있으며 이번 설계 레그에서 cargo를 실행하지 않았다. 구현에서는 실제 owner 진입점 시험과 비교 제거 mutant로 재현해야 한다.

`ANoDuplicateEffects`는 단일 A 제어 pass의 grant/create/prompt ≤1이며 `ALegacyProgress`와 합쳐 해당 성공 case의 정확히 한 번을 확인한다. `ARedeliver`는 wake 횟수만 늘리고 post-grant crash는 `ACrash`에 넣지 않았다. 따라서 **prompt 이후 crash→boot→ack 재관찰의 end-to-end exactly-once 증명은 아니다**. 이 경계는 기존 owner/front claim·Pending reservation·terminal dedup을 이용하는 실제 통합시험 의무다. 새 dedup 권위를 모델만으로 선언하지 않는다.

filesystem bytes/fsync 불확실성, SQL NULL의 실제 decode, 실제 OS lock 순서·Source try-lock·실행기 queued task, history TTL pin·catalog I/O·bootstrap gate 순서는 Rust/PG 검증 대상으로 남는다. `ANotice`는 독립 기존 outbox에 통지가 가능한 경우다. queue와 PG outbox가 모두 영구 불능일 때 notified=true를 주장하지 않는다. DB가 반환되지 않으면 fence가 계속 잡힐 수 있으며 이 기존 위험에 시간 상한을 증명하지 않는다.

### 차트와 재생성

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


모델 산출 경로와 환경: `logs-adm-6/model-checks-r6.log`, `model-results.json`, `raw-r6/`, `model-witnesses.json`, `model-environment.json`; 공식 차트는 `sc-out-adm-6-identity-final/`, `sc-out-adm-6-live-final/`에 있다. 모델링은 착수 30분 정지선 안에 종료했다.


## 8. 진입점·재시작·입력 출처 전수표

### A-fence 기존 보장과 A-admission 잔여

| 경계 | 현 head 좌표·동작 | r6 처리 |
|---|---|---|
| 물리 entry key | tmux_common/runtime_hint_fence.rs:20,118-135 — parent dev/ino/basename bytes | 이미 구현, 재구현 0 |
| 물리 lock set | 같은 파일 :393-467, :410-412 정렬·중복 제거, :430-455 sidecar flock | 이미 구현 |
| source 관찰 | :39-64 runtime observation의 last_offset=0, relay_last_offset=None; :350-367 lock 전후 비교 | cursor 진전은 identity 변경 아님 |
| rollout typed identity | :78,83-112,214-218 | 현재 필드 계약 모델 반영 |
| 최초 publisher 관찰 | :239-278 초기 observation→acquire→fresh witnesses/source 비교 | stale 최초 관찰 거절 이미 구현 |
| reentry/path set | :332-342,475-499 | 같은 guard 전달 |
| tmux publisher host | :509-515 host Absent/Tmux만 허용 | Herdr host 발견 시 게시 거절 이미 구현 |
| Legacy admission | discord/router/message_handler/provider_dispatch/owner_input.rs:693-707,852-875 | 현재 Legacy 전용 proof 없음, r6 잔여 |
| 일반 activity | discord/turn_presence/activity/unbound.rs:319-328 | Missing+clean/정확한 Retired 외 hold; Legacy 전역 Idle 금지 |
| fresh start projection | discord/turn_presence/activity/unbound/live.rs:1048,1084 | 공개 reason 대신 private typed leaf 소비 |
| v3/control debt | turn_orchestrator/queued_head/compat.rs:38-45는 version 2만 Owned | r6 잔여 |
| history pin | queued_head/v2/retained.rs:77-79, source_admission.rs:618-622 | 현재 24시간 직접 만료, debt refs 보호 잔여 |

Rollout identity 포함 필드: rollout_path, session_id, codex_ownership.execution_nonce, proof_seq, proof dev/ino, 실제 rollout source 파일 metadata dev/ino. marker 저장 파일 inode와 JSON cursor는 비교하지 않는다. 일반 sibling 파일 witness는 여전히 inode/bytes를 비교한다. RolloutWitness::capture 실패 시 Witness::File로 남기므로 malformed 자료를 typed proof 성공으로 보지 않는다.

### r6 핵심 진입점 전수표

아래 파일은 src/services/ 상대 경로다.

| 진입점 | 현 head 좌표·확인 |
|---|---|
| Codex Herdr fresh/Pending | codex/herdr_turn.rs:200,223; Pending :240,255,261 그대로 |
| Herdr fence/DB/marker | herdr_launch.rs:393 acquire_async→:397 load_row→:479 Pending CAS→:489 registry→:490 host→:494 spawn→:499 same-fence clear→:503 drop |
| fresh incarnation | herdr_launch.rs:462-478 incarnation/command/nonce 생성, :730-755 binding context 생성 |
| Pending 재사용 | herdr_launch.rs:398-428 awaiting_evidence, fence 해제 후 probe_again; 새 create 권한 아님 |
| Codex direct publisher | codex/tui_session_launch/first_input.rs:200 publish, :210 guard carrier, :220 validate, :226 owner, :227 hint writer |
| Codex watcher | discord/watchers/codex_tui_restore.rs:208 fence→:264 Source→:316 verified metadata→:324 registration→:331 under-fence writer |
| Codex wrapper | codex.rs:1430 observation, :1530 cleanup, :1559 runtime writer, :1603 create 부근 |
| Codex absence | codex/absence_launch.rs:77,93,153,298,308; :93 초기 observation, :308 under-source cleanup |
| Claude direct | claude/tui_session_launch.rs:46 observation→:51 acquire→:57 cleanup→:63 hint→:94/:97 create/respawn |
| Claude wrapper | claude.rs:2407 observation, :2610 cleanup, :2635 hint, :2698 create, :2729 spawn stamp |
| hint writer | tmux_common/session_markers.rs:146 production under_fence, :151 validate/:153 write; :126 normal API는 :125 cfg(test) 전용 |
| normal cleanup | tmux_common.rs:1186 acquire, :1200 under-fence, :1222 under-source |
| explicit Codex clear | tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:206 fence, :222 under-source cleanup |
| clear teardown | discord/commands/control/clear_teardown/execution_cleanup.rs:285 try fence→:290 try Source→:309 registry→:311 cleanup |
| managed reset | discord/commands/control/managed_reset.rs:74-89 fence, :92 under-source adapter, :120-127 기존 process kill, :134-147 기존 tmux/cleanup |
| runtime cleanup | herdr_launch/runtime_marker.rs:11 clear_fresh same guard; :24 clear_retired; :37 outer fence; :66 clear_owned; :86-107 nonce/host/registry/Missing 관문 |
| owner start | provider_dispatch/owner_input.rs:699 inflight/rest, :704-707 activity, :710 target, :852 preclaim, :859 start_waits, :875 claim |
| drive serialization | discord/queue_io/input_owner/drive.rs:88-92; handoff await :102, running 해제 :117 |
| exact front grant/claim | turn_orchestrator/queued_head/v2.rs:398,411 그대로 |
| recovery continuation | provider_dispatch/owner_input.rs:664-681, input_owner/recovery.rs:16,39 그대로 |
| carrier boundaries | provider_dispatch/owner_input.rs:439,472,1025; provider_dispatch.rs:52,613; headless_turn.rs:1068; intake_turn.rs:2179; codex/herdr_turn.rs:65 그대로 |
| 기존 Pending first input | codex/herdr_turn/first_input.rs:79-110 restore_pending; :89 Source→:90 runtime.block_on(load), :96-104 owner/nonce/Pending 재검증 |

기존 Pending first input 복구는 main 952d58fe8ef7db722142c25aac060c8b7aa999a7 (#8066)에 이미 착지했다. codex/herdr_turn/first_input.rs는 main 6a5678349833d596c48dc280a9ce2d6f6683657a와 현 head가 git diff 0으로 바이트 동일하다. 재구현하지 않는다. :89-90 Source 아래 기존 DB await는 A-admission이 새로 만드는 경로가 아니며 수명/권한 범위 확대 0으로 구분한다.

herdr_launch.rs의 A-fence 구간 DB await 중 물리 fence는 유지되지만 Source mutex는 유지하지 않는다. Source는 runtime_marker::clear_fresh의 동기 closure에서 취득한다. r6는 기존 Pending CAS semantics와 DB 무응답 상한 제외 결정을 유지한다. 반환되는 실패의 유한 재시도와 영원히 미반환하는 DB는 다른 범위다.

### v3 reader/source fence/history pin 전수표

아래 좌표는 r5 이후 이동이 없다.

| 파일 | 연결 좌표 |
|---|---|
| turn_orchestrator/queued_head/compat.rs | :5-7 read enum; :12-13 v2_holds_input; :20-34 holds_input; :38-45 codec |
| turn_orchestrator/pending_queue_persistence/queued_recovery.rs | :15-18 catalog; :35-39 checked recovery; :53-59 idle/read-only 분기; :87-91 v2_channels; :104-142 held sources; :145-197 catalog |
| discord/runtime_bootstrap/queued_recovery.rs | :50-51 생산 소비자 1: v2_channels→owner_recover |
| discord/runtime_bootstrap.rs | :35-43 생산 소비자 2: complete_active_reconcile→v2_channels→wake |
| discord/queue_io/input_owner/recovery.rs | :16 adopt/tick, :39 history projection, :46-49 notice drain |
| turn_orchestrator/queued_head/v2/sources.rs | :13-43 fenced; :47-68 Owned restore/namespace/original identity |
| discord/recovery_known_ids.rs | :149 entry; :166-170 owed→Queued/delivered→V2Delivered |
| discord/catch_up/phase1_sweep.rs | :132 merge_v2_held_sources |
| discord/catch_up/phase2_sweep.rs | :134 같은 소비 |
| turn_orchestrator/queued_head/v2/origin.rs | :127 Owned read waiting_origin |
| turn_orchestrator/queued_head.rs | :108 legacy reservation reader의 Owned 거절 |
| turn_orchestrator/dispatch_reservation.rs | :459-460 restart_drain_legacy Owned 보존 |
| turn_orchestrator/queued_head/snapshot.rs | :29 legacy-only parser; v3를 empty 성공으로 변환 금지 |
| turn_orchestrator/pending_queue_persistence/snapshot_io.rs | :101 legacy-only checked loader |
| turn_orchestrator/checked_purge.rs | :128 queue_file, :582 actor-owned purge vs actorless legacy parse |
| turn_orchestrator/queued_head/v2/projection.rs | :162 queue projection holds_input, :220 holds_input |
| discord/runtime_bootstrap/o_writer_host.rs | :352 mailbox.holds_input host-ready 영향 |
| turn_orchestrator/queued_head/compat.rs | :50 clear guard, :59 save guard, :70-77 non-v1 거절 |
| turn_orchestrator/queued_head/v2/retained.rs | :77 expire_binds, :78-79 terminal TTL |
| turn_orchestrator/queued_head/v2/source_admission.rs | :468-472 prune_binds, :618-622 settled bind TTL, :749 prune 소비 |

현재 producer v2_channels는 queued_recovery.rs:88에서 read_catalog(...).snapshots만 취하여 quarantined/scan_errors를 버린다. structured 반환형을 두 실제 소비자 모두에 연결해야 한다. 현재 runtime_bootstrap.rs:36 mark_reconcile_complete가 :39 discovery보다 앞이므로 discovery/gate 성공 뒤로 옮긴다. namespace scan 실패를 성공한 reconcile로 표시하면 안 된다.

Catalog 후보 오류: queued_recovery.rs:177-187 numeric filename 확인 후 read_to_string/inspect 실패를 channel/error quarantined에 남긴다. namespace scan 오류는 :153-159, entry scan 오류는 :164-171. top-level pass id와 catalog_unreadable stage를 이 경계로 운반한다.

기존 compat.rs:48-54 require_clearable은 read/parse 실패 때 clear를 허용하는 main 정책이다. :70-77은 parse 가능한 non-v1을 거절한다. r6에서 기존 권한을 새로 해결했다고 주장하지 않고 actor-owned v3 capture와 legacy-only 오류 경계를 구분한다. main 6a567834에도 동일하다.

### 부팅/재시작/incarnation·인접 제어 경로

| 경로 | 현재 좌표 / r6 경계 |
|---|---|
| boot restore | runtime_bootstrap/queued_recovery.rs:12-24 restore→stale cards→complete_active_reconcile |
| boot recover | 같은 파일 :26-54, :50 Owned 채널 recovery |
| owner re-adoption | provider_dispatch/owner_input.rs:664-681→input_owner/recovery.rs:16-49 |
| process incarnation | herdr_launch.rs:462-478 및 :730-755; nonce/owner 변화 시 A 관찰 무효 |
| root preclaim | discord/herdr_terminate/root_replaced/preclaim.rs:14,37 Known만 처리 |
| Cancel queued | discord/commands/control.rs:715, :737 all-waiting, :749 exact cancel; 실행 중 입력 권한 불변 |
| Stop | discord/commands/control.rs:617; native-stop 기존 보존 계약 |
| lifecycle stop/kill cleanup | turn_lifecycle.rs:310, :501-502 host-gate Cleared 뒤 cleanup |
| explicit Terminate | discord/herdr_terminate.rs:401,437,621 기존 warrant/retire/terminate |
| HTTP forced cancel | queue/http_force.rs:33-42 selection, :76-86 terminate; queue/cancel_effects.rs:306 이후 기존 purge |
| purge | turn_orchestrator/checked_purge.rs:118,311-352,582 exact capture/save 결과 |
| policy close | turn_orchestrator/policy_close.rs:285,341,362는 별도 release fence이며 물리 lock 대체 아님 |

### 입력 출처 분류

정본은 discord/queue_io/input_owner/origin.rs:5-12,26-43,96-99다.

| 출처 | 실제 분류 | A 정책 |
|---|---|---|
| 일반 Discord | context 없음→Discord→person | 기존 일반 경로 유지 |
| iMessage 사람 입력 | person_input(context)+context 있음→Imessage→person | 사람 정책/GUID identity 유지 |
| HTTP headless start | context.origin=headless→HttpStart | synthetic, 저장 ticket/정확한 front 유지 |
| routine/scheduled/기타 context | person_input 아님/headless 아님→Routine | synthetic, 원문 custody 유지 |
| DirectHeadless | cfg(test)만 존재 | 운영 출처로 세지 않음 |
| LocalTui/RemoteTui | owner_takes는 origin과 무관 true | A는 Codex LocalTui+Herdr mapping만 |
| Process/NonTuiProvider | owner_takes=false | A 제외 |

owner_input.rs:205-219,250-256 synthetic/person/prefer_person; :846-865 provenance와 start_waits; :1032-1044 late native read. ingress.rs:457-458 Accepted wake, :496 synthetic stored ticket 요구를 유지한다.

### DB 좌표 보정

src/db/dispatched_sessions/hosted_execution.rs:180-182 SQL NULL→Legacy; :229-230 Found/Missing; :237 load API, :263 query 생성, :268-269 fetch_optional await, :297 Found 구성. install_pending_pg :408; compare_and_set_pg :573; :585 UPDATE query 생성; :600-601 execute await. r5 :588/603를 두 execute await처럼 쓰지 말고 실제 await 좌표 :600-601을 쓴다.

### 증거

- git rev-parse origin/main; git merge-base HEAD origin/main → 6a5678349833d596c48dc280a9ce2d6f6683657a.
- git diff 6a5678349833d596c48dc280a9ce2d6f6683657a HEAD -- src/services/codex/herdr_turn/first_input.rs → 출력 0.
- git show --no-patch --format='%H %s' 952d58fe → 952d58fe8ef7db722142c25aac060c8b7aa999a7 (#8066).
- r5 두 appendix path:line 184개에 git show old:path와 현 path를 difflib equality mapping. same 144/moved 40/manual substituted refs 12/missing 0.
- rg로 v2_channels 생산 호출 두 곳, marker writer/cleanup/Source/기존 제어 caller 확인.
- cargo/PG/clippy/TLC 실행은 이 census에서 하지 않았다. 모델 산출물과 cargo 증거를 이 파일 결과로 대체하지 않는다.

### r5 두 appendix의 모든 행 갱신

입력: r5 design-h-5.md §5 두 appendix, 코드 정본 `289f41b1d0f48fab0dc426e652308eced4326fce`.
원본 행은 모두 유지했다. 원본 path:line reference 184개 중 좌표+줄 내용 동일 144개, 이동/교체 40개, missing file 0. 184는 distinct entrypoint 수가 아니다. 정확히 매칭되는 줄은 difflib로 이동 좌표를 찾고, 교체된 12 reference 종류는 해당 production 심볼을 읽어 수동 고정했다.

분류 보정: `tmux_common/session_markers.rs:126` normal writer와 `tmux_common.rs:6` 재수출은 현 head에서 `#[cfg(test)]` 전용이다. 실제 생산 writer는 `session_markers.rs:146` under_fence (`:151` validate, `:153` write), 생산 재수출은 `tmux_common.rs:14`다. 아래 원본 행을 보존하되 이 보정을 적용해 읽어야 한다. 새로운 A-fence 구현 심볼은 이 원본 raw reference 계수에 합산하지 않는다.

기존 Pending first input 복구는 `952d58fe8ef7db722142c25aac060c8b7aa999a7` (#8066)에 이미 착지했고, `src/services/codex/herdr_turn/first_input.rs`는 main `6a5678349833d596c48dc280a9ce2d6f6683657a`와 현 head가 바이트 동일하다. 재구현 0. `:79-110 restore_pending`, `:89` Source 아래 `:90 runtime.block_on(load(...))`는 기존 경로이며 A-admission이 새로 만드는 DB-await 경로가 아니다. r6에서 이 수명/권한 범위를 넓히지 않는다.

### runtime / 실행 등록 생산 심볼 전체

아래 모든 행의 `src/services/` prefix는 생략했다. r5 원문 행/분류를 유지하되 좌표를 현 head로 옮겼으며, normal writer의 test-only 보정은 위 안내를 우선한다. 행마다 실제 line 목록을 표시하며, 기존 read-only reader에는 A 삭제 기능을 추가하지 않는다. 물리 fence를 거치는 publisher·cleanup은 위 핵심 표에서 따로 분류했다. ProcessBackend/다른 provider reader는 A 대상 배제 증거 또는 기존 독립 경로이며 H 종료 기능으로 연결하지 않는다.

| 파일:line | 함수/소비 분류 | 모델 관계 |
|---|---|---|
| `claude.rs:51,2635,2913,2918` | (module) / execute_streaming_local_tmux / send_followup_to_process | AFenceContender; publisher fence |
| `claude/host_gate.rs:177` | tmux_wrapper_poll_probe | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `claude/process_session_launch.rs:65,74,175` | execute_streaming_local_process | AObserve registry guard; 기존 process 효과 별도 |
| `claude/tui_session_launch.rs:63` | prepare_and_create_claude_tui_session | AFenceContender; publisher fence |
| `codex.rs:68,69,95,647,1559,1603,1628` | (module) / codex_tui_session_turn_lock / execute_streaming_local_tmux / prepare_codex_tui_hook_overrides | AFenceContender; publisher fence |
| `codex/absence_launch.rs:153` | execute_observed | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `codex/herdr_turn.rs:40,223,261` | (module) / fresh_launch / pending_launch | ACreate / 기존 Pending 별도 |
| `codex/herdr_turn/first_input.rs:145` | write | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `codex/process_session_launch.rs:81,88,137,146,170` | execute_streaming_local_process_codex | AObserve registry guard; 기존 process 효과 별도 |
| `codex/tui_session_launch/first_input.rs:53,82,126,227` | create / publish_codex_tui_launch_script / spawn_owned | AFenceContender; publisher fence |
| `codex_tui/execution_end/confirm.rs:28` | observe | AObserve registry guard; 기존 process 효과 별도 |
| `codex_tui/warm_followup.rs:505,1220,1252` | verified_warm / verified_warm_inner / warm_followup_on | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/admin_host_guard.rs:381` | row_is_legacy_tmux | AObserve registry guard; 기존 process 효과 별도 |
| `discord/commands/control/clear_teardown/clear_end_record.rs:135` | vacant | AObserve registry guard; 기존 process 효과 별도 |
| `discord/commands/control/clear_teardown/execution_cleanup.rs:161,309,382` | inflight_ready / normal_end / run_with | AObserve registry guard; 기존 process 효과 별도 |
| `discord/commands/control/managed_reset.rs:120,121` | end_process | AObserve registry guard; 기존 process 효과 별도 |
| `discord/commands/control/relaunch.rs:102` | pane_state_of | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/health/recovery/live_agent_recovery.rs:187` | fence_runtime | AObserve registry guard; 기존 process 효과 별도 |
| `discord/inflight/host_recovery_guard.rs:59,61` | with_inflight_row | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/recovery_engine/manual_rebind_override.rs:64` | runtime_state | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/recovery_engine/rebind_runtime.rs:38` | resolve_rebind_runtime_state | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/router/message_handler/headless_turn.rs:541,562` | start_reserved_headless_turn_admitted | AChange/source evidence; 기존 binding cleanup 별도 |
| `discord/router/message_handler/provider_dispatch/owner_input.rs:1061` | execute_owned | AChange/source evidence; 기존 binding cleanup 별도 |
| `discord/router/message_handler/provider_isolation.rs:377,549` | observed_runtime_kind_for_managed_tmux / reconcile_managed_tmux_runtime_kind_for_config | AChange/source evidence; 기존 binding cleanup 별도 |
| `discord/terminal_ui_obligation.rs:428` | terminal_ui_session_ready_for_input | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/tmux_watcher/loop_poll_prologue.rs:412,413,418` | poll_watcher_output_or_continue | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/tmux_watcher/no_result_exits.rs:121` | handle_no_result_exits | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/tmux_watcher/terminal_readiness.rs:596` | watcher_session_ready_for_input | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/tui_prompt_relay/rehydration.rs:912` | tmux_session_is_codex_tui | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_bridge/tmux_runtime.rs:113` | tmux_ready_for_input_without_tui_pane | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_bridge/tmux_runtime/claude_stop_delivery.rs:378` | interrupt_observed_claude_session | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_bridge/tmux_runtime/native_stop.rs:39` | protected | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_finalizer/watcher_backstop.rs:99` | watcher_backstop_turn_is_terminal | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_presence/activity/unbound.rs:291,292,293,504` | host_scope_result / raw_evidence | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_presence/activity/unbound/live.rs:32,62` | local_trace | AObserve registry guard; 기존 process 효과 별도 |
| `discord/watchers/codex_tui_restore.rs:331` | commit_live_direct_resume_fallback | AFenceContender; publisher fence |
| `discord/watchers/lifecycle/claude_restore.rs:83` | claude_tui_restore_fallback_modified_since | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/zombie_foreground_release.rs:202` | tui_structurally_idle | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `herdr_launch/runtime_marker.rs:45,90` | clear_owned / runtime_paths | AObserve registry guard; 기존 process 효과 별도 |
| `provider/session_probe.rs:34` | tmux | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `qwen.rs:44,45` | (module) | AObserve registry guard; 기존 process 효과 별도 |
| `qwen/session_lifecycle.rs:378,445,469` | execute_streaming_local_process | AObserve registry guard; 기존 process 효과 별도 |
| `session_activity.rs:79` | resolve | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `session_backend.rs:368,383,391,507,532` | insert_process_session / process_session_available_for_followup / process_session_pid / remove_process_session / terminate_process_session | AObserve registry guard; 기존 process 효과 별도 |
| `session_backend/auth_profiles.rs:3,12` | (module) / prepare | AObserve registry guard; 기존 process 효과 별도 |
| `session_host/process_host.rs:39,95` | execution_pid / presence | AObserve registry guard; 기존 process 효과 별도 |
| `session_host/resolve.rs:19,21,36,39,127,129,132,147,148,150,389,390,397,408,409,422,423` | (module) / host_votes / kind_hint / legacy_target_host / resolve_host_kind / unread | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `session_resume.rs:632` | clear_resume_runtime_binding | AChange/source evidence; 기존 binding cleanup 별도 |
| `stale_turn_reconciler.rs:749` | independent_tmux_liveness | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `tmux_common.rs:8,14,1048,1245` | (module) / cleanup_session_temp_files_under_source_authority / tmux_exact_target | AFenceContender; publisher fence |
| `tmux_common/session_markers.rs:126,146,152,157,160` | write_tmux_runtime_kind_marker는 :125 cfg(test); 생산 writer는 :146 under_fence; resolver :157 | AFenceContender; publisher fence |
| `tui_o/cutover/channel_gate.rs:372` | session_kind | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `tui_prompt_dedupe/runtime_binding.rs:927` | clear_tmux_runtime_binding | AChange/source evidence; 기존 binding cleanup 별도 |
| `tui_prompt_dedupe/runtime_binding/codex_policy/absent_open.rs:93,99` | runtime_markers_match / target_absent | AObserve registry guard; 기존 process 효과 별도 |
| `tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:119` | dead | AObserve registry guard; 기존 process 효과 별도 |

### 추가 cleanup / queue reader 생산 심볼 전체

| 파일:line | 함수 | 모델 관계 |
|---|---|---|
| `claude.rs:2610` | execute_streaming_local_tmux | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `claude/tui_session_launch.rs:57` | prepare_and_create_claude_tui_session | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `codex.rs:1530` | execute_streaming_local_tmux | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `codex/absence_launch.rs:308` | execute_observed | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/commands/control/clear_teardown/execution_cleanup.rs:311` | normal_end | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/commands/control/managed_reset.rs:145,147` | end_process | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/recovery_known_ids.rs:166` | merge_v2_held_sources | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/router/message_handler/provider_isolation.rs:547` | reconcile_managed_tmux_runtime_kind_for_config | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/runtime_bootstrap.rs:39` | complete_active_reconcile | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/runtime_bootstrap/o_writer_host.rs:352` | legacy_busy | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/runtime_bootstrap/queued_recovery.rs:50` | restore_queued_and_inflight_work | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/tmux_reaper.rs:346,689` | clean_orphan_sessions / reap_fresh_routine_orphan | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `qwen/session_lifecycle.rs:111` | execute_streaming_local_tmux | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `tmux_common.rs:1186,1192,1219,1222` | cleanup_cleared_session_temp_files / cleanup_session_temp_files / cleanup_session_temp_files_under_source_authority | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:222` | commit | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `turn_lifecycle.rs:504` | stop_turn_with_policy | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `turn_orchestrator/checked_purge.rs:128,582` | queue_file / try_purge_queue | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/dispatch_reservation.rs:459,460` | restart_drain_legacy | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/pending_queue_persistence.rs:8,20,21` | (module) | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/pending_queue_persistence/queued_recovery.rs:53,54,59,87,90,104,111,112,187` | load_pending_queues_checked / read_catalog / v2_channels / v2_held_sources | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/pending_queue_persistence/snapshot_io.rs:101` | load_channel_pending_queue_checked | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/policy_close.rs:285,341,362` | close / fence / reconcile | AChange(epoch); 기존 release fence 별도 |
| `turn_orchestrator/queued_head.rs:97,108,112` | is_set / read_snapshot_meta | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/queued_head/compat.rs:7,12,13,20,38,42,44` | (module) / holds_input / inspect / v2_holds_input | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/queued_head/snapshot.rs:29` | parse_snapshot | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/queued_head/v2/origin.rs:3,127` | (module) / waiting_origin | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/queued_head/v2/projection.rs:162,220` | holds_input / v2_queue | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/queued_head/v2/source_admission.rs:468,662,749` | fresh_candidate / prune_binds | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `turn_orchestrator/queued_head/v2/sources.rs:13,48,49` | fenced / restore | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |

다음 문장은 r5의 검색 범위 설명을 승계한 것이며 이 레그의 새 raw 전수 실행 증거를 뜻하지 않는다. 전수 raw 검색은 runtime 심볼·Codex create/bind, runtime-kind literal 경로, broad cleanup, queue codec/discovery/holds_input/source fence를 따로 보존한다. literal 검색에는 운영 스크립트의 새 writer가 없고, e2e fixture의 파일 쓰기는 시험 경로다. `policy_close`의 remint fence는 이름이 같은 별도 개념이며 물리 lock의 대체물이 아니다. `SnapshotRead` 변경 후 컴파일이 잡는 exhaustive match와 source-scan으로 reader 누락을 다시 확인해야 한다.



## 증거 목록

- 원문 이슈: `gh issue view 8057 --repo itismyfield/AgentDesk --json number,title,body,url,state`의 읽기 결과. 이슈/PR/DAG/Discord 쓰기 0.
- r4/r5 설계와 리뷰: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8057/{design-h-4.md,design-h-5.md,review-design-h-4.out.md,review-design-h-5.out.md}`.
- A-fence 실측/검증 승계 입력: 같은 디렉터리 `report-afence-4.md`. 실측17/+769와 typed witness 의미만 대조했으며 A-fence 시험을 이 레그가 새로 실행한 것은 아니다.
- 현 head census/appendix와 모델 원시 증거: 같은 디렉터리 `logs-adm-6/`, `sc-out-adm-6/`. 구체 실행 수·SHA·출력은 최종 모델 절/보고서에 기록한다.
- 본 레그의 운영 코드 변경·Rust/PG 실행 0. 모델의 GREEN은 구현 완료·배포 완료가 아니다.
