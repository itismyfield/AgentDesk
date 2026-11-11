# Codex Herdr marker-only A 설계 r3

상태: 구현 전 PLAN. **운영 코드 변경 0**. 코드 정본은 `289f41b1d0f48fab0dc426e652308eced4326fce`, 모델/문서 입력은 `dfba87a0ae2a576507f0863e81cfb8f1ead8bfdb`다. 두 SHA 사이 `src/` 차이는 없다. 이 문서는 r2의 A를 대체한다. B(HandoffWaiting·넘김 journal)와 C(종료 포트)는 보류하며, #8077 착지 뒤 공유 계약으로 다시 설계한다. 이 A에는 `/exit`, 화면 판독, pane close, kill, provider 종료 판정 기능이 없다. 아래 CleanupIntent는 파일 정리의 복구 기록이며 predecessor 종료 attempt가 아니다.

## 1. P1 결정: Legacy 부재 증거를 좁은 admission에서 소비한다

**Missing과 Found(Legacy)를 구분하고, 검증한 Legacy에만 start 전용 읽기 전용 projection을 허용한다.** Legacy를 일괄 Idle로 만드는 변경이나 DB 행 삭제/Retired 변환은 하지 않는다. SQL NULL인 정상 구형 행도 시작할 수 있어야 하므로 Legacy를 항상 최종 거절하는 안은 택하지 않았다.

현재 `HostedRecord::decode`는 SQL NULL을 Legacy로 읽고(`src/db/dispatched_sessions/hosted_execution.rs:183`), 실제 행이 있으면 Found다(`:300`). `turn_presence/activity/unbound.rs:319`는 Missing+clean과 정확한 Known Retired만 허용한다. 따라서 r2처럼 hint만 정리하면 Found Legacy는 `hosted_active_or_unknown`이고, `provider_dispatch/owner_input.rs:704`에서 다시 Wait::Event다. `herdr_terminate/root_replaced/preclaim.rs:37`도 Known만 다룬다. 반복 wake는 이 증거를 바꾸지 않는다.

A 대상은 **Codex·로컬 owner·Herdr로 매핑된 TUI 채널**, 실제 시작을 빚진 동일 원문/front이며, 다음 모두를 만족해야 한다.

- canonical/exact lookup이 Missing이거나 동일한 Found Legacy 행이다. alias 불일치·Unknown·Conflict는 거절한다. Legacy 증거는 session row id, canonical identity(provider/token/channel), exact session key, raw SQL NULL 기대값을 포함한다. 동일 id라도 owner/row 내용이 바뀌면 무효다.
- 물리 대상에 대응하는 owner 후보가 정확히 하나다. 현재 token의 lookup 하나로 유일하다고 결론내리지 않는다. canonical/legacy 경로와 연결된 등록 mapping·alias·DB identity 후보를 다른 token/provider까지 읽기 전용으로 대조한다. 전체 후보를 확인할 수 없거나 같은 경로에 다른 owner가 대응하면 거절한다.
- tmux의 정확한 logical target은 **Missing**, process registry는 없음이다. Present/dead pane·shell 잔존·probe 실패는 부재 증거가 아니다. provider가 Ended여도 tmux 객체가 남으면 A 대상에서 제외한다.
- inflight/locator, durable/inflight runtime, 활성 Source/바인딩 및 호스트 증거가 새 실행을 가리키지 않는다. owner 파일·source policy·runtime root·node·기존 nonce의 관측 상태가 일치한다. 처음부터 nonce가 Absent인 구형 Legacy는 다른 증거가 완전할 때만 허용한다. 관찰 후 nonce 소실/변경, 읽기 오류를 Absent로 바꾸지 않는다.
- 확인한 runtime hint는 양 경로 모두 regular file이고 bytes가 `codex_tui`다. foreign/unknown bytes, leaf symlink, 경로/부모/file identity 교체는 거절한다. 처음부터 한 경로가 없으면 그 Absent도 snapshot에 넣는다.

Known Pending/Bound/Unknown은 A에서 변경하지 않는다. **Known Retired도 Legacy permit으로 통과시키지 않는다.** 정상 Retired 경로의 기존 `clear_retired`/host·nonce 조건(`herdr_launch/runtime_marker.rs:14,68`)을 유지한다. A intent 재관찰 중 Retired로 바뀌면 A 요청을 명시 거절한다. 모델의 retired case는 이 음성 대조군이며, 기존 Retired 복구 전체의 증명이 아니다.

### 좁은 API와 소비 순서

새 `codex::legacy_hint`가 파일 정리와 증거 수집을 소유하고, owner의 작은 child adapter가 pre-admission/복구를 조정한다. `owner_input.rs:852`의 root preclaim 뒤, `:859`의 start_waits **전에** A를 관찰한다. 일반 start failure나 claimed owner scope를 만들기 전이다. 사람 입력 경로를 이용해 synthetic 원문을 우회시키지 않는다.

projection은 `Reading::publish_if_current`의 기존 fresh snapshot 재검증 형태를 사용한다(`activity/unbound/live.rs:1084`). 그러나 기존 `local_legacy_absence_for_start`를 그대로 true로 간주하지 않는다. 그 함수는 exact/canonical divergence와 Idle/no_session을 요구하는 별도 경로다(`:412,1099`). 새 작은 `live/codex_marker_absence.rs` child에서 receipt의 **동일 Legacy 행 + owner + paths + front + epoch + 현재 부재**를 비교해 start 전용 `VerifiedMarkerAbsence`를 만든다. `activity_now()`의 일반 결과는 바꾸지 않고, 다른 reader는 계속 Unknown을 본다.

