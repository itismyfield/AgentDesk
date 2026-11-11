# Codex Herdr marker-only A 설계 r5

상태: 구현 전 PLAN. **운영 코드 변경 0**. 코드 정본은 `289f41b1d0f48fab0dc426e652308eced4326fce`, 모델/문서 입력은 `858150d65d7f79b37f35f92184aecb8eb595d569`다. 두 SHA 사이 `src/` 차이는 없다. 이 문서는 r4의 A 전체(fence+admission)를 대체한다. 19:2x 범위 변경에 따라 A-fence의 Pending CAS 대기도 다룬다. Legacy admission·물리 entry lock set·physical→Source 순서·try-lock Retry는 유지하고, DB 대기와 guard 수명을 유한하게 만든다. B(HandoffWaiting·넘김 journal)와 C(종료 포트)는 보류하며, #8077 착지 뒤 공유 계약으로 다시 설계한다. 이 A에는 `/exit`, 화면 판독, pane close, kill, provider 종료 판정 기능이 없다. 아래 CleanupIntent는 파일 정리의 복구 기록이며 predecessor 종료 attempt가 아니다.

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

`start_waits`의 inflight/rest 관문(`owner_input.rs:699`), 대상 pane 선택(`:710`), host/config/root/Source 검증을 모두 남긴다. 기존 activity가 Idle이거나, 새 child가 fresh 내부 snapshot의 **typed `LegacyMarkerOnly` 결과**를 내고 새 proof가 그 **유일한 Legacy 행 차이만** 해소할 때 통과한다. 공개 `Observed.reason`은 `session_identity_unresolved`다(`unbound/live.rs:1048`). `hosted_active_or_unknown`은 그 내부 hold leaf이며 공개 reason 문자열과 비교하지 않는다. `Reading`의 private unbound stamp/snapshot(`activity.rs:175`)를 볼 수 있는 새 live child가 내부 verdict를 sealed enum으로 변환한다. diagnostic용 `NoticeObservation.hold_leaf` 문자열만으로 permit을 만들지도 않는다. 다른 busy/Unknown 원인은 우회하지 않는다. 삭제나 DB mutation은 이 read-only projection에 넣지 않는다.

admission은 물리 fence를 다시 취득하고 fresh proof를 재검증한 뒤 동일 front를 grant/claim한다(`queued_head/v2.rs:398,411`). 대기열로 넘기는 typed 내부 start context는 **물리 guard가 아닌** 관찰 stamp·절대 deadline·한 번 소비할 권리다. 실제 executor가 실행된 뒤 fence를 취득하고 fresh 재검증→Pending CAS·host/spawn 게시 후 해제한다. Source Mutex는 await를 가로질러 보유하지 않는다. 일반 프로세스/다른 owner·다른 nonce의 입력 권리를 이 permit으로 만들지 않는다. 기존 `herdr_launch.rs:425`의 Legacy/Retired 분기, `hosted_execution.rs:411`의 CAS, Startup/nonce/rollout/owner 입력 검증은 유지한다. 서버/socket create의 불명확한 결과와 prompt 재전송은 기존 Pending/owned-input 계약에 맡긴다.


### A guard의 운반 경로와 history pin

실제 운반은 `owner_input::start_front`의 A controller → `OwnerTurn`(`owner_input.rs:472`) → `StreamingTurn`(`provider_dispatch.rs:52`) → **CodexHerdrTurn 생성(`provider_dispatch.rs:613`)** → `CodexHerdrTurn`(`codex/herdr_turn.rs:65`) → 실행 중인 fresh launch다. 물리 File guard를 `spawn_blocking` 대기열이나 clone된 객체에 보관하지 않는다. 내부 `Arc<Mutex<Option<PreparedAStamp>>>` carrier는 same-row 관찰·owner/front/epoch·controller 생존 token·절대 deadline만 담으며 한번 take한다. stamp는 실행 권한 자체가 아니다.

실제 blocking driver가 시작되면 먼저 deadline/cancel/생존 token을 검사하고, physical try-lock을 취득해 새 DB/file/Source 증거를 확인한다. 만료된 queued worker는 fence 취득·CAS·파일 게시 0으로 거절한다. timeout은 `spawn_blocking`의 JoinHandle 바깥이 아니라 **guard를 소유한 프레임 안의 SQLx future**에 적용한다. `Handle::block_on(async { timeout_at(...).await })`로 동작 중인 runtime의 timer를 사용하며 query를 별도 spawn하지 않는다. JoinHandle drop/abort가 blocking worker를 멈춘다고 가정하지 않는다.

같은 실행 프레임의 clear_fresh/rollback 파일 검사는 동일 guard의 under-fence API를 쓴다. guard를 쥔 채 새 blocking task가 시작되기를 기다리지 않는다. Source Mutex는 try variant로 취득하고 바쁘면 guard를 놓고 Retry한다. DB await 동안 Source Mutex는 없다. 일반 입력은 carrier=None이고, A carrier가 있으나 이미 소비/만료됐으면 새 guard 탐색 fallback을 하지 않는다. 일반 생성자 `headless_turn.rs:1068`·`intake_turn.rs:2179`의 None 연결, owner_input.rs:439/1025와 provider_dispatch.rs의 Clone/`..turn` 경계도 배분에 포함한다. 토큰/monotonic deadline은 요청 JSON이나 v3 파일에 직렬화하지 않는다.

