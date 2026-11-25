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
| HandoffDecision(대조) | `src/services/discord/queue_io/decision_drain.rs:22`, 호출 `src/services/discord/agent_handoff.rs:541`·`src/server/routes/agents.rs:1081` | service·HTTP 모두 backend 전에 거절 |
| SelfDeclared(대조, 신규) | `src/services/discord/queue_io/decision_drain.rs:7`, 호출 `src/server/routes/agents.rs:676`·`src/server/routes/agents_turn_deliver.rs:161`; 보존 planner 입구 `src/services/tui_input/human_frame.rs:120` | 본문이 source=decision을 자기 선언하면 400. 보존 frame은 승인 event가 그 바이트를 승인했을 때만 만들어짐 |

모델에 없는 종료(코드가 더 보수적인 곳): owner 수용의 pending/거절과 owner 시작 오류는 미전송 증거로 쓰지 않고
정상화 없이 failed로 닫는다(`decision_target.rs:116`). InvalidTarget도 정상화 없이 failed다.

새 규칙·대조:
- `PreservedNeedsApproval`(Fixed.cfg에 추가): 승인 없는 입력은 보존 planner에 들어가지 못한다. 대조 `Provenance.cfg`(Mode=BadProvenance)가 이 규칙을 깬다.
- `NoClaimCas.cfg`: 예약 UPDATE의 상태 조건을 뺀 대조. `AttemptOnce`를 깬다.

2a 리뷰 반영: Observe 좌표는 2b에서 바뀌었다. 시작 응답(`Started`/`Accepted`)은 전달 증거가 아니라
`may_have_sent`에 turn만 기록한다(`decision_target.rs:129`, `:151`, `:231`). Observe는 아래 2b 절의 receipt 정산이다.

## 조각 2b — 정산·실패 폐기·부팅 복구 (코드 좌표)

좌표는 조각 2b head 기준이다. 정산은 `src/services/discord/queue_io/decision_drain.rs`의 `settle`(:157) 한 함수가 맡고,
owner의 쓰기·drain 안내·재시작 requeue·부팅이 모두 그 함수를 부른다. 여전히 producer는 없다.

| 모델 전이·변수 | 코드 | 메모 |
|---|---|---|
| Journal(신규, `journal`) | `src/db/decisions.rs:199` begin_write, 호출 `decision_drain.rs:236` hand_idle | owner의 idle 쓰기 직전 resolving·normal_wait→may_have_sent. 닫혔거나 이미 journal이 있으면 쓰기 0 |
| DrainCurrent → BeginSend | `owner_input.rs:244`, `:291` → `decision_drain.rs:212` hand_idle | 대기 뒤 owner drive가 그 시점 현재 세션으로 idle 시작. Decision은 busy 합류 0(아래 P2-1) |
| ReleaseHeld(신규, `holds`) | `decision_drain.rs:187` | 입력창 사용 중(NotSent 증명) → may_have_sent→normal_wait, 같은 원본·ticket이 queue에 남음 |
| Observe | `decision_drain.rs:178`(쓰기 Ok, owner settled, completed receipt) | 같은 origin의 receipt만 전달 증거. 시작 응답은 아님 |
| CloseUnknown | `decision_drain.rs:197` | 미확인 쓰기·이전 process의 journal·미완료 receipt → unknown_closed, 원본 안내 0 |
| CloseAbandoned / Crash+Boot(resolving·normalizing) | `decision_drain.rs:177`, 부팅 `:278` recover_before·`:298` recover_boot·`:310` spawn, 호출 `runtime_bootstrap/spawns.rs:216` | 이전 process가 남긴 resolving·normalizing → `interrupted_before_delivery` |
| RefusalFinal·NormalizeFailure·AbsentAfterNormalize의 실행권 종결 → Finalize | `decision_drain.rs:118` fail: `src/db/decisions.rs:212` close(fence) → `queued_head/v2/decision.rs:56` discard_decision → closing→failed | fence 먼저, 정확한 원본만 제거, 그 원본의 안내 의무를 지움. fence 뒤 crash는 `decision_drain.rs:176`(Closing)이 부팅에서 마무리 |
| OwnerPending(신규) | `decision_target.rs:103` close_admitted(:133, :135) → `decision_drain.rs:138` | owner 수용 응답이 pending/거절이면 fence 후 그 원본 제거. 2a의 "failed만 기록"을 대체 |
| NoFailureQueue 유지(재시작 requeue·안내) | `input_owner/recovery.rs:21`, `input_owner/notice.rs:15`, `:116` → settle_channel `decision_drain.rs:264` | requeue_known_not_sent·안내 drain 전에 실패 원본 제거 |
| WakeFailed(대조) | `decision_drain.rs:236` begin_write 거절 → 원본 제거, 쓰기 0 | 닫힌 event는 wake·재시작 뒤에도 쓰지 않음 |
| CancelClear | 기존 owner cancel·clear 경로(`v2/control.rs:192`, `:365`) | 원본 제거는 기존 경로. 그 뒤 settle이 `Held::Gone`으로 normal_wait를 `original_removed` failed로 닫음(`decision_drain.rs:184`) |
| PreserveApproved(신규, P2-4) | `src/services/tui_input/human_frame.rs:120` preserved_decision_body | 승인된 opinion의 같은 바이트일 때만 보존 frame |