`start_waits`의 inflight/rest 관문(`owner_input.rs:699`), 대상 pane 선택(`:710`), host/config/root/Source 검증을 모두 남긴다. 기존 activity가 Idle이거나, reason이 정확히 `hosted_active_or_unknown`이며 새 proof가 그 **유일한 Legacy 행 차이만** 해소할 때 통과한다. 다른 busy/Unknown 원인은 우회하지 않는다. 삭제나 DB mutation은 이 read-only projection에 넣지 않는다.

admission은 물리 fence를 다시 취득하고 fresh proof를 재검증한 뒤 동일 front를 grant/claim한다(`queued_head/v2.rs:398,411`). fence guard는 typed 내부 start context로 Codex fresh/Pending CAS·host/spawn 게시까지 전달하고 그 뒤 해제한다. Source Mutex는 await를 가로질러 보유하지 않는다. 일반 프로세스/다른 owner·다른 nonce의 입력 권리를 이 permit으로 만들지 않는다. 기존 `herdr_launch.rs:425`의 Legacy/Retired 분기, `hosted_execution.rs:411`의 CAS, Startup/nonce/rollout/owner 입력 검증은 유지한다. 서버/socket create의 불명확한 결과와 prompt 재전송은 기존 Pending/owned-input 계약에 맡긴다.

## 2. 물리 fence, 삭제, durable 기록과 wake

### 물리 경로의 fence key

owner tuple이나 논리 키를 hash한 lock으로 구현하지 않는다. **각 물리 directory entry마다** key=`(parent st_dev, parent st_ino, basename bytes)`로 정하고, canonical·legacy entry들의 합집합을 정렬·중복 제거해 **모두** 잠근다. 전체 경로 집합 하나를 hash하면 서로 다른 canonical 경로가 같은 legacy 파일을 공유할 때 다른 lock을 얻으므로 금지한다. 같은 물리 entry를 가리키는 두 alias는 반드시 같은 lock이다. 파일 inode는 snapshot 값이며 fence key가 아니다. unlink/recreate 뒤에도 같은 fence여야 한다.

부모는 신뢰 경로를 물리적으로 해석해 directory fd로 고정하고 key를 확인한다. leaf symlink·불명확한 alias·부모 교체는 변경 0으로 거절한다. 각 부모의 동일 basename에 대응하는 고정 sidecar lock을 `openat`/`flock`으로 사용하며 lock file은 unlink하지 않는다. 같은 부모를 다른 경로로 연 controller도 같은 물리 sidecar를 잠근다. 새 루트나 owner별 lock directory는 쓰지 않는다. 서로 다른 UID/비관리 writer·강제로 바꾼 부모 디렉터리는 신뢰 경계 밖이며 감지되면 기능을 켜지 않는다.

순서는 **정렬된 물리 locks → DB fresh read(필요시 async) → process-local SourceAuthority → 동기 파일 재검증/변경**이다. Source를 잡은 채 flock을 기다리거나 Source를 재진입하지 않는다. normal writer/cleanup은 바깥에서 fence를 취득하고, 이미 source 아래인 watcher/clear/reset은 바깥 호출부에서 fence를 먼저 취득해 under-authority token을 넘긴다. try 경로의 경합은 기존 Retry/보류로 돌려 변경하지 않는다. 기존 read-only Source thunk 전부를 전역 OS lock으로 바꾸지 않는다.

normal/under-fence API 모두 guard의 물리 path set을 확인한다. 이미 fence를 보유한 wrapper의 cleanup, fresh launch의 clear_fresh, A guard를 전달받은 launch는 **같은 guard를 넘겨** 호출하며 flock을 다시 열지 않는다. TLS 재진입 허용이나 자동 key 재탐색으로 해결하지 않는다. Source 아래에서 fence가 필요해진 호출은 바깥으로 올리며, 전체 caller를 못 옮기면 A-admission을 활성화하지 않는다.

직접 TUI·watcher·wrapper publisher와 기존 fresh/retired marker 정리, broad cleanup의 runtime-kind 삭제가 모두 참여해야 한다. broad cleanup 자체를 A에서 호출하지 않는다. 그 기존 호출부에 fence만 연결하며 삭제/kill의 기존 수용 조건과 권한을 늘리지 않는다. raw hint writer는 private로 두고 normal/under-fence API로만 호출한다. 기다렸다 fence를 얻은 publisher는 기존 config·owner·host/spawn/current-source 관문을 다시 읽는다. 새 Herdr host/nonce 또는 다른 실행을 발견한 옛 tmux publisher는 게시/생성을 거절하며, 오래된 pre-lock 관찰만으로 진행하지 않는다. 다른 controller가 부재/owner 조건을 공유한다는 전제를 확인할 수 없으면 A의 유일 owner proof를 발급하지 않는다. watcher는 기존 Source 내부(`codex_tui_restore.rs:253`) 진입 전에 fence를 취득하고 `:317`에서 under-authority writer를 쓴다. Source만의 잠금은 process-local이다(`tmux_common.rs:75`). 이 OS fence 구현은 두 controller 시험으로 확인하기 전에는 증명된 것으로 취급하지 않는다.

### 단계와 crash 처리