Claude도 같은 물리 marker writer다. direct `claude/tui_session_launch.rs:34` cleanup 앞에서 outer fence를 취득해 owner/runtime→prepared host/nonce 게시→create/respawn(`:67`)까지 유지한다. wrapper `claude.rs:2594` cleanup 앞에서 취득해 marker→create(`:2677`)→spawn stamp(`:2707`)까지 운반하고 output tail 전에 놓는다. 중앙 writer 각각을 잠그는 것으로 이 전체 구간을 대체하지 않는다. 실패 복구도 같은 guard의 under-fence API를 사용하며, 기존 종료/재생성 권한은 바꾸지 않는다. giant에는 이 guard 연결만 둔다.

A debt가 참조한 terminal identity의 **직접 만료 지점**은 `queued_head/v2/retained.rs:78`의 24시간 retain이다. 여기에 `not_expired || A_debt.references(identity)` 조건을 붙인다. 관련 settled bind 만료(`source_admission.rs:618`)와 prune도 같은 참조 판정으로 보호한다. outbox ack/명시 정산까지 pin을 유지하며, terminal history를 재발행할 queued input으로 바꾸지 않는다. `migration::anchor_terminals(:56)`는 기존 unanchored 시각 보정만 유지한다. pin을 TTL 재시작이나 영구 전체 history 보관으로 구현하지 않는다.

## 2. 물리 fence, 삭제, durable 기록과 wake

### 물리 경로의 fence key

owner tuple이나 논리 키를 hash한 lock으로 구현하지 않는다. **각 물리 directory entry마다** key=`(parent st_dev, parent st_ino, basename bytes)`로 정하고, canonical·legacy entry들의 합집합을 정렬·중복 제거해 **모두** 잠근다. 전체 경로 집합 하나를 hash하면 서로 다른 canonical 경로가 같은 legacy 파일을 공유할 때 다른 lock을 얻으므로 금지한다. 같은 물리 entry를 가리키는 두 alias는 반드시 같은 lock이다. 파일 inode는 snapshot 값이며 fence key가 아니다. unlink/recreate 뒤에도 같은 fence여야 한다.

부모는 신뢰 경로를 물리적으로 해석해 directory fd로 고정하고 key를 확인한다. leaf symlink·불명확한 alias·부모 교체는 변경 0으로 거절한다. 각 부모의 동일 basename에 대응하는 고정 sidecar lock을 `openat`/`flock`으로 사용하며 lock file은 unlink하지 않는다. 같은 부모를 다른 경로로 연 controller도 같은 물리 sidecar를 잠근다. 새 루트나 owner별 lock directory는 쓰지 않는다. 서로 다른 UID/비관리 writer·강제로 바꾼 부모 디렉터리는 신뢰 경계 밖이며 감지되면 기능을 켜지 않는다.

순서는 **정렬된 물리 try-locks → deadline으로 제한된 DB fresh read/CAS → process-local SourceAuthority try → 동기 파일 재검증/변경**이다. Source를 잡은 채 flock을 기다리거나 Source를 재진입하지 않는다. normal writer/cleanup은 바깥에서 fence를 취득하고, 이미 source 아래인 watcher/clear/reset은 바깥 호출부에서 fence를 먼저 취득해 under-authority token을 넘긴다. try 경로의 경합은 기존 Retry/보류로 돌려 변경하지 않는다. 기존 read-only Source thunk 전부를 전역 OS lock으로 바꾸지 않는다.

normal/under-fence API 모두 guard의 물리 path set을 확인한다. 이미 fence를 보유한 wrapper의 cleanup, fresh launch의 clear_fresh, 실행 중 취득한 A guard를 보유한 launch는 **같은 guard를 넘겨** 호출하며 flock을 다시 열지 않는다. TLS 재진입 허용이나 자동 key 재탐색으로 해결하지 않는다. Source 아래에서 fence가 필요해진 호출은 바깥으로 올리며, 전체 caller를 못 옮기면 A-admission을 활성화하지 않는다.

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

### P1: DB 대기 후보 비교와 선택

r4는 반환되는 오류를 3회로 제한했지만 **반환되지 않는 future**는 제한하지 않았다. hosted 조회의 `fetch_optional(pool).await`(`src/db/dispatched_sessions/hosted_execution.rs:266`)와 Pending CAS의 `.execute(pool).await`(`:588,603`)가 실제 지점이다. pool acquire_timeout(`src/db/postgres.rs:246`)은 연결 획득 상한일 뿐 질의·응답 대기 상한이 아니다. A-fence도 `herdr_launch.rs:473` CAS부터 `:484,488` host/spawn 게시까지 guard를 쥐므로 같은 문제를 가진다. 맥 report-afence-1.md의 DESIGN_DELTA를 이 r5가 해소한다.

| 후보 | 비용/정합성 | 결정 |
|---|---|---|
| (1) guard 소유 프레임의 DB future에 절대 deadline | 기존 `tokio::time::timeout_at` 패턴 재사용. 관찰과 mutating CAS를 구분하면 물리 fence/Source 순서와 CAS 직렬화 유지 가능 | **선택**, 같은 채널 제어도 유한 deadline/checkpoint로 양보 |
| (2) 새 입력/clear/cancel 즉시 통지만 기다려 중단 | 추가 wake는 현재 `again`만 세움(`drive.rs:88`). 이벤트가 안 오면 여전히 멈추므로 deadline을 대체하지 못함 | 새 signal/timer/스위치 추가 안 함. 기존 cancel/front/admission sequence를 checkpoint에서 소비 |
| (3) DB/CAS를 fence 밖으로 옮김 | row와 파일 관찰 사이 변경·late CAS·취소의 정합성을 새로 증명해야 함. 파일 snapshot만으로 PG row의 최신성을 선언할 수 없음 | 이번 최소 수리로 선택하지 않음 |