P2-4 수정(정상 보존 경로):
- `preserved`를 `"none" | "approved" | "unapproved" | "altered"`로 바꾸고 정상 행동 `PreserveApproved`를 더했다.
  `PreservedNeedsApproval`은 `preserved # "none" => accepted /\ preserved = "approved"`.
- 도달성 대조 `PreservedReach.cfg`(Mode=Fixed, `NeverPreserved == preserved = "none"`)는 정상 경로가 있을 때만 깨진다.
- 승인·본문 검사 제거 대조 `PreserveCheck.cfg`(Mode=BadPreserveCheck)는 `PreservedNeedsApproval`을 깬다.

| cfg | 수정 전(2a 모델 + `NeverPreserved == ~preserved`) | 수정 후 |
|---|---|---|
| PreservedReach | **반례 없음(rc=0, 24,546 상태)** — 정상 보존 경로가 없어 Fixed PASS가 공허했음 | `NeverPreserved` 반례(rc=12) — 정상 보존 도달 |
| PreserveCheck | (2a에 없음) | `PreservedNeedsApproval` 반례(rc=12) |
| Provenance | `PreservedNeedsApproval` 반례 | 같음 |
| Fixed | PASS | PASS(`PreservedNeedsApproval`·`JournalBeforeWrite` 포함, 119,114 고유 상태) |

2b 신규 대조: `NoJournal.cfg`(journal 없이 BeginSend → `JournalBeforeWrite`), `PendingLeft.cfg`(owner pending을
원본 제거 없이 failed로 닫음 → `NoFailureQueue`). TLC: `PASS: 22/22 configs`.

모델과 코드가 다른 곳: 코드는 시작 응답을 Observe로 쓰지 않으므로 non-owner·정상화 시작은 receipt가 올 때까지
`may_have_sent`다(producer 활성 전까지 이 receipt 조회는 owner drain·부팅 때만 돈다). 원본 식별은 event의 안정
message id와 source origin_id로 하며, 그 밖의 id로 들어간 원본은 찾지 못해 `Gone`으로 읽는다(운영 진입은 `start::original`의 안정 id만 씀).

## 조각 2c — 실행권 holder·종결 뒤 정리·non-owner 큐 (코드 좌표)

좌표는 조각 2c head 기준이다. producer는 여전히 없다.

