PLAN_READY 1774fee578ab9f61912f15e92fb74a75933b09b7 S4=4a/4b/4c/4d1/4d2/4e1/4e2/4f first-content-fence→custody/migration→all-ingress atomic activation; no_build

# #7272 S4 계획 — V2 운영 접수·전달·복구 연결

## 0. 기준과 이번 단계의 상태

이 문서는 구현 지시 전 독립 설계 리뷰용이다. 코드, 시험, 등록 목록을 바꾸지 않았으며 빌드·시험·clippy·prepush를 실행하지 않았다. 저장소에서는 `git fetch origin main`과 읽기만 했다. checkout·merge·commit·push는 하지 않았다. 아래 예상 파일은 구현 소유권 승인 요청을 겸하며, 겹치는 파일을 지금 수정했다는 뜻이 아니다.

- **M(진입점·main 좌표)**: fetch한 `origin/main` = `1774fee578ab9f61912f15e92fb74a75933b09b7`.
- **L(승인 foundation)**: 작업 HEAD = `fe084241801630af327bb7edac309358ddcf14b4` / `refs/review/i7272-s3d-r1`. S1~S3d는 main에 아직 없다.
- M/L merge-base = `ef9e9c0097f3a5763ca77ae8a7629f9f65e780e9`. S3d 리뷰 단위 base B = `b27cd6c0dffd9ce1f02ec10d0c819f7bf2a5814a`와 혼동하지 않는다. **구현 전 코디네이터가 foundation과 최신 main을 merge하고 실제 base를 고정한다.** 이 계획의 M/L을 합성된 base라고 부르지 않는다. 생성물 충돌은 공식 생성기로만 푼다.
- 정본은 `design-r2-webgpt.out.md`의 **답변 본문 204~814행**이다. `design-r2-carryover.md`, `plan-s3.md`, S1/S2b/S3a/S3b 이월 및 S3c/S3d 리뷰를 대조했다.
- **S3d native**: PID `3338045`는 이미 종료했다. `logs/s3d-native-postreview.rc` = **1**, `POSTREVIEW_FINISHED head=fe084241801630af327bb7edac309358ddcf14b4 current_head=fe084241801630af327bb7edac309358ddcf14b4 rc=1`. `PREPUSH_OK` 없음. 실패는 `scripts/sql_execution_surface_inventory.json`의 origin/main 합성 병합 충돌이며 **compile steps skipped**다. Linux/Windows native 검증 성공으로 재사용하지 않는다. 프로세스를 종료하거나 다시 띄우지 않았다.
- 열린 PR 7건은 GitHub 연결 도구로 **읽기 전용** 조회했다. 당시 모두 open이다. head·파일 전부는 `logs/s4-open-pr-files.json`, 코드 census는 `logs/s4-entry-census.txt`, `logs/s4-content-call-census.txt`에 보존했다. 운영 설정/서버/DB는 조회하지 않았다.

경로 약어: `S/`=`src/services/`, `D/`=`src/services/discord/`, `T/`=`src/services/turn_orchestrator/`, `C/`=`src/services/claude_tui/`. **표에서 M이라고 표시하지 않은 foundation 좌표는 반드시 L로 표시한다.** `M 없음`인 파일에 main 행 번호를 만들어 붙이지 않는다.

### 0.1 main 드리프트

실행 명령: `git log --oneline b27cd6c0dffd9ce1f02ec10d0c819f7bf2a5814a..origin/main -G 'PaneIdentity|spawn_nonce|composer_admission|inject_report_any_turn|inject_human_input|queued_head|turn_admission|first_prompt|held|reader|mirror|rehydrat|fallback|try_inject' -- src`. 원시 결과: `logs/s4-drift-log.txt`. 아래 다섯 행이 이 명령에 걸린 커밋 전부다. `-G`는 의미적 전수 검증을 대신하지 않으므로 관련 diff도 읽었다.

| M 커밋 | 확인한 변경·현재 좌표 | S4 반영 |
|---|---|---|
| `7b0f33d0f` #7365 / #7327 | `D/watchers/codex_tui_restore.rs:122` rehydrated binding fallback, `D/watchers/lifecycle/restore.rs:17` 복원 | **이미 main 충족. 재구현 0.** idle/runtime가 있고 Source가 늦는 경우를 boot resolver가 소비한다. watcher 복원 실패·Source 없음으로 새 runtime을 만들지 않는다. |
| `4141858ba` #7369 / #7308 | `D/health/o_transition/automatic.rs:251` 이후 Codex retention, `S/tui_o/writer/actor/codex_retention.rs`, `store/spool/retention.rs` | 마지막 활동 7일+종료 근거의 reader 퇴역은 **이미 main 충족**. input NotSent·retained 삭제·execution 부재의 증거가 아니다. 해당 retention 정책 수정 0. |
| `4397b97dc` #7379 | `D/tui_prompt_relay/injected_prompt_policy.rs:291` format_ssh_direct_prompt_notification 및 직접 입력 mirror 소비자 | 전체 줄 미러는 **이미 main 충족**. 첫 줄 요약이나 owner frame 기준으로 되돌리지 않는다. 현재의 escaping·mention 무력화·Discord UTF-16 길이 제한도 보존한다. Discord 입력의 native echo만 exact history로 suppress하고 사람이 TUI에서 쓴 전체 입력은 유지한다. |
| `2fac7a5aa` #7370 / #7337 | doctor prompt API/skill manifest 감사와 bootstrap WARN | 새 owner 준비가 실제 launch에 전달한 instruction/manifest와 기존 doctor 결과를 보존한다. doctor/bootstrap 구현 재작성 0. |
| `1774fee57` #7376 / #7350 | config/outbound 문서 경로·줄 참조 정정 | 문서/참조 검사와 생성 절차를 유지. input receipt/전송 의미 변경으로 해석하지 않는다. |