**수치 계약(한 A DB 제어 pass):**

| 구간 | 절대 상한 | 만료 처리 |
|---|---:|---|
| A-fence의 한 DB 수집/CAS 구간 | **3초** | 요청 생성·pool acquire·canonical/exact/owner query 체인까지 같은 deadline. query마다 3초를 다시 주지 않음 |
| A-admission의 입력 전 DB 제어 전체 | **9초** | 최초 관찰부터 재검증/새 실행의 pre-create CAS까지 공유하는 절대 deadline. 최대 실패 3회지만 먼저 9초가 오면 종료 |
| guard를 놓은 뒤 독립 outbox 예약 | **1초** | 실패/timeout은 notice debt·storage hold이며 notified 성공으로 보고하지 않음 |
| DB가 계속 pending인 pass의 전체 제어 대기 | **10초(9+1)** | controller 종료·원문/intent 보존. 새 입력/clear/cancel이 와도 이 상한을 늘리지 않음 |

각 DB 구간의 deadline은 `min(now+3s, pass_deadline)`다. 한 구간 안 여러 SELECT를 순서대로 실행해도 같은 deadline을 쓴다. 관찰 timeout은 Missing/Legacy로 번역하지 않고 typed `ReadElapsed`로 세어 3회 오류 예산을 소비한다. 새로운 knob·daemon timer·주기 polling은 없다. 기존 health DB collection의 공유 절대 deadline/NotStarted 패턴(`src/server/routes/health_api/db_collection.rs:30,35,63`)을 서비스의 작은 `herdr_launch/fenced_wait.rs`에서 재사용한다. health route 모듈에 의존하거나 global pool 설정을 바꾸지 않는다.

이 숫자는 **DB pending으로 인한 A guard 점유/제어 대기**의 상한이다. 기존 native Source 대기 60초, AI 응답, OS 자체가 멈춘 동기 syscall 전체의 hard real-time 상한이라는 뜻은 아니다. SQLx future와 Tokio timer가 정상적으로 poll되는 실행 조건에서 적용한다. DB pending 수용시험에서는 파일/host 단계에 진입하지 않으므로 그 precondition을 실제로 확인할 수 있다. 기존 동기 file/tmux 동작은 A-fence의 별도 경합 시험 대상이며, 특히 기존 tmux create의 `Command::output`(`platform/tmux.rs:170`) 완료 상한까지 이 DB 모델이 증명했다고 쓰지 않는다.

### 같은 채널 입력·clear·cancel과 guard 해제

현재 `drive.rs:102`는 Handoff를 await하고, 새 wake는 again만 세운다. r5는 이것이 즉시 interrupt한다고 주장하지 않는다. 대신 **무응답 DB future를 최대 3초에 버리고 현재 guard를 먼저 drop**한다. cancel token이 있으면 기존 `cancel_requested`(`provider.rs:561`)와 controller 생존 여부를 결과 소비 전후에 검사한다. 그 다음 guard 밖에서 current front/epoch·admission sequence를 확인한다. checkpoint도 남은 9초 절대 budget 안이다.

- 새 입력으로 sequence/front가 바뀌면 원문/intent를 지우지 않고 typed Yield. 기존 START_WAITS/prefer_person·again 재구동으로 양보한다. 새 원문을 옛 receipt의 grant 대상으로 쓰지 않는다.
- clear/cancel/owner epoch 변경이면 추가 unlink/CAS/host/spawn/create를 중단하고 custody를 보존한다. 기존 명시 취소/retained 정산을 사용하며 새 kill을 하지 않는다.
- 이벤트를 즉시 관찰할 수 없어도 현재 DB 구간의 fence는 3초, DB-stall pass의 controller는 10초 안에 놓는다. clear/reset은 기존 try-lock Retry를 유지하며 새로운 대기 mutex나 강제 탈취를 만들지 않는다.
- deadline이 끝난 후 새 worker가 실행되거나 늦은 SQL 결과가 돌아와도 만료 token/epoch를 확인해 새 파일 게시나 create로 넘어가지 않는다.

실제 시스템 검증은 가상 Tokio 시간으로 정확히 3/9/10초 경계를 검사하고, foreground real-time PG 시험은 scheduler 오차를 따로 기록한다(acceptance 3초 guard deadline은 4초 이내, 10초 pass deadline은 11초 이내). 이 1초는 숨겨진 재시도 시간이 아니라 실제 호스트 측정 허용차이며 정책 deadline은 3/9/10이다.

### Pending CAS는 관찰 재시도와 다르다

`install_pending_pg`가 timeout/중단되면 **실패했다고 증명된 CAS가 아니다**. 이미 commit됐거나 나중에 commit될 수 있다. 같은 nonce의 `HerdrLaunchOutcome::Indeterminate{execution_nonce,detail}`(`herdr_launch.rs:220`)로 반환하고 주석의 범위를 Pending 설치 결과 불명확까지 명시한다. `HerdrLaunchError::Pending`의 '미commit' 의미(`:241`)로 반환하지 않는다.