| 모델 전이·변수 | 코드 | 메모 |
|---|---|---|
| PeerRecover(신규, `tookLive`) · CloseUnknown · CloseAbandoned의 종결 조건 | `src/db/decisions.rs` `end_abandoned`(:265), 판정 `unsettled`(:319), holder 기록 `holder`(:158)·`Run`(:164). 호출 `src/services/discord/queue_io/decision_drain.rs` `settle_as`(:239)의 `row.stale` 분기 | 행을 옮길 때마다 holder(worker_nodes instance id)를 적는다. 종결 UPDATE가 "같은 노드가 이번 실행 시작 전에 옮긴 행" 또는 "holder 노드가 online이 아님"을 다시 확인한다. 시각만으로는 닫지 않는다 |
| Crash/Boot의 실행 경계 | `decision_drain.rs` `run`(:54). 묻는 곳: `recover_boot`(:450), `hand_idle`(:307), `decision_target.rs:208`(예약 전), `decision_drain/queued.rs` `begin`(:35) | 실행 시작 시각은 먼저 묻는 쪽이 한 번 고정한다. 이 process가 옮긴 행은 그 뒤 시각이라 자기 부팅 복구가 닫지 않는다 |
| `kept`(신규) · CloseUnknown·Finalize의 `done` 선택 | `decision_drain.rs` `discard`(:136) — owner가 없거나 snapshot commit이 실패하면 원본이 남는다 | PG 종결(실행권)과 owner 원본 제거(정리)는 두 단계다 |
| Sweep(신규) | `decision_drain.rs` `settle_channel`(:386) → `queued_head/v2/decision.rs` `decision_tickets`(:56) → `src/db/decisions.rs` `closed_under`(:358) → `discard_decision`(:70) | owner가 아직 든 Decision ticket을 종결 event와 대조해 requeue·안내 전에 그 원본만 지운다. ticket이 없고 승인 event도 만난 적 없으면 PG를 읽지 않는다 |
| Notice(대조) · OtherNotice | `input_owner/notice.rs` `owed`의 `withheld`(:44), 호출 `:16`·`:128` | 정리가 실패한 Decision 원본의 안내만 보류한다. 다른 입력의 안내는 그대로 나간다 |
| Finalize의 원본 제거(P2-3) | `queued_head/v2/decision.rs` `discard_decision`(:70)의 `gone`(:85) | Unconfirmed·시작 거부·영구 거부로 보관된 Decision 원본을 본문째 지우고 그 시도의 bind를 닫는다. 다른 입력이 섞인 unit과 물리 작업 중인 head는 그대로 둔다 |
| Journal의 승인 대조(P2-4) · SelfDeclared 대조 | `decision_drain.rs` `claim_of`(:89), `hand_idle`의 `load_event` 대조(:329), `refuse_unapproved`(:108) | origin·channel·actor·본문이 승인 event와 같을 때만 journal. 근거 없는 Decision 주장은 쓰기 0, event 상태 불변 |
| DrainCurrent(non-owner) → Journal | `router/intake_dispatch/queued.rs:109` → `decision_drain/queued.rs` `begin`(:35) | 수용 경계에서 대조하고 journal을 잡는다. 닫힌·미승인 원본은 실행하지 않고 큐에 돌려놓지 않는다(`ClosedDecision`). 재시작 drain도 같은 경계를 지난다 |
| ReleaseHeld(non-owner) | `queued.rs:138`·`:201`·`:252`(수용 단계 복귀), `:428`(시작 뒤 큐 복귀) → `decision_drain/queued.rs` `end`(:85) | 아무것도 쓰지 않고 큐로 돌아간 원본만 may_have_sent→normal_wait |
| CancelClear · CloseUnknown(non-owner) | `queued.rs:429`·`:430` → `end`(:85). kickoff의 재투입 생략 `queue_dispatch/kickoff.rs:288` | clear는 failed(`cleared`), journal 뒤 오류는 unknown_closed(`queue_start_failed`). 실패한 Decision 시작은 큐에 다시 넣지 않는다 |
| Observe(non-owner) | `queue_dispatch/kickoff.rs:131` → `decision_drain/queued.rs` `settle_started`(:107) | owner 없는 채널은 kickoff의 턴 경계에서 그 턴의 receipt로 정산한다 |
| ResolvePresent(busy) 유지 | `queue_io/queued_head_inject.rs:124` | Decision head는 busy pane 붙여넣기 대상에서 빠진다 |

수정 전 RED / 후 GREEN(2c 모델, TLC 1.7.4):

| cfg | Mode(수정 전 동작) | 수정 전 | 수정 후(Fixed) |
|---|---|---|---|
| TimeOnlyRecovery | 시각만으로 부팅 복구(P2-1) | `LiveHolderKeeps` 반례: holder가 살아 있는데 다른 노드가 종결 | PASS |
| NoSweepNotice | 종결 뒤 남은 원본을 다시 찾지 않음(P2-2) | `NoDecisionNotice` 반례: 종결된 Decision의 안내가 나감 | PASS |
| NoSweep | 같은 Mode의 liveness | `ClosedLeavesNothing` 반례: 종결 뒤 원본이 끝내 남음 | PASS |
| WideSweep | 정리를 채널 전체로 넓힘(대조) | `OthersKept` 반례: 다른 사용자 원본이 지워짐 | PASS |

Fixed는 `LiveHolderKeeps`·`NoDecisionNotice`·`OthersKept`와 liveness `ClosedLeavesNothing`·`OthersTold`를 더해 통과한다.