B..M의 다른 커밋 `f4cb498a8`(#7373 fixture root guard), `227488f6d`(#7360 PG tx 결과/UTF-8 stderr 경계)는 위 패턴에 걸리지 않는다. 새 시험은 전자의 root guard를 사용하고, PG/명령 오류 처리에서 후자의 수리를 되돌리지 않는다. 이 계획은 #7365·#7369·#7379를 새 구현 성과로 중복 계상하지 않는다.

## 1. S4 범위와 정본 보정

논리 6조각의 **조각 4**다. 지원 대상 Discord의 Claude/Codex × tmux/Herdr 입력을 idle·busy·cold 구분과 무관하게 하나의 durable owner로 받는다. Gateway, catch-up 두 sweep, boot, worker/exact retry, completion-drain과 그 backstop을 모두 연결한다. Source가 아직 없어도 검증된 runtime의 `ReadyWithoutSource`를 pump하고, 첫 content 직전 commit부터 30초 확인 창을 센다. live Unconfirmed도 queued와 같은 retained custody로 옮긴다. 새 production 입력이 V2를 처음 기록하기 전에 queue/history의 명시 migration과 모든 기존 writer/삭제 경계의 공존을 완성한다.

S5는 검증된 조합의 legacy fallback 제거와 격리 native 수용, S6는 불용 writer 정리다. S4에서 fallback 코드가 남더라도 **V2에 접수되거나 Held/Pending/Unknown이 된 입력이 일반 턴/다른 host로 빠지는 실행은 0**이어야 한다. non-TUI/headless/Remote/외부 Obligated origin과 실제 명령·consent·placement·replay authority는 기존 계약을 유지한다.

| 정본/기존 계획 | S4 보정·선택 | 이유·완료 단위 |
|---|---|---|
| r2 §7 조각4 16~26파일 / +600~1000 추정 | **8개 리뷰 단위(4a/4b/4c/4d1/4d2/4e1/4e2/4f)**로 분할. §4에서 파일마다 M/L raw와 순증을 다시 제시 | S3 뒤 남은 transport fence·history·legacy migration·모든 mutator와 실제 ingress/notice 비용을 포함한 추정. 옛 전체 추정 안에 맞췄다고 주장하지 않음 |
| S3 owner/receipt/first-input은 시험 hook뿐 | 기본 모델을 재작성하지 않고 필요한 root의 `cfg(test)`만 마지막에 production으로 승격. effect 반환은 `(typed outcome, exact AttemptRef)` | generic `Result<bool>` closure를 production 프로토콜로 승격하지 않음. adapter가 settle하지 않고 owner가 exact 정산. 4d/4f |
| S3d P2-1·4: callback 누락/TLS fail-open | **4a 첫 커밋**에서 실제 content leaf 전체를 target-aware 공통 경계로 통과시킴. leased target의 scope 없는 content는 거절 | 운영 연결 전 완료. TLS를 스레드 간 자동 상속시킨다는 전제 제거 |
| S3d P2-2 경합을 재시도 필요 보관 | live exact NoContent 증거가 있는 lease/epoch 경합은 동일 순서 Reserved로 복귀·재준비 | 일상적인 두 채널 Herdr 동시 입력이 수동 재시도로 멈추면 안 됨. 4b/4d |
| S3d P2-3 receipt 누적 | **S5 첫 단위로 이월**. item/retained/24h terminal 수명에 붙이는 권고 채택 | 현재는 유실 문제가 아닌 선형 비용(P2, 하루100건 가정 연~1.8MB). 첫 운영 연결과 동시에 wire 수명을 바꾸지 않음. S4에서 비용을 실측하고 무한 tick 쓰기는 먼저 제거. receipt 임의 삭제/상한 인상 없음 |
| S3a P2-1·2를 3d 배정했다고 기록 | **미완료로 정정**: L history `v2.rs:258`는 무조건 publish, `:419`는 사라진 등록을 retained로 취급 | 4b에서 proof를 받은 exact registration만 abandoned 처리하고 no-op publish 제거. 배정 기록을 완료 증거로 쓰지 않음 |
| #6483 B: 없는 resume transcript면 조용히 fresh | **S4에 흡수**. 검증된 runtime 부재에서만 `RequestedResumeMissing → FreshSubstitution`을 명시 준비 결과로 만들고 사용자에게 원 SID→새 SID 전환을 알림 | M `S/claude.rs:1576,1593`. 살아 있는 runtime의 Source 지연은 이 분기로 보내지 않음. #7384 A의 catalog/다른 home 탐색은 재구현하지 않음. 4d |
| §2.2 payload 이후30초, bootstrap no-deadline | payload30초 유지. 별도 **bootstrap 준비 상한 120초(계획 제안 상수)**를 두고 초과시 Unknown/retained+notice | 장시간 stall P2-5(c) 해소. thread/host kill·취소·물리 lease 강제 해제는 없음. 늦게 돌아온 bootstrap의 content callback은 active exact CAS 실패로 content0. 4d, §8 실측으로 값 확정 |
| rollback | arbitrary revert/배열 downgrade 대신 **V2 reader·retained·source fence를 유지하는 호환 rollback** | canonical r2 §7 요구. §3.3 |

## 2. 불변식과 상태/소비 경계

1. **I1 custody 우선**: 원문/RawV1·source별 provenance·선택 prelude·accepted voice·첨부 참조를 먼저 보존한다. durable acceptance 전 reaction/checkpoint/PG delivery 완료 없음. NotCommitted/Full/Pending/실행 접수/비실행 원문 보관은 별도 typed 결과다. Pending은 같은 ticket으로 recover하며 synthetic identity로 새 접수하지 않는다.
2. **I2 단일 정본/순서**: 같은 provider/token namespace/channel의 queue V2 actor만 원문/active/retained/terminal을 변경한다. legacy `intervention_queue`는 V2의 미시도 backlog projection이며 두 writer를 동시에 두지 않는다. retained는 실행 큐 길이/priority에 넣지 않는다. [known A,fresh B]는 provenance로 B만 접수하며 opaque legacy는 통째로 비실행 보관한다.
3. **I3 content 단일 경계**: content는 literal/paste/argv prompt/Herdr text·paste다. load-buffer·C-s·Enter·Escape·capture는 content가 아니다. target-aware leaf가 strict disk commit과 같은 exact key를 확인한다. 한 번 소비한 permit은 재실행할 수 없다. callback 재확인은 멱등 metadata 확인일 뿐 두 번째 content 허가가 아니다. content 가능성이 생긴 뒤의 오류/거절/timeout은 NotSent로 낮추지 않는다.
4. **I4 writer/epoch**: logical writer+검증된 physical writer를 함께 보유한다. scope가 없는 다른 thread도 leased target에는 content0. epoch 교체는 old custody·hold·lease 삭제 근거가 아니다. 살아 있는 writer가 끝나야 같은 physical target의 B를 시작한다. transition→writer→composer 순서, callback은 exact metadata CAS/disk만 하고 UI/Source/composer를 기다리거나 재잠그지 않는다.
5. **I5 시각/복원**: payload 직전 확인 deadline을 한 번 정한다. bootstrap 비용·readiness/stash 비용은30초에서 제외한다. 재시작 MayHaveSent/Unknown/retained에 추가 content0, Reserved만 exact 미시도 증거로 재준비한다. no deadline·Source/receipt/ledger 부재·reader retirement는 NotSent 증거가 아니다. settle 시각은 실제 관측 시각, terminal은 그때부터24h다.
6. **I6 exact receipt**: provider/host/root/execution nonce/source SID·file identity·offset/author/source identity/attempt nonce가 맞는 positive native receipt만 해당 attempt를 정산한다. A의 늦은 receipt는 B를 건드리지 않는다. Source/Resolved/SessionStart/Enter/End·Seal만으로 Delivered0. Claude와 Codex가 같은 기준을 충족해야 gate를 연다.
7. **I7 준비/초안**: Empty 또는 안전하게 읽은 Person 초안은 #7314 A1 stash transaction을 재사용해 submit한다. OwnPending/unreadable/unsafe는 typed Held, 원문·초안 보존. 단순 nonempty 또는 HumanAttached만으로 idle/cold를 영구 대기시키지 않는다. Herdr hold release는 별도 proof이며 receipt 성공과 묶지 않는다.
8. **I8 보존/정리**: 30초 확인 실패는 retained+notice intent의 같은 snapshot commit이다. 원문·passive history를 TTL/End/reader 종료로 지우지 않는다. clear/purge/cancel/overflow/restart는 미시도 큐와 attempted custody를 구분한다. main이 하지 않던 kill·cleanup·move-aside 권한은 추가하지 않는다.
9. **I9 projection/notice**: queue가 정본이고 history/in-memory steer/placeholder/known-ID는 복구 가능한 projection이다. projection 실패는 재전송 허가가 아니다. notice는 기존 outbox의 persistent exact dedupe+non-turn provenance, Discord enqueue/전달 실패는 B 진행을 막지 않는다. 늦은 receipt 뒤 해제는 실제 기존 notice edit를 확인한다.
10. **I10 complete routing**: 다섯 ingress 계열과 모든 wake가 같은 owner에 도달한다. supported target의 Held/Pending/Unknown은 다른 provider/host/일반 turn fallback0. final activation은 한 커밋이며 production build와 test build의 선택 경로를 함께 검증한다.

### 2.1 운영 진입점 전수표 (M 기준)

표의 `유지`도 검증 대상이다. route/권한은 그대로 소비하고 그 뒤 local TUI effect만 V2로 수렴한다. 코드 좌표 전수표와 raw census를 함께 사용한다. 단순 wrapper는 같은 행의 named owner에 포함한다. 구현 때 새 caller/enum variant가 생기면 행과 시험을 먼저 추가한다.

| ID·경계 | M file:line | 바꾸거나 소비하는 상태·계약 | 단위 |
|---|---|---|---|
| G1 Gateway | `D/router/intake_gate.rs:300` / `D/router/intake_gate.rs:1234` / `D/router/intake_gate.rs:1658` | bot/auth/command/attachment/voice 분류 뒤 live와 thread/restart/backlog가 같은 custody. busy 선주입 우회 제거 | 4e/4f |
| G2 source/명령 경계 | `D/router/intake_gate/gate.rs:37` / `D/router/intake_gate/gate.rs:107` / `D/router/intake_gate/gate.rs:161` | 운영 notice loopback0·실제 명령과 RawV1/선택 prelude·source merge 경계 유지 | 4e |
| G3 queue effects | `D/router/intake_gate/queue_effects.rs:116` | voice/raw strip 전 original을 중앙 custody에 인계. queued reaction은 durable 결과 뒤 | 4e/4f |
| G4 queue transaction | `D/router/intake_queue_transaction.rs:123` / `D/router/intake_queue_transaction.rs:339` | 원문 생성·accepted/pending/full·checkpoint/reaction 판정. source별 ACK와 접수 실패 원문 유지 | 4e/4f |
| G5 legacy payload 변환 | `D/router/intake_queue_transaction/queue_payload.rs:14` | 기존 V1 voice 제거 전 V2 handoff; RawV1를 재정의하지 않음 | 4e |
| G6 busy/live | `D/router/intake_gate/busy_inject.rs:198` / `D/router/intake_gate/busy_inject.rs:212` / `D/router/intake_gate/busy_inject.rs:274` | 기존 첨부/voice 승인 보존. Unconfirmed도 owner custody; 일반 turn 재접수0 | 4e/4f |
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
| B3 startup watcher | `D/watchers/lifecycle/restore.rs:17` | #7365/#7381 결과 소비. restore 실패/Source None은 runtime 부재 증거 아님; parent 수정0 권고 | 4d/4e 시험 |
| B4 Codex rehydrate | `D/watchers/codex_tui_restore.rs:122` | 이미 main인 positive rehydrated binding을 resolver가 재사용. reader retirement와 input 상태 분리 | 4d/4e 유지 |
| D1 completion gateway | `D/gateway.rs:730` / `D/gateway.rs:485` | provider gate→marker drain 실제 구현 경계(§7 matrix). V2 처리 전 marker/card 제거0 | 4f |
| D2 completion queue | `D/queue_dispatch.rs:218` / `D/queue_dispatch.rs:350` / `D/queue_dispatch.rs:16` | take-next/finalize/card drain도 V2 상태 소비. Unknown을 legacy 일반 turn으로 실행0 | 4c/4f |
| D3 kickoff | `D/queue_dispatch/kickoff.rs:113` / `D/queue_dispatch/kickoff.rs:338` | busy/idle/startup/backstop 모두 동일 state pump. V1 try_busy false를 V2 fallback 허가로 해석0 | 4f |
| W1 enqueue wake | `D/queue_io.rs:221` / `D/queue_io.rs:296` | 중복/경합 wake coalesce, 상태 없는 wake는 재관측만 | 4e/4f |
| W2 deadline/backstop | `D/queue_io/backstop.rs:276` / `D/queue_io/backstop.rs:550` / `D/queue_io/backstop.rs:572` | payload deadline·bootstrap 예산·lag/startup gap 보충, metadata-only enumerate. no-op publish0 | 4e/4f |
| W3 legacy opt-in | `D/queue_io/queued_head_inject.rs:34` / `D/queue_io/queued_head_inject.rs:80` | 현 env gate의 V1 의미와 V2 single path 분리. 운영 값 조회 안 함 | 4f |
| W4 Source/Resolved | `D/tui_prompt_relay/source_tick.rs:23` / `D/tui_prompt_relay/source_tick.rs:47` / `D/tui_prompt_relay/source_tick.rs:111` | #7381 join retry와 같은 이벤트 사용. bind-once/reconcile, Source 없이도 live safe runtime pump | 4e/4f |
| A1 actor 모든 message | `S/turn_orchestrator.rs:1210` / `S/turn_orchestrator.rs:1741` | Enqueue/TryStartTurn/TakeNextSoft/restore/requeue/finish/restart/purge/Injection을 exhaustive audit. V2 mutable projection 이중 writer0 | 4c |
| A2 queue claim/settle | `T/queued_head.rs:237` / `T/queued_head.rs:650` / `T/queued_head.rs:742` / `T/queued_head.rs:824` | V1 owner와 V2 slot 공존; target supported 판정 뒤 단일 writer. root promote 시 legacy destructive gate 유지 | 4c/4f |
| A3 legacy mend | `T/queued_head/mend.rs:144` / `T/queued_head/mend.rs:196` | V1 Unknown receipt only 의미 유지; legacy 증거로 V2 다른 attempt 정산0 | 4d 유지 |
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
| A14 기존 제어 caller | `D/commands/control/clear_report.rs:46` / `D/commands/control/clear_report.rs:65` | #6485와 순서 합의. readonly 경고가 다른 실패/세션 결과를 가리지 않음 | 4c |
| P1 pre-admission | `D/router/message_handler/pre_admission_control.rs:35` / `D/router/message_handler/pre_admission_control.rs:182` | 실제 명령/첨부 세션 custody를 먼저 분류; 일반 raw 문자열 때문에 명령으로 재해석0 | 4e 유지 |
| P2 context/render | `D/router/message_handler/turn_context.rs:19` / `D/router/message_handler/turn_context.rs:43` | #7314 실제 RawV1/선택 prelude 타입 재사용; reply-context 레인 결과 위에 연결 | 4e 유지 |
| P3 준비/기존 session | `D/router/message_handler/intake_turn.rs:155` | role/profile/model/CWD/worktree/native instruction·recovery/prelude 준비 결과 재사용, render1. inflight/retry stash 수정0 | 4e/4f |
| P4 one-shot context | `D/router/turn_start.rs:252` / `D/router/turn_start.rs:271` | 선택 prelude consume/handback 기존 계약 유지. shared WIP/retry stash를 새 owner 저장소로 대체0 | 4e 유지 |
| P5 assets | `D/router/message_handler/attachments.rs:81` / `D/router/message_handler/attachments.rs:186` | 기존 materialize/retention owner 수명에 결합. owner 반환/retain/restart 뒤 원 upload 참조 유지 | 4e 유지 |
| P6 voice | `D/router/message_handler/voice_announcement_route.rs:9` / `D/router/message_handler/voice_announcement_route.rs:62` | accepted voice/PG claim provenance가 original에 남아야 함. sibling claim 두 번 전사0 | 4e 유지 |
| P7 voice intake | `D/router/message_handler/intake_turn/voice_intake.rs:9` | 이미 승인된 announcement를 재claim 패배로 지우지 않음. prepare helper 재사용 | 4e 유지 |
| F1 final provider legacy | `D/router/message_handler/provider_dispatch.rs:215` | 지원 대상 V2 acceptance 뒤 이 legacy 일반 실행 경로 호출0. Remote/headless/외부 origin만 기존 의미 | 4f 시험/유지 |
| F2 live/queued adapter | `D/health/turn_deliver/inject.rs:119` / `D/health/turn_deliver/inject.rs:372` / `D/health/turn_deliver/inject.rs:446` / `D/health/turn_deliver/inject.rs:557` / `D/health/turn_deliver/inject.rs:796` | 기존 busy-only resolver에서 verified runtime/idle/PreSource까지 owner가 판정. non-test Codex gate는 최종 단위만 해제 | 4d/4f |
| F3 cold resume/absence Claude | `S/claude.rs:1576` | #6483 B 흡수. Source missing alone은 fresh 근거 아님. 명시 FreshSubstitution 결과 | 4d |
| F4 Codex 내부 absence 분기 | `S/codex/absence_launch.rs:55` / `S/codex/absence_launch.rs:242` / `S/codex/absence_launch.rs:374` | execute_observed의 preflight/warm-followup/create 내부 분기를 각각 대조. warm submit/first argv와 end-observation 순서 보존; observer 분리 | 4d |
| F5 Claude Herdr | `S/claude/herdr_turn.rs:155` / `S/claude/herdr_turn.rs:380` / `S/claude/herdr_turn.rs:429` / `S/claude/herdr_turn.rs:624` | 실제 hold file/same execution 증거. current wrapper 순서는 보존하고 owner에서는 Source보다 먼저 submit | 4d |
| F6 Codex Herdr | `S/codex/herdr_turn.rs:113` / `S/codex/herdr_turn.rs:363` / `S/codex/herdr_turn.rs:501` | argv payload0/paste1; 같은 nonce hold release와 late source bind. Source 기다리며 첫 입력 차단0 | 4d |
| L1 Claude legacy content | `C/host_input.rs:569` / `C/host_input.rs:633` | TmuxInput literal/paste와 HerdrInput text/paste 모두 canonical payload leaf. compact/tui_relay의 우회도 포함 | 4a |
| L2 Codex legacy content | `S/codex_tui/host_input.rs:242` / `S/codex_tui/host_input.rs:264` / `S/codex_tui/host_input.rs:462` | TmuxCodexInput literal/paste 및 Herdr content를 같은 경계로. giant raw0 | 4a |
| L3 plain tmux | `S/tui_input/submission_tmux.rs:77` / `S/tui_input/submission_tmux.rs:91` | leased target no-scope fail-closed. load-buffer/key-only는 content로 오분류0 | 4a |
| L4 guarded tmux | `S/tui_input/submission_tmux/guarded.rs:167` / `S/tui_input/submission_tmux/guarded.rs:263` / `S/tui_input/submission_tmux/guarded.rs:270` / `S/tui_input/submission_tmux/guarded.rs:334` | same pane atomic guard와 payload commit; stash C-s/Enter 별도 single transaction | 4a |
| L5 busy Claude | `C/busy_inject.rs:378` / `C/busy_inject.rs:603` / `C/busy_inject.rs:852` | Steer만 busy primitive. 기존 stash/ledger·receipt 보존; 모든 content leaf 위임 source guard | 4a/4d |
| L6 busy Codex | `S/codex_tui/busy_inject.rs:119` / `S/codex_tui/busy_inject.rs:178` | physical epoch atomic 조건 추가; NoContent refusal과 possible-content 오류 구분 | 4a/4d |
| L8 generic SessionHost content | `S/session_host/tmux_host.rs:96` / `S/session_host/herdr_host.rs:219` | generic send_text의 direct transport도 canonical target-aware fence 통과. delegated leaf 중복 publish0, unleased legacy 결과 유지 | 4a |
| L7 direct Herdr content | `S/session_host/herdr_gate.rs:425` / `S/session_host/herdr_gate.rs:433` / `S/session_host/herdr_gate.rs:443` / `S/session_host/herdr_gate.rs:457` | 직접 text/paste/line도 fence 통과. key-only/cancel/terminate 권한은 기존 그대로; callback UI/Source 대기0 | 4a/4d |
| H1 guard | `C/composer_lock.rs:144` / `C/composer_lock.rs:174` | known epoch current capture로만 legacy guard 재평가. epoch 교체만 삭제0 | 4a/4d |
| H2 typed draft hold | `C/input/draft_hold.rs:18` / `C/input/draft_hold.rs:84` | DRAFT_GUARDS 밖 typed hold도 독립 유지. #7314 A2 session-aware ownership 소비, 수정0 | 4d 시험/유지 |
| H3 stash transaction | `C/host_input/guarded_submission/idle_stash.rs:405` / `C/host_input/guarded_submission/idle_stash.rs:466` | #7314 A1 실제 C-s/stash/paste/Enter 거래 재사용. Person 안전 초안만 stash; 중복 구현0 | 4d 시험/유지 |
| H4 execution 증거 | `S/tui_prompt_dedupe/binding_context.rs:98` / `S/tui_prompt_dedupe/binding_context.rs:207` | Known spawn nonce/root/session/host 재사용. giant 부모 수정0, Source 대신 runtime 증거 사용 | 4d 유지 |
| Q1 frame/history 모든 consumer | `S/tui_input/human_frame/history.rs:144` / `S/tui_input/human_frame/history.rs:200` / `S/tui_input/human_frame/history.rs:235` / `S/tui_input/human_frame/history.rs:246` / `S/tui_input/human_frame/history.rs:251` / `S/tui_input/human_frame/history.rs:307` | 기존 비-scoped consumer도 V2 readonly snapshot으로 인식. 미래 포맷을 빈 V1로 취급0 | 4b/4e |
| Q2 in-memory ledger | `S/tui_prompt_dedupe/injected_steer.rs:50` / `S/tui_prompt_dedupe/injected_steer.rs:83` / `S/tui_prompt_dedupe/injected_steer.rs:97` | queue→history→in-memory projection. retained count 때문에 새 slot을 막지 않음; proof 없이 withdraw0 | 4b/4e |
| Q3 native prompt observer | `S/tui_prompt_dedupe/observation.rs:148` / `S/tui_prompt_dedupe/observation.rs:169` / `S/tui_prompt_dedupe/observation.rs:244` | existing context/source를 읽고 exact row claim. native TUI human full mirror 보존 | 4e |
| Q4 synthetic claimant | `D/tui_prompt_relay/synthetic_start/claim.rs:32` | owner frame echo를 새 turn/source로 만들지 않음. 진짜 native 입력은 같은 기존 분류 | 4e |
| Q5 native completion | `D/tui_prompt_relay/anchor_completion.rs:85` | first input/late receipt/full mirror와 completion-drain을 연결, 다른 attempt 정산0 | 4e |
| Q6 native mirror | `D/tui_prompt_relay/injected_prompt_policy.rs:125,291` | #7379 full body 유지, source scope 없는 observer라도 V2 recognition 적용. 수정0 권고 | 4e 시험/유지 |
| Q7 native file facts | `S/tui_o/writer/input_facts.rs:79` / `S/tui_o/writer/input_facts.rs:212` | 전체 consumer 회귀: row offset/ownership 유지. retention/reader retired를 receipt/미송신으로 변환0. 수정0 | 4e 시험/유지 |
| N1 notice outbox | `S/message_outbox.rs:230` | 기존 table/전달기를 재사용, 새 ACK/journal/schema0. failure는 intent 재시도만, B 실행 차단0. giant 수정0 | 4e |
| N2 notice provenance | `S/message_outbox/identity.rs:36` | provider/channel/source/nonce/kind persistent dedupe 및 non-turn provenance. 실제 allowed-bot loopback 시험 | 4e |
| N3 session notice | `S/message_outbox/session_notice.rs:178` | 기존 enqueue와 receipt/index로 실제 edit. dedupe-hit를 해제 메시지 전송으로 간주0. 수정0 권고 | 4e |
| A15 기존 명시 제어 caller | `D/commands/control.rs:388,474,516` / `D/commands/restart.rs:184,294` / `D/commands/skill.rs:146,402` | stop/cancel/clear/restart/skill은 기존 권한만. #6485 부모 수정0; child/actor에서 attempted custody를 보호 | 4c/4f 유지 시험 |
| A16 overflow·실제 RawV1 prefix | `T/overflow.rs:14,19` / `T/queued_head/prefix.rs:22,38` | cap은 실행 큐에만 적용. #7314 eligibility/실제 명령 경계 재정의0; 부모 prefix 수정0 | 4c/4f 유지 시험 |
| H5 AI anchor 종료(physical writer와 별개) | `T/lease_release.rs:21,40` | active-turn anchor release는 wake만 유발하며 physical writer 종료 증거가 아니다. native submit 종료/WriterLease::drop은 L v2/attempt.rs:20의 별도 경계에서 재검증 | 4c/4e2 유지 시험 |
| Q8 누락되기 쉬운 history 소비자 | `S/tui_input/human_frame/history.rs:211,226,371,382` / `S/tui_input/human_frame/history/screen.rs:35` / `C/busy_inject/ownership.rs:136,175` | registered/allows_legacy/receipt_row/unique_attempt·screen·composer_owner_in_session도 V2 reader/projection을 소비. source scope 부재로 빈 store 처리0 | 4b/4e2 |
| F7 기존 replay/effect 권한 | `D/router/message_handler/intake_turn/replay_start.rs:26,94` / `D/router/message_handler/provider_dispatch.rs:215,225,256,451` | 기존 ReplayStart/admit/continuation·input hash·EffectTarget·execution_capacity를 owner EffectPermit으로 대체하지 않음. owner child에서 기존 권한을 소비 | 4d2/4e1/4f |

**80개 경계 행**. 부모/leaf와 단순 wrapper를 구분했으며, read-only 소비자·기존 유지 경계도 포함한다. `git grep` content census의 292행은 시험/호출부를 포함한 원시 후보다. 그 숫자를 production writer 수로 보고하지 않는다. 아래 추가 census 범위와 §7의 production source guard로 leaf 누락을 판정한다.

추가 제어 caller: `D/commands/control.rs`, `D/commands/restart.rs`, `D/commands/skill.rs` 및 `D/commands/diagnostics/`는 기존 mailbox/control 경계를 소비하고 직접 수정하지 않는다(§5 #6485). `S/cluster/intake_worker/o_input.rs`와 consent/O placement는 local owner 권한을 우회할 수 없다. shared inflight 모델·retry stash·`C/input.rs`·`C/host_input/` stash transaction·`S/tui_prompt_dedupe/binding_context.rs` 부모는 수정0이다. 계획에 없는 leaf/직접 writer가 실제 base census에서 추가로 나오면 구현 전에 FILES로 고정한다.


### 2.2 foundation 전이/소비점 (M에 없는 계약은 L 좌표)

| L 좌표 | 현재 상태·S4 계약 | 단위 |
|---|---|---|
| `T/queued_head.rs:91,143,495,534` | test-only V2 module/slot/message/dispatch를 production으로 승격. V1 actor handler가 V2를 우회해 파일을 쓰지 못하게 함 | 4c/4f |
| `T/queued_head/v2.rs:25,65,86` | adopt는 곧 commit한다. 운영에서 자동 implicit migration으로 사용하지 않음; explicit migration 뒤만 writable adopt, 접수는 Custody뿐 | 4b/4c |
| `v2/source_admission.rs:55,98` | ticket receipt로 recover, 누적 receipt는 S5 이월. operator에 Pending을 성공/Unknown 실행으로 표시하지 않음 | 4b/4e |
| `v2/attempt.rs:20,45,95,123,191` | lease/arm/payload_started/add_physical. actual content commit을 모든 leaf에 연결, retryable positive refusal 전이 추가 | 4a/4b/4d |
| `v2/retained.rs:4,36,85,113` | tick/settle/retain_not_sent/projection. no-op publish0, live 경합은 Reserved 복귀, retained late receipt exact 정산 | 4b/4d |
| `v2/sources.rs:43` | legacy fake Claude/tmux와 u64::MAX 제거. 아래 migration 계약 적용 | 4b |
| `S/tui_input/human_frame/history/v2.rs:200,258,392,509` | transaction/project/receipt에 no-op·abandoned proof와 projection 복구. snapshot 없는 history를 NotSent로 추정하지 않음 | 4b |
| `D/health/turn_deliver/inject/first_input.rs:77,116,142,152` | production typed bootstrap dispatcher의 재사용 기반. fake bool effect를 운영 callback으로 그대로 노출하지 않음 | 4d/4f |
| `D/health/turn_deliver/inject/queued_owner.rs:15,232,369` | cfg(test) 선택·generic effect에서 운영 typed dispatcher/settler로 옮김. queued/live 같은 owner | 4d/4f |
| `D/health/turn_deliver/inject/queued_provider.rs:123` | 현 exact ReceiptAdapter는 **Codex만** 허용. Claude도 full-key receipt adapter를 구현·합성 시험하기 전 gate 개방0 | 4d |
| `C/owner_input.rs:47,66,98,181` | scope 없는 통과+test-only capture를 target-aware leaf authorization으로 보강. 스레드 이동의 자동 TLS 상속에 기대지 않음 | 4a/4f |
| `S/tui_input/target_epoch.rs:19,51` | 실제 marker/execution/pane 증거의 재검증. serialized epoch 단독으로 LiveEpoch 생성0 | 4a/4d |
| `T/pending_queue_persistence/snapshot_io.rs:4,18,78` | legacy writer 거절을 없애지 않음. V2는 별도 typed actor branch로만 쓰고 legacy reader는 read-only projection | 4c |
| `D/router/message_handler/input_payload.rs:69` | fresh source constructor를 production에서 raw/선택 prelude types와 연결. 기존 reply_context 교체 거절 유지 | 4e |

### 2.3 구체 전이·migration·hold 계약

**명시 migration**: 지원 대상으로 실제 판정된 채널에 대해 actor의 transition 안에서 현재 raw queue/history를 읽고 검증한 뒤 `migrate_v1_to_v2(context, expected_raw)`를 호출한다. context는 provider/token namespace/channel 및 검증된 runtime 증거를 받으며 설정 파일을 읽어 추정하지 않는다. 외국 namespace/버전 미지원/손상/권한 불명은 write0·Held+경로/수동 조치 안내다. **코드 배포 시 전 채널 파일을 일괄 변환하지 않는다.** 첫 supported 접수 또는 boot adoption이 명시 함수를 호출하되, 이 함수가 old format이라는 이유만으로 시도 가능성을 새로 만들지는 않는다.

- queue V1의 Reserved는 원문과 순서를 남기고 재준비한다. head.original과 items가 불일치하면 임의 first를 고르지 않고 raw 보존 Held로 남긴다. items가0이어도 metadata의 attempted original은 버리지 않는다. MayHaveSent/Unknown은 LegacyUnknown evidence를 가진 retained로 이동해 입력0; provider/target을 Claude/tmux로 꾸미지 않는다. 확인 가능한 기존 context는 별도 evidence로만 붙이고 불명 값은 None으로 둔다. unknown enum variant를 새로 넣어 S2 호환 reader를 깨지 않는다(새 evidence는 optional/default field).
- terminal timestamp가 없는 V1 identity는 **최초 migration 시각을 한 번 persist**하고 그 anchor부터24h fence를 둔다. 재시작마다 anchor를 갱신하거나 u64::MAX로 영구 보존하지 않는다. retained에는 이 TTL을 적용하지 않는다.
- legacy Parked는 전체 원문·role·accepted payload를 보존한다. 복원 시 sender/placement 검증은 실행 가능성만 정하며 V2 retained 원문을 필터 삭제하지 않는다. 미시도라고 입증한 것만 queue로, 나머지는 비실행 retained다. pure RawV1 carrier와 의미상 빈 입력을 혼동하지 않는다.
- history V1→V2는 기존 파일 `injected_frames.json`에서 구조적으로 다른 envelope를 사용한다. 같은 history 파일을 쓰는 기존 reader/writer를 **전부** 먼저 호환시킨다. queue commit 뒤 history projection을 복구할 수 있어야 하고, 두 파일 사이 crash는 delivery/미송신 증거가 아니다. 원 raw readback을 검증한 strict file+parent sync 뒤만 채택한다. queue/history 사이 atomic transaction이나 새 journal을 만들지 않는다.
- 이전 S2a/S2b read-only reader로 실제 S4 writer golden(queue와 history)을 읽고 쓰기/삭제 거절 및 raw 보존을 확인한다(A24). 원문 불명 publish의 candidate/old/third-state를 typed recover하며 unknown 버전을 V1로 downgrade하지 않는다.

**미송신 history 수리**: S3a P2-1의 기본 `orphan → retained`를 무조건 `orphan → abandoned`로 바꾸지 않는다. valid queue Reserved를 **버리기 전** owner가 exact key의 미발급/미진입 proof를 만들거나 live `NotSentProof`를 소비할 때만 해당 registration을 passive `abandoned`로 만든다. collision/known 후보에서는 제외하되 이미 소유한 native row 증거는 보존한다. MayHaveSent·unresolved commit·provenance 없는 V1/orphan은 보수적으로 남긴다. **history armed marker의 부재만으로 미송신을 추론하지 않는다.** queue가 정본인 이 선택은 리뷰의 abandoned 권고를 유지하면서 두 파일 사이 arm 절단점을 안전하게 처리한다. Reserved 복원 전에 proof projection이 실패하면 재타이핑보다 Held를 택한다.

**경합 재준비**: `BeforePayload`인 live exact proof를 소비하고, writer effect가 실제 끝나 lease를 놓은 뒤에만 lease busy/epoch changed를 같은 queue 자리 Reserved로 되돌린다. 필요하면 새 nonce로 새 attempt를 만들되 consumed permit을 재발급하지 않는다. 이전 history는 위 abandoned proof로 처리한다. HumanDraft/OwnPending 등은 자동 retry 이유로 합치지 않는다. 가능 content 뒤 거절과 재시작은 이 경로에 들어올 수 없다.

**Herdr hold**: logical provider/root/endpoint/pane/known execution nonce로 physical key를 만든다(전역 conservative-writer 하나 제거). `release_hold`는 기존 API를 쓰되 owner의 새 호출은 **writer 종료 + 같은 execution 재검증 + composer 안전(Empty 또는 기존 A1 transaction으로 안전하게 stash 가능)**을 모두 요구한다. receipt/Source가 없어도 이 증거가 충분하면 release 후 `ReadyWithoutSource` pump가 진행한다. unlink 또는 parent sync 불명은 Released로 꾸미지 않고 hold/notice intent를 보존한다. 전 nonce hold, typed PreparationHold/Herdr fence/DRAFT_GUARDS 밖 hold도 독립 custody이며 epoch 교체만으로 제거0. `ExecutionFence::reopen`을 Source 부재 해결책으로 무조건 호출하지 않는다.

**no-op 비용**: history는 apply의 변화 여부를 구분해 변화 없는 prepare/receipt/project의 encode/publish/fsync를 건너뛴다. owner tick도 strict confirmed bytes와 같은 candidate면 publish하지 않는다. full-byte CAS/읽기·pending recover·projection은 필요한 경우 유지하고 캐시 동일성으로 foreign write를 성공으로 오인하지 않는다. encode 실패는 cache를 버린다. passive/retained는 임의 GC하지 않는다.


**기존 start 권한과 native lifecycle 인계(4d2/4e1)**: 중앙 owner 접수는 provider-start/replay authority를 새로 발급하지 않는다. `owned_turn_start.rs`는 giant intake의 기존 replay check→managed submission boundary→original live-bridge registration→inflight create→stream sender/observer 준비 블록을 추출·재사용한다. mutable inflight 모델/PG schema/retry stash는 그대로다. `provider_dispatch/input_owner.rs`가 **기존 `ReplayStart::spend`의 exact input hash/EffectTarget과 execution_capacity guard**를 소비한 뒤에만 owner의 typed effect를 호출한다. warm busy input은 기존 busy/replay admission의 target 의미를 유지하며 일반 ProviderStart로 바꿔 권한을 우회하지 않는다. owner permit·host CreatePermit·replay start/continuation은 서로 대체할 수 없다.
+
+인계 값 `OwnedAttemptContext`는 원 source IDs·namespace·immutable AttemptRef·prepared frame digest를 명시적으로 가지고 간다. 이것 자체는 권한이 아니며 actor는 exact key/phase를 다시 검증한다. 현재 head를 다시 찾아 A의 늦은 작업에 B의 key를 붙이지 않는다. 준비/claim용 continuation도 같은 key에 한정해 actor가 기존 turn claim을 수행하고, legacy `TryStartTurn`의 order guard를 통째로 해제하지 않는다. 기존 함수 인수/child의 transient context로 전달하며 inflight/retry 저장 형식에 V2 권한 필드를 덧씌우지 않는다. generic bool hook과 naked callback factory는 test-only로 남긴다. production Scope factory는 **소비된 실제 lease/permit와 exact key**를 받아야 하고 임의 callback만으로 Scope를 만들 수 없어야 한다. blocking effect thread 안에 scope를 명시 설치하며 thread/task 이동 누락은 fail-closed다.
+
+네 provider×host에서 fresh/resume/warm-idle/warm-busy **16셀을 실제 ingress 경유로** 검증하기 전 4f 활성화0. 현재 Codex Herdr busy의 `provider_unsupported`를 지우기만 하지 않는다. `codex_tui/busy_inject/herdr.rs`에서 공통 판독을 추출·재사용하고 실제 `CodexWrites`/HerdrTarget submission·exact native turn receipt로 증명한다. 필요한 capability/공통 API가 없으면 generic Held로 숨기거나 임의 tmux로 보내지 말고 **DESIGN_DELTA/FILES**로 멈춘다. 4d는 이 추가 비용 때문에 target/warm/receipt(4d1)와 cold/observer/start 권한(4d2)으로 미리 나눴다.
+
+content 경계가 중첩 위임될 때는 실제 leaf에서 payload-window publish가 한 번만 일어나야 한다. 상위 wrapper에서 allowed를 먼저 세운 뒤 하위 transport 실패를 미송신으로 바꾸지 않는다. human draft stash/복원 부수효과는 기존 A1 거래의 일부로 별도 trace에 기록하고, 이를 새 owner 입력이나 AI receipt로 세지 않는다. history file lock을 보유한 채 mailbox callback을 기다리지 않는다.
+
+S4 production warm도 준비/stash 전에 기존 시험용 `v2_effect(arm(now))`를 호출하지 않는다. 준비 단계는 deadline 없는 Bootstrap으로 보존하고 **실제 content callback에서** payload 확인30초를 시작한다. 이미 준비가 끝난 즉시 경로는 첫 content 직전 arm을 합칠 수 있지만 내구 접수를 미루지는 않는다. canonical A26의 즉시 접수+Reserved→MayHaveSent→settle 3-publish 합치기는 정본 조각5 비용 정리로 남기고, S4는 실제 queue/history/outbox 쓰기 횟수를 분리 보고한다. 추가 bootstrap/prepare commit을 숨겨 A26 PASS로 부르지 않는다.


### 2.4 Veto/NotSent 전수 분류와 wake (r2 P1-1 이월)
+
+M enum 좌표는 `C/busy_inject.rs:51`(11종), `S/codex_tui/busy_inject.rs:27`(17종), `C/host_input.rs:33`, `S/tui_input/submission.rs:10`, `S/session_host/herdr_gate.rs:33`이다. 아래 표는 S3의 전수 그룹을 유지·갱신한 것이다. (가) 준비 대기, (나) 능력 공백, (다) 영구 입력 오류다. **실제 phase가 우선**이며 content 가능성이 이미 생겼다면 어떤 이름의 사유든 자동 NotSent/retry로 낮추지 않는다. 새 enum은 exhaustive match, 미등록 static 사유는 Held/Unknown으로 fail-closed다. generic NotSent 하나로 진행성 문제를 숨기지 않는다.
+
+| 사유 전수 그룹 | 판정·동작 | wake·사용자 표시 |
+|---|---|---|
+| InvalidInput / InvalidPrompt | (다) raw 본문·원첨부·accepted voice가 모두 없거나 UTF-8 frame64KiB 초과/영구 invalid는 원문 보존 Rejected | 명시 원인 notice 뒤 B 진행. attachment-only/voice와 빈 RawV1 carrier를 빈 입력으로 오인0 |
+| LockContended / WriterContended / input_in_flight / MutationBusy / transition_busy | (가) 미진입 live proof이면 같은 자리 Reserved 재준비 | writer/transition release+backstop. “다른 입력 전송 완료 대기” |
+| HumanAttached | busy Steer의 기존 attach 거절 유지. idle/cold는 draft/identity 검사 후 기존 submit | detach 재관측 또는 idle Submit. attach만으로 idle 무한 Held0 |
+| AttachUnknown / PaneUnavailable / CaptureUnavailable / GeometryChanged / NoComposer / Modal / NotReady | (가) UI/target 준비 대기. 서버/실행 부재로 단정0 | typed UI/target 재관측+backstop. “화면/초안 안전 확인 대기” |
+| Draft / OwnDraft | safe readable Person은 기존 A1 ReadyStash, OwnPending/unreadable/unsafe는 (가) Held | Empty뿐 아니라 safe stash 가능 변화도 wake. 초안 bytes를 덮지 않음 |
+| NotBusy | proven idle이면 owner 안 SubmitNewTurn로 intent 재선택 | native idle/readiness wake, 새 busy-turn이나 새 메시지를 기다리지 않음 |
+| TranscriptUnavailable / session_unresolved | positive live runtime+Source 없음은 ReadyWithoutSource/PreSource. unreadable/identity 불명은 (가) | source/host 재관측. 파일 없음만으로 launch/fresh0 |
+| voice_claim_deferred / uploads_not_ready / upload_materialization_failed | (가) accepted claim/materialize 준비 | 기존 PG 승인/asset 완료+backstop. “음성/첨부 준비 대기”, raw strip0 |
+| episode_unidentified / imsg_reply::UNBOUND | 외부 Obligated origin의 기존 거절 | Discord local pane으로 우회0. 범위 밖 기존 안내 유지 |
+| LoadFailed | positive payload 미진입이면 (가) buffer/transport 준비 실패, 가능 content 뒤면 Unknown | bounded 준비 재관측+backstop, “입력 준비 실패, 원문 보존” |
+| UnpredictableRender | geometry 미확정은 (가), 유효 장문/빈줄/fold/control의 미지원은 (나), 실제 영구 invalid만 (다) | 기존 idle/cold primitive/공통 adapter로 증명. capability 공백은 S5 제거 보류, 영구 일반 NotSent0 |
+| NOT_TYPED_LEAD(!, /, <@, DISPATCH:) / queued_not_eligible | 실제 명령은 기존 분류, 일반 유효 payload 형태 제한은 (나) | RawV1/선택 prelude/source 증거로 판정. prefix eligibility 전역 삭제0 |
+| QueueShown | native queue 표시만으로 무기한 busy 대기0. actual composer/intent/receipt capability 판정 | 준비 상태 변화 또는 지원 adapter. native queue UI 자체를 receipt로 삼지 않음 |
+| NotSteerable / TurnUnknown / TurnChanged | 미진입이면 (가) intent/target 재관측; proven idle는 Submit | native turn/binding/readiness wake. runtime absent 추정0 |
+| LedgerFull | (가) active/planned slot 대기. retained/passive 때문에 cap 도달은 lifecycle 결함 | retain/settle projection 뒤 wake. 4096 cap 인상0 |
+| provider_unsupported / Unsupported(HostKind) | (나)/범위 밖 구분을 **ARM 전** 확정 | 대상 밖 기존 결과·local fallthrough0. 지원 약속 조합의 능력 공백이면 활성화 전 DESIGN_DELTA |
+| Unknown / Conflict / IdentityMismatch / holder_changed / queued_author_mismatch | (가) 권한/identity 불명, possible content 뒤면 Unknown/retained | exact binding/row 재조회·필요시 수동 조치. 이름 fallback으로 identity 검사 우회0 |
+| queued_name_unresolved | 표시 이름만 실패하면 ID 표시 fallback. 실제 author/source 불명은 위 Held | 표시 이름 lookup을 무한 필수 준비조건으로 두지 않음 |
+| queued_gate_off / v2_not_prepared / mailbox_unavailable | V1 scope/gate 또는 내부 준비 실패. V2 supported는 준비/원문 보존으로 분리 | 4f 전 root/gate 완성. 기존 env gate가 꺼졌다고 supported V2가 일반 turn 실행0 |
+| queue_nonempty / source_owned / queued_head_reconciled | source 순서/멱등 처분. retained는 실행 backlog 아님 | active 순서 유지·이미 소유한 source effect0, source별 상태 표시 |
+| queued_head_unknown / queued_paste_unnamed | 이미 시도 가능성이 있는 attempt. 준비 NotSent 아님 | positive receipt scan/기존 deadline→retained, 자동 content0 |
+| SubmissionFence | strict commit/권한 거절, 실제 phase별 보존 | 저장소 회복 wake. 첫 payload 전 key0, 뒤면 Unknown. Err를 성공 ACK로 바꾸지 않음 |
+| Confirmation | payload/Enter 뒤 불명은 Unknown | payload+30초 retained+notice, Source/End만으로 성공/reset0 |
+| AdmissionStopped / ExecutionClosing | 기존 명시 cancel/closing 상태 소비 | 기존 제어 권한만, watcher/owner가 취소 source를 자동 재생성0 |
+| RestoreOn / RestoreUnverified / PolicyUnverified | (가)/(나) host mode/policy 확인 불가 | 기존 정책 재검증·backstop/수동 조치. 설정 변경/다른 host force0 |
+| NoStoredExecution / MarkerOtherHost / MarkerUnverified / ServerChanged / PaneUnverified / OtherNonce / RootReplaced / ProviderReplaced | (가) host/execution authority 불명 | exact 증거 재조회. unknown을 absent로 바꿔 새 launch0 |
+| TerminateUnsupportedServer | 입력과 별개인 제어 capability | owner가 kill/terminate로 해결하지 않음 |
+
+prepare-held 이유는 기존 preparation notice/outbox 경계를 재사용해 **한 source/상태에 안정 identity인 사용자 안내**로도 보인다. Unknown “전달 미확인”과 known-no-content/준비 대기를 같은 문구로 합치지 않는다. outbox 장애면 intent를 보존하고 구조화 로그에 pending을 남긴다. notice 전달 성공을 물리 입력/원문 보존의 선행 조건으로 두지 않는다.

## 3. 활성화와 되돌리기

### 3.1 새 flag 없이 최종 한 커밋에서 연결

4a는 owner 없는 legacy caller에 같은 transport 호출·결과를 유지하는 fence 수리다. 4b~4e의 신규 owner API/route/pump는 기존 test-only root 아래에서 실제 진입점 합성 시험을 완성한다. 중간 review ref를 운영 배포 대상으로 삼지 않는다. dormant 코드를 production에 먼저 풀어 dead_code 경고를 만들거나 새 allow로 숨기지 않는다.

**4f 한 커밋**에서 필요한 root를 production으로 승격하고 중앙 route 및 다섯 ingress 소비자를 함께 연결한다. 새 runtime flag·채널 allowlist·운영 ACK는 없다. 기존 placement/consent/host CreatePermit/durable grant는 그대로 필요하다. 대상 밖 경로는 기존 dispatcher를 사용한다. 지원 대상의 준비/저장소 문제는 Unsupported로 바꿔 우회하지 않고 owner Held/AdmissionPending으로 남긴다.

기존 `ADK_QUEUED_HEAD_INJECT`와 live/busy 실험 gate는 **V1 legacy opt-in** 의미로 남길 수 있지만, 최종 supported V2 route/pump는 그 gate가 꺼졌다고 굶지 않는다. `Origin::queued_head_ready`의 Claude-only 제한은 4f에서 두 provider의 V2 준비·receipt/recovery가 갖춰진 뒤 해제한다. S1의 cfg(not(test)) Codex=false 시험은 이 의도된 활성화 시점에 새 production routing 시험으로 교체한다. 기존 `queued_head/prefix.rs` eligibility는 RawV1·실제 명령 경계를 그대로 소비한다. blanket 문자열/eligibility 삭제로 gate를 열지 않는다. 유효 payload의 busy capability 공백은 owner 안 idle/cold Submit primitive로 처리하며 범위 밖 명령을 새 입력으로 재해석하지 않는다.

`AcceptedExecution / DurableRetained / PendingCustody / Held / Rejected / OutOfScope`의 결과를 구분한다. V2 acceptance 이후 일반 턴 호출0을 실제 진입점에서 단언한다. worker의 delivery acknowledgement는 **durable owner custody 이전0**, 이후에는 그 row의 기존 전달 계약에 따른 소유권 이전만 인정하며 AI 응답/Delivered 성공으로 꾸미지 않는다. Full도 성공으로 바꾸지 않는다. 호출자 raw 보존/기존 durable retry가 먼저이며, 영구 거절 보관을 택하면 같은 snapshot에 원문과 사유를 남긴 별도 DurableRetained 결과여야 한다.

### 3.2 pump·wake와 정산

기존 mailbox 및 queue backstop/listener를 재사용한다. 채널별 coalesced wake를 actor가 직렬 소비하고, 외부 receipt/UI/host 관측은 lock 밖에서 한다. 새 polling daemon이나 delivery journal은 없다. wake가 중복되거나 유실돼도 exact key/phase CAS와 기존 slow backstop으로 안전해야 한다.

| wake 원인 | 연결 지점·반응 |
|---|---|
| 새 접수·복원·앞 입력 정산/보관·clear로 큐 변경 | 기존 post-enqueue/kickoff에 owner wake. retained-only/terminal-only도 복원·reconcile/notice 대상이며 실행 backlog는0 |
| payload 확인 deadline/별도 bootstrap budget | 다음 deadline 예약+기존 backstop. A를 원문 보관한 뒤 writer가 끝나고 safe해진 B 진행. 동일 A 재전송0 |
| writer/transition release | target별 재관측 wake. registry lock·actor/composer lock 안에서 provider를 호출하거나 backoff 대기하지 않음 |
| Empty/ReadyStash/NotBusy/guard 재평가 | 같은 capture+known epoch의 typed 준비 상태로 intent 재선택. 단순 capture 문자열/epoch 증가로 hold 삭제0 |
| Source/Resolved/hook/watchers rehydration | 기존 source tick/restore 완료 이벤트를 소비해 bind-once 및 exact receipt scan. pending runtime+Source None은 ReadyWithoutSource; absence launch 금지 |
| attachment/accepted voice 준비 완료·consent/placement 회복 | 원 source와 같은 ticket을 이어 준비. replay/remote authority 재검증. 다운로드·전사·prelude render를 불필요하게 반복하지 않음 |
| turn completion / broadcast lag / startup listener gap | 두 completion-drain과 listener가 같은 owner wake로 수렴. lag/리스너 설치 전 완료는 전체 ready-channel backstop 재관측으로 보충 |

### 3.3 rollback 하한과 런북 전제

V2를 한 번 기록한 뒤에는 S4 전체 revert나 snapshot 삭제/배열 downgrade를 하지 않는다. 최소 rollback binary는 **queue/history V2 read-only compatibility, retained 비실행 의미, 재도착 source fence, metadata-only adoption, 외국 버전 writer 거절, raw inspection 경로**를 포함해야 한다. 권고는 마지막 승인 head의 이 경계와 owner 복구를 유지하고 문제가 난 effect branch만 fail-closed Held로 되돌리는 호환 수리다. S3d binary로 단순 교체하면 read-only 보호는 있어도 운영 진행은 멈추므로 즉시 서비스 복구로 표현하지 않는다.

진행 중 native writer를 중단/kill하거나 lease를 강제로 놓는 절차는 없다. operator는 provider/channel/source/attempt·phase·deadline·hold 이유·notice 상태를 **본문 없는 관측**으로 확인하고, exact receipt 또는 known-no-content 증거 없이 재전송하지 않는다. 초기화/수동 재입력은 기존 명시 명령의 권한만 쓰며 old attempted original을 보존한다. 실제 운영 데이터/설정 조회, 배포, native 런북 실행은 이 계획에서 하지 않았다. 격리 시험에서 그 상태와 관측 결과가 실제 생성됨을 먼저 입증한다(§7).

## 4. 리뷰 단위와 파일별 예산

각 단위는 바로 앞 승인 head를 base로 따로 잰다. 원격 PR은 최종 기능 조각 뒤 하나다. 다음 표는 **예상 code 순증**, raw는 현재 M/L이다. `G`는 어느 ref든1000줄 초과. G는 각 단위와 최종 PR base M 대비 모두 **raw 증가0 이하**다. 부모의 import/delegation도 예외가 아니며 새 파일 권한이 부모 편집 권한을 자동으로 주지 않는다. 부모에서 같은 분량 이상을 자식으로 옮길 수 없으면 구현 전에 FILES/PLAN_READY로 멈춘다. 모든 신규 child는1000줄 미만을 목표로 한다.

### 4a — 첫 커밋: 모든 content leaf와 scope fence

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `C/owner_input.rs` | 0/299 | — | +100 | target-aware payload gate; legacy no-scope/leased-target 분리 |
| `S/tui_input/writer_lease.rs` | 0/0 | — | +170 | 신규: logical/physical registry 및 비대기 lease 공통화 |
| `S/tui_input/mod.rs` | 21/23 | — | +1 | 자식 선언 |
| `T/queued_head/v2/attempt.rs` | 0/197 | — | -25 | registry 추출·기존 permit/lease 보존 |
| `C/host_input.rs` | 1631/1631 | G | +0 | G: Tmux 두 content 메서드 위임; 부모 raw 증가0 |
| `S/codex_tui/host_input.rs` | 1669/1669 | G | +0 | G: TmuxCodex 두 content 메서드 위임; 부모 raw 증가0 |
| `S/tui_input/submission_tmux.rs` | 134/136 | — | +8 | target 식별자를 공통 leaf에 전달 |
| `S/tui_input/submission_tmux/guarded.rs` | 396/450 | — | +8 | atomic guarded payload의 같은 target 확인 |
| `S/codex_tui/busy_inject.rs` | 396/399 | — | +12 | busy paste 조건에 epoch pin 결합 |
| `S/session_host/herdr_gate.rs` | 843/843 | — | +10 | 직접 text/paste/line 위임도 같은 leaf 경계 |
| `S/session_host/tmux_host.rs` | 207/207 | — | +8 | SessionHost::send_text 직접 호출도 같은 target-aware 경계 |
| `S/session_host/herdr_host.rs` | 370/370 | — | +8 | generic Herdr SessionHost::send_text도 lease/scope authority 확인 |
| `C/composer_lock.rs` | 344/371 | — | +24 | 현재 capture의 Settled로만 legacy guard 재평가 |
| `S/codex/tui_session_launch/first_input.rs` | 0/281 | — | +5 | argv content는 planned logical target 경계 |

합계 **prod 14파일 / +329**. 생성·등록 여유 포함 **17파일 / +449**.

### 4b — migration·미송신 history·no-op 비용

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `S/tui_input/human_frame/history/v2.rs` | 0/744 | — | +110 | exact abandoned proof, no-op/encode 오류 cache |
| `S/tui_input/human_frame/history.rs` | 694/732 | — | +5 | scoped withdraw proof 전달·legacy V2 reader 보존 |
| `T/queued_head/v2/sources.rs` | 0/255 | — | +45 | fake provider/target 및 영구 terminal anchor 제거 |
| `T/queued_head/v2.rs` | 0/124 | — | +12 | implicit adopt와 explicit migration 분리 |
| `T/queued_head/v2/source_admission.rs` | 0/238 | — | +10 | Full/Pending 분리·same ticket recovery 보존 |
| `T/queued_head/v2/commit.rs` | 0/108 | — | +12 | confirmed identical bytes no-op; pending/CAS 검증 유지 |
| `T/queued_head/v2/retained.rs` | 0/150 | — | +30 | retryable NoContent 전이·no-op tick |
| `T/queued_head/v2/attempt.rs` | 0/197 | — | +20 | exact live proof 및 재준비 API |
| `T/queued_head/v2_types.rs` | 0/117 | — | +12 | optional legacy evidence/bootstrap budget 시각 |
| `T/queued_head/v2/migration.rs` | 0/0 | — | +240 | 신규: context+raw CAS 명시 queue migration |
| `S/tui_input/human_frame/history/v2/migration.rs` | 0/0 | — | +95 | 신규: 전 history 파일 원문/후보 보존 전환 |
| `T/queued_head/compat.rs` | 0/38 | — | +12 | A24 미래/추가 필드 읽기 전용 |
| `S/tui_input/human_frame/history/compat.rs` | 0/110 | — | +10 | A24 history golden |
| `S/tui_prompt_dedupe/injected_steer.rs` | 173/238 | — | +20 | abandoned 제외, passive/retained 증거 유지 |

합계 **prod 14파일 / +633**. 생성·등록 여유 포함 **17파일 / +753**.

### 4c — actor 정본·기존 writer/제어 공존

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `S/turn_orchestrator.rs` | 7181/7181 | G | -60 | G: actor queue 분기 자식 위임; 모델/부모 raw 증가0 |
| `T/queued_head.rs` | 933/861 | — | +10 | V2 slot/message/snapshot projection seam |
| `T/queued_head/snapshot.rs` | 0/95 | — | +30 | format별 읽기·projection 분리 |
| `T/queued_head/v2/actor.rs` | 0/39 | — | +25 | 한 actor에서 typed queue 요청 수렴 |
| `T/queued_head/v2/control.rs` | 0/0 | — | +320 | 신규: 기존 mutator 위임 포함 V2 제어 분기 |
| `T/queued_head/v2/projection.rs` | 0/0 | — | +140 | 신규: backlog/known-ID/legacy view projection |
| `T/pending_queue_persistence.rs` | 1199/1117 | G | +0 | G: reader re-export/위임만, raw 증가0 |
| `T/pending_queue_persistence/queued_recovery.rs` | 159/197 | — | +50 | metadata-only channel reader |
| `T/pending_queue_persistence/snapshot_io.rs` | 0/147 | — | +20 | V2 actor branch와 V1 write refusal 공존 |
| `T/dispatch_reservation.rs` | 979/911 | — | +0 | restore child 위임, 기존 marker 권한 보존 |
| `T/dispatch_reservation/restore_snapshot.rs` | 0/73 | — | +35 | dispatch marker merge와 V2 source 소유 검증 |
| `T/queue_enqueue.rs` | 128/128 | — | +15 | V2 enqueue는 Custody; V1 branch 보존 |
| `T/queue_cancellation.rs` | 186/186 | — | +15 | 미시도 queue만 명시 cancel |
| `T/front_requeue.rs` | 149/149 | — | +15 | exact source/attempt 순서 복원 |
| `T/inbound_order.rs` | 267/267 | — | +12 | retained를 실행 backlog에서 제외 |
| `T/overflow.rs` | 48/48 | — | +10 | 기존 cap 의미·원문 custody, attempted 제외 |
| `T/clear_channel.rs` | 322/336 | — | +15 | V2 raw/retained 보존 clear 결과 |
| `T/checked_purge.rs` | 513/513 | — | +20 | checked purge의 V2 non-execution 경계 |
| `T/registry_purge.rs` | 1026/1026 | G | +0 | 다른 namespace/version unlink 거절 유지 |
| `T/mailbox_observation.rs` | 91/91 | — | +10 | 실행 backlog와 custody 관측 분리 |
| `D/commands/control/clear_report.rs` | 240/262 | — | +24 | queue-read-only+다른 원인+세션 결과 동시 표시 |

합계 **prod 21파일 / +706**. 생성·등록 여유 포함 **24파일 / +826**.

### 4d1 — target·warm adapter·exact receipt

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `D/queue_io/input_owner/target.rs` | 0/0 | — | +230 | 신규: positive runtime/absent/ReadyWithoutSource·host policy resolver |
| `D/health/turn_deliver/inject/queued_owner.rs` | 267/475 | — | -60 | generic 시험 closure를 운영 dispatcher와 구분 |
| `D/health/turn_deliver/inject/queued_provider.rs` | 0/195 | — | +55 | Claude 포함 full-key receipt adapter, Codex Herdr 선택 |
| `D/health/turn_deliver/inject/queued_reconcile.rs` | 397/465 | — | +50 | native full-key receipt 읽기·late retained reconcile |
| `D/health/turn_deliver/inject/queued_support.rs` | 81/83 | — | +20 | provider/host/execution 일치 |
| `C/owner_input.rs` | 0/299 | — | +20 | Herdr namespace·typed scope/refusal 처리 |
| `S/tui_input/target_epoch.rs` | 0/68 | — | +20 | native target 재검증; serialized→live 추정 금지 |
| `S/session_host/herdr_gate.rs` | 843/843 | — | +10 | 기존 same-execution reads로 release proof 검증 |
| `S/codex_tui/busy_inject.rs` | 396/399 | — | -100 | 공통 busy 판독/전송 seam 추출; tmux 기존 의미 보존 |
| `S/codex_tui/busy_inject/herdr.rs` | 0/0 | — | +250 | 신규: 공통 판독 재사용+CodexWrites/HerdrTarget warm-busy adapter, 새 UI 거래 복제0 |

합계 **prod 10파일 / +495**. 생성·등록 여유 포함 **13파일 / +615**.

### 4d2 — cold·준비 budget·관찰·기존 시작 권한

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `D/queue_io/input_owner/effect.rs` | 0/0 | — | +90 | 신규: typed 네 조합 Submit/Steer/Bootstrap dispatcher |
| `D/queue_io/input_owner/pump.rs` | 0/0 | — | +270 | 신규: state-driven pump·deadline·bootstrap budget·observer 분리 |
| `D/router/message_handler/provider_dispatch.rs` | 936/936 | — | +8 | 기존 ReplayStart/실행 용량 권한을 소비하는 child 위임 |
| `D/router/message_handler/provider_dispatch/input_owner.rs` | 0/0 | — | +140 | 신규: exact owner context+ReplayStart를 소비한 native effect/observer 경계 |
| `D/health/turn_deliver/inject/first_input.rs` | 0/182 | — | +45 | typed outcome와 production factory seam |
| `S/claude/first_input.rs` | 0/132 | — | +15 | 기존 readiness/Held/Guarded 결과 재사용 |
| `S/claude/herdr_turn.rs` | 929/809 | — | -30 | hold 선택/first prompt의 자식 위임 |
| `S/claude/herdr_turn/first_input.rs` | 0/168 | — | +35 | owner ReadyWithoutSource·hold proof 경계 |
| `S/codex/herdr_turn.rs` | 751/685 | — | -25 | source 이전 first submit, 기존 wrapper 순서 유지 |
| `S/codex/herdr_turn/first_input.rs` | 0/103 | — | +40 | own hold release·Source 늦음 observer |
| `S/claude.rs` | 4252/4252 | G | -40 | G: #6483B session-resolution child 위임 |
| `S/claude/tui_session_launch/prepared.rs` | 0/74 | — | +85 | 명시 FreshSubstitution 결과·original SID provenance |
| `S/codex/absence_launch.rs` | 454/396 | — | +20 | warm/first-create 내부 분기를 resolver 증거로 선택 |
| `S/codex/tui_session_launch/first_input.rs` | 0/281 | — | +12 | argv1/paste0 typed create 결과 |
| `S/herdr_launch/prepared.rs` | 0/66 | — | +15 | 기존 host CreatePermit/consent/durable grant 소비 |

합계 **prod 15파일 / +680**. 생성·등록 여유 포함 **18파일 / +800**.

### 4e1 — 실제 접수·기존 start 권한 인계

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `D/queue_io/input_owner/mod.rs` | 0/0 | — | +110 | 신규: 중앙 typed route facade; 아직 test-only root |
| `D/router/message_handler/intake_turn.rs` | 2553/2485 | G | -140 | G: 기존 replay/start/inflight 준비 블록의 child 추출; 새 모델/저장소0 |
| `D/router/message_handler/owned_turn_start.rs` | 0/0 | — | +300 | 신규: 기존 start 체크·inflight 등록 재사용, exact OwnedAttemptContext 명시 인계 |
| `D/queue_io/input_owner/admission.rs` | 0/0 | — | +150 | 신규: Custody/RawV1·accepted original 접수 |
| `D/queue_io/input_owner/ingress.rs` | 0/0 | — | +120 | 신규: 다섯 ingress의 동일 local TUI handoff |
| `D/queue_io.rs` | 353/355 | — | +10 | test-only root+공통 façade |
| `D/router/message_handler/input_payload.rs` | 0/94 | — | +12 | fresh source/selected prelude production용 준비 |
| `D/router/intake_gate/queue_effects.rs` | 1528/1528 | G | -10 | G: accepted original handoff child seam |
| `D/router/intake_queue_transaction.rs` | 1162/1162 | G | -10 | G: durable ack decision child 위임 |
| `D/router/intake_queue_transaction/queue_payload.rs` | 20/20 | — | +8 | voice/raw strip 이전 V2 분기 |

합계 **prod 10파일 / +550**. 생성·등록 여유 포함 **13파일 / +670**.

### 4e2 — wake·notice·전체 consumer projection

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `D/queue_io/input_owner/mod.rs` | 0/0 | — | +5 | 추가 child 선언; 기존 façade 유지 |
| `D/queue_io/input_owner/recovery.rs` | 0/0 | — | +110 | 신규: metadata-only boot·warm wake 재구성 |
| `D/queue_io/input_owner/notice.rs` | 0/0 | — | +180 | 신규: 기존 outbox persistent identity·late actual edit |
| `D/queue_io/input_owner/projection.rs` | 0/0 | — | +90 | 신규: source/echo/notice projection repair |
| `D/runtime_bootstrap/queued_recovery.rs` | 322/322 | — | +0 | metadata-only adoption 시험 seam; 기존 복원 순서 불변 |
| `D/queue_io/backstop.rs` | 738/738 | — | +10 | owner deadline/메타데이터 보유 채널 wake |
| `D/tui_prompt_relay/source_tick.rs` | 207/207 | — | +6 | 기존 rehydration 완료를 typed wake로 소비 |
| `D/tui_prompt_relay/synthetic_start/claim.rs` | 399/399 | — | +8 | exact owner native echo는 synthetic claim 생성0 |
| `D/tui_prompt_relay/anchor_completion.rs` | 454/454 | — | +6 | injected owner 완료→동일 pump |
| `S/tui_prompt_dedupe/observation.rs` | 931/931 | — | +8 | V2 passive/row ownership reader |
| `S/tui_prompt_dedupe/injected_steer.rs` | 173/238 | — | +10 | queue projection과 retained recognition 연동 |
| `S/tui_input/human_frame/history.rs` | 694/732 | — | +10 | legacy 비-scoped consumer도 V1/V2 읽기 분기 |
| `S/tui_input/human_frame/history/screen.rs` | 70/80 | — | +10 | V2 safe composer recognition |
| `S/tui_input/human_frame.rs` | 251/251 | — | +8 | 기존 frame decoder/row API 호환 |
| `S/message_outbox/identity.rs` | 136/136 | — | +5 | non-turn operational provenance 기존 분류 확장 |
| `D/dispatch_policy.rs` | 545/545 | — | +5 | retained notice가 allowed-bot turn으로 loopback0 |
| `D/router/intake_gate/gate.rs` | 253/253 | — | +5 | 실제 intake provenance gate 보존/시험 |
| `D/recovery_known_ids.rs` | 343/343 | — | +12 | retained/24h terminal/source별 known ID union |

합계 **prod 18파일 / +488**. 생성·등록 여유 포함 **21파일 / +608**.

### 4f — 운영 승격·전체 진입점 동시 전환

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 | 내용 |
|---|---:|:---:|---:|---|
| `T/queued_head.rs` | 933/861 | — | +0 | V2 root/slot/message production 승격 |
| `T/queued_head/v2.rs` | 0/124 | — | +0 | production unused fixture API 정리; 모델 재작성0 |
| `T/queued_head/v2/commit.rs` | 0/108 | — | +0 | Fault/test-only 장치 production 배제·no-growth |
| `S/tui_input/human_frame/history.rs` | 694/732 | — | +0 | V2 reader/writer root production 승격 |
| `C/owner_input.rs` | 0/299 | — | +0 | typed scope·proof의 production constructor 승격 |
| `D/health/turn_deliver/inject.rs` | 893/901 | — | +0 | first-input root/queued Codex gate 전환 |
| `D/health/turn_deliver/inject/queued_owner.rs` | 267/475 | — | +0 | typed owner 선택 production 승격 |
| `D/health/turn_deliver/inject/first_input.rs` | 0/182 | — | +0 | generic fixture API는 test-only 유지 |
| `D/health/turn_deliver/inject/queued_provider.rs` | 0/195 | — | +0 | full-key receipt adapter production 승격 |
| `D/queue_io.rs` | 353/355 | — | +0 | input_owner root production 승격 |
| `D/router/message_handler/provider_dispatch.rs` | 936/936 | — | +0 | owner child production 승격; 기존 replay/실행 용량 권한 보존 |
| `D/router/intake_gate.rs` | 1739/1739 | G | -30 | G: live/thread/restart/backlog handoff, giant raw 증가0 |
| `D/router/intake_gate/queue_effects.rs` | 1528/1528 | G | -15 | G: queue commit/ack를 중앙 child에 위임 |
| `D/router/intake_gate/busy_inject.rs` | 398/398 | — | -20 | accepted 뒤 legacy live injection fallthrough0 |
| `D/router/intake_dispatch.rs` | 923/923 | — | +5 | local placement/consent 이후 owner 접수 |
| `D/router/intake_dispatch/queued.rs` | 493/493 | — | +5 | queued prepared payload 그대로 owner 인계 |
| `D/router/intake_queue_transaction.rs` | 1162/1162 | G | -15 | G: durable acceptance 판정 연결 |
| `D/router/message_handler/intake_turn.rs` | 2553/2485 | G | -30 | G: 준비→owner handoff; system prompt/retry stash 불변 |
| `D/router/message_handler/intake_turn/worker_entry.rs` | 184/184 | — | +8 | claimed/exact-retry 실제 local entry 연결 |
| `D/runtime_bootstrap/queued_recovery.rs` | 322/322 | — | +8 | 명시 migration+metadata-only adoption 연결 |
| `D/queue_io/turn_admission.rs` | 302/302 | — | +5 | catch-up 공통 enqueue/recovery handoff |
| `D/queue_io/queued_head_inject.rs` | 169/169 | — | +10 | V2 pump 우선; 기존 V1 opt-in 분리 |
| `D/queue_dispatch.rs` | 539/539 | — | +10 | completion take-next 전에 owner 소비 |
| `D/queue_dispatch/kickoff.rs` | 419/419 | — | +5 | idle/startup/backstop도 같은 owner |
| `D/gateway.rs` | 1562/1562 | G | -10 | G: marker/card drain 전 owner 결과 판정 |
| `D/queue_io/backstop.rs` | 738/738 | — | +5 | production deadline/writer/completion wake |
| `D/tui_prompt_relay/source_tick.rs` | 207/207 | — | +4 | production source-ready wake 연결 |

합계 **prod 27파일 / -55**. 생성·등록 여유 포함 **30파일 / +65**.

여덟 단위의 순차 수정 합산 추정은 **고유 prod 105파일 / +3826**다. 전체 S4를 한 리뷰 단위로 제출하지 않는다. 이는 diff 실측이 아니며, 구현 base 확정 뒤 각 단위에서 공식 cap과 raw 수치를 다시 잰다.

4b/4d/4e는 새 기능을 test-only root 안에서 준비한다. production 승격 때만 쓰일 API가 운영 빌드의 unused 경고로 새지 않게 root gate를 유지한다. 4a의 registry API는 실제 content fence가 참조하고, 기존 legacy 동작은 owner scope가 없고 target lease도 없을 때 같다. 4f에서 새 gate 해제/unused 정리에 필요한 파일이 27개를 넘거나 생성물 여유를 넘으면 활성화 커밋 전에 PLAN_READY로 재분할한다.

4f의 production 승격은 별도 파일로 세었다. 4a~4e에 이미 나타난 파일이라고 4f 파일 수에서 빼지 않는다. test-only root 해제에 따라 durable writer/retained notice 등록을 함께 **promote**하고 skip pin을 공식 절차로 제거한다. 새 baseline/allowlist/allow 인상은 없다. 공식 생성/등록용 여유는 각 단위 **추가3파일/+120**이며 소진하면 다시 PLAN_READY다. +1200에 닿기 전에 더 나누고, 30파일/+1500 안이라고 구현을 계속 밀어 넣지 않는다.

## 5. FILES 겹침과 착지 순서

이 계획은 fetch 당시 M에 대한 계획이다. 구현 시작 전 코디네이터가 착지/소유를 다시 확인하고 최신 main을 merge해야 한다. 겹치는 파일을 무단 선점하지 않는다. 보류 순서는 아래와 같다.

| 레인/PR (조회 시점 head) | 겹침·범위 | 소유·착지 순서와 대안 |
|---|---|---|
| #6483 B / #7384 A | #7272 쪽 `S/claude.rs` session resolution, A는 catalog/session_resume/commands | B는 4d가 소유. #7384 A 착지 후 merge해 SID/CWD/root 결과를 소비; catalog/다른 auth home 탐색0. #6483 코디네이터에 B 흡수·명시 FreshSubstitution 알림/시험을 종결 증거로 연결 |
| reply-context (WSL 수리 중) | `D/router/intake_gate.rs` **직접 겹침**, `intake_gate/reply_context.rs`, `message_handler/turn_context.rs`는 사용만 | reply-context가 먼저 보유·착지. 그 뒤 main merge/동등성 재대조하고 4f giant 위임 hunk만 #7272가 인수. child seam 입력은 실제 RawV1/선택 reply 타입, 별도 reply 조립0 |
| #7378 /system-prompt (WSL r1d) | `D/router/message_handler/intake_turn.rs` **직접 겹침**, 새 commands/system_prompt.rs·migration은 수정0 | #7378 먼저 보유/착지 권고. S4는 session preparation child 결과를 소비하고 기존 native instruction/manifest 시점을 바꾸지 않음. 새 migration/table/schema0. 접점 배정 미확정이면 FILES 후 대기 |
| #6485 막힌 채널 복구 (WSL) | `commands/control.rs`, `commands/restart.rs`, `commands/skill.rs`, `commands/diagnostics/`는 **수정0**. `commands/control/clear_report.rs` child 및 T/control 상태 계약이 의미상 접점 | #6485가 command 부모/diagnostics를 소유. #7272는 4c의 actor/read-only report child만 합의 후 인수. 새 reset/kill/retry 권한을 명령 레인에 요구하지 않음; runtime state projection/보존 계약을 먼저 전달 |
| [#7380](https://github.com/itismyfield/AgentDesk/pull/7380) `4d12802ef9` open | DB agent sync/retention, db/postgres·maintenance·observability; 조회 파일 11개, 계획 prod 교집합 0개 | prod 직접 중복0. 기존 PG/outbox API만 사용; DB/보존 정책 수정0. 공유 생성물은 착지 뒤 regen |
| [#7357](https://github.com/itismyfield/AgentDesk/pull/7357) `eaffabe326` open | policy PG/ops/db_ops·server/cron_catalog·auto_queue/view; 조회 파일 23개, 계획 prod 교집합 0개 | prod 직접 중복0. policy/ops·auto_queue 부모 소유권 요청0. SQL/lib/PG 공식 inventory만 재생성 |
| [#7366](https://github.com/itismyfield/AgentDesk/pull/7366) `7d4b159779` open | managed Herdr config; bootstrap·doctor·startup_preflight·session_host/herdr/observe·provenance 등; 조회 파일 30개, 계획 prod 교집합 0개 | prod 직접 중복0이나 host witness/boot 의미상 접점. managed config/observe/provenance 부모 수정0. 먼저 착지한 API를 4d resolver가 읽기만 소비; 설정 검증 실패를 우회0 |
| [#7372](https://github.com/itismyfield/AgentDesk/pull/7372) `d1351fe855` open | nextest/CI selectors·PG classifier·scripts/CI; 조회 파일 43개, 계획 prod 교집합 0개 | prod 직접 중복0. script/CI 부모를 수동 편집하지 않음. 착지 후 공식 generator와 actual required-job 선택을 다시 확인; 생성물 예약 이외 script 필요하면 FILES |
| [#7381](https://github.com/itismyfield/AgentDesk/pull/7381) `0d13a46403` open | watcherless rehydration retry: `D/tui_prompt_relay/source_tick.rs`, watchers/lifecycle*.rs, tmux.rs; 조회 파일 11개, 계획 prod 교집합 1개 | **source_tick.rs 직접 중복1**. #7381 먼저 보유/착지한 뒤 4e/4f가 완료-event seam만 인수. watcher restore 부모·tmux giant 수정0, 별도 재등록/새 provider launch/cleanup0 |
| [#7383](https://github.com/itismyfield/AgentDesk/pull/7383) `52a18b8bef` open | deploy allow-degraded·배포 scripts/runbook; 조회 파일 13개, 계획 prod 교집합 0개 | prod 직접 중복0. deploy script/runbook 수정0. 이 waiver로 input-owner 수용/PG/native 실패를 통과시키지 않음 |
| [#7384](https://github.com/itismyfield/AgentDesk/pull/7384) `1847180936` open | #6483 A resume catalog·session_resume·commands/session; 조회 파일 11개, 계획 prod 교집합 0개 | prod 직접 중복0. A 먼저 착지하고 B를 4d에서 흡수. 기존 listed SID가 unresumable인 결과와 live runtime Source 지연을 구분 |
| 생성물 공유 | SQL execution surface inventory·lib/PG lane manifest·ARCHITECTURE 생성 절·skip resolver | 각 단위의 production 여유3파일/+120 예약. 공식 생성기의 비-production 산출물은 실제 변경 수를 따로 보고하며 생성 자체에 FILES를 요구하지 않음. production 분류 파일이 예약을 넘으면 코드 추가 전에 재분할. 등록 목록 행 추가 허용을 임의 CI 정책 편집 허용으로 확대하지 않음 |

원시 PR 파일 목록에는 문서·생성물까지 전부 들어 있다. 공유 SQL/lib/PG inventory와 ARCHITECTURE 생성 절은 각 레인 착지 뒤 공식 생성기로 재생성하며 hand merge/수동 행 편집으로 최신 레인의 등록을 버리지 않는다. #7372가 바꾸는 selector/run mode를 고정 전제로 복제하지 않는다. 신규 ID가 required PR job의 path gate/run mode/platform에서 실제 선택되는지 확인한다.

## 6. 이월 P2 처분

P2를 P1로 승격하지 않는다. 아래 '운영 영향'은 수정하지 않을 때의 결과이고, 가정/비용 항목은 그대로 P2다. 완료로 확인된 항목은 아래쪽의 제외 표에만 묶고 재구현하지 않는다.

| 이월 | 고치지 않을 때 운영 영향 | 권고 수리·커밋/단위 | 증명·고유 뮤턴트 |
|---|---|---|---|
| S3d P2-1 content 누락 | 운영 연결 시 legacy Tmux/직접 content가 deadline/allowed 없이 나가 잘못된 NotSent가 될 수 있음 | **4a 첫 커밋**: 모든 InputTransport/CodexWrites literal/paste 및 direct tmux/Herdr/argv leaf를 target-aware 경계로. callback site 목록을 등록/검증 | 실제 run_legacy(Literal)·CodexTransport·direct Herdr·argv의 Started/commit1. 각 leaf wrapper 제거 mutant→그 고유 단언 RED |
| S3d P2-4 TLS fail-open | effect가 타 thread로 옮겨지면 leased target에도 commit 없이 content 가능(현재 dormant) | **4a 첫 커밋**: logical/physical registry 조회로 scope 없는 leased target content 거절. unleased legacy byte/result 동등 | lease A를 둔 다른 thread의 real SubmissionTmux send_literal: Err/content0. registry 검사 제거 및 effect 안 thread::spawn mutant RED |
| S3d P2-2 경합 보관 | 일상적 두 Herdr 채널 중 하나가 수동 재시도 필요로 멈춤 | **4b/4d에서 닫음**: exact live NoContent proof 소비+writer 종료 뒤 Reserved 복귀; Herdr physical namespace 세분화. 재시작/possible content는 제외 | 동일/서로 다른 Herdr execution 동시 입력: 각 content1, 최종 Started/settle. retain_not_sent로 돌리는 mutant RED. 두 logical alias가 같은 physical이면 동시 content0 |
| S3d P2-3 접수 receipt 누적 | 현재 low-volume 전제에서 snapshot 비용이 선형 증가, 유실 없음(P2) | **S5 첫 단위** 권고(item/retained/24h terminal 수명에 연결), S4 운영 연결 전 필수 차단 수리로 확대하지 않음. 단순 cap/시간만 보고 Pending ticket 삭제 금지 | S4 bytes/commit/restore 비용 공개. S5 5000 admit/settle+24h 경계+Pending 재시작: receipt 수≤items+retained+terminal 및 같은 ticket resolve; cleanup 제거 mutant RED. retained를 무시한 고정 cap 주장0 |
| S3d P2-5(a) legacy guard | legacy와 owner 혼합 때 old None-epoch guard가 계속 owner를 막음 | **4a**: composer lock 아래 현재 known epoch/capture의 Settled만 legacy guard 해제. epoch 교체만 지우지 않음 | legacy guard+검증된 빈 composer→Any, OwnPending/Person unsafe→Held. 재평가 제거/epoch만 삭제 mutant 각각 RED |
| S3d P2-5(b) Codex busy epoch | callback 후 pane 교체 때 atomic paste 조건이 약함 | **4a/4d**: 같은 content gate+Codex guarded if-shell에 actual epoch pin 합성 | callback와 paste 사이 replacement pane: content0/old custody 보존. epoch 조건 제거 mutant RED |
| S3d P2-5(c) bootstrap stall | 오래 멈춘 bootstrap이 MayHaveSent로 head를 계속 차지(P2) | **4d**: 별도120초 준비 budget 제안. Unknown/retained+notice로 논리 전이하되 writer/physical lease를 끝난 것으로 만들지 않음 | 가상 clock budget 직전/도달/late helper 복귀: late content0, 동일 physical B는 writer 종료 전0. lease 강제 해제/late callback 허용 mutant RED |
| S3d P2-5(d), S3a/b/c native 위생 | 검증 미도달을 cross PASS로 오인할 위험 | §0 rc1/compile 미도달로 정정. **실제 base merge 후 공식 SQL regen**, 각 static 및 최종 native/cross 실제 결과로 닫음 | PREPUSH_STATIC_OK와 actual native rc0/PREPUSH_OK를 별도 기록. union0·compile skip을 PASS로 세지 않음 |
| S3a P2-1 abandoned 미완료 | 보내지 않은 등록이 retained collision으로 같은 본문 B 확인을 계속 막음(P2) | **4b, gate 전에**: exact Reserved/positiveNoContent proof로만 abandoned; collision/known에서 제외, row ownership은 보존. armed 부재만 신뢰하지 않음 | 리뷰 재현 reserve→prepare→Reserved restart→같은 본문 새 nonce B→receipt Some(true), content1. abandoned→retained 및 may-have-sent→abandoned mutant 각각 RED |
| S3a P2-2 no-op history 미완료 | row 수×전체 history 게시/fsync로 scan 지연 증가 | **4b**: unchanged apply는 publish0·encode cache error 폐기. projection repair는 계속 | miss receipt/duplicate prepare/unchanged project의 durable probe0·inode/mtime 동일. 매번 publish 복원 mutant RED |
| S3a P2-4 MayHaveSent PG 절단점 | 옛 PG 시험은 tick 후 retained만 복원해 re-arm 거절 표현이 과장됨 | **4e/4f 실제 boot PG**: tick 없이 MayHaveSent에서 새 프로세스 복원, Reserved/Bootstrap/Started/Unknown/retained 각 절단점 분리 | MayHaveSent→Reserved mutant가 실제 추가 content count에서 RED. Reserved의 허용된 첫 content와 나머지 추가0을 별도 이름/로그로 기록 |
| S3a P2-5(a,b) cache/golden | encode 실패 cache와 fixture 자동 갱신이 잘못된 PASS를 만들 수 있음(P2) | **4b** 손대는 history에서 encode Err cache 폐기, golden update 모드는 생성 후 의도적 실패; 다음 별도 검증만 GREEN | encode fault 뒤 다음 tx가 unpublished state를 못 씀. ADK_UPDATE_HISTORY_GOLDEN 모드 fail/일반 모드 fixed hash. cache폐기·생성fail 제거 mutant RED |
| S2b P2-5 clear report | queue_read_only 하나가 다른 저장 실패/실제 세션 초기화 사실을 가림 | **4c** child에서 모든 원인·원문 보존·실제 세션 결과를 함께 표시. 부모 control/restart 정책은 #6485 소유 | queue_read_only+legacy_pending_discard+HerdrHeld/Retired/stop failure 조합. early-return 복원 mutant RED |
| S2b P2-6 skip pin 승격 | test-only skip membership만으로 production writer가 audit에서 빠질 수 있음 | **4f 같은 커밋** 공식 --promote/--sync-skips로 root 이하 신규 writer/retained notice를 actual reachable census에 편입 | 각 승격 module의 skip 잔존 guard RED 및 writer inventory 누락 시 static RED. baseline/allowlist 추가0 |
| S2b P2-7 legacy restore | provider/target 위조와 u64::MAX terminal은 exact reconcile/수명 왜곡(P2) | **4b** explicit context/LegacyUnknown optional evidence, 최초 migration anchor persist. 기존 raw original 불변 | V1 Codex+미상 host fixture→codex 또는 None+legacy evidence; adopt+24h 경계 만료, 재시작 anchor reset0. 고정 Claude/tmux·MAX 복원 mutant RED |
| S2b P2-8(a) no-op tick | production wake마다 R개 retained 전체 fsync | **4b** unchanged confirmed bytes면 publish0, project_slots는 유지 | tick 여러 번 durable probe0·inode 동일, projection 누락이면 repair. no-op gate 제거/repair 생략 mutant 각각 RED |
| S2b P2-8(c) allowed bot loopback | predicate 시험만으로 실제 notice가 새 turn이 안 됨을 증명하지 못함 | **4e/4f** 실제 notice producer→outbox render→Gateway intake 및 phase2 catch-up gate 합성 | allowed announce bot 실제 intake에서 admitted0/content0; 같은 문자열의 진짜 사람 입력은 기존 의미. provenance 제거 mutant RED |
| S1 P2-3 Codex Herdr unsupported | legacy adapter가 ARM 뒤 capability 거절하면 불필요한 Unknown | **4d** capability/intent 선택은 arm 전. supported idle/cold는 실제 Herdr primitive; 아직 불가능한 busy capability는 owner 안 준비/intent로 명시 | Codex Herdr cold argv0/paste1 + positive receipt. capability 거절이면 permit0/MayHaveSent0. ARM 뒤 거절 이동 mutant RED |
| r2 P2-1 확인 시각 | cold 준비가 확인30초를 소모하면 거짓 미확인 보관 | S3d 기본 수리 유지, **4d/4f** 실제 ready/stash/argv 경계와 자율 deadline 연결 | slow bootstrap/stash 후 payload+30s, clock 경계·restart no-deadline은 NotSent0 |
| r2 P2-2 retained 비용/종료 | TTL/End만으로 evidence를 지우거나 비용을 숨길 위험 | **4b/4e** no-op 비용·recognition 비용 계측, 종료는 positive receipt 또는 기존 명시 operator disposition만. receipt 누적은 위 S5 | R=0/1024/5001·같은 본문/query순서·full-reader 회귀. retained TTL drop mutant RED |
| r2 P2-3 같은 본문 retained | 기존 A의 불명 row와 B를 query순서로 임의 정산할 위험 | **4b/4d** abandoned proof와 진짜 ambiguous retained 분리; B 등록/전달은 허용, receipt ownership 모호하면 둘 다 Pending | Aretained/B동문/Cquery순서 교차: B content1, 임의 first-wins0, native row 단일 owner |
| r2 P2-4 absence 내부 / P2-5 누락 경로 | cold/warm/observation 또는 completion 경로에서 owner 우회 가능 | **4d/4f** §2.1 F4와 두 completion/worker/catch-up을 실제 진입점별 증명 | 각 branch의 create/argv/paste/observer/normal-provider 호출 횟수. branch-specific bypass mutant RED |
| r2 P2-6 Parked | old opaque envelope를 미시도로 추정해 자동 재전송할 위험 | **4b/4e** RawV1 provenance 기반 migration, exact unattempted만 queue, opaque는 whole raw retained+retry-required | 혼합 [known A,fresh B], pure RawV1 carrier, legacy opaque fixture hash 불변·추가 content0 |
| r2 P2-7 paste 잔재 draft | Draft라는 generic 사유로 진행 불가 원인이 숨음 | **4d/4e** typed Hold reason/phase·safe Person stash vsOwnPending 대기와 wake를 구조화 로그로 표시 | afterPasteOwnPending은 재타이핑0, human draft safe는 기존 stash transaction1, state reason 관측 |
| r2 P2-8 in-memory ledger | 재시작 passive projection 누락/4096 slot 고갈 | **4b/4e** queue→history/in-memory repair, active cap4096만 유지; retained/passive recognition은 보존 | 같은 source 5001 registration/receipt/slot-return 루프 및 restart 후 native replay. cap 인상·passive TTL 삭제 mutant RED |
| r2 P2-9-1/2/3/4 | 호환 reader·adapter settle·다른 레인 순서·history 이월 | S2/S3 구현은 유지. **4b A24/4d typed identity/4f 원자 배선**, 종료 봉인/reader 퇴역 main 결과 재사용 | old reader write refusal/실제 writer golden·late A≠B·no Source→fresh. 임의 old code 복원0 |

### 6.1 이미 닫힌 항목 — 재구현하지 않음

| 항목 | 확인 증거·S4에서 보존할 시험 |
|---|---|
| S2b P2-1 strict directory sync / P2-3 first 검증 뒤 remove(0) / P2-4 borrow+typed custody | S3a/b 및 L `v2/commit.rs`, `retained.rs:16,78`, `source_admission.rs`. 정상 sync 실패 permit0·동일 identity 두 원문·Pending synthetic 중복 시험을 실제 ingress 회귀에도 포함 |
| S2b P2-2 실제 settle clock | S3d 리뷰 §이월 확인, L `inject/queued_owner.rs:416`. 실제 observer 완료 시각/late retained receipt의24h 검증 유지 |
| S3a P2-3 접수 실패 타입 / S3b P2-1 stable ticket / P2-2 private enqueue / P2-3 OpaqueUnsent | L `source_admission.rs:55`, `v2.rs:65`, `v2/retained.rs`, S3d 리뷰 확인. byte 동일성만으로 Pending 확인하는 옛 방식 복원0, Custody 바깥 production enqueue0 |
| S3b P2-4(a,b,c) reply 보존·memento restore 성공 분기·voice 주석 | S3c/3d 리뷰 확인. L input_payload 기존 reply 충돌 거절, session restore 성공 대입 회귀, accepted voice sibling 경쟁 보존 |
| S3c P2-1 네 실제 submit helper / P2-2 Held/Guarded/readiness 소진 / P2-3 visibility / P2-4(a,b,d) | S3d 리뷰 확인. 기존 helper 안 scoped_source/scoped 유지 시험은 재사용하되 production TLS 누락 방지로 확대. 문서 이동/ratchet 설명을 기능 수리로 다시 계상하지 않음 |
| S1 P2-1/2 Recorded::matches·재시작 고유 뮤턴트 / P2-4 typed attempt | S2a/S2b 이후 승인 증거와 S3 exact completion 유지. S4의 다른 SID/file/offset/nonce/author/target 교차 축마다 고유 단언을 추가 |
| S2b P2-8(b) giant 시험 모듈 분리 | S3d 리뷰가 ID/manifest/CI 갱신 확인. 새 회귀는 새 child, 옛 ID를 되살리지 않음 |

위 표의 '닫힘'은 해당 foundation 구현/리뷰에 대한 기록이다. S4 source/head가 바뀐 뒤 증거 재사용 조건을 충족하지 않는 실행 결과를 새 head의 GREEN으로 옮겨 적지 않는다. S3a/b/c의 native '미완료' 위생은 현재 §0의 **rc1/compile 미도달** 결과로 통합하고 최종 head에서 재검증한다.

## 7. 증명 계획

이번 계획 단계에서 실행한 시험은 **0**이다. 아래는 구현 단위의 수용 기준이다. 각 단위에서 대상 모듈 이름 필터로 실행하고 lib 전체를 lane 안에서 반복하지 않는다. PG는 throwaway wrapper(`AGENTDESK_REQUIRE_PG=1`)를 사용하고, cargo-shim 경유·기본 병렬을 유지한다. --test-threads=1, 운영 PG/포트15432, 실제 AgentDesk tmux는 쓰지 않는다.

### 7.1 RED/GREEN·고유 뮤턴트

| ID·단위 | 시험 대상 | GREEN 수용·실제 진입 | 원인 제거/안전 뮤턴트 |
|---|---|---|---|
| T01 /4a | content leaf 전체: Claude/Codex TmuxInput·SubmissionTmux·GuardedPane·두 busy Attempt·Herdr direct text/paste/line·generic SessionHost send_text·argv | 실제 transport shim/HerdrRig로 commit-before-key, Started1/content1; load-buffer/C-s/Enter/capture commit0 | leaf별 callback 제거→해당 고유 단언 RED. transport executor 자체를 mock해 count만 올리는 시험0 |
| T02 /4a | leased target thread 이동·logical alias·unleased legacy | 다른 thread는 Err/content0; owner scope 안 실제 helper는 scoped/scoped_source 유지; unleased legacy bytes/result 동일 | registry check 제거 또는 effect 안 real submit을 thread::spawn으로 이동→RED. TLS 상속 가정0 |
| T03 /4a | epoch 교체와 legacy/typed hold | callback↔paste 교체 pane content0. legacy guard Settled만 재평가; preparation/Herdr fence old custody 보존 | atomic condition 제거, epoch변경만 guard삭제, safe Person을 무조건Held로 바꾸기→각 RED |
| T04 /4b | strict normal publish 실패·불명 old/candidate/third-state | 실제 durable_probe ParentSync에서 Err/permit0; Pending caller raw/ticket 유지, 같은 synthetic 원문 recover 뒤1개 | sync Err를Ok, Pending을NotCommitted 또는 성공으로 바꾸기, Custody 밖 enqueue 노출→고유 RED |
| T05 /4b | Reserved/KnownNoContent abandoned | 리뷰 재현 동문 B의 실제 receipt Some(true); old row ownership은 보존, unknown orphan은 계속 모호 | abandoned→retained RED, MayHaveSent를 proof없이 abandoned→separate safety assertion RED |
| T06 /4b | no-op tick/history·encode 오류 | inode/mtime+durable write probe로 no-op publish0; queue/history projection은 복구. encode 오류 뒤 raw/cache 동일성 | 무조건 publish 복원, no-op에서 projection 생략, cache 초기화 생략→각 RED |
| T07 /4b | V1/V2 golden·LegacyUnknown·terminal anchor | 실제 writer golden을 승인 S2 reader로 inspect ReadOnlyV2; save/clear/snapshot_io guard별 raw hash 불변; migration24h+restart | old reader가V1로쓰기 허용, clear if-let guard 제거, snapshot_io guard 줄 제거 각각 독립 RED. fake Claude/tmux/MAX 복원 RED |
| T08 /4c | 모든 actor mutation·closed registry | Enqueue/TryStart/TakeNextSoft/requeue/restore/finish/restart/clear/purge/overflow의 V2 branch exhaustiveness, attempted raw 불변 | 하나씩 V1 branch로 보내기·unknown meta를empty로복원·retained-only GC로 잊기→해당 ID RED |
| T09 /4d | 네 실제 bootstrap+observer·positive receipt | legacy wrapper 본문/부수효과 동등 + owner 안 real helper, Claude full-key/Codex submitted가 exact A만 settle | Claude 고정·Codex Herdr unsupported·receipt무시·adapter 직접settle·argv+paste중복→각 RED |
| T10 /4d | Herdr 동시성/ReadyWithoutSource/hold release | 실제 HerdrRig 두 실행: 둘 다 각 content1, global lease 없음. Source없이도 writer종료+sameexecution+safecomposer면 B 진행 | NoContent→retry_required, receipt없으면hold영구유지, writer끝나기전release→각 RED |
| T11 /4d | bootstrap120s와 payload30s·late result | payload window가 시작된 경우 bootstrap timer는 못만짐. bootstrap budget초과 retained, 늦은 callback content0. 완료 clock24h | deadline을arm시작으로, timeout에lease해제, stalecallback 허용, settlement armclock 사용→각 RED |
| T12 /4e | source별 원문/선택 prelude·voice·첨부 수명 | 실제 PG accepted voice+동시 sibling claim, [known A,fresh B], rawreply/selected/WIP/render1 golden, owner반환/retain/restart 뒤 fileopen/hash | voice strip 전 handoff제거·helperbytes변경·retention복사제거·reply덮어쓰기→각 RED |
| T13 /4e | 모든 history/echo consumer | 부하5001 active-slot 회수 + passive/retained same-row ambiguity와 query순서 + #7379 사람 fullmirror | retainedTTLdrop/firstquerywins/firstline요약복원/active+passive cap동일→각 RED |
| T14 /4e | notice 상태와 실제 loopback | atomic retained+intent, outbox down/recover/duplicate/restart, late receipt시 실제 warning edit·non-turn intake0 | 알림실패로B막기·notice누락·persistent dedupe해제·actual edit 대신samekeyenqueue→각 RED |
| T15 /4f | 전 ingress /E-50 PG·fresh process | §7.2 여섯 행 각각 실제 entry 양수, exact native user row와 logical response delivery commit. 새 process boot+concurrent Gateway | 해당 route의owner 호출생략·V2accepted뒤legacy실행·workerACK선행→진입점별 RED |
| T16 /4e/4f | restart 절단점 전수 | 접수불명 /Reserved /Bootstrap /Started(MayHaveSent tick전) /Unknown /retained /receipt뒤delivery전 각각 child process 복원 | MayHaveSent를Reserved로 복원·deadlineNone=NotSent·retained를실행큐로→해당 additional content0 단언 RED |
| T17 /4f | 운영 build 선택·scope/policy 보존 | cfg(not(test)) probe에서 supported Claude/Codex single owner, legacy experiment gate off여도V2진행; Remote/headless/consentblocked동등 | CodexClaudeonly gate복원·새 owner를cfgtest에남김·Source없음=fresh·Pending=Unsupported→각 RED |

재현 가능한 수리는 **수정 전 RED→수정 후 GREEN**을 남긴다. 각 뮤턴트는 빌드 성공·선택 수>0·지정한 고유 단언 실패로만 kill을 인정한다. 컴파일 실패·fixture 전제 실패·0-test는 RED 증거가 아니다. 원복 SHA256이 원 바이트와 같고 원복 GREEN이어야 한다. receipt/callback leaf guard는 한 묶음 생략 뮤턴트 하나로 대신하지 않고 호출 지점별로 기록한다.

### 7.2 실제 PG ingress matrix

각 행은 owner 직접 호출이 아니라 **명시된 기존 외부 진입 함수**를 호출한다. 실제 PG session/voice/intake_outbox/outbox/replay 상태를 만들고, Discord HTTP·tmux·Herdr만 격리 rig/shim으로 바꾼다. test hook은 관측/clock/fault 경계에만 두며 production routing 자체를 mock하지 않는다. 'Gateway 경유'라면서 `v2_enqueue_fixture`를 부른 시험은 해당 행 완료가 아니다.

| 실제 진입점 (M) | 최소 시나리오·관측 | 필수 부정 단언 |
|---|---|---|
| Gateway `D/router/intake_gate.rs:300` | 네 provider×host의 fresh/resume/warm-idle/warm-busy **16셀**(전체 actual ingress union), thread/attachment-only/accepted voice/선택 reply, 두 문단·선행 `/`/mention/`DISPATCH:` 중 실제 명령과 일반 payload 구별. 네 provider×host 대표 조합 | durable owner 전 checkpoint/reaction0, accepted 뒤 legacy provider dispatch0, rendered bytes1회, content1, source별 fence |
| catch-up `phase1_sweep.rs:6,422` 및 `phase2_sweep.rs:4,254` → `api.rs:86` | [known A,fresh B], retained-only/terminal-only known ID, restart/drain 보관 및 허용 bot provenance. 각 sweep 별 독립시험 | A content0/B content1, unrelated checkpoint 전진0, raw voice/prelude 소실0 |
| boot `runtime_bootstrap/queued_recovery.rs:26,52` + worker boot `:11` | queue→inflight→marker→placeholder 순서, items0+retained/terminal/notice metadata 파일, recovery와 Gateway 동시접수, Source None runtime positive | 원문 동일, metadata adoption 누락0, unknown additional content0, old in-flight가 queue original을 지움0 |
| worker `cluster/intake_worker.rs:261` → `intake_turn/worker_entry.rs:49`; exact retry `:135` | 실제 claimed row로 Pending publish→다른 commit/recover→동일 ticket, attachment/voice accepted row, retry authority | durable custody 전 row done0, synthetic 중복0, permission 없는 retry/remote local fallback0 |
| completion A `D/gateway.rs:730` | marker/placeholder drain 직전 A Unknown과 B 대기, 30초 경계 및 늦은 A receipt | B가 V1 take-next로 빠짐0, A receipt가 B 정산0, old marker 선삭제0 |
| completion B `D/queue_dispatch.rs:218,350` + kickoff `:113` | 위와 동일 조건을 독립 chain 경유; completion event lag/리스너 설치 전 완료·backstop | 중복 wake content추가0, 모든 backlog 채널 재관측, retained를 backlog로 계산0 |

**live Unconfirmed**는 Gateway busy 응답을 의도적으로 누락시킨 뒤 timer만 진행해 원문 retained+notice intent가 한 commit에 생기고 B가 진행하는지 위 matrix 안에서 증명한다. receipt 없이 성공 반응/일반 finalizer teardown으로 사라지는 뮤턴트는 RED다. 기존 non-TUI/remote/obligated/명령/consent 거절 회귀는 byte·side effect 동등성을 유지한다.

### 7.3 E-50과 운영 런북 상태의 분리

**S4 PG(E-50의 구조 수용)**: 빈 격리 runtime root+실제 throwaway PG를 쓰는 **새 AgentDesk 시험 프로세스**에서 실제 boot 외부 진입을 실행한 뒤 첫 Discord 입력 하나를 Gateway(별도 worker replay 사례 포함)에 준다. 네 조합에서 owner custody1·create permit1·실제 helper content1·exact native row1·비어 있지 않은 고유 응답 body hash의 **기존 relay/outbox logical answer delivery commit1·anchor/placeholder 완료1**을 센다. 높은 층 응답 함수 호출 수만 세지 않는다. 실제 Discord HTTP 분할 청크/재시도와 logical answer 중복은 별도로 기록한다. Codex tmux는 argv1/paste0, Codex Herdr는 argv payload0/paste1, Claude는 첫 submit1이다. boot recovery와 첫 Gateway를 경합시켜도 추가 content0. 접수 직후/launch 직후/native receipt 뒤 answer commit 전 프로세스 중단·재시작과 같은 source의 catch-up 재도착에서 추가 content0/답변 중복 commit0을 증명한다. fixture가 만든 응답은 native 모델이 실제 추론한 답이 아니므로 **“실제 응답 한 번” 종결 증거로 부르지 않는다.**

**S5 격리 native(E-50 종결, fallback 제거 전)**: coordinator가 승인한 별도 runtime root/DB/격리 tmux·Herdr endpoint에서 실제 provider CLI를 콜드 부트하고 첫 입력1/실제 응답1을 확인한다. 첫 Enter가 UI에 삼켜진 사례도 넣는다. content 재타이핑은0이고, Enter-only 재시도는 **기존 typed submit transaction의 소유자 하나**가 같은 attempt/target/composer 증거로 제한한다. owner timer·backstop·watcher가 각각 Enter 재시도자가 되면 안 된다. 장문 두 문단, attach idle, readable Person stash, Unknown A 뒤 B, Source 늦음 네 조합을 검증한다. 증명 못한 provider×host/capability는 S5 fallback 제거 대상에서 빼고 미완료로 표시한다.

런북 전제도 S4 fixture에서 실제 상태를 생성해 검증한다. (1) strict parent-sync 실패는 AdmissionPending/원문 경로를 보이며 ack0, (2) runtime live+Source None은 ReadyWithoutSource이며 create0, (3) writer 종료+safe composer는 정확 nonce hold release 가능, (4) MayHaveSent 재시작은 retained+notice이며 automatic content0, (5) leased target의 타 thread content는 거절, (6) unsupported/corrupt foreign snapshot은 raw hash 불변, (7) outbox down/늦은 receipt는 notice pending/실제 edit로 관측된다. 이 상태가 current runtime에서 발생 가능하다는 근거는 §2.1의 실제 producer이며, 운영 서버에서 발생 중이라고 추정하지 않는다.

### 7.4 마감 게이트와 증거 형식

각 단위는 `python3 /home/adk/skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py <직전 승인 base> <head>`로 따로 측정한다. 생성물은 공식 cap 분류를 따르며 표의 추가3파일/+120은 보수적 여유다. 비-production 생성 파일 수를 prod 수와 섞지 않되 전체 변경 목록도 보고한다.

단위별 대상 시험·뮤턴트 후 전체 clippy `cargo clippy --workspace --all-targets --all-features --message-format=json -- -W clippy::all`에서 변경 파일 신규 경고0. 공식 source reader를 같은 cargo 환경에서 실행해 **cargo_rc=0, failed=0, pg_not_run=0, 선택 수>0**를 대조한다. 알려진 main 실패는 같은 main/head·실패명·원시 증거로 별도 분류하며 임의0/PASS로 바꾸지 않는다.

`prepush_gates.sh <worktree> --static-only`의 `PREPUSH_STATIC_OK`, prod cap·giant raw0·durable writer census·SQL surface·required test lane 선택을 확인한다. S3d의 SQL inventory 합성 충돌은 merge 후 공식 생성기로 해결한다. helper test-only 모듈의 empty/skip pin은 공식 등록 절차를 따르고 production 승격 시 `check_legacy_supervision_census.py --sync-skips --promote <path>`로 제거한다. no-growth 기준을 baseline/allow/allowlist로 완화하지 않는다.

REVIEW_READY 보고서에는 head/base/단위 diff, prod counts/raw delta, 수리 전후 원시 로그, 실행 필터·선택/실행/실패/skip, PG 실제 사용, SHELL/TMPDIR/cargo profile, 바이너리 경로·SHA256을 적는다. source·dependency 변경 후 옛 바이너리의 결과를 새 head 증거로 쓰지 않는다. 로컬 review ref만 만들며 원격 push/GitHub 쓰기는 코디네이터가 한다. native는 지시된 시점에 detach하고 PID/PGID/log/rc 및 `PREPUSH_OK`까지 구분한다. Windows-gnu cross가 포함된 실제 native 결과 없이 cross PASS라고 하지 않는다. 이번 계획 단계에는 새 detach가 없다.


### 7.5 정본 A01~A27 연결표

| 정본 ID | S4 증명·상태 | S5에 남는 것 |
|---|---|---|
| A01/A07/A08 | T09/T15/T17: 네 조합×fresh/resume/warm-idle/warm-busy16셀 actual ingress, first argv/paste 규칙 | 실제 CLI/host native 수용 뒤 fallback 제거 |
| A02/A03/A04 | T10/T11: timer만으로 보관·B/C 진행, alive writer 비중첩, late A exact 정산 | 장시간 실제 native stall trace 보강 |
| A05/A06/A20/A21/A22 | T04/T07/T16: metadata-only·절단점 fresh process·불명 publish·source fence/24h | 최종 native 재시작 trace, 임의 TTL/삭제 없음 |
| A09/A10/A11 | T09/T10/T11: Source 영구 없음에도 같은 runtime B/create1, observer 대기를 writer에서 분리, foreign execution/file 거절 | 네 CLI의 실제 ReadyWithoutSource 수용 |
| A12/A13/A14 | T05/T13: 동문/5001 입력/slot 회수·late enqueue-command/query순서·row ownership | receipt lifecycle 비용 수리(S3d P2-3)는 S5 첫 단위 |
| A15/A17 | T12/T14/T15: reply/merged/첨부/voice, guild·DM·허용bot·dispatch thread·role/CWD/worktree·provenance | 실제 native asset/초안·host별 수용 |
| A16/A19 | §7.2 각 실제 ingress+T10/T15/T17, typed 준비 해제 wake·event lag | fallback 제거 전 실제 진입 trace 동일성 재확인 |
| A18 | §2.4 Invalid 경계 UTF-8 bytes 65535/65536/65537, raw-empty/attachment-only/accepted-voice, Rejected 원문+notice·B 진행 | 유효 장문/fold/control capability 미증명 조합은 제거 보류 |
| A23/A24 | T07/T08/T14: notice crash/outbox 실패·old reader/clear/purge/drain·호환 rollback | 최종 운영 런북 수용, V2 삭제/배열 downgrade0 |
| A25 | T09/T13/T15: Codex idle/busy→새 턴 anchor/placeholder/완료 각1, Claude resume `_` 유무와 **사용자 literal `_` 보존** | 실제 native observer/Enter-only ownership 수용 |
| A26 | S4는 queue/history/outbox 실제 publish 수 및 접수 직후 crash 보존 계측. durable 접수 지연0 | 정본 조각5의 즉시 접수+Reserved/MayHaveSent/settle queue publish3 합치기. S4에서 미완료를 숨기지 않음 |
| A27 | T17: Remote/headless/대상밖 provider 지원·거절·bytes/부수효과 동등, local pane key0 | shared caller 삭제 금지 |


## 8. 미확정 실측 [U] — 전부 착수/활성화 판정에 남김

| [U] | 현재 아는 것/모르는 것 | 해소 방법·완료 위치·안전한 기본값 |
|---|---|---|
| U01 실제 M+L base | fetch만 했고 merge하지 않음. §0 SHA와 SQL 합성충돌은 사실 | 코디네이터 merge 뒤 경로/line/소유/cap/raw/hand conflict를 재고정. 착수 전; rebase0 |
| U02 열린 PR/진행 레인 최종 head·순서 | 7PR read snapshot은 확보. reply/system-prompt/#6485 working hunk·착지 완료는 미확정 | §5 순서 승인과 최신diff 재확인. 각 겹침 파일 편집 전; 레인 작업을 임의중단하지 않음 |
| U03 운영 flag/채널/host mode 실제 값 | ADK_QUEUED_HEAD_INJECT 코드 gate는 읽었으나 운영 env·설정값은 조회 안 함 | 값이 필요하면 coordinator에게 [U]. 새 V2route는 새flag 없이 code활성화. 설정 읽기/복사0 |
| U04 production promotion의 실제 reachability·경고 | test-only root/receipt adapter 범위 확인. 최종 unused 함수/trait set은 미측정 | 4f Clippy production+all-target+all-features 및 audit --promote. 추가 parent필요하면 FILES, allow/baseline0 |
| U05 TLS 누락과 content leaf 추가분 | 원시292행 후보는 test/caller 포함. 74행표와 leaf분류를 만들었으나 합성base 신규 caller는 아직 없음 | 4a actualbase content leaf inventory+source guard+site mutant. 직접 platform tmux/Herdr/argv 경로도 포함 |
| U06 physical target alias/Herdr namespace | tmux known identity 구조와 기존 전역Herdr key 확인. 실제 endpoint/pane/nonce API 조합 실측 전 | 4a/4d samephysical alias·두 endpoint·같은 endpoint다른execution·serverreplace rigs. serialized witness로 임의live생성0 |
| U07 lock order/metadata callback 지연 | S3d metadata-only 콜백은 확인. 모든 production branch의 lock holding set은 미증명 | 4d realhelper+twoinput+heldcallback 경합, transition→writer→composer·actorUIwait0. timeout으로lease강제해제0 |
| U08 A1/A2/B 실제 safe stash capability 전 조합 | main #7358 타입/transaction은 존재. 네 조합의 모든 draft/fold capability 운영 수용은 미실행 | 4d PG rig로 Empty/PersonSafe/OwnPending/unread/fullfold/resize, S5 isolatednative. 미증명 조합 fallback제거 보류 |
| U09 선택 prelude/role/CWD/options 보존 필드 | RawV1·RetryPromptContext·PendingQueueItem·PreparedPayload은 존재. actualbase 필드별 전체 mapping은 구현 전 고정 필요 | 4e 원본/선택 provenance와 effective session params를 optional V2 metadata에 보존(permit객체 직렬화0), golden/A24. inflight/retry stash 재정의0 |
| U10 voice claim와 durable 접수 사이 crash | accepted announcement helper는 존재. worker/Gateway 동시 실제 PG ingress는 S4 몫 | 4e/4f sibling claim·crash·same source 재도착. claim row만으로 원문삭제/ACK0 |
| U11 첨부 보존 기간·원참조 복원 | 기존 materialize/retain API 사용. 외부URL/asset의 무한수명은 보장 못함 | 4e owner 반환/retain/NotSent/restart 및 clear/purge 뒤 실파일 open/hash. 참조만 남고 bytes가없으면 원문/수동 조치로 보고; 무단 새 cleanup/GC0 |
| U12 legacy queue/history 실제 형태·메타데이터만 파일 | 소스 fixture 형태만 확인, 운영 file/DB는 안 읽음 | 4b V1array/envelope/Reserved/attempted/terminalonly/retainedonly/unknownversion/corrupt fixtures 및 4c bootenumeration. migration은 explicitcontext로만 |
| U13 abandoned proof와 cross-file 절단점 | S3a 미완료 source 확인. armed부재 추론은 채택 안 함 | 4b validReserved proof-beforeforget /NoContent token·projectionfailure·orphanUnknown cutoff 별 시험, independent design review 대상 |
| U14 notice persistent identity→actual edit API | 기존 outbox persistentdedupe/identity/receiptindex는 존재. 실제 warning messageID 복구·edit 경계 조합 미실측 | 4e outbox→realHTTPfixture→restart→late receipt actualedit PG시험. 기존 API로 불가하면 새ACK/journal을 만들지 말고 DESIGN_DELTA |
| U15 production wake 누락/리스너 lag | 현재 source/completion/backstop 진입점 확인. lastwriterrelease→wake 및 metadataonly 열거는 미구현 | 4e/4f dropped/coalesced/lag/presubscribe 완료·readywithoutsource 시험. generic no-wake foreverHeld0 |
| U16 Claude/Codex full-key receipt 교차 축 | 현재 exact adapter는Codex만, Claudelegacyproof와 nativehook은 존재 | 4d Claude full-key·CodexHerdr 실제 adapter, offset/file/SID/root/author/generation/nonce/lateA 축. gate해제 전 |
| U17 #6483 B FreshSubstitution 표시/경계 | M claude.rs는missingtranscript→fresh를debuglog만 남김. #7384 A는별도PR | 4d 검증된runtime없음+requestedSIDmissing에서만fresh. liveSourceNone분리 및 원SID/새SID notice(본문없음); #6483담당 동의 후종결 |
| U18 bootstrap 준비 budget 실제 값 | 120초는 이 계획의 권고 상수이며 현재 운영값/실측이 아니다. payload30초와 독립 | 4d 기존 create/readiness 최대값과 느린 부팅 trace를 비교한 뒤 확정; 가상clock경계시험. 장시간stall에도kill/lease release0 |
| U19 time/readback/file identity 경계 | wall rollback/FS오류는P2전제. actualhost flush와 Windows support 아직검증없음 | 4b parentSync/oldcandidate/thirdstate,4f cross. UnflushedPlatform=permit0; no-deadline/reader퇴역=NotSent0 |
| U20 history·retained·receipt 비용 | history wholepublish와 receipt 누적 source는확인, S4actual costs미측정 | 4b/4e R=0/1024/5001, active/planned/passive별bytes·비교량·CPU·publish/fsync수·latency,A26 조각5 합치기 목표와 S4의 prepare/bootstrap/migration 추가 commit을 분리 |
| U21 receipt 수명 S5 이월 비용 임계 | 낮은 운영량 가정은 severity 근거이며 실제량은 조회하지 않음 | S4 비용공개; S5첫단위 item/retained/24hterminalattachment+A24. fixed ring/대기ticket삭제0. 측정상활성화자체를막을비용이면S4재분할 |
| U22 E-50 격리 native/Auth/CLI host | Mac/WSL실제provider/Auth/테스트서버사용승인/가용성확인전 | S5 coordinator가전용root/DB/socket/endpoint/credentials를지정. 운영config복사0. 사용할 수 없으면 native 미실행·fallback 제거 미완료 |
| U23 E-50 first Enter swallowed/Enter-only owner | 기존 submit transaction이 전송/관측을 소유. 실제 네 조합 UI 수용 전 | S4 rig fault+동일attempt evidence, S5 realnative. Enter-only는동일transaction하나만, timer/watcher새retry0 |
| U24 관측/런북 필드 실제 노출 | 기존AttemptRow/diagnostics는V2retained세부전부를아직보이지않음 | 4e pump 구조화로그에provider/channel/source/attempt/phase/reason/deadline/notice상태(본문0),실제fixture상태로검증. #6485부모추가필요하면FILES |
| U25 rollback 호환 binary/운영 절차 | S3d read-only 보호는 있으나 즉시 진행 보장이 아니다. 배포는 이번 범위 밖 | S4종결시V2기록뒤compatiblefailclosed수리diff와격리복원런북고정, coordinator가배포결정. rawdelete/downgrade/kill0 |
| U26 required PR job·PG lane/nextest 변경 | #7372진행중, 기존수동filter를정본으로고정할수없음 | 매단위공식generator로실제testID/pathgate/runmode/platform양수확인. helper-module예외는등록절차·기간/이유명시, production은promote |
| U27 static/native/cross 최종 결과 | 현재 native rc1·compile 미도달. 이번 새 검사0 | 실제merge뒤SQL공식regen→static,4f및최종기능headnative/cross rc/PREPUSH표식. main기존실패는exact증거로분류 |
| U28 실제파일수/순증/giant raw | §4는현재M/L계수+추정이며새diff없음 | 각 단위 공식 cap·main/단위 base raw를 대조. production 여유 소진/+1200 전에 PLAN_READY. 단순부모import도증가권한없음 |
| U29 새 Veto/host error 원인 | M enum및static사유를읽음. 최신merge PR로추가될수있음 | §2.4 exhaustive match/source census갱신,미분류는Held/Unknown으로failclosed; 새사유를genericNotSent로합치지않음 |
| U30 start 권한·claim·관찰 인계의 실제 타입 seam | M replay_start는 intake child, StreamingTurn/ReplayStart는 기존 start 지점에 존재. S4 직접 helper 호출로 이 권한을 건너뛰면 안 됨 | 4d2/4e1에서 owned_turn_start/provider_dispatch child 추출·명시 exact context·기존 spend/capacity/claim 유지. 모델/constructor 변경이 계획 파일을 벗어나면 FILES, 잘못된 hash/target/refused continuation이면 content0 시험 |
| U31 Codex Herdr warm-busy 공통 adapter | 현 run_herdr는 provider_unsupported. 새 helper를 지웠다는 사실만으로 capability가 생기지 않음 | 4d1에 공통 판독 추출+실제 HerdrTarget/CodexWrites adapter 예산 배정. native queue/turn receipt/fold/초안 안전성을 actual ingress16셀에서 확인 전4f 활성화0. 미해결은 DESIGN_DELTA |

각 [U]는 해소 전 완료로 세지 않는다. **모르는 운영 설정값을 읽는 대신 coordinator 질의/격리 fixture/실제 base 소스 검증으로 해소**한다. source에서 결정할 수 있는 값과 운영 정책 승인을 섞지 않는다. unresolved item 때문에 새 flag·schema·journal·cleanup 권한을 추정해 넣지 않는다.

## 9. 리뷰 집중 항목

| 항목 | 막는 방법·남는 한계 |
|---|---|
| 경계 시각 | payload30초/별도 bootstrap budget/settled_at24h를 분리하고 직전·동시·직후와 restart를 가상 clock으로 검증. 시계 역행은 기존 severity P2이며 NotSent 근거로 쓰지 않음 |
| 동시성 경합 | exact NoContent proof+writer 종료 후만 Reserved 재준비, TLS 없는 leased-target content 거절, 두 Herdr endpoint/동일 physical target 경합 시험. stall writer 강제 해제 없음 |
| 옛 형식 복원 | explicit provider-context migration, LegacyUnknown 원문 보관·terminal 최초 anchor, 실제 writer golden A24. V2 삭제/배열 downgrade 없음 |
| 문서로만 선언한 가드 | content leaf caller census+production reachability·mutator exhaustive dispatch guard·site별 뮤턴트. 새 leaf/enum이 등록 없이 생기면 fail-closed |
| 잘못된0/PASS | native rc1과 compile 미실행을 명시. actual ingress/realPG·선택 수·고유 RED·원복 hash·source-reader rc를 함께 확인. E-50 PG rig와 실제 native 응답 증거를 분리 |

이번 PLAN은 현재 구현을 재판정하는 Opus 코드 리뷰가 아니다. P0/P1를 새로 주장하거나 기존 P2를 승격하지 않았다. 다음 단계는 코디네이터의 파일/순서 승인과 WebGPT pro 독립 설계 리뷰다.

## 계획 증거 목록

- `logs/s4-drift-log.txt`: 지정 B..M -G 명령과 다섯 commit 원시 결과.
- `logs/s4-open-pr-files.json`: 열린 PR 7개의 read-only head/state와 파일 전부.
- `logs/s4-entry-census.txt`, `logs/s4-content-call-census.txt`: M 원시 함수/호출 후보(시험 포함). `logs/s4-entry-table.json`: 본문80행 전수표의 현재 사본.
- `logs/s4-file-budget.json`: 현재 M/L raw·giant·8개 단위 파일별 예상 순증. 고유 prod105파일/+3826 추정이며 단위별 cap을 따로 적용한다.
- `logs/s4-plan-facts.json`: 정본·이월 SHA256, M/L/base/native 결과·본 계획 hash·작성 완료 시각.
- 이번 실행: 코드 변경0, 빌드0, 시험0, PG0, 새 native0, commit/checkout/merge/push0. `git fetch origin main` 외 저장소 변경 없음. 테스트 바이너리·GREEN/RED 증거는 이번 PLAN에 없다.