한 launch pass에서 이 CAS는 **한 번**만 발행한다. timeout 뒤 새 nonce/CAS 재실행·host/spawn 게시·registry install·provider create 0이다. `retire_unsent`도 timeout을 근거로 호출하지 않는다. DB 서버의 late commit/row가 뒤늦게 나타나는 가능성을 보존하며, 후속 관찰은 canonical row·same nonce와 기존 unplaced/Pending 복구로 합류한다. 늦은 CAS가 clear 이후 나타났다고 무조건 지우거나 옛 host 파일을 복원하지 않는다. row equality/owner CAS가 실제로 stale인지 판정해야 한다.

Written이 확정된 뒤만 같은 guard로 파일을 게시한다. 명시 취소 또는 file publication 오류에서 **확정된 자기 Pending**의 기존 retire 절차가 필요하면 physical guard를 먼저 놓고 남은 budget 안에서만 시도한다. retire 결과가 불명확하면 Pending을 보존한다. SQLx future drop은 서버 rollback 증명이 아니다. pool return cleanup(`db/postgres/pool_return.rs:20`, 자체 5초 budget)을 guard 보유 중 기다리지 않고, 물리 guard가 그 cleanup task로 이동하지 않게 한다. 전역 statement/acquire timeout 설정·baseline은 바꾸지 않는다.

### r4의 continuation과 영구 실패 계약 유지

반환되는 오류의 source는 여전히 **같은 awaited callback future의 다음 반복**이다. 초기 오류+추가 2회, 성공한 중간 step으로 실패 수를 reset하지 않는다. 다만 9초 절대 budget이 먼저 오면 세 번째 시도를 억지로 시작하지 않는다. retry마다 locks를 놓고 same namespace/cleanup-id/front/epoch·row/file snapshot을 재관찰한다. partial unlink는 같은 intent의 같은 entry만 idempotent하게 잇고, commit 결과 불명확은 기존 commit::recover로 확인한다.

pre-grant retry body는 payload/launch를 다시 실행하지 않는다. Ready 이후의 기존 owned-input 처리·prompt 후 ack 복구를 이 모델의 retry와 혼동하지 않는다. writable queue의 최종 실패는 known-unsent retained+notice debt로 정산한다. queue publication이 계속 실패하면 confirmed bytes/candidate와 원문을 그대로 두고 hold_front로 grant를 철회한다. 독립 PG outbox를 1초 안에 예약하지 못하면 notice debt/error이지 notified가 아니다. PG와 queue store가 모두 영구 불능인데 통지를 완료했다고 선언하지 않는다.

boot의 read/adopt/재관찰도 동일 pass에 묶는다. 재시작 때 monotonic deadline을 파일에서 복원하지 않고 새 live pass의 deadline을 만들되 원문/cleanup-id/nonce는 바꾸지 않는다. 이 예산은 한 pass의 상한이며 무한 재시작 전체의 횟수 원장이 아니다.

### catalog 실패를 controller에 넘기는 실제 경계와 notice key

현재 `read_catalog`는 filename에서 channel을 해석한 다음 read/decode 오류를 `quarantined`로 빼고(`queued_recovery.rs:177`), `v2_channels(:88)`는 성공 snapshot만 보므로 `runtime_bootstrap/queued_recovery.rs:50`에 도달하지 않는다. r5는 **catalog 단계부터** 실패를 잃지 않는다.

`v2_channels`의 기존 public gateway/reexport 이름은 유지하되 반환을 구조화한 `OwnedRecoveryDiscovery { ready, failed, namespace_errors }`로 바꾼다. 이름은 historical v2이며 실제 v2/v3 discovery다. `failed`는 **(provider, channel, expected path, namespace, stage, error)**를 보존한다. 새 공개 함수를 별도 reexport하는 대신 기존 gateway 결과를 넓히고, 실제 production 소비자 두 곳을 모두 고친다: `runtime_bootstrap/queued_recovery.rs:50`은 ready와 failed를 처리하고, `runtime_bootstrap.rs:39`는 성공 채널 wake와 namespace 오류 gate를 구분한다. ready 성공 수만으로 recovery 완료를 선언하지 않는다.

`owner_recover`는 provider_dispatch.rs:28과 message_handler.rs:77의 기존 reexport alias다. 새 공개 함수를 추가하지 않고 이 함수의 recovery target이 ready channel 또는 catalog failure metadata를 받게 하며, 일반 ChannelId 입력은 기존 경로로 변환한다. 따라서 별도 message_handler gateway 파일 변경을 배분에서 숨기지 않는다.

filename에서 channel을 확인한 read/UTF-8/JSON decode/unsupported schema 실패도 한번의 bootstrap 안에서 bounded controller로 전달한다. 현재 로컬 Codex Herdr/기존 input-owner adoption 대상인지 검증하고, eligible 대상만 기존 owner 채택·migration에 합류한다. 일시 오류 뒤 v1/v2/v3로 읽히면 해당 기존 legacy/owner adoption 경계를 사용하며 새 원문을 생성하지 않는다. 일반 다른 provider나 소유 불명 대상의 quarantine을 임의로 쓰기 가능한 queue로 바꾸지 않는다.

