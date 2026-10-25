PLAN_READY a525e9be9886d9dd8f85b5c7f1a0c03176b377c6 S4-r2=A안·사전판독1회→paste→Enter; merge-only

# #7272 S4 r2 — A안 위에서 운영 유입을 단일 owner에 연결

## 0. 기준·이전 판과의 관계

- M: main `28eee2f87fa44e1a67152d3b15f1e7e84e5f99dd`(#7410, #7386 조각1 착지). L: 이 레그 병합 head `a525e9be9886d9dd8f85b5c7f1a0c03176b377c6`. 아래 좌표는 표시한 M/L을 구분한다. 손으로 푼 병합 hunk와 실행 결과는 `report-s4-replan.md`가 정본이다.
- `plan-s4.md`는 A안 결정 **전**의 역사 자료로 그대로 남긴다. 13:5x WebGPT 시도는 회수 전 시간 초과였으며 **리뷰 지적도 CLEAN 판정도 없다**. 이 문서는 새 독립 설계 리뷰 입력이며 구현 완료 선언이 아니다.
- 사용자 A안(10-25 14:42)이 설계 r2/S3/S4-r1의 UI 안전·확인 계약보다 우선한다. #7386 원문과 #7402 제거 범위를 읽었다. source/운영 설정값을 혼동하지 않으며 운영 config·서버·DB는 읽거나 바꾸지 않았다.
- 이번 레그는 main 병합과 S1~S3의 **A안 적응**만 소스에 반영한다. 아래 S4 코드·HTTP 변경·새 권한은 구현하지 않는다. push/PR/GitHub/DAG 쓰기는 하지 않는다. DAG는 코디네이터 소유라는 사용자 지시를 스킬 일반 절차보다 우선한다.
- 완료한 S1~S3 검증도 기반/동작이 달라졌으므로 자동 승계하지 않는다. 새 병합 head의 실제 target/static 결과와 compile 실패를 구분한다.

약어: `S/`=`src/services/`, `D/`=`src/services/discord/`, `T/`=`src/services/turn_orchestrator/`, `C/`=`src/services/claude_tui/`. M에 없는 owner/history V2 좌표는 L이다. 구현 전에는 #7402/#7330/#7317 착지 상태와 실제 API를 다시 고정한다.

### 0.1 S3d 이후 main 드리프트

`git log --oneline b27cd6c0dffd9ce1f02ec10d0c819f7bf2a5814a..28eee2f87fa44e1a67152d3b15f1e7e84e5f99dd -G 'before_paste|stash|queued_head|restore_tmux|rehydrat|retire|FreshTurn|raw_prompt|submission_fence' -- src`의 결과는 `logs/s4r2-main-drift.log`에 고정한다.

| main commit/변경 | 이 계획의 처분 |
|---|---|
| `ebec3c266` 및 #7410 `28eee2f87f` — #7386 A Claude 제출 | 사전 판독/paste/Enter가 정본. S3 callback은 content 전으로만 옮김 |
| `f5b4a33d3` — fixed-size guarded launch 제거 | main의 일반 create를 유지. 크기/geometry guard용 launch를 복구하지 않음 |
| `9b18d7e9a`, `3be7dee2a`, `785bf6a31`, `d8412b56a` — readiness·메뉴·single-read 시험 | 새 process FreshTurn/readiness 경계는 유지하고 사후 UI proof·자동 lock은 제거 |
| `7b0f33d0f` #7365 — rehydrated Codex watcher 복원 | 실제 재바인딩 결과를 target resolver에서 소비. watcher 부재/Source None으로 create 추정0 |
| `4141858ba` #7369 — 오래된 reader 퇴역 | input 원문·runtime 존재·NotSent 판정과 분리. reader 재생성 권한 추가0 |
| #7379 직접 입력 전체 미러(기존 M에 이미 있음) | 기존 mirror/row 분류를 유지. owner input echo만 중복 turn으로 해석하지 않음 |
| 경고 정리·#7320 휴면 restart 제거·reply/system-prompt 변경 | 삭제된 경로를 복원하지 않음. prepared helper의 session_prompt 기록과 render 순서는 main을 따른 병합으로 정리 |

## 1. 바뀐 계약 — A안을 기준으로 다시 세움

**한 번 판독 → 선택적 C-s 한 번 → bracketed paste → 짧은 고정 지연 → Enter 한 번 → 반환**이 Claude busy/idle의 제출 거래다. 메뉴가 보이면 보내지 않는다. 입력창에 글이 보이면 C-s로 stash하고, 비어 있으면 C-s를 누르지 않는다. 입력창 판독이 안 되면 stash 없이 진행한다. footer·attach·resize·cursor 변화 때문에 paste 뒤 Enter를 막지 않는다. paste 뒤 UI 소유 판독, stash 확인, 초안 복원 감시, receipt 대기와 자동 입력 잠금은 없다. pane 소멸/실제 transport 실패는 별도 쓰기 오류다.

FreshTurn의 새 화면 생성/readiness 준비 대기와 거래 안의 draft/menu 판독1회를 구분한다. 실제 create 뒤 입력창이 아직 그려지지 않은 동안은 기존 bounded readiness 준비를 유지하고, 준비된 거래의 paste 뒤에는 더 판독하거나 Enter를 막지 않는다. 준비 대기를 followup label로 건너뛰는 오류도 허용하지 않는다.

A안의 `Injected`는 **로컬 paste/Enter 거래가 성공했다는 의미**다. AI가 읽었다거나 답변이 전달됐다는 증명이 아니다. 반대로 native receipt가 없다는 이유로 이를 Unconfirmed로 낮추거나 다음 입력을 막지 않는다. AI 응답 observer/anchor/outbox lifecycle은 기존대로 별도다. history 등록은 native echo 중복 방지용으로 유지하고 receipt는 진단으로만 소비한다.

| 구 S4-r1/이월 계약 | r2 처분·최소 계약 | 위치 |
|---|---|---|
| OwnPending/unreadable/unsafe draft이면 자동 Held | **폐기**. visible 글은 C-s, unreadable은 C-s 없이 진행. modal만 사전 대기. 대기 여부에 과거 자동 DraftGuard를 사용하지 않음 | A submit 재사용, 4a/4d |
| paste 뒤 ownership/attach generation/geometry/cursor guard로 Enter 제어 | **폐기**. 기존 A 거래에 새 post-check·epoch UI guard를 얹지 않음 | 4a/4d, #7402 뒤 |
| receipt 없음→30초 Unknown→보관/notice→다음 입력 | **폐기**. successful Enter는 local Submitted terminal. receipt absence timer/receipt 실패 Discord 오류/automatic lock0 | 4a/4e2 |
| Source/receipt가 없으면 hold release·다음 입력 제한 | **폐기**. positive runtime에 Source None이어도 A 거래로 전달. old hold는 자동 잠금으로 복원하지 않음. 원문 custody는 독립 | 4d |
| one-shot permit·실제 writer lease·durable 원문·exact attempt | **유지**. UI 사후 확인을 없앤 것이 저장/권한/동시 writer를 없앤 것은 아님. lease는 거래 종료에 놓고 AI 완료/receipt를 기다리지 않음 | 4a/4c |
| S3b stable ticket, 원문/선택 prelude/voice/첨부 | **유지**. Pending은 동일 ticket recover, 두 번째 synthetic admission 금지. 원문을 이미 전달한 것처럼 표시하지 않음 | 4b/4e1 |
| #7386 조각1의 stash 채널 게이트 | 현재 M에는 존재. #7402가 모든 Claude 채널의 단일 A 거래로 바꾸는 것이 정본 | #7402 선행, S4에서 게이트 복제0 |
| Codex에도 A안이 이미 있다고 가정 | **금지**. M의 Codex busy에는 await_own/receipt가 남아 있다. #7386 조각2의 A primitive/소유 승인을 확인한 뒤 Codex production 연결 | 4d/4f 선행 조건 |
| 실제 실패와 receipt 미확인을 섞은 Unknown | `Submitted`, `DefinitelyNotSubmitted`, `WriteUncertain`을 구분. 원문 보존·재시도 여부는 실제 I/O 증거에만 따름 | 4a |

### 1.1 상태·권한 불변식

1. durable 접수 전 성공 ACK/checkpoint/worker done0. raw·author/source IDs·role/provider/CWD/worktree·선택 prelude·accepted voice·asset 참조는 소유자가 보존한다.
2. 각 source는 기존 actor 하나가 접수/중복/순서/원문 보관을 결정한다. live/catch-up/boot/worker/completion-drain이 같은 controller를 소비한다. 새 DB schema/flag/ACK/journal은 만들지 않는다. managed_submission의 확정 미전송 terminal도 기존 exact-birth 결과 경계의 최소 확장으로만 다룬다.
3. 물리 writer는 같은 target에 하나다. **현재 전송 중** scope 없는 content는 거절하고, 거래가 끝나면 lease를 놓는다. UI 미확인 때문에 자동 잠금이 남지 않는다. 새 registry는 delivery 상태/초안 소유 원장이 아니다.
4. 원문 접수·실제 content 직전 durable metadata·동일 exact attempt completion은 유지한다. callback은 metadata CAS/disk만 하며 UI/Source/composer를 기다리거나 재잠그지 않는다. actor가 effect 완료를 기다리는 동안 callback을 막는 cycle0.
5. successful A 거래는 native receipt 없이 local Submitted로 종료한다. observer는 그 성공을 만들기 위한 대기 조건이 아니며 receipt absence notice0. true write ambiguity는 원문 보관/fence이며 자동 재전송0, 그러나 그 자체로 B의 입력을 잠그지 않는다.
6. restart의 기록 부재/Source 없음/새 generation은 미송신 증거가 아니다. 반면 확정 load-buffer 실패는 content0인 positive outcome이다. 그 attempt는 MayHaveSubmitted로 남기지 않는다.
7. 원문 custody와 UI draft/stash는 별개다. A안은 human draft 자동 복원·확인을 요구하지 않는다. old DRAFT_GUARDS/RecoveryRequired를 V2 metadata에서 되살리지 않는다. 메뉴 사전 대기와 실제 policy/permission 거절만 준비 대기다.
8. 기존 host CreatePermit·consent·placement/replay authority·execution capacity는 유지한다. writer permit은 replay start/host permit을 대신하지 않는다. 대상 밖 Remote/headless/외부 origin은 기존 권한을 유지하고 local fallback0.
9. 기존 history는 echo/row 분류·diagnostic projection이다. local Submitted/확정 미송신 attempt의 live slot을 반환하고, lifetime4096 때문에 후속 입력이 막히지 않게 한다. passive는 임의 TTL/End+Seal로 삭제하지 않는다.

A 결과 타입과 저장 wire enum을 구분한다. successful local Submitted는 기존 terminal identity/source fence로 마무리하고, queue의 기존 InputPhase에 호환되지 않는 Submitted enum을 임의 추가하지 않는다. 확정 미전송은 **attempt만** 끝내며 원 source를 성공 terminal로 만들지 않는다. 필요 diagnostic completion 필드는 optional/default로 두고 A24 actual golden으로 옛 reader의 ReadOnlyV2 인식을 확인한다.

### 1.2 10-26 00:48 결정 두 건

**① 실제 launch/재생성 경계의 FreshTurn**: 원 요청 이름이 followup/warm인지로 readiness kind를 고정하지 않는다. actual runtime create/recreate 결과가 Fresh라면 FreshTurn primitive와 준비 대기를 선택한다. 기존 살아 있는 runtime+늦은 Source는 Fresh가 아니다. resume SID를 새 process에서 여는 것도 runtime-create 경계의 FreshTurn과 provider resume 옵션을 구분한다. 모든 create leaf가 같은 typed classification을 반환하고 warm fallback/absence launch/Herdr pending→create에서도 유지한다. normal existing runtime만 warm/busy intent를 쓴다. legacy warm+recreate 묶음을 그대로 호출해 내부에서 launch가 숨어 발생하게 두지 않는다.

**② 확정 LoadFailed**: load-buffer 실패가 확정되고 paste/Enter가0이면 `DefinitelyNotSubmitted(LoadFailed)`로 attempt를 종결한다. MayHaveSubmitted/전달미확인/성공 source terminal로 남기지 않는다. prepared 원문과 admission ticket은 남으며 같은 큐 자리의 준비 재시도 또는 명시 retry-required 보관으로 옮긴다. 권고는 **attempt는 끝내고 원문은 retryable queue에 남기는 방식**이다. backstop은 기존 준비 재시도만 담당하며 무한 즉시 재킥0. paste 요청 이후 응답 불명과 restart의 미진입 추정에는 이 결과를 쓸 수 없다. C-s가 이미 갔다면 그것은 별도 stash 부수효과이며 UI를 확인/복구하는 새 감시자는 만들지 않는다.

현재 L `C/busy_inject/stash.rs:25,36`는 실제 load-buffer 실패와 owner metadata callback 거절을 모두 `LoadFailed`로 표현한다. S4는 **Veto 이름만으로 확정 미전송 종결을 선택하지 않는다**. 실제 load 단계·content 호출0·exact live attempt 증거를 typed 결과에 보존하고, metadata publish 불명은 먼저 같은 ticket/CAS로 복구한다. 이 cause 구분도 4a의 고유 단언 대상이다.

현 managed record(M `D/inflight/managed_submission.rs:16,40,70,90`)는 `Waiting`/`MayHaveSubmitted`와 raw를 보존하는 `Unrecognized`를 이미 가진다. 4a 권고는 **기존 phase 필드에 확정 미전송 terminal을 추가**하고 exact birth+소비한 live no-content proof로만 기록하는 최소 확장이다. terminal rank는 MayHaveSubmitted보다 높여 stale snapshot이 덮지 못하게 하고, old reader는 이를 Unrecognized로 원문 보존/fail-closed해야 한다. Waiting으로 되돌려 crash 재송신 근거를 만들지 않는다. 이 호환을 actual old-reader fixture로 증명하지 못하면 DESIGN_DELTA다. 새 DB schema/별도 journal/광범위 inflight 모델 개편은 하지 않는다.

## 2. 진입점·소비점 전수표 (M + 표시한 L)

다음은 운영 연결 때 누락을 검토하는 표다. 같은 부모의 단순 wrapper는 명명한 owner 행에 포함한다. 현재 leg의 병합 해소 표와 혼동하지 않는다. API 이름은 #7402 착지 뒤 재확인하며 health/turn_deliver/inject* 안에 새 owner 코드를 두지 않는다.

| 경계 | M 좌표(표시한 foundation V/N8은 L) | 연결 때 지킬 계약 | 단위 |
|---|---|---|---|
| G1 Gateway | `D/router/intake_gate.rs:183` / `D/router/intake_gate.rs:1117` / `D/router/intake_gate.rs:1541` | bot/auth/command/attachment/voice 분류 뒤 live와 thread/restart/backlog가 같은 custody. busy 선주입 우회 제거 | 4e/4f |
| G2 source/명령 경계 | `D/router/intake_gate/gate.rs:37` / `D/router/intake_gate/gate.rs:107` / `D/router/intake_gate/gate.rs:161` | 운영 notice loopback0·실제 명령과 RawV1/선택 prelude·source merge 경계 유지 | 4e |
| G3 queue effects | `D/router/intake_gate/queue_effects.rs:116` | voice/raw strip 전 original을 중앙 custody에 인계. queued reaction은 durable 결과 뒤 | 4e/4f |
| G4 queue transaction | `D/router/intake_queue_transaction.rs:123` / `D/router/intake_queue_transaction.rs:339` | 원문 생성·accepted/pending/full·checkpoint/reaction 판정. source별 ACK와 접수 실패 원문 유지 | 4e/4f |
| G5 legacy payload 변환 | `D/router/intake_queue_transaction/queue_payload.rs:14` | 기존 V1 voice 제거 전 V2 handoff; RawV1를 재정의하지 않음 | 4e |
| G6 busy/live | `D/router/intake_gate/busy_inject.rs:198` / `D/router/intake_gate/busy_inject.rs:212` / `D/router/intake_gate/busy_inject.rs:274` | 기존 첨부/voice 승인 보존. A 성공은 local Submitted, 실제 write ambiguity만 custody; 일반 turn 재접수0 | 4e1/4f |
| R1 placement/consent/replay | `D/router/intake_dispatch.rs:247` / `D/router/intake_dispatch.rs:715` | remote/blocked/consent/worker authority 유지. LocalPermit 뒤 supported TUI만 owner | 4e/4f |
| R2 queued 재구성 | `D/router/intake_dispatch/queued.rs:71` / `D/router/intake_dispatch/queued.rs:247` | merged source/reply/uploads/voice를 그대로 준비·인계; reserved source 두 번 접수0 | 4e/4f |
| R3 일반 worker | `S/cluster/intake_worker.rs:261` / `S/cluster/intake_worker.rs:644` | 실제 PG claimed row/accepted payload 전달. Durable custody 전 done0. parent 수정0·아래 entry로 수렴 | 4f 시험/유지 |
| R4 worker/exact retry | `D/router/message_handler/intake_turn/worker_entry.rs:49` / `D/router/message_handler/intake_turn/worker_entry.rs:98` / `D/router/message_handler/intake_turn/worker_entry.rs:135` | claimed 원문·기존 retry permit/lineage 그대로. protected/external source 우회0 | 4f |
| C1 catch-up phase1 | `D/catch_up/phase1_sweep.rs:6` / `D/catch_up/phase1_sweep.rs:422` | known IDs와 source 분리 뒤 동일 접수. phase1 실제 진입 시험 | 4f 시험/유지 |
| C2 catch-up phase2 | `D/catch_up/phase2_sweep.rs:4` / `D/catch_up/phase2_sweep.rs:254` | phase2 별도 시험. Source 없음으로 신규 turn 발사0 | 4f 시험/유지 |
| C3 catch-up 공통 | `D/catch_up/api.rs:86` | 두 sweep의 실제 trait 기본 enqueue→공통 mailbox entry. API parent 수정0 | 4f 시험/유지 |
| C4 central enqueue/recovery | `D/queue_io/turn_admission.rs:266` / `D/queue_io/turn_admission.rs:189` | same ticket custody/closed actor 재해석. durable 뒤만 wake·ACK | 4e/4f |
| C5 기존 wrapper | `D/mod.rs:1574` / `D/mod.rs:2476` / `D/mod.rs:2525` | Giant 수정0. 실제 하위 actor/queue_io에 수렴함을 call graph 가드. wrapper로 V2를 우회하지 못함 | 4c/4f 시험 |
| B1 Gateway/worker boot | `D/runtime_bootstrap/queued_recovery.rs:11` / `D/runtime_bootstrap/queued_recovery.rs:26` / `D/runtime_bootstrap/queued_recovery.rs:52` | metadata-only도 adopt. queue→inflight→marker→placeholder 순서·sender/ownership 보존, migration 실패 hold; sender 거절로 V2 raw를 삭제하지 않음 | 4c/4e/4f |
| B2 inflight 복원 | `D/recovery_engine/restore_inflight.rs:70` | 기존 PG/파일 복구가 V2 original/retained를 중복 dispatch하거나 지우지 못함. parent 수정0 | 4c 시험/유지 |
| B3 startup watcher | `D/watchers/lifecycle/restore.rs:21` | positive watcher 복원 결과를 재사용. Source/reader 부재를 runtime 부재로 추론0. 부모 수정0 | 4d/4e2 시험 |
| B4 Codex rehydrate | `D/watchers/codex_tui_restore.rs:122` | 이미 main인 positive rehydrated binding을 resolver가 재사용. reader retirement와 input 상태 분리 | 4d/4e 유지 |
| D1 completion gateway | `D/gateway.rs:730` / `D/gateway.rs:485` | provider gate→marker drain 실제 구현 경계(§7 matrix). V2 처리 전 marker/card 제거0 | 4f |
| D2 completion queue | `D/queue_dispatch.rs:218` / `D/queue_dispatch.rs:350` / `D/queue_dispatch.rs:16` | take-next/finalize/card drain도 V2 상태 소비. Unknown을 legacy 일반 turn으로 실행0 | 4c/4f |
| D3 kickoff | `D/queue_dispatch/kickoff.rs:113` / `D/queue_dispatch/kickoff.rs:338` | busy/idle/startup/backstop 모두 동일 state pump. V1 try_busy false를 V2 fallback 허가로 해석0 | 4f |
| W1 enqueue wake | `D/queue_io.rs:221` / `D/queue_io.rs:296` | 중복/경합 wake coalesce, 상태 없는 wake는 재관측만 | 4e/4f |
| W2 deadline/backstop | `D/queue_io/backstop.rs:276` / `D/queue_io/backstop.rs:550` / `D/queue_io/backstop.rs:572` | 준비 경합·startup gap·비실행 retained 재열거. 성공 Enter의 receipt timer는 제거; metadata-only/backstop no-op publish0 | 4e2/4f |
| W3 legacy opt-in | `D/queue_io/queued_head_inject.rs:34` / `D/queue_io/queued_head_inject.rs:80` | 현 env gate의 V1 의미와 V2 single path 분리. 운영 값 조회 안 함 | 4f |
| W4 Source/Resolved | `D/tui_prompt_relay/source_tick.rs:27` / `D/tui_prompt_relay/source_tick.rs:51` / `D/tui_prompt_relay/source_tick.rs:115` | #7381 join retry와 같은 이벤트 사용. bind-once/reconcile, Source 없이도 live safe runtime pump | 4e/4f |
| A1 actor 모든 message | `S/turn_orchestrator.rs:1210` / `S/turn_orchestrator.rs:1741` | Enqueue/TryStartTurn/TakeNextSoft/restore/requeue/finish/restart/purge/Injection을 exhaustive audit. V2 mutable projection 이중 writer0 | 4c |
| A2 queue claim/settle | `T/queued_head.rs:237` / `T/queued_head.rs:650` / `T/queued_head.rs:742` / `T/queued_head.rs:824` | V1 owner와 V2 slot 공존; target supported 판정 뒤 단일 writer. root promote 시 legacy destructive gate 유지 | 4c/4f |
| A3 legacy mend | `T/queued_head/mend.rs:144` / `T/queued_head/mend.rs:196` | legacy Unknown의 exact 증거는 보존하되 A 성공에 receipt 대기 재도입0. 다른 attempt 정산0 | 4b/4d |
| A4 live reservation | `T/injected_inputs.rs:168` / `T/injected_inputs.rs:362` / `T/injected_inputs.rs:414` | live source 순서/소유권을 V2 custody와 중복 부여0. 기존 out-of-scope 계약 보존 | 4c/4f |
| A5 순서/병합 | `T/inbound_order.rs:69` / `T/inbound_order.rs:123` | retained는 실행 barrier 아님. active/reserved/dispatch lease 순서만 소비 | 4c |
| A6 enqueue/overflow | `T/queue_enqueue.rs:5` / `T/queue_enqueue.rs:122` | fresh source dedupe 후만 merge. cap/full을 성공이나 old attempted 삭제로 처리0 | 4c |
| A7 cancel/take | `T/queue_cancellation.rs:51` / `T/queue_cancellation.rs:95` | 미시도 queue만 명시 discard, active/retained exact custody 유지 | 4c |
| A8 marker/requeue/restart | `T/dispatch_reservation.rs:234` / `T/dispatch_reservation.rs:306` / `T/dispatch_reservation.rs:470` | 원문과 marker/full source IDs 순서 보존. no-op/foreign format writer 우회0 | 4c |
| A9 persistence 모두 | `T/pending_queue_persistence.rs:455` / `T/pending_queue_persistence.rs:548` / `T/pending_queue_persistence.rs:635` / `T/pending_queue_persistence.rs:816` / `T/pending_queue_persistence.rs:882` | writer와 remove-all-token의 V2 refusal 유지. 읽기는 metadata-only 포함. 새 저장소0 | 4b/4c |
| A10 clear | `T/clear_channel.rs:76` / `T/clear_channel.rs:214` | clear 결과의 미시도 삭제와 attempted 보관 분리. V2 snapshot unlink0 | 4c |
| A11 checked purge | `T/checked_purge.rs:7` / `T/checked_purge.rs:352` | 동일 snapshot/namespace 검증·원문 보존. 새 강제 purge 권한0 | 4c |
| A12 mailbox lifecycle | `T/registry_purge.rs:103` / `T/registry_purge.rs:403` | 닫힌 actor의 V2 메시지도 refuse; retained-only registry GC가 복구/notice를 잊지 않게 재열거 가능 | 4c |
| A13 recovery known IDs | `D/recovery_known_ids.rs:55` / `D/recovery_known_ids.rs:152` | queued/active/retained/terminal+source별 fence union. metadata-only 통과 필수 | 4e |
| A14 기존 제어 caller | `D/commands/control/clear_report.rs:46` / `D/commands/control/clear_report.rs:65` | queue read-only 경고를 기존 실패 결과와 함께 표시. 강제 unlink/move-aside 권한 추가0 | 4c |
| P1 pre-admission | `D/router/message_handler/pre_admission_control.rs:35` / `D/router/message_handler/pre_admission_control.rs:182` | 실제 명령/첨부 세션 custody를 먼저 분류; 일반 raw 문자열 때문에 명령으로 재해석0 | 4e 유지 |
| P2 context/render | `D/router/message_handler/turn_context.rs:19` / `D/router/message_handler/turn_context.rs:43` | #7314 실제 RawV1/선택 prelude 타입 재사용; reply-context 레인 결과 위에 연결 | 4e 유지 |
| P3 준비/기존 session | `D/router/message_handler/intake_turn.rs:157` | S3b 준비/render1 재사용. role/profile/model/CWD/worktree/recovery/prelude 유지, render 순서는 최신 main. inflight/retry 모델 재정의0 | 4e1/4f |
| P4 one-shot context | `D/router/turn_start.rs:252` / `D/router/turn_start.rs:271` | 선택 prelude consume/handback 기존 계약 유지. shared WIP/retry stash를 새 owner 저장소로 대체0 | 4e 유지 |
| P5 assets | `D/router/message_handler/attachments.rs:81` / `D/router/message_handler/attachments.rs:186` | 기존 materialize/retention owner 수명에 결합. owner 반환/retain/restart 뒤 원 upload 참조 유지 | 4e 유지 |
| P6 voice | `D/router/message_handler/voice_announcement_route.rs:9` / `D/router/message_handler/voice_announcement_route.rs:62` | accepted voice/PG claim provenance가 original에 남아야 함. sibling claim 두 번 전사0 | 4e 유지 |
| P7 voice intake | `D/router/message_handler/intake_turn/voice_intake.rs:9` | 이미 승인된 announcement를 재claim 패배로 지우지 않음. prepare helper 재사용 | 4e 유지 |
| F1 final provider legacy | `D/router/message_handler/provider_dispatch.rs:215` | 지원 대상 V2 acceptance 뒤 이 legacy 일반 실행 경로 호출0. Remote/headless/외부 origin만 기존 의미 | 4f 시험/유지 |
| F2 live/queued adapter | `D/health/turn_deliver/inject.rs:119` / `D/health/turn_deliver/inject.rs:372` / `D/health/turn_deliver/inject.rs:446` / `D/health/turn_deliver/inject.rs:557` / `D/health/turn_deliver/inject.rs:800` | #7402가 제거/재배치하는 기존 adapter 목록. S4 core는 queue_io child에 둠. 기존 queued/live helper 복제·승격0, Codex gate는 4f만 | 4d/4f (#7402 뒤) |
| F3 cold resume/absence Claude | `S/claude.rs:1573` | #6483 B 흡수. Source missing alone은 fresh 근거 아님. 명시 FreshSubstitution 결과 | 4d |
| F4 Codex 내부 absence 분기 | `S/codex/absence_launch.rs:55` / `S/codex/absence_launch.rs:242` / `S/codex/absence_launch.rs:374` | preflight/warm-followup/create 분기마다 actual create면 FreshTurn. argv first1/paste0·observer 순서 보존; 재생성 뒤 warm kind 금지 | 4d |
| F5 Claude Herdr | `S/claude/herdr_turn.rs:155` / `S/claude/herdr_turn.rs:380` / `S/claude/herdr_turn.rs:429` / `S/claude/herdr_turn.rs:624` | Herdr create/기존runtime/hold 전수. automatic UI lock을 owner에 이식0, 실제 writer 종료/동일 execution과 원문 custody 분리 | 4d |
| F6 Codex Herdr | `S/codex/herdr_turn.rs:113` / `S/codex/herdr_turn.rs:363` / `S/codex/herdr_turn.rs:501` | Herdr 신규/기존runtime 분리. argv content0/paste1, Source 없이 A 입력. actual create는 FreshTurn | 4d |
| L1 Claude legacy content | `C/host_input.rs:569` / `C/host_input.rs:633` | TmuxInput literal/paste와 HerdrInput text/paste 모두 content fence. compact/tui_relay 우회 포함; giant 부모 raw0 | 4a |
| L2 Codex legacy content | `S/codex_tui/host_input.rs:242` / `S/codex_tui/host_input.rs:264` / `S/codex_tui/host_input.rs:462` | TmuxCodexInput literal/paste 및 Herdr content를 같은 경계로. giant raw0 | 4a |
| L3 plain tmux | `S/tui_input/submission_tmux.rs:77` / `S/tui_input/submission_tmux.rs:91` | leased target no-scope fail-closed. load-buffer/key-only는 content로 오분류0 | 4a |
| L4 guarded tmux | `S/tui_input/submission_tmux/guarded.rs:164,229,293` (old stash 메서드는 M에서 삭제) | legacy guarded leaf는 #7402 퇴역 대상과 대조. 필요 content fence만 잔존 leaf에 적용, A post-check/guard 복구0 | 4a (#7402 뒤) |
| L5 busy Claude | `C/busy_inject.rs:428,498` / `C/busy_inject/stash.rs:6` | A before_paste의 판독1회와 stash::submit 재사용. receipt 진단은 입력 성공/진행 가드가 아님. main이 지운 paste_into_empty 복구0 | 4d (#7402 뒤) |
| L6 busy Codex | `S/codex_tui/busy_inject.rs:119` / `S/codex_tui/busy_inject.rs:178` | Codex A2 선행. paste 뒤 epoch/cursor 재검사 제안 폐기; 거래 시작 target 검증과 content fence만 적용 | 4a/4d |
| L8 generic SessionHost content | `S/session_host/tmux_host.rs:96` / `S/session_host/herdr_host.rs:219` | generic send_text의 direct transport도 canonical target-aware fence 통과. delegated leaf 중복 publish0, unleased legacy 결과 유지 | 4a |
| L7 direct Herdr content | `S/session_host/herdr_gate.rs:425` / `S/session_host/herdr_gate.rs:433` / `S/session_host/herdr_gate.rs:443` / `S/session_host/herdr_gate.rs:457` | 직접 text/paste/line도 fence 통과. key-only/cancel/terminate 권한은 기존 그대로; callback UI/Source 대기0 | 4a/4d |
| H1 guard | `C/composer_lock.rs:144` / `C/composer_lock.rs:174` | 기존 guard 저장소는 A 입력 admission에 연결하지 않음. S3d P2-5(a)의 재평가/자동 해제 수리 대신 자동 잠금 자체를 제거하는 #7402 소비 | 4d (#7402 뒤) |
| H2 typed draft hold | `C/input/draft_hold.rs:18` / `C/input/draft_hold.rs:84` | old typed draft hold는 #7402 삭제/잔존 목록 대조. V2 원문 보존은 UI hold와 분리; generation만으로 원문 삭제0 | 4d (#7402 뒤) |
| H3 stash transaction | `C/host_input/guarded_submission/idle_stash.rs:95` / `C/host_input/guarded_submission/idle_stash.rs:138` | main A idle 경로 확인용. 보이는 글만 C-s1, unreadable이면 stash 없이 진행. 사후 복원 확인0. 여기 새 S4 로직 작성0 | 4d (#7402 뒤) |
| H4 execution 증거 | `S/tui_prompt_dedupe/binding_context.rs:98` / `S/tui_prompt_dedupe/binding_context.rs:207` | Known spawn nonce/root/session/host 재사용. giant 부모 수정0, Source 대신 runtime 증거 사용 | 4d 유지 |
| Q1 frame/history 모든 consumer | `S/tui_input/human_frame/history.rs:144` / `S/tui_input/human_frame/history.rs:200` / `S/tui_input/human_frame/history.rs:235` / `S/tui_input/human_frame/history.rs:246` / `S/tui_input/human_frame/history.rs:251` / `S/tui_input/human_frame/history.rs:307` | 모든 비-scoped consumer도 V2 인식·raw 보존. receipt는 진단/echo 분류만. future format을 빈 V1로 취급0 | 4b/4e2 |
| Q2 in-memory ledger | `S/tui_prompt_dedupe/injected_steer.rs:50` / `S/tui_prompt_dedupe/injected_steer.rs:83` / `S/tui_prompt_dedupe/injected_steer.rs:97` | queue→history→memory projection. local Submitted/확정 미전송은 live slot 반환, 원문/fence는 독립 보존 | 4b/4e2 |
| Q3 native prompt observer | `S/tui_prompt_dedupe/observation.rs:148` / `S/tui_prompt_dedupe/observation.rs:169` / `S/tui_prompt_dedupe/observation.rs:244` | existing context/source를 읽고 exact row claim. native TUI human full mirror 보존 | 4e |
| Q4 synthetic claimant | `D/tui_prompt_relay/synthetic_start/claim.rs:32` | owner frame echo를 새 turn/source로 만들지 않음. 진짜 native 입력은 같은 기존 분류 | 4e |
| Q5 native completion | `D/tui_prompt_relay/anchor_completion.rs:85` | AI 완료와 input 거래 정산 분리. 다른 attempt 정산0, receipt 없이 다음 input 진행. 기존 completion-drain wake 유지 | 4e2 |
| Q6 native mirror | `D/tui_prompt_relay/injected_prompt_policy.rs:125,291` | #7379 full body 유지, source scope 없는 observer라도 V2 recognition 적용. 수정0 권고 | 4e 시험/유지 |
| Q7 native file facts | `S/tui_o/writer/input_facts.rs:79` / `S/tui_o/writer/input_facts.rs:212` | 전체 consumer 회귀: row offset/ownership 유지. retention/reader retired를 receipt/미송신으로 변환0. 수정0 | 4e 시험/유지 |
| N1 notice outbox | `S/message_outbox.rs:230` | 기존 table/전달기를 재사용, 새 ACK/journal/schema0. failure는 intent 재시도만, B 실행 차단0. giant 수정0 | 4e |
| N2 notice provenance | `S/message_outbox/identity.rs:36` | provider/channel/source/nonce/kind persistent dedupe 및 non-turn provenance. 실제 allowed-bot loopback 시험 | 4e |
| N3 session notice | `S/message_outbox/session_notice.rs:178` | 기존 enqueue와 receipt/index로 실제 edit. dedupe-hit를 해제 메시지 전송으로 간주0. 수정0 권고 | 4e |
| A15 기존 명시 제어 caller | `D/commands/control.rs:388,489,531` / `D/commands/restart.rs:203,313` / `D/commands/skill.rs:145,350` | stop/cancel/clear/restart/skill은 기존 권한만. #6485 부모 수정0; child/actor에서 attempted custody를 보호 | 4c/4f 유지 시험 |
| A16 overflow·실제 RawV1 prefix | `T/overflow.rs:14,19` / `T/queued_head/prefix.rs:22,38` | cap은 실행 큐에만 적용. #7314 eligibility/실제 명령 경계 재정의0; 부모 prefix 수정0 | 4c/4f 유지 시험 |
| H5 AI anchor 종료(physical writer와 별개) | `T/lease_release.rs:21,40` | active-turn anchor release는 wake만 유발하며 physical writer 종료 증거가 아니다. native submit 종료/WriterLease::drop은 L v2/attempt.rs:20의 별도 경계에서 재검증 | 4c/4e2 유지 시험 |
| Q8 누락되기 쉬운 history 소비자 | `S/tui_input/human_frame/history.rs:211,226,371,382` / `S/tui_input/human_frame/history/screen.rs:35` / `C/busy_inject/ownership.rs:22,60` | registered/allows_legacy/receipt_row/unique_attempt·screen 전체 V2 인식. composer ownership 진단을 A submit 차단에 재사용0 | 4b/4e2 |
| F7 기존 replay/effect 권한 | `D/router/message_handler/intake_turn/replay_start.rs:26,94` / `D/router/message_handler/provider_dispatch.rs:215,225,256,451` | 기존 ReplayStart/admit/continuation·input hash·EffectTarget·execution_capacity를 owner EffectPermit으로 대체하지 않음. owner child에서 기존 권한을 소비 | 4d2/4e1/4f |
| N4 A 거래 실제 leaf | `C/busy_inject.rs:432` / `C/busy_inject/stash.rs:6` | 판독1회→선택 C-s→paste→Enter. receipt 대기/자동 잠금0. callback은 content 직전 metadata만 | 4a/4d (#7402 뒤) |
| N5 main managed attempt | `D/inflight/managed_submission.rs:293` / `D/inflight/managed_submission.rs:313` / `D/inflight/managed_submission.rs:237` | 현재 claim이 load 전에 MayHaveSubmitted를 만듦. 확정 LoadFailed의 terminal hook을 기존 exact birth 경계에 추가, 모델/schema 복제0 | 4a |
| N6 Claude warm(M)/fresh(L) | `C/hosting/warm_followup.rs:676` / `C/hosting/fresh_lifecycle.rs:39` | old warm wrapper의 내부 재생성은 #7402 먼저. actual launch 결과 FreshTurn, legacy 실행기 새 배선0 | 4d (#7402 뒤) |
| N7 Codex warm/recreate | `S/codex_tui/warm_followup.rs:425` / `S/codex_tui/warm_followup.rs:1183` / `S/codex/absence_launch.rs:55` | warm fallback/create leaf가 FreshTurn 분류. 확인 안 된 Source 부재만으로 재생성0; giant raw0 child로 반환 | 4d |
| N8 실제 launch/create | `S/claude/tui_session_launch.rs:12` / `S/codex/tui_session_launch/first_input.rs:22` / `S/claude/herdr_turn/first_input.rs:11` / `S/codex/herdr_turn/first_input.rs:9` | Claude/Codex × tmux/Herdr의 actual create·first-submit 경계. provider resume 옵션과 FreshTurn readiness 구분, first content1 | 4d |
| N9 HTTP Legacy와 origin | `S/imsg_reply/mod.rs:187` | 기존 PG reserve/replay를 Legacy origin에도 재사용하는 독립 HTTP 델타. route 좌표는 아래 표, 새 journal/schema0 | 4h |
| N10 race_loss 실제 경계 | `D/router/message_handler/intake_turn/runtime_transition.rs:4` / `D/router/message_handler/intake_turn/race_loss.rs:8` / `D/router/intake_dispatch/queued.rs:32` | queued head busy는 원문·source IDs·위치를 유지, 신규 도착만 tail. shared helper 전부 삭제0 | 4e1/4f (#7330 뒤) |
| V1 V2 actor 입구 | `T/queued_head.rs:92` / `T/queued_head/v2/actor.rs:7` | 현재 cfg(test)인 root/message/slot을 4f에서만 승격. V1 handler의 이중 파일 writer 차단 | 4c/4f |
| V2 접수와 custody | `T/queued_head/v2/source_admission.rs:44` / `T/queued_head/v2/source_admission.rs:93` | stable ticket receipt로 pending recover. candidate bytes 비교/새 synthetic admission 금지 | 4b/4e1 |
| V3 adopt/reserve | `T/queued_head/v2.rs:25` / `T/queued_head/v2.rs:86` | explicit migration 통과 뒤 writable adopt. enqueue는 private, Custody 밖 production 접수0 | 4b/4c |
| V4 permit/content | `T/queued_head/v2/attempt.rs:84` / `T/queued_head/v2/attempt.rs:123` / `T/queued_head/v2/attempt.rs:192` | one-shot exact permit, content 전 metadata. A안에서 successful submit의 receipt deadline 대기 제거 | 4a/4c |
| V5 completion/retained | `T/queued_head/v2/retained.rs:4` / `T/queued_head/v2/retained.rs:36` / `T/queued_head/v2/retained.rs:85` | local Submitted/NoContent/WriteUncertain 분리. definite pre-content 경쟁은 같은 자리 Reserved, 실제 불명은 비실행 보존 | 4a/4b |
| V6 content scope | `C/owner_input.rs:47` / `C/owner_input.rs:61` / `C/owner_input.rs:195` | 지금 test-only factory. production root는 실제 lease/exact permit으로만 설치, scope 없는 leased target fail-closed | 4a/4f |
| V7 history projection | `S/tui_input/human_frame/history/v2.rs:281` / `S/tui_input/human_frame/history/v2.rs:392` / `S/tui_input/human_frame/history/v2.rs:509` | echo/진단 projection·no-op sync0·positive no-content abandoned proof. history/receipt 없음으로 자동 재전송0 | 4b/4e2 |
| V8 새 source constructor | `D/router/message_handler/input_payload.rs:69` | RawV1·선택 prelude 재사용, 원문/승인/첨부 포함. API 이름/bytes는 M rendering과 동등 | 4e1 |
| V9 legacy writer guard | `T/pending_queue_persistence/snapshot_io.rs:4` / `T/queued_head/compat.rs:10` | V2 read-only 거절을 유지. production typed owner branch만 V2 쓰기, destructive downgrade0 | 4b/4c |

위 80개 기존 경계와 추가 A/managed/Fresh/HTTP/원문 경계는 source caller inventory다. N6의 fresh_lifecycle와 N8·V1~V9는 L 좌표이며 나머지는 M이다. `src/server/routes/agents_turn_deliver.rs:108,193,209,213`(M)의 actual route/Legacy branch/response도 4h에 포함한다. old stash 메서드/paste_into_empty는 삭제로 기록하고 빈 주변 줄을 현 진입점으로 세지 않았다. 기준·원문·대응 줄은 `logs/s4r2-entry-map.json`에 남긴다. 구현 전 최종 main에서 신규 content leaf/ingress를 다시 전수 검색하고 표 밖이면 FILES로 고정한다.

actual terminal outcome은 `local Submitted`와 `AI answer delivered`를 분리한다. HTTP body의 injection 성공을 AI 응답 성공 footer로 변환하지 않는다. current `OriginalLiveBridge`/inflight/replay 모델을 복제하지 않고 기존 시작/응답 관찰 경계에 exact context를 인계한다.

## 3. 연결 방법과 rollback

새 운영 flag 없이, 먼저 4a~4e의 root를 시험 경로에서 준비하고 **4f 한 커밋에서 다섯 ingress와 production root를 함께 연결**한다. 중간 review ref는 운영 배포하지 않는다. #7402와 Codex A안 API가 준비되기 전에는 old post-check path를 fallback으로 남겨 S4 수용을 통과시키지 않는다. target scoped legacy experiment gate가 꺼져도 최종 V2 controller가 굶지 않게 하며, 실제 명령·author·placement/consent·대상 밖 경계는 유지한다.

새 controller는 `queue_io/input_owner/*`, actor는 기존 `queued_head/v2/*`에 둔다. 기존 health injector와 분리하되 **UI 거래를 하나 더 만드는 것은 아니다**. 4d adapter는 #7402/A2의 공통 A leaf를 호출한다. current 후보 seam은 C/busy_inject의 before_paste와 stash::submit, native first_input helper다. 최종 API가 외부에서 호출 불가하면 이름을 추정해 복제하지 말고 #7402 착지 뒤 필요한 export만 FILES로 고정한다. old first_input test hook을 legacy injector 밑에서 통째로 production 승격하지 않는다.

physical metadata/known spawn nonce와 origin/replay 권한을 붙이는 지점은 **거래 시작·content 전**뿐이다. 이것이 post-paste UI 재확인/자동 잠금으로 번지면 안 된다. target-mismatch/pane-gone의 실제 거절은 typed error이며 unknown composer는 A안대로 진행한다. callback 이전 load와 C-s는 owner payload 성공 증거가 아니고, callback 멱등성도 두 번째 paste 허가는 아니다.

runtime live+Source None은 ReadyWithoutSource로 같은 execution에 입력한다. confirmed absence만 host CreatePermit으로 create하며 #6483 B의 missing resume transcript→fresh 대체는 명시 결과/사용자 안내로 남긴다. native reader 퇴역은 runtime 부재/미송신 증거가 아니다. launch/first-submit이 끝나면 writer를 놓고 AI output observer를 분리한다.

V1→V2는 provider/token/channel context+expected raw CAS를 받는 명시 migration이다. unknown/foreign/corrupt는 원문 보존·거절한다. V1 attempted original은 보수적 retained로 옮기고 Source/파일 부재로 재송신하지 않는다. provider/target을 Claude/tmux로 꾸미지 않고 terminal의 최초 adoption 시각을 한 번 저장한다. metadata-only 파일도 복원한다. queue가 원문 정본이며 history·known-ID·notice는 재구성 가능한 projection이다.

V2 기록 후 rollback은 삭제/배열 downgrade가 아니다. 호환 reader, 원문 custody, source fence, retained 비실행 의미를 유지하는 compatible fail-closed binary가 하한이다. A안을 되돌려 post-check/RecoveryRequired 잠금을 복구하는 rollback은 하지 않는다. writer 강제 해제/kill/새 정리 권한도 없다.

## 4. 리뷰 단위·파일·크기

각 리뷰 단위 prod30파일/순 코드+1500이 상한이다. `pr_cap_prod.py`의 실제 상수도 FILES=30/NET=1500임을 확인했다. +1200 전에 재분할한다. 표는 현재 M/L raw와 **예상 code 순증**이며 실제 diff가 아니다. G는 어느 기준이든1000줄 초과이고 raw 증가0 이하를 별도 검증한다. 새 child를 만든다고 giant 부모 편집 권한이 자동으로 생기지 않는다. 각 단위는 바로 전 승인 head를 base로 측정한다.

### 4a — A안 결과·전송 권한·두 00:48 결정

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `T/queued_head/v2_types.rs` | 0/117 | — | +25 |
| `T/queued_head/v2/attempt.rs` | 0/197 | — | +25 |
| `T/queued_head/v2/retained.rs` | 0/150 | — | +45 |
| `C/owner_input.rs` | 0/313 | — | +90 |
| `S/tui_input/writer_lease.rs` | 0/0 | — | +140 |
| `S/tui_input/mod.rs` | 21/23 | — | +1 |
| `C/host_input.rs` | 1631/1631 | G | +0 |
| `S/codex_tui/host_input.rs` | 1669/1669 | G | +0 |
| `S/tui_input/submission_tmux.rs` | 134/136 | — | +10 |
| `S/session_host/herdr_gate.rs` | 843/843 | — | +12 |
| `S/session_host/tmux_host.rs` | 207/207 | — | +8 |
| `S/session_host/herdr_host.rs` | 370/370 | — | +8 |
| `S/claude/first_input.rs` | 0/132 | — | +12 |
| `S/codex/tui_session_launch/first_input.rs` | 0/281 | — | +12 |
| `D/inflight/managed_submission.rs` | 473/473 | — | +8 |
| `D/inflight/managed_submission/outcome.rs` | 0/0 | — | +65 |

추정 **16 prod 파일 / +461**. 추가 production 여유3파일/+120 포함 **19파일 / +581**.

### 4b — writable V2·보존·history migration

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `T/queued_head/v2.rs` | 0/124 | — | +15 |
| `T/queued_head/v2/sources.rs` | 0/255 | — | +50 |
| `T/queued_head/v2/commit.rs` | 0/108 | — | +15 |
| `T/queued_head/v2/source_admission.rs` | 0/238 | — | +10 |
| `T/queued_head/v2/retained.rs` | 0/150 | — | +25 |
| `T/queued_head/v2_types.rs` | 0/117 | — | +10 |
| `T/queued_head/v2/migration.rs` | 0/0 | — | +250 |
| `S/tui_input/human_frame/history/v2.rs` | 0/744 | — | +90 |
| `S/tui_input/human_frame/history/v2/migration.rs` | 0/0 | — | +80 |
| `S/tui_input/human_frame/history.rs` | 694/732 | — | +8 |
| `S/tui_input/human_frame/history/compat.rs` | 0/110 | — | +8 |
| `T/queued_head/compat.rs` | 0/38 | — | +8 |
| `S/tui_prompt_dedupe/injected_steer.rs` | 173/238 | — | +15 |
| `T/queued_head/snapshot.rs` | 0/95 | — | +15 |

추정 **14 prod 파일 / +599**. 추가 production 여유3파일/+120 포함 **17파일 / +719**.

### 4c — actor·전체 writer와 제어·boot projection

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `S/turn_orchestrator.rs` | 7181/7181 | G | -80 |
| `T/queued_head.rs` | 933/861 | — | +10 |
| `T/queued_head/v2/actor.rs` | 0/39 | — | +30 |
| `T/queued_head/v2/control.rs` | 0/0 | — | +300 |
| `T/queued_head/v2/projection.rs` | 0/0 | — | +120 |
| `T/pending_queue_persistence.rs` | 1199/1117 | G | +0 |
| `T/pending_queue_persistence/queued_recovery.rs` | 159/197 | — | +50 |
| `T/pending_queue_persistence/snapshot_io.rs` | 0/147 | — | +20 |
| `T/dispatch_reservation.rs` | 979/911 | — | +0 |
| `T/dispatch_reservation/restore_snapshot.rs` | 0/73 | — | +25 |
| `T/queue_enqueue.rs` | 128/128 | — | +15 |
| `T/queue_cancellation.rs` | 186/186 | — | +15 |
| `T/front_requeue.rs` | 149/149 | — | +15 |
| `T/inbound_order.rs` | 267/267 | — | +12 |
| `T/overflow.rs` | 48/48 | — | +8 |
| `T/clear_channel.rs` | 322/336 | — | +15 |
| `T/checked_purge.rs` | 513/513 | — | +20 |
| `T/registry_purge.rs` | 1026/1026 | G | +0 |
| `T/mailbox_observation.rs` | 91/91 | — | +10 |
| `D/commands/control/clear_report.rs` | 240/262 | — | +24 |

추정 **20 prod 파일 / +609**. 추가 production 여유3파일/+120 포함 **23파일 / +729**.

### 4d — A안 실제 submit·FreshTurn·observer 분리

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `D/queue_io/input_owner/target.rs` | 0/0 | — | +220 |
| `D/queue_io/input_owner/effect.rs` | 0/0 | — | +130 |
| `D/queue_io/input_owner/pump.rs` | 0/0 | — | +180 |
| `D/router/message_handler/provider_dispatch.rs` | 936/936 | — | +8 |
| `D/router/message_handler/provider_dispatch/owner_input.rs` | 0/0 | — | +140 |
| `C/owner_input.rs` | 0/313 | — | +20 |
| `S/claude/first_input.rs` | 0/132 | — | +15 |
| `S/claude/herdr_turn.rs` | 929/809 | — | -30 |
| `S/claude/herdr_turn/first_input.rs` | 0/168 | — | +25 |
| `S/codex/herdr_turn.rs` | 751/685 | — | -20 |
| `S/codex/herdr_turn/first_input.rs` | 0/103 | — | +30 |
| `S/codex/absence_launch.rs` | 454/396 | — | +15 |
| `S/codex/tui_session_launch/first_input.rs` | 0/281 | — | +12 |
| `S/claude/tui_session_launch/prepared.rs` | 0/75 | — | +60 |
| `S/herdr_launch/prepared.rs` | 0/66 | — | +15 |
| `S/claude.rs` | 4250/4250 | G | -30 |
| `S/claude/tui_session_launch.rs` | 596/578 | — | +8 |
| `C/hosting/fresh_lifecycle.rs` | 235/181 | — | +8 |
| `S/codex_tui/warm_followup.rs` | 1291/1291 | G | -10 |
| `S/codex_tui/warm_followup/launch_kind.rs` | 0/0 | — | +65 |
| `S/codex_tui/busy_inject.rs` | 396/399 | — | +3 |
| `S/codex_tui/busy_inject/herdr.rs` | 0/0 | — | +100 |

추정 **22 prod 파일 / +964**. 추가 production 여유3파일/+120 포함 **25파일 / +1084**.

Codex Herdr busy child는 A2의 공통 A 거래/판독과 실제 CodexWrites/HerdrTarget을 잇는 adapter 예산이다. UI scanner/stash 거래를 복제하지 않는다. A2가 이 seam을 아직 제공하지 않으면 구현 전에 DESIGN_DELTA로 고정한다. `busy_inject.rs` 인계·export 변경도 A2 착지 뒤에만 한다.

### 4e1 — 모든 ingress·원문 접수와 start 권한

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `D/queue_io.rs` | 353/355 | — | +1 |
| `D/queue_io/input_owner/mod.rs` | 0/0 | — | +100 |
| `D/queue_io/input_owner/admission.rs` | 0/0 | — | +150 |
| `D/queue_io/input_owner/ingress.rs` | 0/0 | — | +120 |
| `D/router/message_handler/intake_turn.rs` | 2541/2473 | G | -100 |
| `D/router/message_handler/owned_turn_start.rs` | 0/0 | — | +240 |
| `D/router/message_handler/input_payload.rs` | 0/94 | — | +20 |
| `D/router/intake_dispatch.rs` | 923/923 | — | +10 |
| `D/router/intake_dispatch/queued.rs` | 493/493 | — | +15 |
| `D/router/intake_queue_transaction.rs` | 1162/1162 | G | -20 |
| `D/router/intake_gate.rs` | 1602/1602 | G | -30 |
| `D/router/intake_gate/queue_effects.rs` | 1528/1528 | G | -10 |
| `D/router/intake_gate/busy_inject.rs` | 398/398 | — | -20 |
| `D/router/message_handler/intake_turn/worker_entry.rs` | 184/184 | — | +10 |
| `D/queue_io/turn_admission.rs` | 302/302 | — | +10 |
| `D/catch_up/api.rs` | 180/180 | — | +5 |
| `D/runtime_bootstrap/queued_recovery.rs` | 322/322 | — | +0 |
| `D/recovery_known_ids.rs` | 343/343 | — | +15 |

추정 **18 prod 파일 / +516**. 추가 production 여유3파일/+120 포함 **21파일 / +636**.

### 4e2 — 준비 wake·원문 보관 notice·mirror

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `D/queue_io/input_owner/mod.rs` | 0/0 | — | +5 |
| `D/queue_io/input_owner/recovery.rs` | 0/0 | — | +120 |
| `D/queue_io/input_owner/notice.rs` | 0/0 | — | +170 |
| `D/queue_io/input_owner/projection.rs` | 0/0 | — | +80 |
| `D/queue_io/backstop.rs` | 738/738 | — | +15 |
| `D/tui_prompt_relay/source_tick.rs` | 217/217 | — | +8 |
| `D/tui_prompt_relay/anchor_completion.rs` | 454/454 | — | +5 |
| `D/tui_prompt_relay/synthetic_start/claim.rs` | 399/399 | — | +6 |
| `S/tui_prompt_dedupe/observation.rs` | 931/931 | — | +10 |
| `S/tui_prompt_dedupe/injected_steer.rs` | 173/238 | — | +10 |
| `S/tui_input/human_frame/history.rs` | 694/732 | — | +10 |
| `S/tui_input/human_frame/history/screen.rs` | 70/80 | — | +5 |
| `S/tui_input/human_frame.rs` | 251/251 | — | +8 |
| `S/message_outbox/identity.rs` | 136/136 | — | +5 |
| `D/dispatch_policy.rs` | 545/545 | — | +5 |
| `D/router/intake_gate/gate.rs` | 253/253 | — | +3 |
| `D/recovery_known_ids.rs` | 343/343 | — | +8 |
| `D/queue_io.rs` | 353/355 | — | +3 |

추정 **18 prod 파일 / +476**. 추가 production 여유3파일/+120 포함 **21파일 / +596**.

### 4f — production root와 다섯 ingress 원자 연결

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `T/queued_head.rs` | 933/861 | — | +0 |
| `T/queued_head/v2.rs` | 0/124 | — | +0 |
| `T/queued_head/v2/commit.rs` | 0/108 | — | +0 |
| `S/tui_input/human_frame/history.rs` | 694/732 | — | +0 |
| `C/owner_input.rs` | 0/313 | — | +0 |
| `D/queue_io.rs` | 353/355 | — | +0 |
| `D/router/intake_gate.rs` | 1602/1602 | G | -15 |
| `D/router/intake_gate/queue_effects.rs` | 1528/1528 | G | -10 |
| `D/router/intake_dispatch.rs` | 923/923 | — | +8 |
| `D/router/intake_dispatch/queued.rs` | 493/493 | — | +8 |
| `D/router/intake_queue_transaction.rs` | 1162/1162 | G | -10 |
| `D/router/message_handler/intake_turn.rs` | 2541/2473 | G | -20 |
| `D/router/message_handler/intake_turn/worker_entry.rs` | 184/184 | — | +8 |
| `D/runtime_bootstrap/queued_recovery.rs` | 322/322 | — | +8 |
| `D/queue_io/turn_admission.rs` | 302/302 | — | +5 |
| `D/queue_io/queued_head_inject.rs` | 169/169 | — | +10 |
| `D/queue_dispatch.rs` | 539/539 | — | +8 |
| `D/queue_dispatch/kickoff.rs` | 419/419 | — | +8 |
| `D/gateway.rs` | 1562/1562 | G | -10 |
| `D/queue_io/backstop.rs` | 738/738 | — | +5 |
| `D/tui_prompt_relay/source_tick.rs` | 217/217 | — | +3 |
| `D/router/message_handler/provider_dispatch.rs` | 936/936 | — | +0 |

추정 **22 prod 파일 / +6**. 추가 production 여유3파일/+120 포함 **25파일 / +126**.

### 4h — #6775 HTTP 델타 수용·기존 origin admission 재사용

| prod 파일 | 현재 raw M/L | giant | 예상 순 코드 |
|---|---:|:---:|---:|
| `src/server/routes/agents_turn_deliver.rs` | 412/412 | — | +0 |
| `src/server/routes/agents_turn_deliver/legacy_origin.rs` | 0/0 | — | +160 |
| `S/imsg_reply/mod.rs` | 337/337 | — | -45 |
| `S/imsg_reply/admission.rs` | 0/0 | — | +145 |

추정 **4 prod 파일 / +260**. 추가 production 여유3파일/+120 포함 **7파일 / +380**.


4b~4e의 새 root/API는 4f까지 `#[cfg(test)]` 경계로 두어 dormant dead_code를 늘리지 않는다. 4a의 registry는 실제 leaf가 참조하고 scope 없는 unleased legacy에는 main 동등이다.

4a는 content fence·typed outcome, 4b/4c는 persistence와 actor, 4d는 실제 A primitive, 4e는 caller와 wake/notice, 4f는 atomic production 연결이다. 4h는 HTTP 독립 델타다. generation/등록은 해당 main의 공식 명령만 쓰며 #7317이 없앤 목록을 되살리지 않는다. 비-production 생성물은 cap 분류와 전체 파일 수를 구분해 보고한다. 새 allow·allowlist·baseline 상향0. production 승격 시 test-only skip pin을 공식 promote 절차로 제거한다.

## 5. 겹침·소유·착지 순서

| 상대 | 접점 | 결정·최소 순서 |
|---|---|---|
| **#7402 미니** | health/turn_deliver/inject*·idle_stash.rs·legacy_submission, submit canary/옛 guarded 제거 | **#7402 먼저**. S4는 여기에 새 core를 쓰지 않는다. 공통 A submit API와 warm/create classification 접점이 필요하면 착지 뒤 export/작은 delta만 별도 FILES. 그 branch를 현재 worktree로 가져오거나 임의 수정하지 않음 |
| **#7330 WSL** | discord/router·recovery, replay start·continuation/withheld 권위 | #7330이 headless/recovery를 소유, #7272가 TUI input owner를 소유. 4e1/4f router·boot 부모 편집은 #7330 착지/소유 인계 뒤. withhold/expired를 새 TUI retry로 우회0. exact replay spend·capacity 기존 API를 소비 |
| **#7317 CI** | CI workflows·nextest·test inventory/PG filter·static checker | 코드/정책 소유는 #7317. S4는 공식 생성/등록만, old source readers/등록 목록이 퇴역하면 새 공식 job/actual selected IDs로 갱신. 수동 workflow 편집0 |
| #7386 조각2(Codex) | Codex busy/idle A primitive | M에는 old post-check가 남음. 4d/4f 전 조각2의 담당/착지/API를 코디네이터가 고정. 같은 제거를 두 레인이 중복 구현하지 않음 |
| #6483 B | Claude missing resume transcript의 명시 fresh 대체 | 원래 S4 흡수 범위 유지. source 지연과 absence 분리. giant parent는 child 위임/감소만, 운영값 조회0 |

이번 leg의 source 수정은 user가 위임한 **main merge의 hand-resolution**이다. S4 소유 선점으로 보지 않는다. 특히 old idle_stash에 추가됐던 S3 시험은 A안 시험으로만 적응하고, #7402가 지울 실행기를 수리하지 않는다.

## 6. 이월 처리표

| 이월 | 현재 확인·A안 보정 | 처리 커밋/단위 | 고유 증명·뮤턴트 |
|---|---|---|---|
| S3d P2-1 callback 없는 content | TmuxInput/CodexWrites 및 generic SessionHost 직접 writer 누락은 남음. 모든 실제 content는 canonical fence 한 곳 통과 | **4a 첫 커밋**, root 승격 전에 닫음 | owner scope actual legacy literal/paste·Herdr·argv별 commit1/content1. 각 leaf callback 제거→RED; load/C-s/Enter/capture는 payload count0 |
| S3d P2-4 TLS fail-open | thread-local 상속을 기대하지 않음. 현재 leased physical/logical target의 scope 없는 writer를 거절, unleased legacy는 main 동등 | **같은 4a 첫 커밋** | 실제 thread::spawn transport가 Err/content0. registry 검사 제거 또는 하위 leaf 우회 mutant RED; scope 없는 일반 caller는 정상 |
| S3d P2-2 경합 거절→보관 | S4에서 닫음. 두 채널 Herdr global key를 canonical endpoint/root/pane/nonce key로 분리. positive no-content 경쟁만 같은 자리 Reserved | 4a 전이 + 4d target, 4f 전에 GREEN | 두 실제 Herdr rig의 동시 cold에서 각각1, 같은 target은 직렬. retain_not_sent로 경합 보내기 mutant RED. writer 살아 있는 동안 lease 강제해제0 |
| S3d P2-3 receipt 영구 누적 | **S5 첫 비용/lifecycle 단위로 이월**. 유실 대신 선형 쓰기 비용인 P2. stable ticket proof 보존이 우선. S4에 write bytes/latency 공개, 진행 방해 실측이면 재분할 | S5 item/retained 부착+terminal 24h 창 | 5000 admission/settle 뒤 items+retained+24h 상한, Pending recover 유지. 정리 제거 mutant RED. unresolved ticket 임의 eviction0 |
| S3d P2-5(a) legacy guard owner 차단 | old guard 재평가를 새로 구현하지 않음. **A안에 따라 auto-lock admission을 제거**하는 #7402 소비. 원문 custody는 별개 | #7402 뒤 4d/4f | legacy guard 값이 있어도 A visible/empty/unreadable 거래가 규정대로 진행. guard 검사 재삽입 mutant RED; old attempted raw 보존 |
| S3d P2-5(b) Codex post-paste epoch 없음 | A안이 post-paste guard를 없앴으므로 **그 권고 폐기**. pre-transaction target/lease 검증만 유지 | Codex A2 뒤 4a/4d | resize/attach/epoch 변화로 paste 뒤 Enter 차단0, wrong target 사전거절과 actual pane gone 분리 |
| S3d P2-5(c) bootstrap 무상한 | 장기 stall은 P2. 새 강제kill/lease 강제회수0. 거래가 끝난 오류/진짜 ambiguity는 원문 보존 후 다음 진행. source/receipt 대기로 제출 거래가 매달리지 않음 | 4d/4e2; 살아 있는 writer 강제 취소는 미도입 | stopped/slow writer의 두 번째 content0. Source None/receipt absent에서도 submit 반환. no-effect 추정 timeout 재전송 mutant RED |
| S3d P2-5(d) 미완 static/native/lint | 옛 native rc1은 합성 inventory 충돌, compile 미실행. 이 leg에서 merge+target+static 다시 실행하고 보고서 실제값만 기록 | 이번 merge; S4 각 단위 lint/static, 최종 PR 전 native | PREPUSH_STATIC_OK와 binary hash/PG 실제값을 별도 확인. Windows/native 미실행을 PASS로 세지 않음 |
| S3c P2-1 실제 helper 합성 | S3d에서 **닫힘**: 네 provider×host helper가 actual owner cold closure 안에 있음. main A 변경 뒤 그대로라고 추정하지 않음 | 이번 S1~S3 재검증, 4d/4f actual A 합성 강화 | 실제 helper effect 안 scoped_source/scoped 유지+content1. submit을 std::thread::spawn으로 옮기면 4a fence에서 content0/RED |
| S3b P2-1 Pending receipt | S3d source_admission ticket로 **닫힘**(bytes==candidate 비교 제거). 새 boot/tick caller도 같은 Custody를 사용 | 이번 회귀, 4b/4e1 실제 접수 | 같은 synthetic raw 두 번·중간 commit·불명 publish 후 recovery에서 원문 두 개/각 ticket1. bytes 비교로 되돌리기 mutant RED |
| S3b P2-3 RetainedOpaque 안내 | S3d OpaqueUnsent+retained_retry_required로 **닫힘**. A 성공 receipt 없음은 이 안내 대상 아님 | 이번 회귀, 4e2 실제 outbox | 확정 미송신은 ‘보관됨/재시도 필요’, true ambiguity만 전달미확인. 성공 receipt 없음 notice0. OpaqueUnsent→Unknown mutant RED |
| 10-26 00:48 ① FreshTurn | request label 대신 actual create/recreate 결과가 정본 | 4a typed kind + 4d 모든 launch leaf; #7402 앞서기0 | followup/warm→create·resume process create·Herdr pending create의 FreshTurn/readiness·first1. existing live+Source None은 create0. old label mutant RED |
| 10-26 00:48 ② 확정 LoadFailed | claim 뒤 content0 확정 실패는 attempt terminal, MayHaveSubmitted 잔존0. 원문은 같은자리 retryable; possible paste/restart 추정에는 적용0 | 4a managed exact-birth outcome child+V2 전이; 4d 실제 A 경로 | fail load-buffer→paste0/Enter0, managed phase와 V2 attempt 모두 확정 미전송 terminal, raw/ticket 유지. failure를 MayHaveSubmitted로 남기는 mutant RED |
| S3a P2-1·P2-2·P2-4 | exact 미진입 proof의 history abandoned·no-op publish·live slot 회수는 여전히 필요. receipt 대기와 무관 | 4b/4e2, S5 비용 측정 | fsync error Err/permit0, no-op fsync0, exact identity 하나만 passive화, restart key count·5001 live slot 회수 |
| S3c P2-2·P2-3·P2-4, S3b P2-2/P2-4(b) | S3d에서 Held/Guarded·MAX_READY_ATTEMPTS, private enqueue, 문서 주석, session restore 성공 시험으로 닫음 | 이번 회귀. obsolete draft-lock 분기는 #7402/A안 기준으로 폐기 | API visibility 확대 source guard RED. actual modal/readiness branch·memento 성공 유지. 없는 old delivery type로 compile 실패를 kill로 세지 않음 |
| S3a P2-5 cache/golden 위생 | encode 실패의 unpublished cache와 golden 생성의 잘못된 GREEN 방지 | 4b history를 손댈 때 | encode Err 뒤 다음 tx가 unpublished state를 못 씀. golden 생성 모드는 의도적 실패, 다음 별도 검증만 GREEN. cache 폐기/생성 fail 제거 mutant RED |
| S2b P2-5·P2-7·P2-8(a,c) | clear 복합 결과·legacy provenance/terminal anchor·no-op tick·notice 실제 loopback은 남은 production 증명 | 4b/4c/4e2/4f | readonly+다른 실패 함께 표시, provider/target 조작0·최초24h anchor, no-op sync0, actual allowed-bot Gateway/catch-up 접수0/사람 대조 |
| S2 호환·등록·control 잔여 | metadata-only·LegacyUnknown·A24·read-only clear 결과는 production 승격 전에 재검증 | 4b/4c/4f | V2 actual golden을 old read-only reader로 읽기/쓰기거절/hash 동일. required job 실제 선택수>0·test-only pin 공식 promote |


S3d가 닫았다고 기록한 S3c P2-1 실제 helper 합성, S3b P2-1 stable admission ticket·P2-3 OpaqueUnsent는 foundation 증거다. **A안 위에서 재검증**하며 그냥 미완료로 되돌려 재구현하지 않는다. S3a history no-op/abandoned, S2b legacy metadata 복원/clear report/production skip-pin 승격은 남아 있으므로 4b/4c/4f에 유지한다. 이전 receipt 대기·자동 draft-lock의 수리 요구는 A안에 맞게 폐기/진단으로 분류하고 묵시적으로 되살리지 않는다.

## 7. 흡수 수용 항목과 S5 경계

### 7.1 #6736 DoD 4줄 — S5 수용 시험

| 원 DoD | S5의 구체 시험·관측 |
|---|---|
| 독립 설계 리뷰 CLEAN 뒤 구현 | 이 r2를 독립 설계 리뷰에 보내고 승인된 단위별 코드 리뷰를 완료. 회수 못 한 첫 WebGPT를 CLEAN으로 세지 않음 |
| sweep 경계의 늦은 live: 주입1·큐 실행0·유실0·중복0 | 실제 Gateway+phase1/phase2에서 sweep가 먼저 M을 접수, 6.6초 늦은 live가 같은 M을 보아도 같은 owner ticket. 실제 paste/Enter1·일반 provider turn fallback0·원 source 보존 |
| paste 뒤·settle 전 crash: 기존 Unconfirmed 창의 중복 상한 | fresh process를 paste 뒤/Enter 뒤/local settle 전 각각 끊어 복원. 가능한 쓰기 경로의 원문 보존/fence로 **추가 자동 content0**을 목표로 하고 기존 창보다 넓히지 않음. receiver receipt로 재송신 허가0 |
| gate 없는 채널·idle 채널 main 동등 | 대상 밖 gate 없는 경로는 M과 같은 bytes/부수효과, 대상 V2 idle는 A안으로 input1. old experiment flag와 새 owner 지원 범위를 나누어 비교 |

이 수용은 #6736의 별도 “자기 큐 단독 항목 빼앗기” 예외를 구현하는 것이 아니다. 처음부터 actor 하나가 source를 접수하여 두 권위 기간을 없앤다. gate 조건을 전역 삭제해 시험을 통과시키지 않는다.

### 7.2 #6775 — HTTP 델타 수용

HTTP는 Discord message-ID 접수로 위장하지 않는 별도 델타다. 외부 Obligated source의 기존 episode/ReplyObligation·author/placement 권한을 보존한다. 물리 target writer fence는 공통이지만 HTTP origin dedupe와 Discord 재도착 fence를 합치지 않는다.

M `src/server/routes/agents_turn_deliver.rs`의 delivery_response는 Unconfirmed를 **200 + delivery=unconfirmed**로 반환한다. enabled iMessage 경로에는 `imsg_reply::deliver_once`의 PG origin reserve/replay가 이미 있다. **이미 있는 기능을 V2 신규 journal로 재구현하지 않는다.** Legacy 경로는 그 wrapper를 건너뛸 수 있으므로, 상태 코드만 5xx/409로 바꾸어 브리지가 다시 보내게 하지 않는다.

4h 권고는 기존 origin admission/cached-response 로직을 작은 child로 추출해 **origin_id가 있는 Legacy HTTP에도 같은 dedupe 경계**를 재사용하는 것이다. ReplyObligation 생성 여부는 원래 source의 권한에 따르며 일반 HTTP에 iMessage reply contract를 강제로 붙이지 않는다. 새 schema·flag·ACK·journal0. 같은 origin+agent/provider/channel/author/source/body는 한 번만 backend에 진입하고, 충돌 payload는 content0으로 거절한다. 옛 row/hash의 호환은 보수적 replay/거절이며 두 번째 전송0이다.

actual axum POST+throwaway PG+real A writer shim으로 다음을 입증한다: 동일 origin 연속/동시 POST, A-success reply, true write ambiguity의 **200 unconfirmed body**, response 저장 전 실패/프로세스 재시작, 동일 origin 재도착에서 총 pane content1. 다른 origin은 각각1. no-origin Legacy는 idempotency를 약속하지 않으며 기존 응답/권한을 유지한다. 기존 API로 origin 보호를 안전하게 일반화할 수 없다면 **DESIGN_DELTA**로 멈추고 HTTP 상태 변경을 하지 않는다. predicate만으로 #6775 DoD를 닫지 않는다. 이 경로는 남아 있는 Legacy 경로로 분류하고 origin dedupe를 임시 조치로 택한다. 실제 델타 수용 뒤 그 근거와 동일-origin pane1 증거를 이슈에 기록하는 일은 코디네이터에게 넘긴다(이 레그 GitHub 쓰기0).

### 7.3 #6679 — race_loss 꼬리 enqueue 판단

**권고: 지원 대상 V2 queued 입력의 꼬리 재큐 경로는 없앤다. shared race_loss helper 전체는 지우지 않는다.** V2는 transition busy 때 head를 새 도착으로 재구성하지 않고 같은 original/source/position을 유지한다. V1에서 이미 dequeue된 경계라면 원 Intervention+lease로 앞에 복원하고 slow backstop만 건다. 새 도착은 기존처럼 뒤로 간다. 즉시 재킥/새 created_at/흡수 source 축소0.

M의 `intake_turn/runtime_transition.rs`→`race_loss.rs`→mailbox enqueue/abandon과 `intake_dispatch/queued.rs`의 원본 복원 경계를 비교한다. 이 router/recovery 파일은 #7330과 먼저 순서를 합의한다. S4는 owner 경로가 이 tail branch를 호출하지 않음을 증명하고, S6 caller census가0인 V2 전용 잔재만 지운다. headless/대상 밖 caller가 남는 일반 helper는 유지하고 main의 기존 결함을 넓히지 않음을 좌표로 보고한다.

S4 PG actual dequeue→transition busy에서 head1이 맨 앞/다음 take도head1, 같은 내용 tail 항목에서도 원문 둘과 source별 실행을 보존한다. 새 도착 busy는 tail 대조. S5의 격리 PA-S8(Claude/Codex, M1/M2/M3, 동일 body·merged source 포함) 반복에서 역전/유실/중복0. 운영 PA-S8·배포는 코디네이터 몫이며 실행하지 않았으면 흡수 종결이라고 하지 않는다.

### 7.4 E-50과 A안 actual ingress

S4는 actual boot/Gateway/catch-up/worker/completion 함수와 실제 PG를 통과하는 격리 rig다. 새 시험 프로세스의 첫 input1/create1/local paste·Enter1 또는 Codex argv1/paste0, 기존 observer의 nonempty logical response delivery commit1을 센다. callback/mock count만으로 E-50을 종결하지 않는다.

S5는 coordinator가 지정한 temp root/DB/socket/Herdr endpoint·실제 CLI로 네 provider×host×fresh/resume/warm-idle/warm-busy의16셀 수용을 돌린다. 첫 Enter가 삼켜지는 경우에도 A안 거래가 자동 재확인/두 번째 Enter를 보내지 않음을 포함한다. 기존 r1의 “Enter-only 자동 재시도 소유자” 제안은 **A안이 Enter1회를 요구하므로 철회**한다. 미응답은 AI observer/운영자 진단이지 input writer가 다시 누를 권한이 아니다. 원문·단일 입력·응답1 여부와 실제 미실행을 분리한다.

## 8. 증명·뮤턴트·마감 계획

- A 판독: before capture1, visible text→C-s1/paste1/Enter1, empty/unreadable→C-s0/paste1/Enter1, modal→content0. 좁은46칸/footer 잘림·attach/resize·판독 실패는 이후 자동 lock0. **post-capture/receipt wait/RecoveryRequired 재도입 mutant가 이 A 고유 단언에서 RED**.
- S3d P2-1/4: 실제 content leaf와 generic SessionHost/argv inventory. scope/lease 누락 thread는 content0, unleased legacy는 M 동등. callback site 각각 제거 mutant RED. callback은 첫 owner payload 직전이고 load/C-s/Enter/capture를 잘못 payload 성공으로 세지 않음.
- FreshTurn: warm/followup/absence/Herdr 경로에서 실제 create leaf를 통과시키고 kind=FreshTurn/readiness 준비→A 거래1. old request label만 쓰는 mutant RED. positive live+SourceNone은 create0 대조.
- 확정 LoadFailed: buffer load를 확정 실패시키면 paste0/Enter0, attempt의 MayHaveSubmitted 없음, raw/ticket 보존. failed result를 possible-submitted로 남기는 mutant RED. paste 요청 뒤 transport 불명은 별도 WriteUncertain이며 자동 retry0.
- Custody: 실제 parent-sync fault에서 NotCommitted/Pending/success를 구분. 동일 synthetic raw의 두 접수, recovery 사이 다른 commit을 넣어도 same-ticket 원문1. Pending→success 또는 새 synthetic admission mutant RED.
- queue/history: 실제 writer golden을 승인된 read-only reader로 읽기/쓰기거절/hash 보존(A24), legacy provider/target 불명 보존, metadata-only boot, identity 한 개만 제거. no-op publish0과 passive slot5001 회수. history 부재/receipt 부재를 재전송으로 바꾸는 mutant RED.
- 실제 ingress: Gateway handle_event, 두 catch-up sweep, Gateway/worker boot, claimed worker/exact retry, gateway completion drain와 queue_dispatch completion 각각 하나 이상. durable 앞 ACK/worker done0, V2 accepted 뒤 old provider fallback0. duplicate wake도 local content1.
- notice: preparation modal/actual write ambiguity/영구 오류 원문 보관만 사용자 안내. receipt absent 안내0. outbox 장애·restart에도 custody 유지, B 입력을 receipt/outbox 성공에 종속시키지 않음. allowed bot notice가 actual Gateway/catch-up에 재섭취0.
- 각 구현 단위는 source/의존성이 바뀐 head에서 target→고유 mutant RED→원복 hash/GREEN→clippy 신규 경고0→공식 source reader→static 순서. compile 실패/0-test/fixture 실패는 mutant kill이 아니다. WSL cargo-shim·throwaway PG·기본 병렬, full-lib 반복/--test-threads=1 금지.
- #7317 이후에는 그때의 실제 required job/run mode/platform/selected ID를 대조한다. 예전 inventory/SQL baseline을 퇴역 상태에서 되살리지 않는다. static PASS와 compiler/Windows native PASS를 구분한다. 이 계획의 마감은 native/운영 수용 완료가 아니다.

## 9. 미확정 [U]와 리뷰 집중 항목

| 이전 [U] | r2의 상태·해소 방법·안전한 기본값 |
|---|---|
| U01 실제 M+L base | **이번 merge로 확정**(§0 SHA). hand hunk·target/static은 보고서 실제값. 다음 구현 때 착지한 main을 다시 merge/review |
| U02 열린 PR/진행 레인 최종 head | #7402/#7330/#7317이 새 소유 기준. §5 착지/인계 전 쓰기0, 과거 7PR 목록을 현재 open이라고 재사용0 |
| U03 운영 flag/채널/host mode 값 | 읽지 않음. 필요하면 coordinator [U]. 새 owner flag0; 최종 root는 코드로 함께 활성화, 운영 config 복사0 |
| U04 promotion reachability·경고 | 4f production/all-target/all-feature clippy·공식 promote 및 actual caller census. dormant helper root는 그 전 cfg(test), 새 allow/baseline0 |
| U05 TLS 누락과 content leaf 추가 | §2와 actual base source census 대조. 4a direct tmux/Herdr/generic host/argv site별 guard·mutant, 표 밖이면 FILES |
| U06 physical alias/Herdr namespace | 4a/4d logical+physical alias, endpoint/pane/root/known nonce 실제 rig. 같은 physical 동시 writer0, serialized witness 단독 신뢰0 |
| U07 lock order/callback 지연 | 실제 branch의 holding set은 구현 때 증명. metadata-only mailbox CAS/disk, actor가 UI/effect 완료를 기다리는 cycle0. stalled lease 강제 해제0 |
| U08 네 조합의 stash capability | **A안으로 보정**: readable text→C-s1, empty/unreadable→C-s0, modal→content0. #7402/Codex A2 뒤 4d와 S5 native. safe-draft 사후 감시로 대체0 |
| U09 prelude/role/CWD/options mapping | 4e1 RawV1/선택 prelude/accepted payload와 effective session params golden/A24. inflight/retry stash 재정의0, permit 직렬화0 |
| U10 voice claim와 durable 접수 crash | 4e1/4f actual PG Gateway/worker sibling claim·crash·same source 재도착. claim row만으로 원문 삭제/성공 ACK0 |
| U11 첨부 수명·원참조 복원 | 4e1 owner 반환/retained/NoContent/restart/clear 뒤 file open/hash. 외부URL 무한 수명 보장0, 새 cleanup/GC0 |
| U12 legacy queue/history 실제 형태 | 운영 file은 안 읽음. 4b/4c array/envelope/metadata-only/attempted/unknown/corrupt fixture. explicit context+raw CAS 외 migration0 |
| U13 abandoned와 cross-file 절단점 | exact Reserved/positive no-content proof-before-forget. history armed marker 부재만으로 미송신0. projection 실패/unknown orphan 보존, 4b |
| U14 notice identity→actual edit | 4e2 기존 outbox/receipt index로 실제 PG+HTTP/restart 합성. A receipt absent notice 제거. 부족하면 DESIGN_DELTA, 새 journal/ACK0 |
| U15 wake 누락/lag | 4e2/4f writer release·startup·coalesce/drop·metadata-only 재열거/ReadyWithoutSource. wake는 재관측만, old receipt timer로 성공 입력 차단0 |
| U16 full-key receipt 교차 축 | **진단/echo 전용으로 보정**. 4d/4e2 SID/file/offset/root/nonce/lateA의 exact 분류, receipt 없음은 제출 재시도/자동 lock 근거0 |
| U17 #6483 B FreshSubstitution | 4d 요청 SID missing과 positive live+SourceNone 분리. 명시 대체/원SID·새SID 안내, 기존 catalog/auth 탐색 범위 유지. 담당과 종결 증거 연결 |
| U18 bootstrap budget | r1의 **120초를 철회**(실측값 아님). 기존 readiness/create의 bounded I/O 외 새 제출 watchdog 없음. writer 끝나기 전 강제 해제/kill0; 장기 stall P2 공개 |
| U19 시각/readback/FS/Windows | 4b parent fsync/old-candidate-third state·새 head cross. UnflushedPlatform permit0. receipt 없음/reader 퇴역/시계 도달로 NoContent 추론0 |
| U20 history/retained/receipt 비용 | 4b/4e2 R=0/1024/5001의 bytes·CPU·publish/fsync·latency 공개. A26 비용 합치기는 S5, migration/prepare 쓰기를 숨기지 않음 |
| U21 receipt 수명 비용 임계 | S3d P2-3 S5 첫 단위 item/retained/24h terminal로 이월. S4 진행 자체를 막는 비용 실측이면 재분할. Pending ticket 임의 eviction0 |
| U22 E-50 native/Auth/CLI 환경 | S5 coordinator가 전용 temp root/DB/socket/endpoint/credentials 지정. 운영 config 복사0. 미실행은 미실행 |
| U23 첫 Enter swallowed/재시도 소유자 | **A안으로 보정**: 자동 Enter-only 재시도 없음. S4 fault rig+S5 native에서 Enter1·추가 content0, 미응답은 observer/운영 진단 |
| U24 런북 상태 실제 노출 | 4e2 구조화 로그에 provider/channel/source/attempt/phase/reason/custody/notice, 본문0. 실제 fixture 상태로 검증; diagnostics 부모 필요하면 FILES |
| U25 rollback binary/운영 절차 | §3 compatible read-only fail-closed+source fence/custody 하한. raw delete/downgrade/자동lock 부활0. 배포 결정은 coordinator |
| U26 required job/PG/nextest | #7317 이후 actual test IDs/path gate/run mode/platform 선택>0 확인. 퇴역 목록·SQL checker 복구0, 공식 생성/등록만 |
| U27 static/native/cross 결과 | 이번 merge target/static 실제값과 종전 native rc1 분리. S4 최종 기능 head의 native/cross는 PR 전 별도, 미실행 PASS0 |
| U28 파일수/순증/giant | 각 실제 단위 base/head cap30/+1500·giant raw0. +1200 또는 예약 초과 전에 PLAN_READY. 단순 부모 import도 예외0 |
| U29 새 Veto/host 오류 | 4a typed exhaustive 분류. 확정 미전송/가능 쓰기/로컬 성공 분리, 새 static 사유는 fail-closed. unread UI를 generic Held로 바꾸지 않음 |
| U30 start 권한/claim/observer seam | 4d/4e1 기존 ReplayStart exact hash/EffectTarget/capacity/continuation과 OriginalLiveBridge를 소비. wrong/refused 권한이면 content0; 범위 밖 FILES |
| U31 Codex Herdr warm-busy | old provider_unsupported를 지우기만 하지 않음. Codex A2 primitive+actual CodexWrites/HerdrTarget을 4d16셀에서 증명. 부족하면 DESIGN_DELTA, unsupported fallback0 |
| U32 #7402 공통 A API/삭제 목록 | 4a/4d 전 최종 source 고정. legacy executor/inject*/idle_stash에 새 S4 core0. 착지 뒤 필요한 export만 FILES |
| U33 Codex A2 담당·착지·API | M에 A2 완료 가정0. 담당 API 인계 전 4d/4f 활성화0. 두 레인의 중복 제거 구현0 |
| U34 확정 LoadFailed terminal API | 4a managed exact-birth 결과와 V2 attempt 모두 종료. 없으면 최소 outcome 확장만 설계 리뷰, 기존 MayHaveSubmitted 방치0/새 schema0 |
| U35 HTTP origin/bridge 실제 retry | 4h 실제 axum/PG+pane에서 stable origin 연속/동시/restart 총1, old hash/mismatch content0. no-origin client는 idempotency 약속0; API 일반화 불가 시 DESIGN_DELTA |

| 리뷰 집중 항목 | 차단 방법·남는 반례 |
|---|---|
| 경계 시각 | Fresh classification과 content 전 callback/actual local completion 시각 구분. receipt30초를 A 입력 성공 판정에 남기지 않음 |
| 경합 | transaction 중 writer 하나, 끝나면 release. 자동 draft lock0, scope 누락 content0. 오래 멈춘 writer 강제 해제0 |
| 옛 형식 복원 | raw·LegacyUnknown 원문·source fence 유지, attempted 재전송0, read-only rollback. old post-check/자동잠금은復元하지 않음 |
| 문서로만 선언한 가드 | actual transport/actual ingress·fresh create leaf·managed LoadFailed phase 시험과 site별 mutant로 증명 |
| 잘못된0/PASS | compile/0-test를 GREEN으로 세지 않음. 공식 merge script 표식 외 JSON/충돌표식도 확인. 새 head/binary hash/PG 실제실행/static을 분리 |

이 r2의 병합 손 해소와 검증은 보고서에, S4 구현·독립 리뷰·native/운영 수용은 후속 단위에 남긴다. S4-r1에서 A안과 충돌하는 보장을 유지한 채 단어만 바꾸지 않는다.