모델과 코드가 다른 곳: 모델의 PeerRecover는 holder가 죽은 뒤에만 켜지지만, 코드는 holder 노드가 `worker_nodes`에서
online이 아닐 때를 "죽음"으로 읽는다(leader의 heartbeat 정리가 offline으로 바꾼다). 클러스터를 끈 단일 노드는 instance id가
하나라서 같은 노드 규칙만 쓴다. Closing 행의 마무리(Finalize)는 holder를 확인하지 않는다 — 원본 제거가 빠져도 Sweep이 다시 찾는다.

## 조각 3 — 게시·회수·비활성 버튼·intake 차단, 2c 리뷰 P2 ①②⑤ 대조 (모델 먼저)

조각 3의 코드는 아직 없다. 아래 행동은 설계 진입점 전수표의 N행(신규 예정 지점)에 대응하고, P2 대조는 2c head의 코드 좌표에 대응한다.

| 모델 전이·변수 | 대응 | 메모 |
|---|---|---|
| PostBegin → PostFinish(`posts`) | 설계 N01(ask)·N08(provider 최초 글) | intent를 먼저 저장하고 starter를 한 번 게시한다. 응답을 들으면 `disabled`, 못 들으면 `posting`에 남는다 |
| Reconcile | 설계 N08(reconcile) | 응답 유실 뒤에는 request marker로 같은 글을 찾는다. 새 POST를 하지 않는다 |
| PostRejected | 설계 §5의 확정 4xx | 게시 확정 거절은 `post_failed`, 자동 재POST 없음 |
| `disabled` phase · Activate | 설계 §13 P2-3 표의 조각 3·4 행 | 조각 3은 비활성 버튼과 "아직 선택을 받지 않음"만 노출한다. Activate는 조각 4의 배선이다 |
| ThreadMessage(대조, `leaked`) | 설계 E49·N22 | decision 글의 메시지는 일반 agent intake로 가지 않는다. guard는 포럼을 기준으로 해야 thread id 저장 전(`posting`)에도 성립한다 |
| Bootstrap · `tempHeld` (P2 ①) | `src/services/discord/queue_io/decision_drain.rs` `run`(:54)·`node`(:63), `src/db/decisions.rs` `HOLDER`(:155) | 2c head는 cluster 등록 전에 물으면 임시 이름(hostname-pid)을 holder로 고정한다 = `TempHolderId` |
| MarkOffline · OfflineSettle · `gone` (P2 ②) | `decision_drain.rs` `recover_before`(:429), `src/services/cluster/node_registry.rs` `mark_stale_worker_nodes_offline`(:490) | 2c head에는 offline 전환에서 정산을 다시 부르는 배선이 없다 = `NoOfflineSettle` |
| RefuseKept · `bound` (P2 ⑤) | `src/services/turn_orchestrator/queued_head/v2/decision.rs` `decision_tickets`(:56), `v2/refused.rs` `refuse_not_sent`(:12) | 2c head의 sweep은 bind에서만 후보를 모은다 = `BindOnlySweep` |

수정 전 RED / 후 GREEN(TLC 1.7.4, 모델 작성 때 `PASS: 34/34 configs`; TtlGuard·SnapshotRegister·BootRePost는 3b가, OverrideUnshared는 3b 수리가 더해 `PASS: 38/38 configs`):

| cfg | Mode | 수정 전(RED) | Fixed(GREEN) |
|---|---|---|---|
| RePost | 응답 유실 뒤 새 글 POST | `OnePost` 반례 | PASS |
| EarlyButtons | 비활성 단계에서 선택 수신 | `NoEarlyChoice` 반례 | PASS |
| NoIntakeGuard | intake 차단 없음 | `NoIntakeLeak` 반례 | PASS |
| GuardByThread | 저장된 thread id로만 차단 | `NoIntakeLeak` 반례(게시 응답 유실 구간) | PASS |
| TempHolderId | P2 ①: 등록 전 임시 이름으로 시도를 옮김 | `LiveHolderKeeps` 반례 | PASS |
| NoOfflineSettle | P2 ②: offline 전환 뒤 재정산 없음 | liveness `AbandonedEnds` 반례 | PASS(`Gone.cfg`) |
| BindOnlySweep | P2 ⑤: bind에서만 후보 수집 | `NoDecisionNotice` 반례 | PASS |
| TtlGuard | 3a의 30초 판독만 믿는 guard(등록·miss 조회 없음) | `NoIntakeLeak` 반례 | PASS |
| SnapshotRegister | 등록을 판독 집합 안에 두는 guard(늦은 판독이 지움, 다른 process는 모름) | `NoIntakeLeak` 반례 | PASS |
| BootRePost | 재시작 뒤 `posting` 요청을 다시 POST | `OnePost` 반례 | PASS |
| OverrideUnshared | 포럼 cache만 조회하는 guard(설정 override 포럼은 공유 증거 없음) | `NoIntakeLeak` 반례 | PASS |