| 단계 | durable 상태·허용 효과 | 모델 |
|---|---|---|
| 최초 관찰 | 양 경로·owner/row·front/epoch snapshot. 소유 불명/probe 실패면 hint 변경 0, 입력 0으로 명시 정산 | AObserve |
| intent commit | 기존 queue가 원문을 계속 소유. `CleanupIntent{namespace, identity, front, epoch, row expectation, path snapshots}`를 기존 fsync/CAS publish로 저장. source 원문을 별도 복제하지 않음 | AIntent |
| lock 재취득·재검증 | intent commit 동안 놓았던 locks를 다시 취득하고 모든 증거 재관찰. commit 불확실/DB unreadable은 unlink 0 | ARecheck |
| canonical unlink | exact `codex_tui`와 snapshot 일치 파일만 삭제. 나머지 증거/파일 보존 | AUnlinkCanonical |
| legacy unlink | 같은 조건. canonical 성공 뒤 legacy 실패는 Partial, 완료를 기록하지 않음 | AUnlinkAlias/APartialUnlink |
| 정리 receipt commit | 양 경로 부재와 동일 owner/front/epoch를 확인한 `CleanReceipt`. unlink와 별도 단계. 실패/재시작이면 intent가 남음 | ACommit |
| outbox commit | receipt에 대응한 안정 event id=`namespace/cleanup-id/front/epoch`의 WakeOutbox를 CAS. receipt가 있는데 outbox가 없으면 boot가 재생성 | AOutbox |
| wake delivery | owner drive에 wake. ack 전 재전달 가능. wake 자체가 grant가 아님 | AWake/ARedeliver |
| read-only admission | locks 재취득, current row·source·owner·paths·front·epoch 재검증, 정확한 front claim. grant/create/prompt 각각 한 번 | AAdmission/AGrant/ACreate/APrompt |
| ack | 동일 event id와 소비/정산 결과가 durable인 뒤 outbox/intent 해소. 원문/terminal identity dedup 유지 | AAck |

unlink 뒤 receipt commit 전 crash도 **이미 durable인 intent**에서 재관찰한다. 이 intent가 처음 읽었던 파일과 증거가 일치하고 다른 실행이 없을 때만 해당 entry의 현재 Absent를 idempotent 완료로 인정한다. 새로운 파일·nonce가 있으면 지우지 않는다. hint를 되살리거나 Source/O cursor/spawn/owner 파일을 정리하지 않는다. partial unlink 뒤 두 번째 경로를 읽을 수 없으면 완료로 간주하지 않으며, 저장/관찰이 회복되면 재개한다. 영구 저장 장애의 자동 전달은 약속하지 않는다.

front·epoch가 바뀌면 이전 event가 새 front에 grant하지 않는다. 해당 원문이 여전히 queue에 있으면 정확한 identity로 보존한 채 **A control의 명시 거절/미전송 통지**로 정산하고, 일반 start Opaque/두 번 retry 경로에 넣지 않는다. terminal 처리가 필요하면 원문을 알려진 미전송 retained 상태로 옮기는 것과 notice debt를 같은 commit에 기록한다. 이미 취소/settled된 원문이면 기존 terminal receipt를 재사용해 ack하며 두 번째 통지를 만들지 않는다. 다른 namespace/owner로 옮기지 않는다. 이 제한은 자동 우선순위 변경을 재시도 권한으로 쓰지 않기 위한 것으로, 통지 후 원문 재시도 정책은 기존 custody 계약을 따른다. 부재 proof가 유효한 정상 Legacy/Missing 경로는 자동 시작한다.

wake transport는 at-least-once이며 물리 wake 0/1/2회를 구분한다. 논리 grant/create/prompt의 중복 0은 기존 drive 직렬화(`input_owner/drive.rs:88`), front grant/claim과 reservation/CAS를 유지해 보장한다. wake ack가 유실됐다고 입력을 다시 보내지 않는다. outbox ack는 기존 start의 결과/확정된 retained·terminal 상태와 연결하며 claim만 보고 완료라고 쓰지 않는다.

### v3 codec·boot·source fence 전수 연결

V2 struct의 unknown 필드 무시를 이용해 v2에 journal 필드만 덧붙이지 않는다. A 기록이 생기는 commit에서 **version 3**으로 승격하고, 기존 v1/v2는 읽어 보존한다. A가 없는 파일을 일괄 다시 쓰지 않는다. v3는 알려진 typed schema만 허용하고 unknown version/record는 fail-closed다. 기존 raw-byte CAS·rename·directory fsync·candidate recovery(`queued_head/v2/commit.rs:17`)를 재사용하며 새 저장소를 만들지 않는다.

새 codec은 v2/v3를 공통 Owned read view로 제공한다. 아래 reader 전부가 이 view를 쓰거나, 의도적으로 legacy-only 오류를 반환해야 한다. 역사적 `v2_channels`/`v2_held_sources` 이름은 호환 wrapper로 남겨도 실제 의미는 owned v2/v3이고, 이름 때문에 version 3을 누락하면 실패다.

