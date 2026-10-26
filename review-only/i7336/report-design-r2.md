DESIGN_READY r2 main=fb2219535d889de858a3d9e1429d32aa6e31d309 code_changes=0 commits=0

# #7336 설계 r2 보고

설계: [design-r2.md](/home/adk/adk/lane-runs/i7336/design-r2.md)

설계 좌표는 고정 worktree SHA다. 작성 중 origin/main이 `c5477168cfdb2591aeebb77aa091d3fe13bca545`(#7416/#7390)로 전진했다. 공유 intake_dispatch의 ConsentHeld 오류 타입 변경(String→Refusal)을 읽었으며 최신 인터페이스 재고정은 [U02/U04]에 포함한다. merge/reset은 하지 않았다. #7415는 종료 전 재조회에서도 open이었다.

## 요약

[12:2x 최신 사용자 정의](https://github.com/itismyfield/AgentDesk/issues/7336#issuecomment-6407581059)를 확인해 r2를 작성했다. r1의 전원 증명·정상 종료/maintenance 통지·노드 전체 quiesced ACK·대부분의 return barrier·`/node-return`·신규 세션 namespace는 폐기했다. r1 문서는 이력으로만 남기고 고치지 않았다. **power witness 가용성은 더 이상 [U]나 인수 선행 조건이 아니다.**

권고는 **입력마다 home 수신 가능 여부를 기존 heartbeat/readiness로 판정하고, 받을 수 없으면 현 gateway Mac으로 보내는 것**이다. 다음 새 입력에서는 home을 다시 본다. 받은 Mac의 local 세션이 있으면 사용하고 없으면 fresh로 만든다. 다른 Mac에서 home의 세션 ID를 resume하지 않는다. 기존 세션 키가 hostname을 포함하므로 새 namespace를 만들 필요가 없다.

같은 입력은 기존 source idempotency와 intake owner CAS/stamp로 한 실행자에 고정한다. routing 변경을 source retry 허가로 삼지 않는다. home이 살아 있는데 연결만 끊긴 경우에도 B를 선택할 수 있으며 알림은 “연결할 수 없어”라고 쓴다. old generation의 submit/응답은 기존 owner key를 소비하는 작은 효과 adapter로 막아야 한다. CAS만으로 이미 시작한 provider 효과가 없어졌다고 하지 않는다.

B 작업 중 home 복귀와 새 입력이 겹치면 새 입력은 기존 durable 보관소에서 기다린다. B의 현재 native 작업·답을 마치고 **기존 정상 session-end**를 확인한 뒤 home으로 보낸다. 상위 `/clear`는 큐를 비우고 transition timeout 뒤 진행하는 동작이 있어 재사용하지 않는다. 세션 종료 하위 경로만 exact local 대상에 연결하며 새 kill·삭제 권한은 없다.

전환 알림은 두 종류다.

- home→B: `맥북에 연결할 수 없어 이 채널의 입력을 맥미니에서 처리합니다.`
- B→home: `맥북 연결이 돌아와 이 채널의 입력을 다시 맥북에서 처리합니다.`

같은 B로 계속 보내거나 native 세션만 재생성하면 추가 알림0이다. routing owner generation이 바뀐 tx에 알림 outbox 한 건을 묶고, owner 행의 영속 배달 포인터·같은 nonce·기존 history reconcile로 재시작 중복을 막는다. 일반 outbox TTL dedup만으로 1회 보장을 주장하지 않는다. POST 결과 불명은 재POST하지 않고 uncertain이며 visible 배달 완료로 세지 않는다.

## 켜는 범위

**채널별 on, 기본 off**를 권고한다. 기존 `cluster.intake_routing` 아래 제안 키는 `input_fallback_enabled: false`와 `input_fallback_channel_ids: []`다. 앞의 키가 전체 kill switch다. 별도 PG 정책 테이블/새 UI/API는 만들지 않는다. home은 기존 `/node` 명시값→agent default이며 fallback owner로 덮어쓰지 않는다.

kill switch off는 새 fallback 선택을 중단하지만 begun 입력·기존 owner fence·정상 종료·미배달 전환 알림은 마무리한다. 즉시 A를 열거나 B 작업/대기 입력을 지우지 않는다. 운영 설정을 열거나 수정한 것은 없다.

## 크기·구현 단위

| 구분 | r1 | r2 |
|---|---:|---:|
| 고유 prod 파일 추정 | 38~50 | **26~32** |
| 순 prod code 추정 | +2,700~3,900 | **+1,350~1,950** |
| 리뷰 단위 | 4 | **2** |
| 시험 raw 추정 | +1,200~2,000 | +700~1,200 |

전원/lifecycle producer·새 정책 UI·명시 반환·새 namespace·큰 return 상태를 빼 코드량과 리뷰 단위를 약 절반으로 줄였다. 남는 것은 source/owner 결합, 정상 세션 종료 호출, O 고정 home 접합, 재시작 알림 배달이다. 이 부분까지 빼고 +400~900으로 선언하지 않는다.

- **P1:** 입력별 routing·canonical source/owner 결합·local 세션 선택·효과 stamp. 14~19 prod 파일, +700~1,000.
- **P2:** 정상 복귀·O/gateway 연결·전환 알림·최종 on. 15~21 prod 파일, +650~950.

파일이 단위 사이에 재등장하므로 행 파일 수 합계는 고유 수가 아니다. 리뷰 상한은 각 prod 30파일/순 +1500이며 +1200 예상 전에 PLAN_READY로 다시 나눈다. 실제 공식 cap은 구현 SHA에서 잰다. P1만으로 기능을 켜지 않는다. 실제 이번 변경은 **저장소 0파일/+0, 커밋0**이다.

#7415가 머지되면 giant 줄 수로 차단하지 않는다. 조사 시 이슈는 open이고 현재 worktree는 옛 검사다. 증가 예상은 설계 §9.2에 적었다. 주요 giant는 intake_router_hook.rs(2773, +20~40), session_runtime.rs(1182, +10~20), outbound/delivery.rs(1481, +8~20), gateway.rs(1562, 필요시 +8~20)다. owner_record.rs(2398)·message_outbox.rs(1355)·server/mod.rs(3032)는 기본 재사용하며 필드/API 연결이 필요하면 작은 증가를 실제 계수한다. 줄 맞춤 child 이동은 없다. r1의 node_registry/args/shutdown/restart_report 변경은 제거했다.

## 재사용·현재 main의 공백

모두 `fb2219535d889de858a3d9e1429d32aa6e31d309` 기준이다. 구성 요소만 이미 있고 새 수용 조건 전체가 완료된 항목은 없으므로 ALREADY_DONE은 아니다.

| 구성 요소 | 좌표 | 재사용/필요 변경 |
|---|---|---|
| heartbeat offline | `src/services/cluster/node_registry.rs:737` | 새 전원 판정 대신 입력 availability 판단에 재사용 |
| owner CAS·source-independent idempotency | `src/services/cluster/intake_router_hook/owner_record.rs:76`, `:407`, `:542` | acquire/admission 생성은 dormant이므로 실제 입력 연결 필요 |
| stamped worker fence | `src/db/intake_outbox/worker_status.rs:55`, `src/services/cluster/intake_worker.rs:434` | local/즉시 주입도 같은 stamp 소비 |
| host별 세션 키 | `src/services/discord/adk_session.rs:118` | local 키/locator 대조. 새 namespace 없음 |
| 정상 session-end | `src/services/discord/commands/control/clear_teardown.rs:57` | exact idle B session만 종료. 상위 clear/큐 삭제는 호출하지 않음 |
| 기존 종료의 실제 권한 | `src/services/discord/commands/control/managed_reset.rs:71` | 기존 process/tmux 종료·launch 파일 정리 경로만 재사용. 새 대상/시그널/삭제 fallback 없음 |
| O foreign home | `src/services/tui_o/cutover.rs:143` | routing permit에 한해 새 B local source 허용, peer store 채택0 |
| preferred gateway 양보 | `src/services/discord/runtime_bootstrap/gateway_lease.rs:589` | B 작업/답·정상 세션 종료 전 voluntary cancel 방지 |
| tx outbox/nonce 복구 | `src/services/message_outbox.rs:448`, `src/services/discord/task_notification_delivery/card_post.rs:29` | 세대별 배달 포인터와 routing 전용 nonce 소비점 연결 |
| 첨부 BundleV1 | `src/services/cluster/attachment_transfer.rs:18` | 기존 byte/hash 전달 유지. 실제 첨부 수용은 별도 실측 |

## #7272·#7330 겹침과 순서

설계 §9.3에 file:line별 표가 있다. 입력 단일 경로와 공유하는 것은 intake_gate/busy_inject, intake_dispatch, worker_entry, turn_start/session_runtime, finalize_epilogue 및 O bootstrap이다. 재전송 활성화와 공유하는 것은 source preflight, worker claim, stale-resume/withheld, headless/outbox 답의 처분 경계다.

권고 순서는 **#7415 + #7272 공통 입력 계약/공유 파일 착지 → #7330 최신 보호 API/활성 조합 확정 → P1 → P2 → 실측/on**이다. 두 이슈 전체를 새로 해결하라는 조건은 아니다. 다만 forwarded/host 조합을 #7336이 독자 활성화하거나 옛 branch마다 별도 라우터를 추가하지 않는다. 코디네이터가 최신 SHA/FILES를 고정한다.

## [U] 목록과 실측

| ID | 미확정 | 확인 방법 |
|---|---|---|
| U01 | 실제 home/서로 다른 host key·PG 생존/gateway/auth/workspace | 금지 설정을 열지 않고 승인된 health/build-info/capability·시험 환경으로 코디네이터 확인. L01로 A 종료 뒤 B의 동일 PG/bundle 접근 관측 |
| U02 | #7272 최종 공통 permit/held 입력/native 완료 API | 최신 SHA·caller census와 busy 복귀·old generation·next dequeue recording fixture. source 집합을 복제한 반환 원장 없음 |
| U03 | #7330 source 처분·local/forwarded 활성 조합 | 최신 보호 API/시험 확보, old open route/같은 source cross-node 재실행0 검증. unsupported producer 독자 활성화 금지 |
| U04 | host별 정상 close 결과·O home/gateway 호환 | Claude/Codex tmux/Herdr의 exact target·close 거절·crash 재확인·O 복귀 fixture. 미확인 조합은 on 금지 |
| U05 | notice ACK/history와 이미 전송 중인 HTTP·PG 연결 동시 소실 | fake transport의 ACK 유실/nonce 창/페이지 절단부터 검증, 승인 채널에서 같은 nonce의 message ID 복구. uncertain은 sent/PASS로 세지 않음 |
| U06 | migration 슬롯·실제 크기·#7415 착지 | 구현 SHA의 공식 cap/giant·raw 줄 수, coordinator FILES/번호 배정 |
| U07 | gateway 공백/attachment-only/thread catch-up | source ID→bundle hash→받은 Mac 파일→provider 읽기의 L02 실측 |

r1의 “급작스러운 전원 차단 증거원”과 “maintenance 인수 정책”은 폐기해 이 목록에 없다.

## 수용 시험

설계 §8에 T01~T18와 L01/L02를 정의했고 각각 RED 조건·뮤턴트를 적었다. 중심 시험은 다음과 같다.

- home 없음→B 세션 없으면 fresh1·응답1·알림1, 다른 Mac에서 home resume0.
- 계속 없음→m2/m3도 B에서 처리, 세션 재사용, 추가 알림0.
- 복귀→B 정상 session-end 후 A 실행, 반환 알림1.
- busy 복귀→B 작업/답 완료 전 A 실행0, 새 입력 보존, 두 Mac 응답 overlap0.
- 같은 입력이 live/catch-up/worker·재시작·owner 변경을 거쳐도 실행1, 다른 node/새 attempt로 rerun0.
- 첨부-only/본문+첨부·held 복귀·remote에서 byte/hash 일치, 누락 시 text-only 실행0.
- 알림 tx/POST ACK 유실·TTL 경과·B→A→B에서 이벤트/nonce/visible message를 각각 계수.

이번에는 코드와 시험을 추가하지 않았으므로 RED/GREEN/뮤턴트·cargo·PG·clippy·prepush 실행은 0이다. 이 목록은 시험 결과가 아니다.

## 리뷰 집중 항목

| 항목 | 막는 방법 / 남은 반례 |
|---|---|
| 경계 시각 | 입력 직전 DB heartbeat/readiness·held 재개 직전 재검사. TTL 경계 fixture. 전원/새 return cutoff 판정 없음 |
| 동시성 | source/attempt와 owner generation을 분리해 재사용. first submit/send까지 guard 유지, B 완료·정상 close 전 home 입력0. old callback은 exact generation/session 비교 |
| 옛 형식 복원 | hostname 포함 기존 local/legacy key 사용, peer ID resume0. NULL stamp/old delayed output을 current 권한으로 추정하지 않음 |
| 문서뿐인 가드 | dormant stamped admission·test-only writer join·headless 한정 nonce를 실제 production 연결해야 함. CAS/helper가 있다는 이유로 완료 아님 |
| 잘못된 0/PASS | 계속 B 알림0은 실제 다음 입력 실행과 짝지음. outbox enqueue1≠visible 알림1. unknown history/ACK·PG/live 미실행은 완료0 |
| 정상 종료 범위 | 상위 clear의 queue discard/timeout-overtake 제외. 기존 exact local end만 사용. 새 kill·삭제·session 전체 스캔 없음 |
| 잔여 불확실성 | wire에 나간 HTTP+PG 연결 동시 소실을 PG lock이 취소한다고 주장하지 않음. 기존 uncertain transport fixture로 관찰하며 전원/전체 quiesced 설계를 되살리지 않음 |

## 증거 목록

- 최신 댓글 snapshot: [issue-7336-comments-r2.json](/home/adk/adk/lane-runs/i7336/logs/issue-7336-comments-r2.json).
- #7415 조사 snapshot: [issue-7415-r2.json](/home/adk/adk/lane-runs/i7336/logs/issue-7415-r2.json).
- 재개 process 확인: [r2-resume-audit.json](/home/adk/adk/lane-runs/i7336/logs/r2-resume-audit.json). 이전 레그가 남긴 진행 job0, 새 cargo/검증 job을 띄우지 않음.
- r2 source 좌표/발췌·SHA256: [r2-source-evidence.txt](/home/adk/adk/lane-runs/i7336/logs/r2-source-evidence.txt).
- 변경 후보 raw 줄 수: [r2-candidate-line-counts.json](/home/adk/adk/lane-runs/i7336/logs/r2-candidate-line-counts.json).
- HEAD/clean·좌표 범위·산출 SHA256: [r2-design-verification.json](/home/adk/adk/lane-runs/i7336/logs/r2-design-verification.json).
- 실제 저장소 diff cap은 동일 HEAD·clean의 r1 [design-cap.txt](/home/adk/adk/lane-runs/i7336/logs/design-cap.txt)를 재사용: prod0/+0. 구현 예상 크기의 cap PASS로 사용하지 않음.
- 환경: WSL lane `/home/adk/adk/wt/i7336d`, bash. PG 실제 실행0, cargo profile/TMPDIR 시험 환경·시험 바이너리/SHA256·실행/실패/skip 수 해당 없음. REVIEW_READY 보고가 아니다.

금지 운영 설정/사본·launchd.env를 읽거나 검색하지 않았다. 운영 서버/DB/runtime/tmux/herdr·GitHub·DAG 쓰기0, 저장소 수정·커밋0이다. 새 산출물은 design-r2.md·report-design-r2.md와 r2 로그뿐이다. 코디네이터가 [U]와 공유 인터페이스/FILES를 고정한 뒤 독립 설계 리뷰와 구현 두 단위로 인계한다.

DESIGN_READY