**P2 ①②⑤는 2c head 코드가 대조 Mode 쪽이다.** Fixed·Gone의 PASS는 조각 4 착수 조건으로 넘긴 목표 계약이고, 현재 코드의 증명이 아니다(producer가 없어 지금은 도달하지 않는다).

모델의 한계: 돌아오지 않는 노드는 `Gone.cfg`에서만 본다(Fixed의 crash는 항상 재부팅한다). 그 노드의 큐 파일·화면은 그 노드만 고칠 수 있어서 `ClosedLeavesNothing`·`OthersTold`·`EventuallySettled`는 `gone`이면 면제한다. 상태 수를 2c 수준으로 묶으려고 `disabled` 단계의 자동 archive, `unknown`이 아닌 동안의 offline 표시, Fixed에서의 RefuseKept는 뺐다.

### 조각 3a 코드 좌표 (요청 저장·API·CLI·cap·intake 차단, 게시 없음)

좌표는 조각 3a head 기준이다. 3a는 게시하지 않으므로 모델에서는 `new` → `posting`(PostBegin, intent 저장)까지만 코드가 있다. PostFinish·Reconcile·`disabled`는 3b다.

| 모델 전이·변수 | 코드 | 메모 |
|---|---|---|
| PostBegin(intent 저장) | `src/server/routes/decisions.rs` `ask`(:24) → `src/services/discord/decisions/mod.rs` `store`(:105) → `src/db/decisions/requests.rs` `create_once`(:40) | 질문·선택지·cap hash를 한 transaction으로 저장한다. idempotency key 하나가 요청 하나다. 응답은 202 `publication: pending` |
| (읽기, 전이 없음) | `src/server/routes/decisions.rs` `show`(:66) → `requests.rs` `load_owned`(:125) | 그 요청을 물은 cap에게만 읽힌다. cap과 cap hash는 읽기에 없다 |
| requester cap | CLI `src/cli/decision.rs` `ask`(:71)·`get`(:92) → `src/cli/decision/owner_cache.rs` `prepare`(:55)·`of_request`(:75), 서버 `src/services/discord/decisions/requester.rs` `hash`(:18) | cap은 요청을 보내기 전에 사설 cache에 쓰이고, 서버는 hash만 저장한다 |
| ThreadMessage 대조(`NoIntakeGuard`·`GuardByThread`) | `src/services/discord/router/intake_gate.rs:235` → `src/services/discord/decisions/ingress.rs` `consumes`(:92) → `contains`(:46) → `load_forums`(:24) | 부모가 Decision 포럼(ready 행 + 설정 덮어쓰기)인 thread의 메시지는 dedup 전에 intake에서 빠진다. thread id를 보지 않으므로 게시 응답 유실 구간(`posting` ∧ `posts`=1)에도 성립한다 |

모델과 코드가 다른 곳(3a, 3b에서 닫음 — 아래 3b 절): 차단은 포럼 집합을 30초마다 다시 읽는다(`ingress.rs` `REFRESH`:14). 방금 기록된 포럼은 다음 판독까지 최대 30초 동안 모른다. 모델의 `GuardByForum`은 포럼을 항상 아는 것으로 둔다. 3b가 게시 전에 그 포럼을 이 process의 집합에 넣어 이 창을 닫아야 한다. thread의 부모는 intake가 이미 읽은 값과 gateway cache에서만 얻는다. 둘 다 없으면(채널 조회 실패 + cache 없음) 판정하지 못하고 기존 intake로 간다.

### 조각 3b 코드 좌표 (게시·회수·비활성 버튼, 포럼 기억)

좌표는 조각 3b head 기준이다. 3b가 PostFinish·Reconcile·PostRejected·`disabled`를 코드로 만들고, 3a가 남긴 "포럼을 모르는 구간"을 모델에 넣었다(`known`·`inflight`, 대조 TtlGuard·SnapshotRegister·BootRePost). TLC 1.7.4 `PASS: 38/38 configs`(Fixed PASS; w2 수리에서 OverrideUnshared를 더했다).