모든 후보는 path가 expected namespace 안의 numeric channel filename인지 검증한다. 실제 read가 안 된 파일을 v3라고 단정하지 않고, raw bytes·원문 identity·cleanup-id를 꾸며 내거나 지우지 않는다. directory scan 자체가 실패해 channel을 알 수 없으면 namespace error로 남기며 채널을 추측하지 않는다. 그런 namespace를 성공한 reconcile로 표시하거나 입력을 재발행하지 않는다. 이미 read 가능한 다른 채널의 기존 처리와 오류 보존을 분리한다.

notice key는 다음 두 가지다. raw 오류 문자열·mtime/inode·랜덤 retry id를 넣어 재시도마다 알림이 바뀌지 않게 한다.

- cleanup-id를 확인한 경우: `A-io:v1:SHA256(provider, namespace, cleanup_id, stage)`.
- cleanup-id조차 못 읽은 경우: `boot-queue-io:v1:SHA256(provider, namespace, channel_id, normalized_expected_path, recovery_pass_id, stage)`.

`recovery_pass_id`는 **top-level boot recovery 호출에서 한번만** 정하고 모든 catalog 후보·재시도에 같은 값을 운반한다. 원문/turn/실행 nonce가 아니라 notice dedupe용 활성화 id다. 같은 pass의 read→decode 변동이 있더라도 stage를 `catalog_unreadable`로 정규화해 같은 알림 1개로 묶는다. 다음 실제 boot는 별도 복구 실패 알림을 낼 수 있다. callback마다 새 id를 만들지 않는다. channel도 모르는 namespace 오류는 기존 scan-error/health 경계에 남기고 가짜 채널 notice를 만들지 않는다. 기존 non-turn persistent-dedupe PG outbox를 사용하며 운영 코드/서비스에 새로운 저장소를 만들지 않는다.

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

## 3. 모델 결과와 증명 범위

기존 AHintSpec에 **DB가 영원히 pending인 행동**, 초 단위 clock, 3초 slice/9초 pass/1초 notice, 그 도중 새 입력·clear·cancel을 넣었다. read와 CAS를 구분하며, CAS는 timeout 뒤에도 서버 commit이 일어날 수 있다. 같은 attempt의 CAS 재발행·후속 파일/create 효과는 허용하지 않는다. 기존 cfg·expected 행은 바이트 동일하게 두고 새 3행/3cfg만 추가했다.

| 새 cfg | 실제 plain TLC | 저장소 checker 결과 |
|---|---|---|
| HerdrHintADbBefore | safety exit 12, property-only exit 13 | DGuardBound / DWaitProgress RED (3 runs) |
| HerdrHintADbFixed | exit 0 | GREEN (2 runs), 11 case / 1934 distinct / depth 20 |
| HerdrHintADbNoTimeout | safety exit 12, property-only exit 13 | DGuardBound / DWaitProgress RED (3 runs) |

Fixed는 4 invariants(원문 보존·3초 guard·read≤3/CAS≤1·이 pending 영역의 후속 효과 0)와 DWaitProgress를 검사한다. 실제 `WF(AHintNext)`와 clock tick fairness를 적용한다. timer 만료는 다음 tick보다 먼저 처리하는 추상화이며 OS hard real-time 보증이 아니다. 17개 reachability witness로 모든 input/clear/cancel이 **fence를 쥐고 DB가 pending인 중** 발생한 것과, 해제/late CAS/notice 불능의 비성공 정산을 확인했다.

Before의 property-only trace는 clear/cancel이 발생해도 clock=10, fence=true, pendingDb=true에 남는다. Fixed의 read는 timeout 예산을 소비하고, incoming이 있으면 원문을 지우지 않고 Yield한다. CAS는 한 번의 요청 후 Indeterminate이며 늦은 서버 commit을 모델에서 허용한다. 이 영역의 후속 marker/create/prompt는 0이다.

이 모델은 DB 대기 region의 점유/제어 종료를 증명하며, 실제 SQLx drop/RAII, queued worker guard 소유, client/network 상태, 부팅 catalog I/O, real-time jitter, 전체 Startup/Bound·효과 뒤 crash 재전송을 증명하지 않는다. r4에서 축소한 replay 증명 범위도 유지한다. 기존 ANoDuplicateEffects의 단선 카운터를 end-to-end exactly-once 증명으로 쓰지 않는다. 물리 path/Source/filesystem의 실제 구현은 Rust/PG 수용시험이다.

## 4. A 시험 계획·착지 조각·크기

