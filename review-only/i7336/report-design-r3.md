DESIGN_READY r3 source=c5477168cfdb2591aeebb77aa091d3fe13bca545 worktree_head=fb2219535d889de858a3d9e1429d32aa6e31d309 code_changes=0 commits=0

# #7336 설계 r3 보고

설계: [design-r3.md](/home/adk/adk/lane-runs/i7336/design-r3.md)

리뷰: [review-design-r2-webgpt.out.md](/home/adk/adk/lane-runs/i7336/review-design-r2-webgpt.out.md)의 “ChatGPT 답변:” 이후를 기준으로 삼았다. P1 3건·P2 3건의 수정 방향을 채택해 역할과 상태 전환부터 다시 작성했다. **독립 설계 리뷰 2회차 입력**이며, 이번 작성 레그가 CLEAN을 자체 판정하거나 구현 시험 PASS를 선언한 것은 아니다.

## 요약

home H는 공유 PG 설정, execution owner E@g는 채널 실행권, gateway G는 token별 입력 수신권이다. 입력 target은 `H if available else peer(H)`이며 G나 preferred를 target으로 대신 쓰지 않는다. O owner가 gateway와 다른 Mac이어도 자기 local source와 production REST port로 응답한다. 채널 반환을 위해 bot 전체 gateway를 handback하지 않는다.

`/node`는 owner/input이 없는 채널에서도 **성공 응답 전에 PG에 저장**한다. 모든 노드는 같은 resolver를 읽는다. reset은 inherit tombstone이며 늦은 local import로 되살리지 않는다. 옛 loaded 값이 충돌하거나 한쪽 보고가 없으면 명시 설정으로 해결할 때까지 conflict/pending이다. 운영 파일은 읽지 않았다.

제어 가능한 old owner는 현재 native 작업·답 완료와 exact normal end 후 전환한다. old owner가 unavailable이면 **old generation의 새 효과를 막고 새 source를 건강한 home에 먼저 admission**한다. 종료 성공으로 표시하지 않고 exact old session 정리는 deferred로 남긴다. old Mac 재접속 시 원 generation 출력은 계속 막히고, 늦은 cleanup이 새 세션을 종료할 수 없다. 정상 종료 일시 실패는 node-local reconciler가 추가 사용자 입력 없이 재시도한다.

전환 알림 K에는 채널 identity·generation·from·to가 들어간다. K는 Discord nonce뿐 아니라 실제 outbox session_key와 persistent dedupe에도 포함된다. 같은 event는 owner 행의 notice_outbox_id로 같은 row를 되찾으며 failed/cancelled 이후도 포인터를 지우지 않는다. 정상 배달/조회 복구는 visible1, 외부 결과 불명은 visible≤1+uncertain으로 구분한다.

초기 pilot 목록과 최종 derived 정책을 분리했다. 완성 후에는 home/binding을 해석할 수 있는 새 채널·상속 thread도 자동 대상이며 전체 kill switch는 유지한다. r1의 power witness·maintenance 알림·node 전체 quiesced ACK·전용 반환 명령은 되살리지 않았다.

## 지적별 처리표

모든 좌표는 source SHA `c5477168…` 기준이다. 아래는 **선택한 설계와 구현 시 증명 계약**이며 시험 결과가 아니다.