| reader/경계 (src/services/ 아래) | v3 계약 | 모델/검증 대응 |
|---|---|---|
| `turn_orchestrator/queued_head/compat.rs:38` inspect, `:17` holds_input | v2/v3 typed Owned view. queued/attempted/owed 외 **미완료 intent·receipt·outbox debt만 있는 파일도 true** | AHoldsInput/ABoot, NoOutboxDiscovery RED |
| `turn_orchestrator/pending_queue_persistence/queued_recovery.rs:87,90` v2_channels | outbox-only 포함 boot discovery. 빈 items라고 파일을 idle 목록으로 빼지 않음 | ABoot |
| 같은 파일 `:104,112,187` held sources·catalog | v3 typed decode. queued/attempted/owed ids와 delivered terminal ids를 구분하고 catch-up dedup 유지 | ANoSilentLoss, source-fence 회귀 |
| `discord/runtime_bootstrap/queued_recovery.rs:50` | 발견한 모든 owned debt를 owner_recover로 채택·재관찰·wake | ABoot/ARecover/AOutbox |
| `discord/runtime_bootstrap.rs:39` | active reconcile 뒤의 owner wake도 v3 채널 포함. 기존 wrapper가 공통 discovery를 부르므로 별도 알고리즘 없음 | AWake |
| `discord/queue_io/input_owner/recovery.rs:16,39` | adopt/tick/notice에 A intent/receipt/outbox 재관찰을 연결. 미정산 상태를 먼저 drop하지 않음 | ARecover/ANotice/AAck |
| `turn_orchestrator/queued_head/v2/sources.rs:13,48` | restore·namespace·original identity 확인. journal/outbox가 참조한 ids도 fenced. debt 정산 전 참조 history prune 금지 | ANoDuplicateEffects |
| `discord/recovery_known_ids.rs:166`, `discord/catch_up/phase1_sweep.rs:132`, `phase2_sweep.rs:134` | v3에서도 owed/delivered projection 유지. **wake debt만으로 delivered id를 다시 owed로 만들지 않음** | source-fence·cursor 시험 |
| `turn_orchestrator/queued_head/v2/origin.rs:127` | 원문이 없고 notice/outbox만 남아도 owed 상태를 발견, 새 admission으로 재발행하지 않음 | outbox-only/NoSilentLoss |
| `turn_orchestrator/queued_head.rs:108` | legacy reservation reader가 v3도 owned로 거절 | legacy reader 음성 대조 |
| `turn_orchestrator/dispatch_reservation.rs:459` | shutdown restart_drain이 v3를 legacy empty/save로 덮지 않음 | v3 boot 보존 시험 |
| `turn_orchestrator/queued_head/snapshot.rs:29`, `pending_queue_persistence/snapshot_io.rs:101` | legacy-only parser 유지: v3는 명시 오류. empty 성공으로 변환 금지 | 원시 bytes 동일 시험 |
| `turn_orchestrator/checked_purge.rs:582` | actor-owned v3는 기존 owner capture 경로. actorless legacy parse는 오류로 보존. 새 purge 권한 없음 | cancel/purge 대조 |
| `turn_orchestrator/queued_head/v2/projection.rs:162,220`, `discord/runtime_bootstrap/o_writer_host.rs:352` | 공통 holds_input은 outbox debt까지 true, ack 뒤 false. boot/host-ready와 순환 hold가 생기지 않는 통합 시험 | outbox-only boot→ack; 실제 boot 시험 필요 |
| `turn_orchestrator/queued_head/compat.rs:49,62,69` | legacy clear/save guard는 non-v1 거절 유지. version3를 downgrade하지 않음 | clear/stop 읽기 실패·취소 회귀 |

`SnapshotV2::holds_input`의 확장은 입력 수가 아니라 **owner가 정산할 debt**를 뜻한다. outbox-only boot 대상은 될 수 있어도 payload 재발행 대상은 아니다. `Owner::fenced`와 catch-up의 owed/delivered 구분은 이 차이를 유지해야 한다. 이 reader 세트는 같은 A-admission 조각에 착지한다.

## 3. 모델 결과와 범위

기존 `HerdrSession.tla`의 같은 변수 공간에 별도 `AHintSpec`을 추가했다. HandoffSpec/B·C cfg와 상속 Pending 모델은 바꾸지 않았다. hosted lookup 값은 **Missing / Legacy / Retired**로 분리했다. A의 before는 r2가 제안한 정리+wake 뒤 현재 admission을 그대로 쓴 반례이며, main에 정리 기능이 이미 있다는 뜻은 아니다.

| cfg | 실제 TLC exit | 규칙 결과 |
|---|---|---|
| HerdrHintALegacyBefore | 13 | AProgress RED: observe→intent→recheck→unlink 양쪽→commit→outbox→wake→admit→wait 영구 stutter |
| HerdrHintAMissingBefore | 0 | 같은 준비에서 Missing은 진행. Legacy 차이를 분리한 대조군 |
| HerdrHintAFixed | 0 | 15 case, 270 distinct states, 6 invariants + AProgress GREEN |
| HerdrHintANoFrontCheck | 12 | ANoStaleGrant RED: proof 뒤 front 변경을 무시한 grant |
| HerdrHintANoOutboxDiscovery | 13 | AProgress RED: outbox-only boot 상태를 발견하지 못함 |

저장소 검사기로 위 5 cfg를 실제 검사했으며 **5 cfg/11 TLC runs**, expected set 일치다. PROPERTY에는 실제 `WF_vars(AHintNext)`가 적용된다. 원자적으로 묶지 않은 단계는 intent/재검증/각 unlink/receipt/outbox/wake/admission/ack이며, partial unlink, unlink 뒤 commit 전 crash, commit 뒤 outbox 전 crash, front·epoch 변경, 정리 후 파일·row 교체, wake 재전달을 별도 행동으로 넣었다. 17개 positive reachability witness를 실제 state dump에서 확인했다.

AProgress는 정상 Missing/Legacy에서는 전달, 부적격·교체/취소 경계에서는 원문 보존+명시 통지, 역사만 남은 outbox에서는 ack로 A control이 끝남을 검사한다. 무한한 저장 장애·연속 epoch 변경·무한 신규 front 선점이 없고 controller/notice가 결국 실행된다는 fairness 가정이 있다. 단일 원문, 양 경로, crash·교체·부분 실패 각 1회, wake 재전달 1회를 bound로 삼는다.

모델의 `ACreate/APrompt`는 성공한 기존 downstream의 **효과 수 추상화**다. 실제 Pending/Startup/Bound, create reply 유실, 소스·파일 내용의 실제 보존, 두 프로세스 flock 구현, OS unlink/fsync 원자성은 구현 시험과 기존 Pending/Herdr 모델에 맡긴다. 물리 path set은 모델의 단일 공유 fence로 추상화했으므로 key 충돌/alias 구현이 GREEN으로 증명된 것은 아니다. 기존 HerdrSession의 네 historical violation은 보존하며, B·C의 r2 GREEN을 A의 새 증거로 쓰지 않는다.

## 4. A 시험 계획·착지 조각·크기