| 시험/뮤턴트 | 검증할 실제 효과 |
|---|---|
| DB pending 중 새 입력·clear·cancel | 오류 즉시 반환 fake가 아닌 pending Future와 실제 throwaway PG lock/미응답 probe를 사용. DB 시작 barrier 뒤 각 사건을 주고, guard 3초 deadline/real 4초 이내, A pass 10초 deadline/real 11초 이내 종료. 원문 보존·clear 재취득 가능·stale 파일/실행 효과 0. timeout wrapper 제거 뮤턴트 RED |
| Pending CAS 미확정·late commit | UPDATE가 server에서 pending/응답 유실인 동안 취소/timeout. CAS 요청 1, 같은 nonce Indeterminate, host/spawn/registry/create 0. server late commit을 풀어도 자동 retire/새 nonce/create 없음. 후속 exact-row/unplaced 관찰 확인 |
| queued worker와 guard 수명 | blocking executor를 대기시킨 상태로 deadline 만료. 물리 guard가 대기열/Arc carrier에 없음, worker 시작 뒤 만료 stamp 거절. JoinHandle timeout 뒤 작업이 계속돼도 늦은 mutation 0. 실제 guard-holder의 SQLx timeout/drop flag 확인 |
| bootstrap catalog 오류 경계 | 실제 bootstrap 1회 호출에서 filename 확인 뒤 read/UTF-8/decode 실패→회복. failed candidate가 controller까지 도달·같은 원문 채택 1. unknown cleanup-id notice key는 pass 동안 일정·retry 알림 1, raw/candidate bytes 보존. namespace scan 실패를 ready 성공으로 세지 않음 |
| 생산 literal·API 전수 | codex.rs 모듈 선언, SnapshotV2 생성 2곳(v2.rs:155, migration.rs:31), v2_channels 실제 소비자 2곳을 컴파일로 확인. v1/v2/v3 및 outbox-only·quarantine 기존 대조군 유지 |
| 일시 partial/commit 실패, 새 사건 없음 | 초기 drive/recover 호출 1회만 하고 새 입력·재시작·별도 wake를 막는다. canonical unlink 뒤 legacy unlink 1~2회 실패 또는 intent/receipt commit Before/AfterRename 실패를 주고 I/O 회복. 같은 callback이 완료하고 Startup/Bound·hold 해제, 원문/키 전송 1. 첫 오류에서 return하는 뮤턴트 RED |
| 영구 실패·boot 실패 | 오류 3회 뒤 controller가 끝남, 다음 자동 시도 0. unlink 영구 실패와 queue publication 영구 실패를 나눠 raw bytes·candidate 보존·PG outbox 통지 1. boot 복구 1회 호출 안에서 transient 회복/소진 확인. PG unavailable을 통지 성공으로 표시하지 않음 |
| 공개 reason 회귀 | 실제 Observed=Unknown/session_identity_unresolved + private hosted_active_or_unknown Legacy로 typed permit 성공. 같은 공개 reason인 다른 private leaf는 거절, general activity는 Unknown 유지. public reason 문자열 비교 뮤턴트 RED |
| guard 운반·history pin | StreamingTurn Clone 후 CodexHerdrTurn 경계에서 guard take는 1회, double take 거절, clear_fresh 중첩 lock 0. 24시간 경과 tick(:78)·expire_binds(:618)에도 A debt 참조 history/ids 보존, ack 뒤 기존 만료 재개 |
| effect 뒤 ack 전 crash·중복 wake | 실제 owner fixture에서 prompt 뒤 ack 전 crash→boot→동일 outbox 재관찰 및 admission 재호출. grant/reserve/native submit 각각 1. 이 시험을 TLC의 단선 효과 카운터로 대체하지 않음 |
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

상수 FILES=30/NET=1500, PLAN 정지선 +1200을 각 리뷰에 적용한다. A-fence→A-admission의 두 리뷰 경계는 유지하되 내용과 수치를 갱신한다.

| 조각 | r4 | r5 | 독립 착지 조건 |
|---|---|---|---|
| A-fence | 14파일/+299 | **15파일/+434** | 기존 publisher가 실제 사용하는 물리 fence + 유한 DB/CAS helper. v3/controller 없이 활성 호출자 존재. guard DB 대기 3초와 CAS Indeterminate 시험이 통과해야 착지 |
| A-admission | 26파일/+998 | **30파일/+1135** | A-fence 위에서 codec·모든 reader·catalog 실패·controller·cleanup·typed admission을 함께 활성화. PLAN 여유 +65, 추가 prod 파일이 생기면 분할/FILES를 먼저 보고 |
| 전체 distinct | 39파일/+1297 | **43파일/+1569** | codex.rs/herdr_launch.rs 2개 공통. 전체를 한 리뷰 조각으로 구현하지 않음 |

A-fence는 r4의 'DB를 기다려도 됨'을 암묵적으로 유지하지 않는다. **r5가 bounded DB await 예외를 명시적으로 허용하며**, 기존 맥 브리프의 무조건 DB/network 대기 금지를 이 계약으로 갱신해야 한다. physical entry set/순서/try Retry·기존 kill/cleanup 수용 조건은 유지한다. pure metadata 관찰 timeout은 일시 실패, CAS timeout은 미확정으로 달리 처리한다.

A-admission 30파일에는 모듈 선언·production literal·두 bootstrap 소비자도 포함했다. 기존 reexport 함수의 structured 반환형을 사용하므로 새 공개 gateway/reexport 파일을 누락시켜 크기를 맞춘 것이 아니다. 실제 production v2_channels 소비자 전수는 두 곳이다. 구조가 달라 새 API/reexport/연결 파일이 필요해 31파일이 되면 구현을 시작/확대하지 않고 추가 분할 계획을 낸다. 시험·공식 생성물은 실제 diff에서 별도 집계한다. 기능 PR/착지 묶음은 코디네이터가 정한다.

A-fence의 helper는 현재 fresh launch가 사용하므로 dormant 코드가 아니다. A-admission은 A-fence가 먼저 착지한 뒤 독립 리뷰/착지할 수 있고, cleanup만 먼저 활성화하거나 v3 reader/boot 오류 처리를 빼서 분할하지 않는다. B·C는 포함하지 않는다.

