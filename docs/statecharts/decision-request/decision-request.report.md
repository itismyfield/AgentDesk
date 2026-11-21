# Decision request 모델 초안 — design-3 최종

코드 기준 1161162a18c87a0514f53dd7ae5dac3c830149cb.
최종 설계 요지: [decision-forum.design.md](../../decision-forum.design.md).
이번 formal 변경·실행 구간: 2026-11-17 10:50~11:13 KST(UTC 01:50~02:13), 30분 이내 종료.
그 뒤에는 설계·코드 좌표·추정·보고서만 정리했다. 코드/cargo/PG/운영 쓰기 없음.

DecisionRequest.tla의 개정2 single input workflow와 DecisionForum.tla는 바꾸지 않았다.
이 모델/cfg 20개의 SHA256 바이트가 개정 2 사본과 같아 완료 결과를 재사용했다.
새 DecisionDiscussion.tla/cfg 15개가 의견/메시지 ID/선택 경합/입력 close fence/requester reply·close/
scan cursor/수명 flags를 다룬다. 저장소 기대값은 [expected.tsv](../expected.tsv)의 decision-request 행이다.

새 정상 3개:
- DiscussionFixed: 2,318,891 생성 / 487,184 고유 상태, 안전 규칙 13+진행성 2 PASS.
- DiscussionMixed: 173,595 고유 상태, 의견+텍스트 선택+버튼 PASS.
- DiscussionForeign: 79,629 고유 상태, 두 번째 작성자 비운영자일 때 claims/writes/choice 0 PASS.

기대 반례 12개:
- DiscussionDuplicate: terminal source 재관측 때 원장을 무시하여 같은 의견 effect2.
- DiscussionClosedAdmission: 닫힘 뒤 새 message를 accepted로 기록.
- DiscussionClosedWrite: 닫힘 fence와 원본 종결을 지키지 않는 writer 재개.
- DiscussionRaw: source body를 trim/변형해 raw forwarding 불일치.
- DiscussionEarlyNotice: effect 증거 전에 “의견 전달됨”.
- DiscussionOpinionClose: 의견만으로 글 닫음.
- DiscussionScope: 다른 agent의 requester action 수락.
- DiscussionReplyState: 닫힌 글의 reply가 논리 Open을 만듦.
- DiscussionCursor: 높은 gateway ID로 cursor를 올려 낮은 source 누락.
- DiscussionClassify: opinion 상징 kind를 choice로 오분류.
- DiscussionNoArchive: ClosedEventuallyArchived 단일 PROPERTY의 temporal CE.
- DiscussionDisplay: PG lifecycle과 Discord 표시의 즉시 일치 CE.

재사용 20개: 정상4+의도한CE16. 새15: 정상3+의도한CE12.
총35: 정상7+CE28, 모두 기대 판정 일치. 미해결 제품 결함 28개라는 뜻이 아니다.
Safety exit12/temporal exit13을 구별하며 parser error와 불완전 trace를 결과로 세지 않았다.
초안 비ASCII TLA 문자열은 lexer가 거부하여 ASCII 상징 raw 값으로 바꾸고 최종 모델을 검사했다.
실제 한글/emoji/CR/LF/기존 sanitizer 문구는 구현 fixture·TUI payload 비교 대상이다.

규칙:
NoDuplicateOpinion / OperatorOnly / ChoiceOne / NoClosedAdmission / NoClosedWrites /
OpinionRawPreserved / OpinionKeepsOpen / OpinionNoticeTruth / RequesterOnly /
ReplyNoLifecycleChange / CursorCoversAll / ClassMatches / ViewTruth.
진행성은 ClosedEventuallyArchived와 OpinionEventuallyAccounted.
Spec에서 실제 WF(Boot), WF(TurnBoundary), WF(Progress), WF(Repair)를 적용한다.

범위와 추상화:
- 한 요청, 답글2개와 버튼1개, 한 번의 재시작, 제한된 requester reply/close.
- 숫자/이름 parser는 BaseKind로 추상화한다. 실제 정규식·label uniqueness는 설계 분류표와 구현 시험으로 확인.
- source 원장과 cursor는 두 synthetic message ID의 complete-prefix로 추상화한다.
  실제 Discord pagination/권한/SQL 격리는 별도 시험이다.
- requester actor는 cap 검증을 마친 추상 역할이다. 모델 PASS가 실제 token authorization 코드의 증명은 아니다.
- Close 정산은 R2 closing/finalize 프로토콜을 collapse한 것이다. PG/queue 원자 transaction을 주장하지 않는다.
- R2는 승인 event당 1 workflow로 사용하며, selection은 request당1개, opinion은 source마다1개다.
  실제 event별 다중 인스턴스/전체 FIFO 구현은 별도 통합 시험이 필요하다.
- NoClosedWrites는 닫힘 뒤 새 writer 시작 금지다. 이미 나간 원격 write의 늦은 적용을 kill/undo하지 않는다.
- terminal 실패 원본은 재스캔으로 다시 입력하지 않는다. 정상 busy original만 같은 ticket으로 이어간다.
- 원문 보존의 기존 장애 좌표(API/headless trim, human_frame newline 정규화, ai_screen sanitizer,
  non-owner sanitizer, receipt frame 재구성)는 설계 요지의 진입점 E57~E61과 P2-2 행에 있다.