| 지적 | 그대로 두면 운영에서 생기는 일·근거 | r3 해소 위치/방법·권고 이유 | 증명할 시험·뮤턴트 |
|---|---|---|---|
| P1-1 home/owner/gateway 혼합 | preferred=A, Y.home=B에서 B 복귀 후에도 O가 B에서 시작하지 못함. `src/services/tui_o/channel_policy.rs:357`, `src/services/tui_o/writer/host.rs:505`, `src/services/discord/runtime_bootstrap/o_writer_host.rs:88` | 설계 §1/4/5. home 또는 정확한 peer만 선택. cutover/intake_route는 owner permit, host는 production owner REST와 local source 사용. 선택 전 capability와 CAS 뒤 writer 준비를 구분. 전역 handback보다 채널 단위 책임이 맞고 서로 다른 home 채널을 함께 처리할 수 있어 권고 | R01 최소64 배치, R02 같은 token 두 home, R15 non-gateway ctx=None. target=gateway/home=preferred/가짜 gateway bool/이름 하드코딩 복원 mutant RED |
| P1-2 local `/node` | A에서 저장 성공 뒤 다음 일반 입력 전에 G=B가 되면 B가 옛 home으로 실행. `src/services/discord/commands/node.rs:67`, `:78`, `src/services/discord/settings/write.rs:447` | 설계 §3. 신규 PG preference+revision/operation. `/node` commit/read-back 후 성공, reset tombstone, owner row 없이도 저장, late import CAS. 기존 PG default와 결합하는 공유 정본이 파일 복제보다 단순해 권고 | R03 분리 root+공유 PG에서 /node→일반 입력 없이 G 교체→첫 입력, reset/역방향/no owner. R04 충돌·늦은 import·ACK 유실. local 우선/첫 admission까지 write 지연/reset 삭제 mutant RED |
| P1-3 unavailable old fallback | A 복귀 뒤 B를 끄면 B의 close ACK를 받을 수 없어 건강한 A의 m2가 멈춤. `src/services/discord/commands/control/clear_teardown.rs:59`, `src/services/discord/herdr_terminate.rs:383`은 local 종료 경로 | 설계 §6/7. unavailable은 old source 보존·세대 revoke·새 source admission·physical cleanup deferred. B 재접속은 restore 전 PG 확인. cleanup은 generation/op/native pin 대조. 정상 end 권한을 넓히지 않으면서 다음 입력을 처리하므로 이 예외를 권고 | R06 A off→m1 B 완료→A on 입력 없음→B off→m2, 반대 방향 반복. R07 B old submit/POST0·원 source replay0·새 B session 종료0. 무조건 ACK 요구/current g 재부착/이름만 종료 mutant RED |
| P2-1 close 실패 후 재개 | transient close 실패가 해소되어도 새 완료 callback이 없어 held 입력이 남을 수 있음. `src/services/discord/commands/control/clear_teardown.rs:97` | 설계 §7. gateway/standby의 독립 bounded reconciler가 같은 pending/정리 요청을 방문. 2초 cadence+callback wake, exact op 재확인, 3회/고정 기한, 영구 거절은 needs_operator. 입력을 더 요구하는 것보다 보관된 요청의 재개 책임이 명확해 권고 | R08 실패→해소→추가 입력 없이 A 실행1. R09 재시작·불응·영구 거절·m3가 기한 연장하지 않음. 실패 row 제외/callback만 재개 mutant RED |
| P2-2 nonce/dedupe 접합 | 같은 session/reason으로 A→B→A→B 전환하면 마지막 알림이 기존 dedupe에 빠질 수 있음. `src/services/message_outbox/identity.rs:107` | 설계 §8. K→`routing-transition:<F>` session_key→실제 dedupe, 같은 K→nonce. `src/services/message_outbox.rs:246` persistent tx helper와 notice_outbox_id 재사용. TTL 충돌의 None 대신 기존 id로 수렴하므로 권고 | R13 TTL 내 세 전환은 서로 다른 id/nonce·각 visible1, ACK 유실 같은 event는 같은 id. generation/F 제거 mutant RED. R14 외부 결과 불명을 sent로 처리하지 않음 |
| P2-3 pilot/최종 범위 | 별도 allowlist를 계속 관리하면 새 정상 home 채널·thread가 빠질 수 있음 | 설계 §9. 초기 pilot만 제한 목록, 최종 derived는 shared home/binding 및 기존 thread 상속에서 자동 파생. 전체 off는 기존 source/fence/custody를 유지. 중복 설정 관리가 줄어 권고 | R17 새 채널/thread/DM·home conflict·off 중 begun/held/deferred. 영구 allowlist 누락/unknown→임의 gateway 실행/off→fence 해제 mutant RED |