| 조각 | 파일 | +순증 | 책임 |
|---|---|---:|---|
| A-fence | `src/services/tmux_common/runtime_hint_fence.rs` | 174 | 물리 entry별 flock·경로 해석·under-authority token |
| A-fence | `src/services/tmux_common.rs` | 15 | fence 모듈 연결·normal cleanup의 fence→Source 순서 |
| A-fence | `src/services/tmux_common/session_markers.rs` | 20 | normal writer와 fence 보유 writer 분리 |
| A-fence | `src/services/codex/tui_session_launch/first_input.rs` | 8 | owner/runtime 게시 전체의 outer fence |
| A-fence | `src/services/discord/watchers/codex_tui_restore.rs` | 10 | outer fence와 under-authority writer |
| A-fence | `src/services/codex.rs` | 6 | wrapper 게시의 물리 fence 연결만 |
| A-fence | `src/services/codex/absence_launch.rs` | 6 | under-source cleanup outer fence |
| A-fence | `src/services/tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs` | 6 | explicit clear 기존 정리 경계의 fence |
| A-fence | `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs` | 6 | try fence→try Source 순서와 Retry |
| A-fence | `src/services/discord/commands/control/managed_reset.rs` | 6 | 기존 under-source end의 fence token 전달 |
| A-fence | `src/services/herdr_launch.rs` | 45 | fresh DB 절대 deadline·CAS Indeterminate·실행 프레임 내 fence 취득/해제 |
| A-fence | `src/services/herdr_launch/runtime_marker.rs` | 18 | 동일 guard의 동기 under-fence 검사, guard 보유 spawn 대기 금지 |
| A-admission | `src/services/codex/legacy_hint.rs` | 200 | deadline 내 fresh 관찰·fence 재취득 증거 대조 |
| A-admission | `src/services/discord/router/message_handler/provider_dispatch/owner_input/legacy_hint.rs` | 239 | 3초 slice·9초 절대 budget·3회 실패·중단 checkpoint |
| A-admission | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs` | 13 | controller 호출·held/typed guard 운반 연결만 |
| A-admission | `src/services/discord/turn_presence/activity.rs` | 3 | Reading 기존 접근 사용·sealed typed admission 결과 연결 |
| A-admission | `src/services/discord/turn_presence/activity/unbound/live.rs` | 2 | 새 child 연결만 |
| A-admission | `src/services/discord/turn_presence/activity/unbound/live/codex_marker_absence.rs` | 68 | private snapshot/leaf를 typed LegacyAbsence로 변환·public reason 비교 금지 |
| A-admission | `src/db/dispatched_sessions/hosted_execution.rs` | 40 | 물리 대상에 대응하는 owner 후보의 읽기 전용 유일성 조회 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2_types.rs` | 45 | v3 CleanupIntent/CleanReceipt/WakeOutbox typed 필드 |
| A-admission | `src/services/turn_orchestrator/queued_head/compat.rs` | 45 | v3 unknown schema quarantine, empty 성공으로 변환 금지 |
| A-admission | `src/services/turn_orchestrator/queued_head.rs` | 4 | legacy meta guard가 v3도 owned로 거절 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2.rs` | 18 | A 상태·terminal/blocked memo·child 연결 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/sources.rs` | 15 | restore·namespace·source fence |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/legacy_hint.rs` | 174 | intent/receipt/outbox/ack CAS·정산 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/commit.rs` | 8 | codec version 검증·기존 fsync/recover 유지 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/origin.rs` | 8 | outbox-only origin debt 조회 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs` | 16 | prune_binds/expire_binds의 A debt 참조 pin |
| A-admission | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs` | 55 | 성공과 read/decode 오류를 함께 반환하는 구조화 discovery |
| A-admission | `src/services/turn_orchestrator/dispatch_reservation.rs` | 4 | shutdown legacy drain의 v3 보호 |
| A-admission | `src/services/discord/queue_io/input_owner/recovery.rs` | 28 | catalog 오류 대상도 같은 controller로 재관찰 |
| A-admission | `src/services/codex/herdr_turn.rs` | 14 | guard 대신 once-take stamp를 받고 실제 실행 중 fence 취득 |
| A-admission | `src/services/herdr_launch.rs` | 20 | stamp 만료시각 운반·DB/CAS 분류·late worker 거절 |
| A-fence | `src/services/claude/tui_session_launch.rs` | 12 | cleanup 전부터 prepared/host/nonce 게시와 create/respawn까지 outer guard |
| A-fence | `src/services/claude.rs` | 12 | wrapper cleanup 전부터 create·spawn marker까지 outer guard 운반만 |
| A-admission | `src/services/discord/router/message_handler/provider_dispatch.rs` | 22 | deadline stamp carrier를 실제 CodexHerdrTurn으로 이동 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/retained.rs` | 16 | tick의 terminal history 직접 만료(:78)에 debt pin 적용 |
| A-admission | `src/services/discord/router/message_handler/provider_dispatch/owner_input/storage_failure.rs` | 50 | cleanup-id 불명 boot notice key·1초 통지 예산 |
| A-admission | `src/services/discord/router/message_handler/headless_turn.rs` | 1 | StreamingTurn 일반 생성자에 A carrier None 연결만 |
| A-admission | `src/services/discord/router/message_handler/intake_turn.rs` | 1 | StreamingTurn 일반 생성자에 A carrier None 연결만 |
| A-fence | `src/services/herdr_launch/fenced_wait.rs` | 90 | 공통 유한 DB 대기·절대 deadline·read/CAS 결과 분류 API |
| A-admission | `src/services/codex.rs` | 2 | 새 legacy_hint child 모듈 선언만 |
| A-admission | `src/services/turn_orchestrator/queued_head/v2/migration.rs` | 2 | 생산 SnapshotV2 literal A 필드 기본값 |
| A-admission | `src/services/discord/runtime_bootstrap/queued_recovery.rs` | 18 | catalog 오류도 bounded controller로 넘기는 구조화 후보 loop |
| A-admission | `src/services/discord/runtime_bootstrap.rs` | 4 | reconcile 뒤 구조화 discovery의 ready 후보만 wake |