| 시험/뮤턴트 | 검증할 실제 효과 |
|---|---|
| Missing vs Found Legacy PG 회귀 | synthetic kept 원문만 사용. 같은 codex_tui hint·tmux Missing·registry 없음에서 각각 cleanup→wake→start→Startup/Bound→hold 해제, grant/create/prompt=1·skip=0. 일반 사람 메시지 성공으로 대체하지 않음 |
| P1 RED→GREEN | 기존 reader 그대로면 Legacy만 두 번째 drive에서 Wait::Event, 새 좁은 proof로 실제 시작. Legacy proof 소비 제거 뮤턴트 RED |
| 다른 row/owner/nonce | 행이 Pending/Bound/Retired로 변경, owner/alias/token 불일치, raw NULL 변경, registry 삽입 시 새 grant/삭제 0. 기존 Known Retired nonce 조건 회귀 유지 |
| 부분 unlink | 두 경로 중 첫 삭제 뒤 두 번째 I/O 실패. intent만 유지, 입력 0. 재관찰 뒤 같은 경로만 마무리; 교체 파일/foreign runtime는 바이트 보존·명시 통지 |
| crash 각 간격 | intent 전후, canonical/legacy unlink 각각 후, receipt 전후, outbox 전후, wake 후 ack 전 restart. stale memory 없이 원문 복구·같은 event 재전달·효과 1 |
| source fence | phase1/phase2 catch-up에서 v3 owed는 owed, delivered는 delivered. outbox-only가 delivered 원문을 새 queue로 복제하지 않음. Source/O cursor/spawn/owner bytes 비교 |
| outbox-only boot | items/head/retained가 비고 outbox debt만 있는 파일. catalog→v2_channels→owner_recover→complete_active_reconcile wake→ack. holds_input true→false, host-ready와 순환 Wait 0. discovery/holds_input 제거 뮤턴트 RED |
| front/epoch·wake 경합 | 삭제/commit/admission 각 경계에서 prefer_person, cancel/stop/clear, owner epoch 변경과 wake/ack 재배송. 잘못된 front grant 0, 원문/notice debt 보존, generic start_rejected로 보내지 않음. front 비교 제거 뮤턴트 RED |
| 물리 fence 두 controller | 별도 프로세스·다른 token/논리 alias가 같은 legacy entry를 공유. per-entry lock 경합, 다른 canonical 경로에서도 동일 legacy lock. 락 순서 반전·서로 다른 parent·부모/leaf 교체·ambiguous mapping 시 변경 0. writer/cleanup/Source 재진입 교착 0 |
| publisher 전수 | direct·watcher·wrapper·fresh/retired marker cleanup·under-source clear/reset를 각각 삭제 사이에 경합. fence 제거/owner recheck 제거 뮤턴트 RED. process-registry가 나타나면 부재 proof 거절 |
| codec·구형 바이너리 | v1/v2 정상 파일 동일 의미, v3 unknown field/version·read/sync 오류 보존. legacy parser/clear/save/drain이 v3를 empty나v2로 덮지 않음. terminal/source identity pin 확인 |

이 시험은 계획이며 Rust 시험을 실행했다고 보고하지 않는다. 구현 레그에서는 throwaway PG·기본 병렬·CI 필터 선택 수=통과 수/skip 0, 변경 파일 clippy 경고 증가 0, source-scan failed 0, static prepush와 required test id 등록을 확인한다.

### 파일별 prod 순증 배분

상수는 `/home/adk/skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py:15`의 **FILES=30, NET=1500**이다. PLAN 정지선은 +1200이다. A 전체는 **32 distinct prod 파일/+1100**으로 추정한다. 파일 상한 때문에 단일 리뷰 조각으로 내지 않는다. A 내부를 **A-fence(12파일/+275)**와 **A-admission(21파일/+825)** 두 리뷰 조각으로 나눈다. 기능 PR/착지 묶음 구성은 코디네이터가 정하며, 조각마다 별도 PR을 열자는 제안은 아니다. `herdr_launch.rs` 한 파일이 양쪽에 연결되므로 distinct는 32다. 둘 다 +1200 전이며, B·C 구현을 포함하지 않는다. 시험·생성 목록은 prod 수치와 별도로 보고한다.

A-fence는 기존 publisher/cleanup이 즉시 쓰는 직렬화 변경만 착지한다. 사용처 없는 새 cleanup 기능이나 v3 journal을 먼저 넣지 않는다. A-admission은 A-fence 위에서 codec/모든 reader/cleanup/projection/wake를 함께 활성화한다. reader만 덜 착지하거나 새 A 호출자를 분리해 미검증 상태로 활성화하지 않는다. 예산은 예상 prod code 순증이며 구현 diff가 +1200 또는 파일 상한에 접근하면 새 범위 합의 전에 멈춘다.