새 방향 제안은 **지금 채택**했다. r2를 먼저 활성화하고 나중에 global handback 의존을 제거하면 같은 hot file과 권위 경계를 두 번 바꾸므로 권고하지 않는다. 대가는 아래의 실제 owner REST·공유 home/custody 구현량이다.

## 크기와 구현 단위

| 단위 | 범위 | prod 파일 / 순 code 예상 | 활성화 상태 |
|---|---|---|---|
| P-A | pure 판정·공유 home 모델/schema·대칭 fixture·알림 K/renderer | 10~14 / +550~750 | 지금 착수. 외부 실행/POST/normal end/production home write0 |
| P-B | `/node` PG writer/read/import·canonical source·PG staged→pending·Bundle pin·공통 first-effect 접합 | 12~18 / +600~900 | 필요한 #7272 source/effect 계약만 고정. fallback off |
| P-C | 비-gateway O/REST·원 output stamp/PG guard·owner reconciler·normal end/deferred/retry | 13~19 / +650~950 | 양방향 실행/종료·복원 fixture를 닫음 |
| P-D | persistent notice tx/sender·CAS/promotion 결합·pilot/derived/off·수직/live 검증 | 9~15 / +350~550 | 계약 충족 뒤 검증 채널 pilot, 마지막 derived |

총 고유 **34~44 prod 파일 / 순 +2,150~3,150**, 시험 raw +1,000~1,700, 문서 별도다. r2의 +1,350~1,950보다 늘었다. 공유 `/node`, 비-gateway O writer, 로컬 파일을 대신할 PG pending admission, 지연 정리의 비용을 r2가 빠뜨렸기 때문이다. 전원 증명/전체 node quiescence 비용을 되살린 것은 아니다.

각 리뷰 단위는 prod30파일/순+1500 아래이며, 구현 중 **+1200 예상 전에 PLAN_READY로 분할**한다. 이는 추정이고 구현 cap PASS가 아니다. 공통 helper 활성화/등록 비용도 해당 단위에 센다. 줄 맞춤 child 이동이나 cfg(test)로 prod 분량을 감추는 설계는 없다.

#7415는 조회 시 open이다. source의 giant raw 차단이 유지된 동안 해당 증가 조각은 그 착지를 확인해야 한다. 착지 뒤에는 경고만 따르고 새 숫자 상한을 만들지 않는다. 설계 §12에 파일별 raw 기준/증가를 적었다. 기존 giant 변경 후보는 intake_router_hook(+20~40), owner_record(+10~25), db/intake_outbox(+15~35), session_runtime(+10~25), outbound/delivery(+10~25), gateway(+10~25), config(+10~25), intake_gate(+10~25), runtime_bootstrap(+5~15)다. message_outbox/server/turn_finalizer/pending_queue는 기본 재사용 +0 및 필요한 연결 증가를 별도 표시했다. writer/host·intake_worker는 1,000줄을 새로 넘을 수 있다.

## 즉시 선행 조각과 머지 순서

P-A는 #7272/#7330 전체 완료를 기다리지 않는다.

1. **A1:** target 판정과 M1~M8×preferred×provider×writer 대칭 fixture. preview read만 연결.
2. **A2:** shared home schema/helper·revision/reset/import 우선순위·throwaway PG fixture. production `/node`/import writer는 아직 연결하지 않음. migration 번호는 U03.
3. **A3:** 알림 renderer·K/nonce·실제 dedupe mapping·transport fixture. 운영 event/sender 연결 없음.

새 dormant helper는 test gate 또는 실호출자와 함께 두고 warning baseline/allow를 늘리지 않는다. 준비 조각의 preview·enqueue fixture는 운영 라우팅/배달 성공으로 세지 않는다.

