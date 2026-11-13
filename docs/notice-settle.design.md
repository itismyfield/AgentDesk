# Input notice evidence 정착 설계

기준: `79bebdd4eaa29b289bf88412825baa7d70a6ac77` (2026-11-13 12:00 KST main).
정본 입력: `/home/adk/adk/lane-runs/i8112/issue-and-comments.md` 전체.
상태: 독립 설계 리뷰 CLEAN 후 구현. 리뷰의 네 권고(scope 세대 캐시, retained Discord proof 범위, 메모리 로그, source 경계 fairness)를 구현 계약에 반영했다. 운영 변경·배포는 하지 않는다.

목표는 입력 접수나 실행 권한을 바꾸지 않고, notice 게시 의무가 없다는 증거가 있는
record를 durable 종결하고 나머지 record의 pending 로그를 실행 수명에서 한 번으로 제한하는 것이다. 재시작 뒤 추가 한 번은 허용한다.
세션이 바뀌었다거나 오래되었다는 사실만으로 직접 입력의 게시 의무를 없애지 않는다.

## 관측과 main에 이미 있는 기능

정본 댓글의 배포 SHA는 위 기준과 같다. 10분 로그 6,147줄 중 `remains pending` 2,802줄(46%),
adk-cc pending 356 record 중 `[User: …]` 시작 311 record, 가장 오래된 입력 11-10 09:37이다.
CPU는 top 161.6%, 기동 후 평균 약 179%; sample은 prepare_notices 634, Batch::refresh 354,
relevant 99, sha256 약 300, pane_liveness 314+207, legacy-body-census-writer 730이다.
이는 이슈의 관측값이며 이 레인에서 다시 측정한 값이 아니다. 311건 모두에 정착 증거가 있다는
뜻도 아니다. 운영 raw record·설정·DB는 읽지 않았다.