- 입력 실패는 정상화 뒤 사유 표시·폐기, 선택/ID/hash/결과 사실은 남긴다.
- 논리 close와 물리 archive/lock을 나눴다. requester reply는 잠시 editable이어도 gate를 재오픈하지 않는다.
- 영구 PG/Discord/권한 장애·무한 busy/무한 crash 진행성은 보장하지 않는다.
- 같은 body를 쓴 다른 message ID는 별도 의견이다. 같은 ID edit는 새 입력이 아니다.

실행 환경 Bash, Java21.0.13, TLC1.7.4, workers2, Xmx2g.
jar SHA256: 936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88.
모델 폴더에서:
    java -XX:+UseParallelGC -Xmx2g -cp <jar> tlc2.TLC -workers 2 -config <Discussion*.cfg> -metadir <tmp> DecisionDiscussion.tla
CI와 같은 기대값 확인은 `python3 scripts/ci/statecharts_tlc.py --jar <tla2tools.jar> --base origin/main`이다.
.chart.mmd는 수작업 요약이며 전체 상태 공간의 자동 그림이 아니다.

## 조각 2a — event 예약·첫 시도 (코드 좌표)

DecisionRequest 모델의 아래 전이가 코드에 들어왔다. 좌표는 이 조각의 head 기준이다.
값 범위는 설계 요지의 진입점 전수표(E03·E04·E10~E14·E46·E47·E67~E70)에서만 가져왔다.
producer가 없어 운영 경로에서는 실행되지 않는다(`agentdesk::decision_input`로만 내보냄).

| 모델 전이 | 코드 | 메모 |
|---|---|---|
| Claim | `src/db/decisions.rs:130` reserve_first_attempt, 호출 `src/services/discord/health/turn_deliver/decision_target.rs:190` | not_started→resolving UPDATE 승자만 진행. 패자는 AlreadyReserved |
| ResolvePresent(busy→queued) | `decision_target.rs:103`(owner가 원본 수용), `:154`(mailbox enqueue) | normal_wait, 원본 1개 |
| ResolvePresent(idle→prepared) → BeginSend | `decision_target.rs:127` | non-owner 시작은 호출 안에서 쓰므로 may_have_sent를 먼저 기록 |
| Observe(같은 시도의 증거) | `decision_target.rs:132` | 시작이 첫 prompt로 전달 → delivered. 두 번째 deliver 없음 |
| QueueRefused → RefusalNormalize | `decision_target.rs:160`, `:208` | enqueue 거절은 미전송 확정 |
| NormalizeAbsent | `decision_target.rs:204`, `:208` | 채널을 가진 runtime 없음 → resolving→normalizing. PG가 두 번째 정상화를 거절(`src/db/decisions.rs:148`) |
| NormalizeSuccess | `decision_target.rs:218` | normalizing→resolving, 다시 현재 대상 해석 |
| NormalizeSuccess(첫 prompt 전달) | `decision_target.rs:212` | delivered로 끝, 뒤의 start 0 |
| NormalizeFailure | `decision_target.rs:223` | failed(사유) |
| AbsentAfterNormalize·RefusalFinal | `decision_target.rs:208` | 정상화를 이미 썼으면 failed |
| Uncertain(정상화 결과 불명) | `decision_target.rs:224` | may_have_sent로 남기고 재전송 0. 닫기는 2b |
| HandoffDecision(대조) | `src/services/discord/queue_io/decision_drain.rs:22`, 호출 `src/services/discord/agent_handoff.rs:541`·`src/server/routes/agents.rs:1080` | service·HTTP 모두 backend 전에 거절 |
| SelfDeclared(대조, 신규) | `src/services/discord/queue_io/decision_drain.rs:7`, 호출 `src/server/routes/agents.rs:675`·`src/server/routes/agents_turn_deliver.rs:161`; 보존 planner 입구 `src/services/tui_input/human_frame.rs:120` | 본문이 source=decision을 자기 선언하면 400. 보존 frame은 승인 event가 그 바이트를 승인했을 때만 만들어짐 |

모델에 없는 종료(코드가 더 보수적인 곳): owner 수용의 pending/거절과 owner 시작 오류는 미전송 증거로 쓰지 않고
정상화 없이 failed로 닫는다(`decision_target.rs:116`). InvalidTarget도 정상화 없이 failed다.

새 규칙·대조:
- `PreservedNeedsApproval`(Fixed.cfg에 추가): 승인 없는 입력은 보존 planner에 들어가지 못한다. 대조 `Provenance.cfg`(Mode=BadProvenance)가 이 규칙을 깬다.
- `NoClaimCas.cfg`: 예약 UPDATE의 상태 조건을 뺀 대조. `AttemptOnce`를 깬다.

범위 밖 — 조각 4 활성 조건에 2b 착지: DrainCurrent(대기 뒤 owner·non-owner 재해석), Finalize(원본 제거),
CloseUnknown·CloseAbandoned, Crash/Boot 복구, CancelClear, WakeFailed 거절. 2a는 normal_wait·may_have_sent 뒤를 정산하지 않는다.