| 모델 전이·변수 | 코드 | 메모 |
|---|---|---|
| PostBegin(journal, `inflight`) | `src/services/discord/decisions/publication.rs` `post`(:102) → `src/db/decisions/publication.rs` `journal`(:63) | `stored`→`posting` CAS 한 번과 버튼 handle을 한 transaction으로 쓴다. CAS를 잃은 run은 POST하지 않는다 |
| PostFinish(`posts`) | `publication.rs` `send_post`(호출 :162) | POST 한 번. 2xx면 `posted`(:100)로 `disabled`, 응답 유실·5xx·연결 오류는 `posting`에 남는다 |
| PostRejected | `publication.rs` :177 → `db/decisions/publication.rs` `refused`(:118) | 400/401/403/404는 `post_failed`. 다음 ask는 같은 거절을 돌려줄 뿐 POST하지 않는다 |
| Reconcile | `publication.rs` `publish`(:48) → `reconcile`(:191) → `find_post`(:215) | `posting`이면 포럼의 active thread 중 우리 봇이 시작했고 starter 작성자가 우리 봇이며 starter의 마지막 줄이 이 요청의 marker인 글만 회수한다(인용된 marker는 아니다). 못 찾으면 `posting`(202)에 남는다 |
| BootRePost(대조) | 해당 코드 없음 | 재시작 뒤에는 `inflight`가 없다. `posting` 요청은 Reconcile만 탄다 |
| `disabled`(view `not_yet`) · EarlyButtons(대조) | `src/services/discord/decisions/card.rs` `render`(:27)·`post_body`(:72), `src/services/manual_steer/action_handle.rs` `to_decision_custom_id`(:32) | 버튼은 모두 disabled이고 custom-id는 `decision:v1:<handle>`이다. manual steer의 parse는 이를 남의 것으로 거절하고, Decision 클릭을 받는 handler는 없다(조각 4) |
| `known`(포럼 기억) · Refresh · StaleRefresh | `src/services/discord/decisions/ingress.rs` `register`(:24, 호출 `publication.rs` :135)·`contains`(:69)·miss 조회(:90, `known_in_pg`:30) | 게시 process는 journal 전에 포럼을 등록 집합에 넣는다. 등록 집합은 30초 판독이 덮어쓰지 않는다. 판독과 miss 조회는 ready 포럼 행에 더해 journal된 `decision_requests.forum_id`도 본다(:53). journal은 POST 전에 commit되므로 설정 override 포럼도 다른 process가 PG에서 안다. 그래서 Fixed는 메모리에 기대지 않는다 |
| ThreadMessage(대조) | `src/services/discord/router/intake_gate.rs:235` → `ingress.rs` `consumes`(:147) | REST 부모 또는 gateway cache 부모(`cached_parent`)가 Decision 포럼이면 dedup 전에 멈춘다. cache가 forum이 아닌 채널로 아는 부모는 조회하지 않는다 |

모델과 코드가 다른 곳(3b):

- 모델의 Fixed는 "POST 전에 PG가 그 포럼을 공유 증거로 가진다"를 가정으로 둔다. 코드에서는 journal 행이 `forum_id`를 POST 전에 commit하고(`db/decisions/publication.rs` `journal`:63), guard가 그 열을 읽는다. 자동 해석 포럼은 그보다 앞서 ready 행도 있다(`publication.rs` :128~:133). w1은 설정 override 포럼에 공유 증거가 없었다 = `OverrideUnshared`(리뷰 P1-1).
- `provider_bot`(:81)은 Context에 더해 현재 `bot_connected`(:89)를 요구한다. gateway를 handback·self-fence한 runtime은 Context가 남아도 게시하지 않는다(리뷰 P1-2). 모델은 봇 연결 상태를 변수로 두지 않고 결정적 시험으로 대신했다(아래 표 밖).
- PG 조회가 실패하는 순간 다른 process(그 포럼을 등록하지 않은 process)는 그 메시지를 막지 못한다. 모델은 PG 장애를 다루지 않는다.
- Reconcile은 active thread만 본다. 응답을 잃은 글이 회수 전에 보관(archive)되면 찾지 못하고 `posting`(202)에 남는다. 새 POST는 하지 않는다.