| 최소 계약·현재 좌표 | 레인/합칠 순서 |
|---|---|
| canonical source와 register/begin: `src/db/replay_disposition/write/start.rs:8`, `:32`, `:135` | #7272 source owner의 필요한 조각 + #7336 P-B. #7330 전체 복구 재전송 활성화는 선행 아님 |
| 공통 raw 경계: `src/services/discord/router/intake_gate.rs:1117`, `src/services/discord/router/message_handler/intake_turn/worker_entry.rs:49`, `src/services/cluster/intake_worker.rs:434` | #7272 adapter/bootstrap/owner signature 고정→P-B 접합. Discord hook만 켜지 않으며 HTTP/routine도 같은 permit 소비 |
| Mac 간 custody: `src/services/turn_orchestrator/pending_queue_persistence.rs:92`, `src/services/cluster/intake_router_hook/owner_record.rs:542` | 로컬 queue는 shared가 아님. **#7336 P-B**가 PG staged/promotion·checkpoint ACK·Bundle pin 소유 |
| 보호 read와 output: `src/services/discord/router/intake_dispatch/replay_admission.rs:253`, `src/services/discord/turn_bridge/headless_delivery.rs:276` | #7330 read/처분 API와 origin stamp 타입만 고정→P-B/C 접합. routed 신규 TUI source producer는 반드시 연결 |
| normal 완료/다음 입력: `src/services/discord/turn_bridge/finalize_epilogue.rs:69` | #7272 observer 접점→P-C wake/reconciler. callback 하나에만 재개를 의존하지 않음 |
| owner REST/O: `src/services/discord/runtime_bootstrap/o_writer_host.rs:88`, `src/services/tui_o/writer/host.rs:505` | **#7336 P-C**가 production REST/owner gate 구현. 일반 bootstrap 정리 전체를 기다리지 않음 |

순서는 **P-A 즉시 → 최소 공통 source/effect 계약+P-B → P-C → P-D pilot → derived**다. #7415 착지는 giant 증가 조각 앞에서 확인한다. 겹치는 파일·상세 책임은 설계 §10/12의 표에 있다. 최신 FILES/공통 타입/미착지 조각 SHA는 코디네이터가 고정한다.

## [U]와 실측 방법

핵심 동작을 미정으로 남기지 않았다. 아래는 운영 값·호스트별 증거·착지 조정이다.

| ID | 미확정 | 실측/해소 |
|---|---|---|
| U01 | 실제 pair ID·shared PG 생존·REST/auth/workspace·local DB timeout | 승인된 health/build-info/capability 및 시험 환경으로 코디네이터 확인. 금지 설정은 읽지 않음. 양 방향 home 종료 뒤 다른 Mac의 PG/REST 실행과 L01 확인. PG가 home과 함께 사라지면 활성화 조건 미달 |
| U02 | 운영 legacy home 후보·충돌 | 기존 runtime의 이미 loaded node-ID 요약만 제출. 같은 값/한쪽 명시 없음/충돌/보고 누락 규칙 적용, 명시 `/node`로 해결. 파일/사본 scanner 없음 |
| U03 | migration 번호·최신 공통 API/FILES·실제 크기·#7415 착지 | 번호는 coordinator 배정. 구현 SHA로 공식 cap/raw 증가 측정, +1200 전 분할. 현재 cap 로그는 저장소 변경0의 근거일 뿐 예상 diff 검증 아님 |
| U04 | #7272 필요한 source/effect 조각의 최신 SHA | C1/C2/C5 API·caller census와 효과 직전 race fixture로 고정. 미착지면 P-A 진행/P-B 공통 계약 조각 제공 가능. 전체 이슈 대기하지 않음 |
| U05 | 양 Mac의 O source/binding·native normal end | R01/R05/R07/R15/R16을 Claude/Codex·Legacy/O와 지원하는 tmux/Herdr 경로에서 실행. 역방향 off로 완료를 주장하지 않음 |
| U06 | 외부 ACK/history 권한·전송 중 PG 소실의 잔여 한계 | mock timeout/ACK 유실/조회 결과와 승인 채널 실측. uncertain을 sent/PASS로 바꾸지 않음. 리뷰가 인정한 한계를 새 P1로 승격하지 않음 |
| U07 | catch-up/thread/첨부 PG→다른 Mac 수직 전달 | R11/R12/L02: 이전 runtime root 접근 불가 뒤 같은 source/body/bundle hash→target local file→provider 응답까지 관측. 미실행은 NOT_RUN |