| 조각 | 파일 | +순증 | 책임 |
|---|---|---:|---|
| A-fence | `src/services/tmux_common/runtime_hint_fence.rs` | 174 | 물리 entry별 flock·경로 해석·under-authority token |
| A-fence | `src/services/tmux_common.rs` | 15 | fence 모듈 연결·normal cleanup의 fence→Source 순서 |
| A-fence | `src/services/tmux_common/session_markers.rs` | 20 | normal writer와 fence 보유 writer 분리 |
| A-fence | `src/services/codex/tui_session_launch/first_input.rs` | 8 | owner/runtime 게시 전체의 outer fence |
| A-fence | `src/services/discord/watchers/codex_tui_restore.rs` | 10 | outer fence와 under-authority writer |
| A-fence | `src/services/codex.rs` | 6 | wrapper 게시의 fence 연결만 |
| A-fence | `src/services/codex/absence_launch.rs` | 6 | under-source cleanup outer fence |
| A-fence | `src/services/tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs` | 6 | explicit clear 기존 정리 경계의 fence |
| A-fence | `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs` | 6 | try fence→try Source 순서와 Retry |
| A-fence | `src/services/discord/commands/control/managed_reset.rs` | 6 | 기존 under-source end의 fence token 전달 |
| A-fence | `src/services/herdr_launch.rs` | 10 | Pending CAS부터 host/spawn 게시까지 fence 연결 |
| A-fence | `src/services/herdr_launch/runtime_marker.rs` | 8 | 기존 nonce 검사 유지·같은 fence 사용 |
| A-admission | `src/services/codex/legacy_hint.rs` | 184 | 부재 witness 수집·정리·partial unlink 재관찰 |
| A-admission | `src/services/discord/router/message_handler/provider_dispatch/owner_input/legacy_hint.rs` | 149 | pre-admission·receipt 검증·wake/거절 연결 |
| A-admission | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs` | 8 | 작은 prelude와 typed guard 전달만 |
| A-admission | `src/services/discord/turn_presence/activity.rs` | 3 | 읽기 전용 Reading 접근 연결 |
| A-admission | `src/services/discord/turn_presence/activity/unbound/live.rs` | 2 | 새 child 연결만 |
| A-admission | `src/services/discord/turn_presence/activity/unbound/live/codex_marker_absence.rs` | 60 | same-row Legacy 전용 읽기 전용 admission projection |
| A-admission | `src/db/dispatched_sessions/hosted_execution.rs` | 40 | 물리 대상에 대응하는 owner 후보의 읽기 전용 유일성 조회 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2_types.rs` | 45 | v3 CleanupIntent/CleanReceipt/WakeOutbox typed 필드 |
| A-admission | `src/services/turn_orchestrator/queued_head/compat.rs` | 40 | v2/v3 Owned read view·holds_input·fail-closed codec |
| A-admission | `src/services/turn_orchestrator/queued_head.rs` | 4 | legacy meta guard가 v3도 owned로 거절 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2.rs` | 10 | child 연결·v2 유지/v3 승격 초기화 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/sources.rs` | 15 | restore·namespace·source fence |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/legacy_hint.rs` | 174 | intent/receipt/outbox/ack CAS·정산 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/commit.rs` | 8 | codec version 검증·기존 fsync/recover 유지 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/origin.rs` | 8 | outbox-only origin debt 조회 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs` | 8 | 미정산 receipt 참조의 prune 방지 연결 |
| A-admission | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs` | 35 | v3 boot discovery·owed/delivered catch-up fence |
| A-admission | `src/services/turn_orchestrator/dispatch_reservation.rs` | 4 | shutdown legacy drain의 v3 보호 |
| A-admission | `src/services/discord/queue_io/input_owner/recovery.rs` | 15 | boot에서 intent/receipt/outbox를 재관찰·ack |
| A-admission | `src/services/codex/herdr_turn.rs` | 8 | A start의 fence guard를 fresh launch로 전달만 |
| A-admission | `src/services/herdr_launch.rs` | 5 | A guard 보유 중 Pending/host/spawn 게시 후 해제 |

`codex.rs`(3020), `provider_dispatch/owner_input.rs`(1586), `activity/unbound/live.rs`(1000줄 초과), `tmux_common.rs` 등 giant에는 guard 전달/child 연결만 둔다. DB helper는 현재 838줄인 hosted execution의 기존 읽기 책임 안이다. algorithm·OS fence·durable 전이는 새 작은 child가 소유한다. wrapper/legacy의 기존 정리·kill 조건을 확장하지 않는다.

## 5. 진입점 전수표

다음 핵심 표에 기계 수집 appendices를 합친 범위다. 좌표는 코드 정본 SHA 기준 `src/services/` 상대 경로다. `git grep`의 raw 결과를 Rust production token으로 걸러 test-only/comment를 구분했다. parser의 uncertain test boundary 2건(`rehydration.rs:385`, `session_backend.rs:86`)은 공개하고 해당 생산 호출 좌표를 유지했다. 표의 비 Codex reader는 기존 소비·삭제 권한 불변을 확인하기 위해 포함했다.

| 진입점 | A 계약 | 모델 대응 |
|---|---|---|
| `codex/herdr_turn.rs:200,223` fresh | A guard 전달·새 Pending/host stamp까지 fence, 기존 CAS 유지 | AGrant/ACreate |
| 같은 파일 `:240,255,261` Pending 재사용 | 새 cleanup/새 create 없음. P의 등록부 복구·기존 Startup 검증 유지 | 기존 PendingRegistration 모델, A 범위 밖 |
| `herdr_launch.rs:456,472,482,492` | fresh Pending CAS·registry·host/spawn 게시/기존 marker cleanup과 같은 fence | ACreate; downstream 별도 |
| `codex/tui_session_launch/first_input.rs:196,220,221` | owner/runtime publisher 전체 outer fence; Herdr-configured 기존 거절 유지 | AFenceContender/ARecheck |
| `discord/watchers/codex_tui_restore.rs:253,317` | fence→Source→verified registration·under-authority hint writer. Source 재진입 금지 | AFenceContender/ARecheck |
| `codex.rs:1425,1520,1546,1588` | wrapper 기존 Herdr 거절 유지. 삭제/owner 게시/생성 경계 fence. A는 LegacyTmuxWrapper hint를 정리하지 않음 | fence; live takeover 범위 밖 |
| `codex/absence_launch.rs:77,149,283,293` | 기존 거절·Source 검증 유지, under-source cleanup outer fence | ARecheck; 기존 caller 권한 불변 |
| `tmux_common/session_markers.rs:125`, `tmux_common.rs:1183,1196` | 정상 writer/cleanup과 under-authority variant가 같은 물리 fence 참여. A는 broad cleanup 호출 금지 | AUnlinkCanonical/AUnlinkAlias의 구현 전제 |
| `tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:206,220` | 기존 explicit_clear Source closure 앞 fence | AChange(epoch), 권한 불변 |
| `discord/commands/control/clear_teardown/execution_cleanup.rs:283,298`, `managed_reset.rs:78,117` | fence→try Source, 기존 end/kill 판단 유지. 경합 Retry | AChange(epoch), 권한 불변 |
| `herdr_launch/runtime_marker.rs:14,26,68,91` | 기존 Fresh/Retired host·nonce·Missing 관문 유지, outer fence | A와 별도 nonce 엄격 경로 |
| `session_backend.rs:368,383,507` | process registry present는 정리 불가. 기존 process launch/termination 권한을 A에서 호출하지 않음 | AObserve의 probe/owner evidence; 실제 insert 경합 시험 |
| `provider_dispatch/owner_input.rs:699,704,852,859,875` | A prelude, 나머지 rest/target/claim 관문 유지. global Unknown→Idle 변경 없음 | AAdmission/AGrant |
| `turn_presence/activity/unbound.rs:199,319`, `live.rs:1084` | 기존 일반 판정 유지, 새 start 전용 projection은 fresh Reading에만 적용 | lookup 분리·AAdmission |
| `input_owner/drive.rs:88`, `queued_head/v2.rs:398,411` | 중복 wake는 직렬 drive/front claim에 합류 | ARedeliver/ANoDuplicateEffects |
| `input_owner/ingress.rs:445,458`, `owner_input.rs:250` | 새 원문 Accepted wake·prefer_person 보존. A receipt는 다른 front 권한이 아님 | AChange(front) |
| boot discovery·source fence·outbox-only reader | 위 v3 reader 표 전체. token namespace·owed/delivered 보존 | ABoot/ARecover/AAck |

### runtime / 실행 등록 생산 심볼 전체

아래 모든 행의 `src/services/` prefix는 생략했다. 행마다 실제 line 목록을 표시하며, 기존 read-only reader에는 A 삭제 기능을 추가하지 않는다. 물리 fence를 거치는 publisher·cleanup은 위 핵심 표에서 따로 분류했다. ProcessBackend/다른 provider reader는 A 대상 배제 증거 또는 기존 독립 경로이며 H 종료 기능으로 연결하지 않는다.

| 파일:line | 함수/소비 분류 | 모델 관계 |
|---|---|---|
| `claude.rs:51,2616,2890,2895` | (module) / execute_streaming_local_tmux / send_followup_to_process | AFenceContender; publisher fence |
| `claude/host_gate.rs:177` | tmux_wrapper_poll_probe | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `claude/process_session_launch.rs:65,74,175` | execute_streaming_local_process | AObserve registry guard; 기존 process 효과 별도 |
| `claude/tui_session_launch.rs:37` | prepare_and_create_claude_tui_session | AFenceContender; publisher fence |
| `codex.rs:68,69,95,647,1546,1588,1612` | (module) / codex_tui_session_turn_lock / execute_streaming_local_tmux / prepare_codex_tui_hook_overrides | AFenceContender; publisher fence |
| `codex/absence_launch.rs:149` | execute_observed | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `codex/herdr_turn.rs:40,223,261` | (module) / fresh_launch / pending_launch | ACreate / 기존 Pending 별도 |
| `codex/herdr_turn/first_input.rs:145` | write | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `codex/process_session_launch.rs:81,88,137,146,170` | execute_streaming_local_process_codex | AObserve registry guard; 기존 process 효과 별도 |
| `codex/tui_session_launch/first_input.rs:51,79,122,221` | create / publish_codex_tui_launch_script / spawn_owned | AFenceContender; publisher fence |
| `codex_tui/execution_end/confirm.rs:28` | observe | AObserve registry guard; 기존 process 효과 별도 |
| `codex_tui/warm_followup.rs:505,1220,1252` | verified_warm / verified_warm_inner / warm_followup_on | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/admin_host_guard.rs:381` | row_is_legacy_tmux | AObserve registry guard; 기존 process 효과 별도 |
| `discord/commands/control/clear_teardown/clear_end_record.rs:135` | vacant | AObserve registry guard; 기존 process 효과 별도 |
| `discord/commands/control/clear_teardown/execution_cleanup.rs:161,296,363` | inflight_ready / normal_end / run_with | AObserve registry guard; 기존 process 효과 별도 |
| `discord/commands/control/managed_reset.rs:92,93` | end_process | AObserve registry guard; 기존 process 효과 별도 |
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
| `discord/turn_bridge/tmux_runtime.rs:118` | tmux_ready_for_input_without_tui_pane | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_bridge/tmux_runtime/claude_stop_delivery.rs:378` | interrupt_observed_claude_session | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_bridge/tmux_runtime/native_stop.rs:39` | protected | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_finalizer/watcher_backstop.rs:99` | watcher_backstop_turn_is_terminal | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_presence/activity/unbound.rs:291,292,293,504` | host_scope_result / raw_evidence | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/turn_presence/activity/unbound/live.rs:32,62` | local_trace | AObserve registry guard; 기존 process 효과 별도 |
| `discord/watchers/codex_tui_restore.rs:317` | commit_live_direct_resume_fallback | AFenceContender; publisher fence |
| `discord/watchers/lifecycle/claude_restore.rs:83` | claude_tui_restore_fallback_modified_since | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `discord/zombie_foreground_release.rs:202` | tui_structurally_idle | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `herdr_launch/runtime_marker.rs:34,78` | clear_owned / runtime_paths | AObserve registry guard; 기존 process 효과 별도 |
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
| `tmux_common.rs:5,6,1045,1213` | (module) / cleanup_session_temp_files_under_source_authority / tmux_exact_target | AFenceContender; publisher fence |
| `tmux_common/session_markers.rs:125,129,134,137` | resolve_tmux_runtime_kind_marker / write_tmux_runtime_kind_marker | AFenceContender; publisher fence |
| `tui_o/cutover/channel_gate.rs:372` | session_kind | AObserve/ARecheck evidence; 기존 reader 권한 유지 |
| `tui_prompt_dedupe/runtime_binding.rs:927` | clear_tmux_runtime_binding | AChange/source evidence; 기존 binding cleanup 별도 |
| `tui_prompt_dedupe/runtime_binding/codex_policy/absent_open.rs:93,99` | runtime_markers_match / target_absent | AObserve registry guard; 기존 process 효과 별도 |
| `tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:119` | dead | AObserve registry guard; 기존 process 효과 별도 |

### 추가 cleanup / queue reader 생산 심볼 전체

| 파일:line | 함수 | 모델 관계 |
|---|---|---|
| `claude.rs:2594` | execute_streaming_local_tmux | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `claude/tui_session_launch.rs:34` | prepare_and_create_claude_tui_session | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `codex.rs:1520` | execute_streaming_local_tmux | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `codex/absence_launch.rs:293` | execute_observed | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/commands/control/clear_teardown/execution_cleanup.rs:298` | normal_end | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/commands/control/managed_reset.rs:117,119` | end_process | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/recovery_known_ids.rs:166` | merge_v2_held_sources | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/router/message_handler/provider_isolation.rs:547` | reconcile_managed_tmux_runtime_kind_for_config | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `discord/runtime_bootstrap.rs:39` | complete_active_reconcile | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/runtime_bootstrap/o_writer_host.rs:352` | legacy_busy | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/runtime_bootstrap/queued_recovery.rs:50` | restore_queued_and_inflight_work | AHoldsInput/ABoot/source fence; legacy-only parse는 명시 오류 보존 |
| `discord/tmux_reaper.rs:346,689` | clean_orphan_sessions / reap_fresh_routine_orphan | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `qwen/session_lifecycle.rs:111` | execute_streaming_local_tmux | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `tmux_common.rs:1183,1185,1193,1196` | cleanup_cleared_session_temp_files / cleanup_session_temp_files / cleanup_session_temp_files_under_source_authority | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
| `tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:220` | commit | AUnlink 구현의 동일 fence 전제; 기존 삭제 조건 불변 |
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

전수 raw 검색은 runtime 심볼·Codex create/bind, runtime-kind literal 경로, broad cleanup, queue codec/discovery/holds_input/source fence를 따로 보존한다. literal 검색에는 운영 스크립트의 새 writer가 없고, e2e fixture의 파일 쓰기는 시험 경로다. `policy_close`의 remint fence는 이름이 같은 별도 개념이며 물리 lock의 대체물이 아니다. `SnapshotRead` 변경 후 컴파일이 잡는 exhaustive match와 source-scan으로 reader 누락을 다시 확인해야 한다.

## 6. 위험별로 운영 세션을 잘못 끝내지 않는 방법

| 위험 | 관문·남은 범위 |
|---|---|
| live/unknown/dead-pane 종료 | A는 물리 키 전송·kill 포트를 보유하지 않는다. Missing+registry 없음 이외 hint 변경 0 |
| 다른 token/channel/논리 alias | owner mapping 유일성 + 물리 entry lock set. 후보 불명은 거절 |
| nonce 없음 vs nonce 소실 | 처음부터 없는 Legacy만 별도 증거로 허용. 관찰 후 소실/변경은 거절 |
| Legacy 행이 남아 반복 Wait | global activity 변경 없이 same-row projection을 admission에서 소비. LegacyBefore liveness RED/Fixed GREEN |
| stale proof·front·epoch 변경 | 삭제 전·재관찰·grant 각각 비교. 바뀌면 원문 보존+미전송 통지, 다른 front grant 0 |
| unlink 후 crash / wake ack 유실 | intent 선행 commit, 양 경로 재관찰, receipt/outbox debt boot discovery, 안정 event id+front claim |
| source/cursor/owner 증거 삭제 | A는 확인된 codex_tui runtime hint만 unlink. broad cleanup을 A에서 재사용하지 않음 |
| lock을 문서로만 선언 | A-fence부터 모든 실제 mutation caller를 사용처로 연결하고 두 프로세스 alias 경쟁 시험 전에는 A-admission 활성화 금지 |
| 기존 clear/reset kill 권한 증가 | 기존 file:line 조건을 유지하고 outer fence/Retry 연결만 변경. 새 end 판정·kill 호출 없음 |
| 잘못된 0/PASS | 15 case/270 상태/17 reachability witness와 Legacy/NoFront/NoBoot 실제 RED. Rust/운영 acceptance는 미실행으로 명시 |

## 증거 위치

외부 설계 정본 사본: `/home/adk/adk/lane-runs/i8057/design-h-3.md`.
이 leg 원시 로그: `/home/adk/adk/lane-runs/i8057/logs/h3/`.
TLC focused checker: `tlc-focused.log`; 실제 exit/state dump: `raw-final/`; positive witness: `model-evidence-final.json`.
production inventory: `entrypoints.json`(122 refs), `extra-entrypoints.json`(65 refs), 각 raw grep와 parser diagnostics.
이 187개는 서로 겹치는 reference 집합이므로 distinct entrypoint 수로 합산하지 않는다.
설계/모델만 커밋하며 cargo·PG·clippy·prod static prepush는 실행하지 않는다. 최종 보고서에 실제 커밋 SHA·read-back·의존 cfg checker와 크기 출력을 남긴다.