`codex.rs`(3020), `provider_dispatch/owner_input.rs`(1586), `activity/unbound/live.rs`(1000줄 초과), `tmux_common.rs` 등 giant에는 guard 전달/child 연결만 둔다. DB helper는 현재 838줄인 hosted execution의 기존 읽기 책임 안이다. algorithm·OS fence·durable 전이는 새 작은 child가 소유한다. wrapper/legacy의 기존 정리·kill 조건을 확장하지 않는다.

## 5. 진입점 전수표

다음 핵심 표에 기계 수집 appendices를 합친 범위다. 좌표는 코드 정본 SHA 기준 `src/services/` 상대 경로다. `git grep`의 raw 결과를 Rust production token으로 걸러 test-only/comment를 구분했다. runtime 표의 uncertain test boundary 2건(`rehydration.rs:385`, `session_backend.rs:86`), 추가 표의 2건(`runtime_bootstrap/o_writer_host.rs:37`, `policy_close.rs:106`)을 공개하고 해당 생산 호출 좌표를 유지했다. 총 4건은 cfg(test) 인자/필드/variant 경계로 수동 확인했다. 표의 비 Codex reader는 기존 소비·삭제 권한 불변을 확인하기 위해 포함했다.

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
| `claude/tui_session_launch.rs:34,67`, `claude.rs:2594,2677,2707` | cleanup 앞에서 outer guard, 실제 create/respawn·spawn stamp까지 운반 | AFenceContender의 실제 구현 의무 |
| `provider_dispatch.rs:52,613`, `owner_input.rs:439,1025`, `headless_turn.rs:1068`, `intake_turn.rs:2179`, `codex/herdr_turn.rs:65` | Clone carrier와 한번 take, 실제 생성 경계 누락 없이 전달 | AGrant/ACreate 연결·구현 시험 |
| `queued_head/v2/retained.rs:78`, `source_admission.rs:618` | 직접 terminal/bind TTL에서 A debt pin | 모델 범위 밖 실제 TTL 회귀 |
| `queue_io/input_owner/drive.rs:101,117`, `owner_input.rs:664,681` | 기존 awaited callback/boot future를 놓지 않고 bounded continuation | APartialUnlink/ARetryContinue/ARecover |
| `held_watch.rs:64,88`, `retained.rs:46` | 이번 retry 원천으로 사용하지 않음, 기존 동작 유지 | 모델에 가짜 arm/deadline 없음 |
| `db/dispatched_sessions/hosted_execution.rs:266,411,588,603`, `db/postgres.rs:246` | query 전체 deadline, acquire_timeout에 의존 안 함, CAS 한 번/미확정 | DAcquire/DTimeout/DMayCommit |
| `herdr_launch.rs:473,484,488,492` | CAS부터 게시까지 현재 guard 소유 프레임의 deadline; timeout 시 게시 0 | CAS timing region, 실제 파일은 수용시험 |
| `pending_queue_persistence/queued_recovery.rs:177,187,88`, `runtime_bootstrap/queued_recovery.rs:50`, `runtime_bootstrap.rs:39` | failed tuple을 structured discovery에 보존하고 bounded controller로 인계 | 모델 밖 bootstrap 회귀 |
| `codex.rs:24`, `queued_head/v2.rs:155`, `queued_head/v2/migration.rs:31` | child 선언 및 생산 SnapshotV2 literal 기본값 누락 없음 | 컴파일/source-scan |
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
| 잘못된 0/PASS | DBBefore/NoTimeout의 bound·진행성 실제 RED, Fixed 11 case/1934 상태/17 witness. 실제 SQLx/RAII/boot/Startup 검증은 미실행이며 scope 밖을 GREEN으로 세지 않음 |

## 증거 위치

외부 설계 정본 사본: `/home/adk/adk/lane-runs/i8057/design-h-5.md`.
이 leg 원시 로그: `/home/adk/adk/lane-runs/i8057/logs/h5/`. r3의 생산 전수표 raw는 동일 코드 SHA의 logs/h3에 보존한다.
TLC focused checker: `tlc-focused.log`(3 cfg/8 runs); 실제 exit/state dump와 property-only cfg: `raw/`; positive witness: `model-evidence.json`.
production inventory: `entrypoints.json`(122 refs), `extra-entrypoints.json`(65 refs), 각 raw grep와 parser diagnostics.
이 187개는 서로 겹치는 reference 집합이므로 distinct entrypoint 수로 합산하지 않는다.
설계/모델만 커밋하며 cargo·PG·clippy·prod static prepush는 실행하지 않는다. 최종 보고서에 실제 커밋 SHA·read-back·의존 cfg checker와 크기 출력을 남긴다.