## 리뷰 집중 항목

- **경계 시각:** home revision/owner generation·DB NOW 기반 고정 control deadline을 저장. Busy native의 턴 시간과 제어 timeout을 구분하며 m3로 기한을 리셋하지 않음.
- **동시성:** source begin·actual submit/output 모두 원 stamp 사용. owner CAS와 마지막 raw 효과는 같은 key로 직렬화. local transaction timeout은 사라진 client의 무한 lock을 방지하고 이미 wire에 간 결과는 uncertain으로 보존. old close callback은 exact native pin 비교.
- **옛 형식 복원:** shared home 우선·inherit tombstone·명시 명령 revision 우선. legacy import는 pending/conflict를 임의 승자로 풀지 않음. old output/위임 home_epoch를 새 execution generation으로 재표시하지 않음.
- **문서로만 선언한 가드:** non-gateway production REST, owner permit 기반 O start, PG stage→pending, routed register/begin을 실제 caller에서 증명해야 함. ctx=None 및 원 runtime queue 접근 불가 fixture 필수.
- **잘못된 0/PASS:** 같은 peer 알림0은 다음 source의 실행1과 함께 확인. enqueue/visible, normal-end/deferred, preview/live를 구분. R01~R20+L01/L02마다 RED 조건·뮤턴트가 있으며 이번에는 전부 미실행.
- **권한 범위:** main의 하위 local normal end만 재사용. `/clear` 상위 queue 삭제·새 peer kill/삭제·운영 설정 변경 없음. unavailable 처리의 old 미결 작업을 새 source로 rerun하지 않음.

## 증거 목록

- Worktree HEAD `fb2219535d889de858a3d9e1429d32aa6e31d309`를 유지하고 코드 변경/커밋0. source 근거는 리뷰와 같은 `c5477168cfdb2591aeebb77aa091d3fe13bca545`의 `git show` snapshot이다. `logs/r3-source-manifest.json`에 파일별 raw 줄 수/SHA256, `logs/r3-source-evidence.txt`에 좌표 발췌를 보관한다.
- 리뷰 원문 SHA256: `cca6bc5c44868dc48bb1c17f99e277186336996f2ffa70cc03bf640d2a9ae4d5`. 추출한 답은 `logs/r3-review-response.txt`, 최신 정의/대칭성 API read-back은 `logs/issue-7336-comments-r3.json`, #7415 조회는 `logs/issue-7415-r3.json`이다.
- `logs/r3-design-verification.json`에 clean status·좌표 범위·r1/r2 산출물 hash 보존·새 산출물 hash·진행 중 프로세스 유무를 기록한다. 좌표 범위 검사는 기능 시험 PASS가 아니다.
- 같은 clean HEAD의 기존 `logs/design-cap.txt`(HEAD 대 HEAD: prod0파일/순+0 CAP PASS)를 재사용한다. r3 예상 구현량의 cap 통과 근거로 사용하지 않는다.
- 시험 바이너리/SHA256: N/A. 실행 시험·뮤턴트·cargo/PG/clippy/prepush/source-scan: **NOT_RUN_DESIGN_ONLY**. PG 실제 실행0, cargo profile N/A. raw design 근거는 위 logs, 실행 환경 SHELL=/bin/bash; TMPDIR 값은 verification에 기록한다.
- 재개 시 기존 background job을 확인했으며 이어받을 cargo/검증 프로세스0, 이번 레그 background job 시작0. r1/r2 결과를 다시 돌리지 않았고 금지 운영 파일/DB는 읽지 않았다.

DESIGN_READY