| 이미 충족된 항목 (모두 기준 main) | 코드 증거 | 이번 조각의 취급 |
|---|---|---|
| native census 캐시 (#8112 A, `4e0db0cccd` 포함) | `src/services/tui_input/human_frame/history/notice_owner/census.rs:45`, `:56`, `:119` | cursor/전체 fingerprint 검증과 128 source/8MiB 한도를 재구현하지 않음 |
| same-source resume의 서로 다른 실행 nonce | `src/services/tui_input/human_frame/history/notice_owner.rs:277`; `src/services/tui_o/writer/actor/notice_resume_tests.rs:98` | immutable resume context로 기존 source를 증명하는 경로 유지 |
| known Discord 입력은 새 notice evidence를 만들지 않음 | `src/services/tui_o/writer/input_notices.rs:129`, `:151`; `src/services/tui_o/writer/actor/notice_owner_tests.rs:615` | 이 빠른 경로 유지 |
| UUID/내용 충돌·원본 변경 거절 | `src/services/tui_o/store/ledger/input_notices.rs:217`, `:234` | terminal 뒤에도 유지 |
| Prepared → 첫 POST poll의 동기 admission, 불확실 POST 복구 | `src/services/tui_o/writer/deliver.rs:614`, `:643`, `:850` | 새 정착 판정이 이 게시 경계를 우회하지 않음 |
| confirmed clear의 기존 제외 정책 | `src/services/tui_o/writer/input_notices/admission.rs:177`, `:207`, `:216` | 넓히지 않음; 일반 incarnation 변경을 Clear로 취급하지 않음 |

## 0단계: 코드 진입점 전수표

아래 좌표는 모두 기준 SHA이다. `writer/actor/notice_owner*`에는
시험만 있으며 production 소유권 구현은 `src/services/tui_input/human_frame/history/notice_owner.rs`이다.
검색 원문은 외부 보고서의 `notice-entrypoints.txt`, `lifecycle-entrypoints.txt`에 있다.

| 역할/진입점 | file:line | 읽거나 만드는 값·범위 / 경계 |
|---|---|---|
| Discord idle/turn 입력 프레임 | `src/services/discord/router/response_format.rs:161`; `src/services/tui_input/human_frame.rs:97`, `:109` | 일반 `[User: name (ID: digits)]`; idle과 plain inject가 같은 가시 bytes 사용 |
| created execution의 첫 prompt 제출 | `src/services/claude_tui/hosting/fresh_lifecycle.rs:175`; `src/services/discord/health/turn_deliver/inject/first_input.rs:35`, `:49`, `:164` | prepare_history → owner permit consume → with_attempt 안에서 physical submit; native JSONL user row는 provider가 기록 |
| owner 등록/재시작 동기화 | `src/services/turn_orchestrator/queued_head/v2/sources.rs:158`, `:214` | active/retained input을 Registry::prepare/project로 연결; restored registration도 같은 native attribution에 도달 |
| busy 주입 등록·영수증 collector | `src/services/claude_tui/busy_inject.rs:596`, `:644`; `src/services/tui_input/human_frame/history/v2.rs:566` | enqueue 영수증과 native user/command UUID 영수증은 구별됨 |
| durable 등록 생성/PreSource 해소/수동 이관 | `src/services/tui_input/human_frame/history/v2.rs:354`, `:395`, `:434` | namespace+전체 key, source target/provider/session/path/dev/ino/offset; live/planned/passive, abandoned 제외 |
| 예약 입력 | `src/services/scheduled_messages/owner_input.rs:196`, `:206`, `:211`, `:297` | owner original과 `origin=scheduled`, source metadata; metadata만으로 native row 소유권을 추정하지 않음 |
| slash/기계 입력 정책 | `src/services/discord/tui_prompt_relay.rs:50`; `src/services/discord/tui_prompt_relay/injected_prompt_policy.rs:89`, `:141` | human-active/local-only 판정; bare slash 문자열과 machine envelope를 구분 |
| capture/replay의 projection 연결 | `src/services/tui_o/writer/rotation/owe.rs:103`, `:116`; `src/services/tui_o/writer/pieces.rs:109`, `:115` | complete CapturedRecord start/end/line → project_record; notice feature는 Claude+transcript turn 채널만 |
| origin 생성 | `src/services/tui_o/writer/rotation/owe.rs:125`, `:145` | source의 최초 정확한 attachment 또는 rotation link; logical_session/binding_seq/nonce. 현재 실행 nonce와 같다는 보장 없음 |
| 새 record 분류·identity seal | `src/services/tui_o/writer/input_notices.rs:61`, `:73`, `:112`, `:145`, `:165` | Discord/비인간은 None; Known은 None, Pending/원본 부족은 Err; UnitKey=(channel,Claude,UUID,InputNotice) |
| evidence 생성/회수 | `src/services/tui_o/writer/input_notices/evidence.rs:8`, `:17`, `:44`, `:57` | project가 Ok(None)이 아니면 raw+선택 prompt/digest/key/payload/origin 보존; origin/key 없는 record도 가능 |
| migration floor | `src/services/tui_o/writer/input_notices/evidence.rs:64` | frozen source lower/시간/기존 binding checkpoint; 새 TTL이 아님 |
| 최초 boot freeze | `src/services/tui_o/writer/actor/notice_work.rs:61`, `:129`; `src/services/tui_o/writer/actor.rs:361` | resume capture 전 finite source/history cohort, binding checkpoint와 freeze 시각 |
| materialize append | `src/services/tui_o/writer/actor/notice_work.rs:169`, `:200`, `:204` | 기존 payload canonical 유지, covers면 재append 없음 |
| evidence validate/merge | `src/services/tui_o/store/ledger/input_notices.rs:106`, `:166`, `:189`, `:217` | source/start/end/raw 일치, optional 단조 보강; complete는 prompt/digest/key/payload/origin 모두 Some |
| ledger append/replay | `src/services/tui_o/store/mod.rs:483`; `src/services/tui_o/store/ledger.rs:447`, `:794`, `:839` | append 후 apply; complete unknown JSON variant는 damage 오류, truncate 대상 아님 |
| notice format floor/handover | `src/services/tui_o/store/input_notice_floor.rs:13`, `:43`; `src/services/tui_o/writer/input_notices.rs:39` | `input_notice.floor=1`, migration_started durable 확인 뒤 relay→writer handover |
| startup/incarnation deriver | `src/services/tui_o/writer/actor/fresh_resume.rs:24`, `:135` | rescope는 Codex만; Claude notice 만료 신호로 사용 불가. rebuild가 ledger를 지우지 않음 |
| boot/주기 재진입 | `src/services/tui_o/writer/actor.rs:393`, `:400`, `:449`, `:876`, `:903` | 한 actor loop에 prepare 두 번; follow/capture/tend 뒤 새 evidence persist |
| pending 열거·origin 재시도 | `src/services/tui_o/writer/actor/notice_work.rs:238`, `:256`, `:268` | Posted/Excluded 외 전부 clone; 미완성은 binding log 재조회 |
| Batch owner snapshot | `src/services/tui_input/human_frame/history/notice_owner.rs:67`, `:80`, `:100`, `:132` | history 읽기, source별 census, origin별 후보; stable history/context/native fingerprint 재확인 |
| candidate/blocked 생성 | `src/services/tui_input/human_frame/history/notice_owner.rs:176`, `:225`, `:264`, `:309` | source/owner/nonce/context 불일치 → hash별 registration_source_mismatch; unbound → unbound_pre_source |
| native attribution | `src/services/tui_input/human_frame/history/notice_owner.rs:362`, `:407` | 정확 UUID receipt 우선; native 순서로 unused 등록 하나 소비; 같은 hash의 blocked 후보는 Unowned를 허용하지 않음 |
| 최종 owner 판정 | `src/services/tui_input/human_frame/history/notice_owner.rs:132` | Owned / Unowned / Pending(reason). reason은 증거 상태가 아닌 진단 사유 |
| admission/exclude | `src/services/tui_o/writer/input_notices/admission.rs:25`, `:45`, `:72`, `:79`, `:242` | 현재 source 불필요 판정도 complete origin과 tmux authority 뒤에 있음; Excluded는 UnitKey 필요 |
| actor 재준비/사유 로그 | `src/services/tui_o/writer/actor/notice_work.rs:295`, `:328`, `:411` | migration_unresolved/incomplete/authority_busy/ownership reason; reason 교대 시 다시 info |
| legacy migration 소비 | `src/services/tui_o/writer/input_notices/migration.rs:98`, `:116`, `:126`, `:194`, `:207` | 모든 evidence를 읽음; 불확실 기존 메시지는 MigrationUnresolved 및 후보 ID 예약으로 보존 |
| durable state의 이관 소비 | `src/services/tui_o/store/ledger/input_notices.rs:246`, `:284`; `src/services/tui_o/store/ledger.rs:449` | Imported→Excluded, unresolved는 감사 부채; 새로운 proof 없는 무조건 청소 불가 |
| 실제 게시/재시도/확인 | `src/services/tui_o/writer/deliver.rs:589`, `:621`, `:850`; `src/services/tui_o/writer/local_retry/notice_selection.rs:12` | Prepared/Posted/NotFound/Ambiguous; unresolved POST와 local retry는 만료로 대체 불가 |
| debt/retention/queued 소비 | `src/services/tui_o/writer/actor/notice_work.rs:390`, `:434`, `:457`; `src/services/tui_o/writer/actor.rs:729`, `:835` | debt count와 raw-independent/notice-only 판정 모두 새 terminal을 보아야 함 |
| memory reset/로그 수명 | `src/services/tui_o/writer/deliver.rs:204`, `:265`; `src/services/tui_o/writer/actor/notice_work.rs:267`, `:330`, `:353` | 기준 main의 HashMap<RecordId,String>은 restart와 eligible 전환에 지워짐; 새 구현은 nonterminal 동안 유지 |

모델 값은 이 표에서만 추상화했다. RecordId는 `(SourceId,start,end)`, InputClass는
native 귀속 증거의 owned/direct 또는 기존 renderer가 거부하는 policy, proof unknown은
선택 필드 부족·snapshot/refusal 오류이다. closed는 strict binding log가 증명한 이전 source→후속 source 경계이고
그 자체는 면제 증거가 아니다. logs/posts의 0..2는 두 번째 사건을 검출하는 유한 카운터이다.
before/after/mutant는 검증 설정이며 새 운영 설정값이 아니다.

## 입력 출처와 registration_source_mismatch

| 출처 | 새 record 경로 / 즉시 정착 가능한 최초 지점 | 게시 의무 |
|---|---|---|
| Discord idle/직접 전달 | wrap_user_prompt_with_author → provider user row → Projection::project | 기존 classifier의 명확한 relay 또는 정확한 native ownership proof면 불필요 |
| Discord mid-turn/plain inject | plain_inject → durable Registry → enqueue → user/command UUID → projection | 등록의 정확한 record 귀속을 증명한 뒤 불필요; enqueue만으로는 부족 |
| 직접 TUI | provider user row → project_record | human-active이고 Unowned이면 한 번 게시. 동일 body/전체 header twin도 각각의 UUID로 판단 |
| 예약 | owner_input::owned/start 또는 observer → provider row | 해당 native row를 owner 증거로 귀속한 경우만 불필요; 예약 metadata나 문구만으로 제외 금지 |
| slash | machine envelope/local-only 정책 → input_notice_content | 기존 정책이 None인 경우 불필요; 임의 bare `/…` 전체를 새로 제외하지 않음 |

새 입력도 막히는 코드상 경로는 다음과 같다. `discord_input`은
`is_discord_relayed_user_prompt`를 부른다. 이 함수는 `extract.rs:202`에서 opens_inject이면 false이고,
opens_inject는 `human_frame.rs:230`의 전역 history::known을 읽는다. 따라서 평범한 헤더도 등록이 있으면
notice 후보가 되고, exact native receipt가 없으면 project는 Pending evidence를 남긴다.
Batch::candidates는 `notice_owner.rs:225`의 source/관련 execution 확인 또는 `:241`의 historical_match
실패를 hash별 blocked에 넣는다. attribute는 `:407`에서 blocked를 매번 Pending으로 반환한다.
record 나이와 무관한 경로여서 11:48/11:56 새 메시지도 해당할 수 있다.

원본 attachment nonce는 source 첫 binding이고, 새 inject의 target nonce와 다를 수 있다.
main은 이미 **resume + exact expected native session**인 경우를 처리한다. 따라서 이번 관측을
"resume 지원이 없다"고 단정하지 않는다. 정확히 어떤 source/path/file/nonce/기대 source 값이
운영 311건에서 어긋나는지는 정본 수치만으로 확정할 수 없다. 구현 회귀 fixture는 각 분기를
분리하고, source/nonce 비교를 통째로 제거하는 수정은 금지한다.

## 선택안: actor 안의 단조 정착 + 경계 증거 재사용

| 안 | 비용/새 입력/재시작 | 위험 |
|---|---|---|
| A: writer tick의 공통 reconciliation | boot 재생과 새 입력 모두 동일 함수. terminal 이후 owner/census/원본 clone 대상에서 제외. snapshot 읽기 비용은 아직 열린 scope만 부담 | tick 변경이므로 terminal/index/debt/최종 admission의 일관성을 함께 시험해야 함 |
| B: boot/세션 경계 일괄 정리 + tick 로그만 억제 | boot 한 번은 싸지지만 현재 incarnation의 새 11:48/11:56형 record는 다음 경계까지 계속 재처리. boundary 누락/재시작 타이밍별 경로가 늘어남 | 로그만 조용해지고 CPU 루프가 남거나 boot 전용 cleanup과 capture가 경합 |

**A를 선택한다.** 새 child 모듈 `writer/actor/notice_settlement.rs`가 retained record와 session proof를
판정한다. boot/경계는 입력 증거를 제공하고 별도 삭제 루틴은 만들지 않는다. giant actor/ledger/deliver에는
선언·연결·거절 분기만 둔다. 입력 custody/checkpoint/Gateway wake는 손대지 않는다.

순서:

1. 기존 원본 검증을 통과한 Materialized를 읽고 이미 terminal인 record를 제거한다.
2. 원본만으로 **기계 입력/local-only 정책상** notice 불필요인 record를 origin 복구·tmux authority 전에 종결한다. retained Discord 헤더 bool은 Policy proof로 사용하지 않는다.
   새 명확한 Discord 입력은 기존 Ok(None) 경로를 유지한다. 재생된 pending에 대해서만 감사 종결을 쓴다.
3. retained injection은 source-bound native receipt 또는 stable census의 Owned를 확인한다.
   원본 UUID/frame digest, full source identity, namespace+전체 attempt key, source expectation,
   logical owner/channel/runtime-root와 historical execution 증거를 함께 보존한다.
   현재 incarnation의 liveness/pane 확인은 **불필요한 안내의 제외**를 막지 않게 한다.
4. source 최초 attachment와 다른 nonce의 exact receipt는 별도 historical proof helper에서 검증한다.
   등록 자체의 source/target 및 immutable execution context가 같은 provider/channel/logical owner/root와
   expected native source를 가리켜야 한다. fresh/resume launch mode를 구분해 시험하고, 기존
   resumed_execution 경로는 유지한다. source/nonce가 맞지 않는 후보는 그대로 blocked이다.
   target context를 읽을 수 없으면 proof를 만들지 않는다. V1은 있는 source/receipt만 이용하고
   없는 nonce/namespace를 발명하지 않는다. 단순 class-hash 일치나 history::known은 proof가 아니다.
5. Unowned direct는 기존 admission과 migration reconciliation, Prepared/첫 POST poll을 통과한다.
   preflight 결과를 게시 permit로 저장하지 않는다. history/context 변동은 기존 Pending으로 되돌린다.
6. terminal 반영 후 남은 record로만 Batch::refresh/prepare/migration/debt/owed 처리를 한다.
   migration의 cohort 완성 판정에서도 audited no-notice terminal은 해결된 항목으로 세고,
   기존 capture cursor 상한과 unresolved candidate ID 예약은 그대로 보존한다.
   정책 terminal과 exact receipt 경로는 origin/payload 부족이어도 proof가 충분한 경우만 가능하다.
   origin이 없어서 owner/channel을 증명할 수 없는 receipt는 정착시키지 않는다.

### Durable 종결과 만료 규칙

새 `NoticeEntry::Settled { record, settlement }`를 도입했다.
Policy의 early settlement는 neutral parser로 증명 가능한 기존 machine/local-only 모양의 충분조건으로 제한한다.
렌더러가 추가로 거절하는 복합 wrapper/Provider Session Reuse 모양은 기존 complete admission에서 처리한다.
ledger가 Discord renderer를 역방향 호출하지 않으며, 충분조건 밖의 불완전 record는 증거를 보존한다.
settlement는 typed proof와 optional `expired_after`를 갖는다. None은 not_required, 검증된 boundary seq가
있으면 expired이며 Posted/기존 Excluded/LegacyImported는 그대로 terminal이다.
proof는 unknown field를 거절하는 typed enum으로 원본 digest와 정책 종류 또는 정확한 native ownership 증거,
그리고 expired인 경우 경계 seq/이전 source와 execution을 가진다. 단순 reason 문자열은 허용하지 않는다.
`NoticeState`에 RecordId별 terminal map을 재생하고 열린 record는 같은 terminal 판정으로 계산한다. raw evidence는 삭제하지 않는다.
키 있는 종결은 기존 Excluded 소비자에도 반영하되 하나의 ledger entry apply로 함께 바뀐다.
키 없는 non-notice도 record terminal로 감사 가능하다. 같은 proof 재생은 멱등, 충돌은 violation이다.
terminal 뒤 native key 보강은 같은 ledger apply에서 Excluded 인덱스에도 반영해 종결을 다시 열지 않는다.

| 조건 | 허용 동작 | 금지 동작 |
|---|---|---|
| 기존 deterministic no-notice 정책 또는 정확한 Owned proof | 현재 세션이면 not_required | `[User:` 접두사/본문 hash만으로 정착 |
| 위 면제 proof + 확정된 과거 세션 경계 | expired와 proof/경계 ledger 기록 | 단순 프로세스 재시작, nonce 차이, source missing을 면제 proof로 사용 |
| 오래된 11-10 record, 나이만 알려짐 | 증거 재평가, 없으면 보존 | TTL/현재 시각으로 폐기 |
| incarnation 교체지만 direct 게시 부채 | 기존 게시·복구 계속 | incarnation 다름만으로 expired |
| Prepared/효과 불확실/미해소 retry | 기존 확인/재시도 계약 유지 | 뒤늦은 proof로 기존 효과를 지우거나 Prepared를 terminal로 덮음 |
| migration_unresolved | 원래 후보 예약·감사 유지. 독립된 no-notice proof가 있으면 record terminal 추가 | 옛 Discord 메시지의 존재만으로 직접 입력이 이미 게시됐다고 선언 |

Expired에 벽시계 상수는 사용하지 않는다. 경계는 strict binding log의 `old=record.source`인 비거절 후속 Bound/Resolved와 증가하는 seq로
확인한 source 교체로 제한한다. Pending은 정확히 한 번 해소된 source와 같은 owner/provider/channel을
확인해야 한다. 같은 source를 재사용한 nonce 교체는 종료 증거가 아니므로 proof가 있으면
not_required로 정착시키고, 없으면 보존한다. confirmed clear는 기존 제외 경로를 그대로 사용한다. 정착 증거는 경계 관측과 독립적이어야 한다.
현재 source 전체를 한 번에 만료하지 않고 각 RecordId별로 검사한다. /clear의 direct 제외는 위에 적은
기존 정책 범위로만 유지하며 새 expiry의 면제 proof로 합쳐 확장하지 않는다.

append/재생 전에 raw record/범위/UUID/digest와 proof의 동일성을 검증한다. ledger wrapper가
현재 Piece/Posted/Prepared와 충돌하는 종결을 거절한다. write 실패 시 index에서 빼지 않고 actor를
기존 I/O fail-closed 경로로 보낸다. Materialized 보강은 terminal을 다시 열지 않으며 원본 충돌도 계속 거절한다.
중복 UnitKey의 다른 record도 기존 UUID 내용 충돌 및 Posted/Excluded 중복 방지를 통과해야 한다.

format floor `1\n`은 notice 기능이 필요한 ledger라는 현재 의미로 유지한다. 새 변형을
기존 Materialized의 optional 필드에 숨기지 않는다. 구 main reader는 complete unknown variant에서
`ledger.rs:839`의 damage로 멈추며 줄을 절단하지 않는다. 새 reader는 기존 floor/옛 entry를 그대로 복원한다.
호환되지 않는 구 binary로의 rollback은 ledger를 읽지 못할 수 있다. 자동 변환/삭제/구 binary 배포 허용을
추가하지 않으며, unsupported-entry의 파일 bytes 보존을 시험 계약으로 둔다.

### 구현 진입점 보완 (현재 코드)

| 읽기/갱신 지점 | 값과 역할 |
|---|---|
| `store/ledger/input_notices/settlement.rs`의 revision/terminal/settle/apply | revision은 immutable evidence의 optional 필드 존재 bit와 unresolved bit; terminal map은 source/range별 proof와 optional boundary seq |
| `history/notice_owner/generation.rs`의 WorkCache::select/track_contexts/retry | scope별 history/native/context/binding fingerprint와 cursor, record revision, 실행별 quick retry 사용 여부/기한 |
| `history/notice_owner.rs`의 Scope::current/Batch::refresh/decision/owned_proof | scope 재사용, context unreadable 진단, full source/UUID 귀속 proof; 정상 정착은 source 교체 불필요 |
| `writer/actor/notice_settlement.rs`의 changed_notices/settle_notice/boundary | source+UUID 또는 machine/local-only Policy 종결, strict original+successor binding 검증, no prepared send |
| `writer/actor/notice_work.rs`의 prepare_notices/defer_notice | changed record만 raw clone/authority 준비; 최초 삽입만 로그하고 eligible 때 맵 유지 |

### pending 로그

기존 `notice_pending` 메모리 맵을 nonterminal record가 남는 실행 수명 동안 유지한다.
최초 삽입에만 info를 쓰고 이후에는 reason만 갱신한다. eligible→pending 왕복에서 맵을 지우지 않고,
terminal이 된 record만 제거한다. 재시작은 맵이 비어 추가 한 번을 허용한다.
정보 로그 억제를 위한 durable ledger entry는 추가하지 않는다.

이번 구현은 별도 주기 요약 로그를 추가하지 않는다. pending 수는 ledger의 Settled/Excluded/Posted와
미정착 evidence 및 owed에서 계산하며 log map 크기로 계산하지 않는다. `remains pending`은 실행별 최초
defer에만 쓴다. 운영의 채널별 record 수는 coordinator가 배포 뒤 ledger 기준으로 대조한다.

## 상태 차트와 liveness 범위

[차트](statecharts/tui-o-writer/notice-evidence.chart.mmd),
[TLA+](statecharts/tui-o-writer/NoticeEvidence.tla),
[검증 보고](statecharts/tui-o-writer/notice-evidence.report.md).
새 모델은 WriterWake의 실제 Route 전이를 EXTENDS하여 custody/checkpoint/execute-once 및
blocked self-wake 규칙을 함께 검사한다. 기존 BootSpec과 InputNoticeSettlement의 source/UUID 순서 모델도
수정 없이 재검증했다. 개별 record 만료 모델이 pairing 알고리즘을 대신 증명한다고 주장하지 않는다.

INVARIANT: 직접 입력 게시 정확히 1, 조용한 유실 0, 면제 proof 없는 종결 0,
게시해야 할 direct의 만료 0, Prepared 보존, 재생 시 terminal 보존, record당 실행 수명별 pending 로그 최대 1(재시작 후 추가 1 허용).
PROPERTY: `EveryPendingTerminates`는 pending → eventually Posted/Excluded/Expired이다.
세 경로 중 Excluded는 notice 불필요의 정착이며 입력 실행/답변 완료와는 별개다.

**불변식과 무조건 liveness를 동시에 만족한다고 선언할 수 없는 경우가 있다.**
direct인지 injected인지 구별하는 유일한 증거가 영구 소실됐으면 정착/게시/만료 어느 쪽도 안전하게
선택할 수 없다. 기존 main도 `notice_owner_tests.rs:639`에서 이 보존을 요구한다.
GREEN의 환경 전제는 필요한 원본/귀속 증거가 결국 확보되고, 기존 actor/권한/전송 확인이 공정하게
진행하며, 재시작이 유한하다는 것이다. 영구 증거 부재를 모델에서 빼지 않았고 Unavailable은 의도적 RED이다.
이 경우는 실제 남은 pending 수에 포함해 보고한다. 이름을 바꾼 격리 상태나 hidden debt=0으로 위장하지 않는다.
따라서 운영 356→0은 아직 증명한 목표가 아니며, 잔여 미증명 record의 수용은 독립 리뷰 결정 사항이다.

| 설정 | 기대 결과 | 실제 TLC |
|---|---|---|
| Before (현재 stuck 경로) | EveryPendingTerminates RED | 동일 |
| AfterOwned / AfterPolicy / AfterDirect | 모든 safety/선택 PROPERTY GREEN | 동일 |
| NoSettle (원인 제거) | EveryPendingTerminates RED | 동일 |
| ExpireUnproven | NoQuietLoss, NoExpireOwedDirect, DirectEventuallyPosted RED | 동일 |
| ReasonLog | PendingLoggedOnce RED | 동일 |
| DropLog (이제 수용하는 재시작 초기화) | 실행별 1회 GREEN | 동일 |
| SameSource / Boundary | source 유지/별도 경계 fairness 모두 GREEN | 동일 |
| WaitBoundary | source 유지 중 EveryPendingTerminates RED | 동일 |
| DropReplay | ReplayKeepsTerminal RED | 동일 |
| Unavailable | EveryPendingTerminates RED | 동일 |

17 config, checker 실패 0(예상 RED 포함), fairness precheck·singleton temporal 실행을 포함한다.
모델 GREEN은 Rust 회귀 시험 통과나 운영 pending 0/CPU 개선의 증거가 아니다.

## CPU와 배포 뒤 수치 계약

main은 한 loop에 prepare를 두 번 부르고, nonterminal evidence를 clone/해시한 다음 owner snapshot을
재구축한다. A의 census 캐시가 읽기를 막아도 scope별 relevant/context와 admission 비용은 남는다.
terminal index를 prepare, Batch::refresh, migration work, debt 계산에 공통 적용하면 무거운 비용은
전체 356이 아니라 열린 record/scope 수에 비례한다. 전체 evidence의 가벼운 set membership 순회는
남을 수 있으나 종결된 record에 대한 원본 clone·SHA·origin 복구·context/pane 조회는 0이어야 한다.

메모리상 scope별 dependency 세대 캐시를 둔다. evidence revision, history/native/context/binding의
파일 fingerprint, capture cursor를 비교해 같으면 원본 clone·origin 복구·snapshot/context·authority
재계산을 생략한다. context가 아직 없을 때도 경로를 관찰해 생성/교체가 세대를 바꾸게 한다.
관측 불가능한 오류/authority busy에는 5초 간격의 제한 재평가를 허용한다. 같은 source 파일을 읽던
일시 native I/O 오류에는 기존 즉시 복구 계약을 지키는 재시도 한 번을 허용하고, 반복 오류는 5초로 제한한다.
최종 게시 admission은 캐시하지 않으며 실제 POST 때 기존 fence/snapshot 검증을 다시 한다.
A의 native census 캐시와 역할이 다르고, pending verdict 자체가 영구 결론으로 승격되지 않는다.

legacy-body-census-writer의 독립 CPU는 이번 notice 정착만으로 해소됐다고 주장하지 않는다.

| 배포 뒤 동일 10분 창의 지표 | 이전 | 목표/판정 |
|---|---|---|
| 개별 `input notice remains pending` 줄 | 2,802/10분 | 동일 record 신규 줄 0; 총 줄 ≤ 창 안에서 실행별 처음 defer된 distinct record 수. 기존 미로깅 record와 재시작의 최초 1회는 별도 집계 |
| adk-cc pending record | 356 (header 시작 311) | proof가 있는 Discord/정책 record는 0 pending; 종료 시 `356 - 실제 terminal + 신규 - 신규 terminal` 대조. 311 전부 proof 확인 시 기존 잔여 상한 45 |
| 나머지 10채널 | 채널별 수 미제공 | 배포 전/후 distinct RecordId 수를 각 채널별로 남김. 합계/로그줄을 record 수로 오인하지 않음 |
| known terminal 재평가 | tick마다 evidence 스캔 | origin/owner/SHA/pane 수행 0, 재시작 후에도 동일 |
| CPU top/sample | top 161.6%, 평균 약179% | coordinator의 같은 조건 원문 측정으로 의미 있는 감소 판정. 아직 수치 PASS 아님 |

이 레인은 재시작·배포·운영 로그 쓰기를 하지 않는다. 10분 운영 지표는 착지 후 coordinator가 수집한다.

## 구현 조각·시험 계약·크기

| 조각 | 파일 계획 | prod 순 코드 추정 |
|---|---|---|
| S1 durable terminal proof와 replay | ledger/input_notices.rs, 새 input_notices/settlement.rs, ledger.rs 연결 | +250 |
| S2 원본/귀속 proof와 세션 경계 정착 | 새 history/notice_owner/generation.rs, notice_owner.rs 연결, 새 actor/notice_settlement.rs, notice_work.rs 연결 | +400 |
| S3 terminal 소비/로그/계측 | migration.rs, admission.rs, notice_work.rs, deliver.rs의 선언/연결 | +120 |
| 시험 | 기존 input_notices/tests.rs와 notice_owner_tests.rs, 새 notice_settlement_tests.rs, notice_work_tests.rs | 테스트 +450~650 (prod cap 제외) |
| 문서/공식 생성 목록 | 본 설계·상태 차트, 필요시 공식 sync | prod 제외 |

prod 약 10파일 / +770, 불확실성 여유 +300까지도 PLAN 선 +1200 이내; 최종 cap 30파일/+1500.
다른 레인 소유 파일 지정 없음. A의 census.rs는 수정 계획 없음. 시간 추정: 구현 60~90분,
대상 검증/뮤턴트 30~60분, clippy/source scan/static gate는 슬롯 상황에 따라 별도이며 이번 설계의 상한과 구별한다.

| 시험 계약 | 수정 전 RED / 수정 후 GREEN 기준 |
|---|---|
| 동일 actor 반복·원본 없는 no-notice | 불필요 record가 incomplete/authority_busy 반복 → 한 번 audited terminal, POST 0, 열린 집합 0 |
| 신규/복원 Discord exact row | valid UUID+source receipt, fresh/same-source resume/과거 incarnation 각각. receipt 없는 foreign nonce/path/file/offset은 계속 보존 |
| 직접 twin 보존 | 동일 body/전체 frame 뒤의 다른 UUID는 정확히 POST 1; receipt/정착에 이전 attempt 재사용 금지 |
| 11-10형 오래된 record | closed+no-obligation proof만 expired, 같은 시각/경계의 owed direct는 게시/복구 계속 |
| 로그 | reason 4종 회전, eligible 왕복, 실행별 로그 ≤1(재시작 후 추가1 허용); 로깅 실패·원본 충돌 경계 포함 |
| 경계/경합 | clear 직전/직후, A→B→A, context/history/native stamp를 판정 직후 변경, Prepared/uncertain POST 중 경계 변경: 직접 입력 소실·중복 0 |
| 옛 형식/복원 | floor1·V1·V2 passive/PreSource·key 없는 evidence; terminal append crash 전후; unknown variant 구 reader는 bytes 동일하게 거절 |
| 무거운 반복 비용 | 356 fixture 중 311에 실제 면제 proof 부여. 정착 후 반복 prepare에서 settled record별 origin/owner/SHA/pane 재계산 0, 새 direct fixture는 게시 |
| migration/retention | unresolved candidate 예약 유지; terminal은 migration completeness를 막지 않음; raw 독립성/debt/owed가 같은 terminal 판단 사용 |
| 원인 제거 뮤턴트 | 정착 연결 제거→pending 재현, 최초 삽입 guard 제거→사유 회전 로그 재현, expiry 면제 guard 제거→direct 소실; 복원 bytes SHA256 동일 |

구현 검증은 cargo-shim + 지정 profile, 기본 시험 병렬, 필요한 PG 시험은 pg_throwaway로 실행한다.
notice/input 접수 기존 시험을 재사용하고 자기 파일 clippy 경고 0을 확인한다. 새 lib 시험 ID는
required PR job이 선택하도록 공식 inventory/lane check로 확인한다. source_scan_readers failed 0 및
prepush --static-only의 PREPUSH_STATIC_OK 후에만 REVIEW_READY를 낸다. 구현 검증 결과와 실행 수는 레인 report-impl-b1.md에 기록하며 미실행을 0 tests/PASS로 표기하지 않는다.


## Frame별 context와 최종 admission 재시도 보완

동일 source scope의 다른 frame context는 정상 retired sweep으로 사라질 수 있다
(`src/services/tui_prompt_dedupe/binding_context.rs:563`). history의 `relevant`는
Related/Unrelated/Uncertain으로 나누고, context 읽기 실패/경합은 해당 등록의 frame hash만
Blocked로 만든다. planned/raw-null 등록의 불확실성도 같은 hash의 직접 게시를 막는다.
경로 fingerprint 관측은 유지해 context 복구 시 재평가하며, 무관 hash A의 직접 twin은 POST1을 보장한다.

최초 eligible 뒤 최종 admission이 거절돼도 source/range별 메모리 재시도 기한5초를 둔다.
`deliver_owed`는 기한 전 notice를 회전시키고 뒤의 Body를 계속 처리한다. terminal은 바로 회수하고,
최종 거절은 prepare 재평가도 같은5초 기한에 예약한다. 기한 이후에는 prepare와 최종 admission을 새로 실행해 Prepared/첫 POST 현재 상태 검증을 유지한다.
이 기한은 종결·만료·게시 permit이 아니며 pending/debt를 지우지 않는다.

pending 수백 건이라는 현재 관측에서 영구 Pending map의 절대 상한/soft-limit 경보는 추가하지 않는다.
관측만 추가하는 제안은 이번 수용 범위에서 기각했으며 기존 terminal 회수 규칙을 유지한다.
