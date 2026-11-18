# 재시작 뒤 proof 없는 Herdr 입력: 제한 복원과 유한 미전송 정산

설계 레그만의 제안이다. 제품 코드·운영 상태는 변경하지 않았다. 정본은 [#8222 본문](https://github.com/itismyfield/AgentDesk/issues/8222)이며, 아래 모든 제품 좌표는 조사 기준 main `8e80f9b06ad039a8817641773c5fa1ce2e2d4119`다. #8214와 #8217(1118a)이 모두 포함된다. 구현은 독립 pro 설계 리뷰 뒤 시작한다.

<!-- doc-path-refs: proposed readiness/restored_reply.rs v2/readiness_hold.rs -->

## 1. 원인과 이미 충족된 계약

`Context::read` → `idle_ready`의 일반 `readiness::observe`가 None → running observer Unknown → Held → `owned_busy::early`의 defer → 같은 probe Unknown → `held_watch` Keep 순환이다. prepare도 `composer::when_idle`도 시작하지 않으므로 #8217의 유한 창은 작동하지 않는다. 증거 없는 상태에서 원본이 대기열 앞에 있다는 사실만으로는 전달도 정산도 일어나지 않는다.

| main이 이미 제공하는 계약 | main 증거 | 이번 처리 |
|---|---|---|
| 완료 응답 `end_turn` 뒤 알려진 idle metadata는 일반 observer에서 Idle | `src/services/tui_turn_state/claude_meta.rs:7`, `:25`, `:50`, `:57` | 재구현하지 않는다 |
| 1116h: running fallback은 marker 너머 Busy만 보완하며 새 Idle을 만들지 않음 | 같은 파일 `:108`–`:118`; 호출 `src/services/claude_tui/host_input/busy/herdr.rs:251` | 변경 0. 새 복원은 이 observer에 넣지 않는다 |
| readiness의 현재 source·revision·Stamp·예약 검사 | `src/services/claude_tui/readiness/mod.rs:137`, `:355`, `:412` | 그대로 거친다 |
| metadata가 움직인 준비 대기의 #8217 유한 재평가·최종 거절 | `src/services/claude_tui/readiness/watched.rs:9`, `:54`, `:114`; `src/services/claude/herdr_turn/composer.rs:181` | 이미 들어간 입력은 재구현하지 않음. 진입 전 Held와 marker Waits가 남음 |
| body 없는 최종 거절, 실패 영수증, 안내 의도 1개 | `src/services/turn_orchestrator/queued_head/v2/refused.rs:12`–`:63` | 같은 처분을 attempt 이전 front에도 제공한다 |
| 복원된 Idle로 Busy writer가 직접 전송하지 않음 | `src/services/claude_tui/host_input/busy.rs:245`; `src/services/claude_tui/host_input/busy/idle_compaction_tests.rs:89`–`:99` | main 검사를 보존하고 회귀 시험에 포함 |

기존 #8214 PG 시험 `src/services/discord/router/message_handler/provider_dispatch_idle_compaction_tests.rs:8`은 실제 front의 wake/delivery를 확인한다. 단, 현재 실행의 SessionStart 증거가 있는 경우이므로 #8222의 proof 없는 재시작 재현을 대신하지 않는다. 이 레그에서 제품 PG 시험을 실행하거나 통과했다고 주장하지 않는다.

## 2. 진입점 전수표와 모델 값의 출처

이 표를 상태 차트 변수 범위의 유일한 출처로 쓴다. `1/2`는 실제 ID 두 개의 동치류(같음/교체), boolean은 해당 검사 통과/실패다. 논리 시간 `age=0..5`는 기존 2초 LOOK의 다섯 구간으로 제안하는 10초 deadline을 표현한다. 실제 구현은 횟수 대신 절대 deadline을 쓴다.

| ID / 진입·소비 경로 | main file:line / 실제 상태·출처 | 설계 동작 / 모델 값 |
|---|---|---|
| E01 daemon 부팅·재시작 | `src/cli/dcserver.rs:969`; `src/services/claude_tui/readiness/mod.rs:45`, `:83`–`:87` | initialize 순서 유지. ledger/proof 유실, 첫 read가 restart floor. `phase=restart`, `proof=false` |
| E02 SessionStart와 resume 기록 유실 | `readiness/mod.rs:158`–`:175`, `:550`–`:563`; `readiness/resume_record.rs:11`, `:62` | 기존 resume 기록부터 사용. 없으면 새 reply 복원 검사 또는 최종 미전송. `complete/pinned/afterLaunch` boolean |
| E03 pane 재사용·native/source 재바인딩 | `readiness/source.rs:23`–`:78`; `host_input/busy/herdr.rs:320`–`:362` | nonce/native/dev+ino가 하나라도 바뀌면 이전 증거 사용 금지. `nonce/native/inode=1..2`, `pinned` boolean |
| E04 Busy 초기 관측 | `host_input/busy/herdr.rs:233`–`:272`, `:276`–`:312` | 일반 proof 다음에 current-reply 전용 복원 API 시도. 기존 hard-terminal 복원은 새 Busy 진입에 노출하지 않음. 키 없이 Idle 위임. 없으면 Held. `phase=queued,held`, `turn=reply,unknown` |
| E05 early 사람 입력 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:53`–`:100` | 정확한 front에 typed readiness hold/deadline을 owner가 기록. fresh/legacy 분기 유지 |
| E06 late 사람 입력 | 같은 파일 `:194`–`:264` | 같은 hold helper 사용. `unwound` 검사 뒤 기존 active row가 풀린 front만 owner가 판정 |
| E07 managed running 재진입 | 같은 파일 `:9`–`:49`, `:304`–`:323` | Busy는 종전 경로. Held는 같은 typed 정책. 다른 row/attempt를 정산할 권한 없음 |
| E08 probe와 watcher | `host_input/busy.rs:192`–`:199`; `src/services/discord/queue_io/input_owner/held_watch.rs:15`, `:62`–`:66`, `:95`–`:147` | Unknown/Blocked는 read-only Keep; 정확한 front의 deadline에는 owner 정산 요청. `age=0..5`, `phase=waking`; generic arm은 유지 |
| E09 front·watch 세대와 소유권 | `owned_busy.rs:350`–`:380`; `held_watch.rs:124`–`:140`; `src/services/turn_orchestrator/queued_head/v2.rs:445`, `:510` | probe 전후 및 actor commit에서 QueueIdentity/세대 일치. `front/generation=1..2`, `owned/gate` boolean |
| E10 Idle drive와 composer | `input_owner/drive.rs:97`–`:115`, `:174`; `src/services/claude/herdr_turn/composer.rs:137`–`:205` | wake는 전송권 아님. driver가 재관측·예약·current·first-key gate. `phase=reserved,sent,done`; `keys=0..1` |
| E11 prepare 안의 대기와 거절 | `readiness/watched.rs:54`–`:127`; `input_owner/effect.rs:109`–`:135` | #8217 재평가를 보존. typed 원본 deadline이 있으면 재대기로 연장하지 않음. `refusals=0..1`, `reason`은 아래 거절 사유 |
| E12 Cancel·clear·큐 삭제 | `v2/clear.rs:230`–`:234`; `owned_busy.rs:350`; `composer.rs:204` | 같은 actor gate에서 취소/기존 정산이 이기면 늦은 probe/deadline은 no-op. `gate=false`, `owned=false` |
| E13 Terminate·pane 소멸 | `src/services/discord/herdr_terminate.rs:535`, `:603`, `:664`; `busy/herdr.rs:234` | 기존 termination fence와 종료 권한만 사용. 새 close/kill 없음. 미시작 front가 남으면 사유 정산; 이전 실행 증거로 새 pane 전달 금지 |
| E14 `/stop`·first-key 경쟁 | `src/services/turn_orchestrator/herdr_user_stop.rs:55`; `src/services/provider/herdr_before_start.rs:115`; `src/services/claude_tui/owner_input.rs:114`, `:121` | stop 의도와 cancel은 다름. 기존 gate 소유자가 정산. keyed/possibly-sent이면 새 최종 미전송 대상 제외 |
| E15 prompt·tool·Stop continuation | `src/services/tui_turn_state/claude_meta.rs:50`, `:76`; `readiness/evidence.rs:136`, `:184`; `readiness/mod.rs:185`, `:611`–`:636` | 복원 불가. `turn=prompt,tool,stop-context`, `fence=true` |
| E16 pending·compact·clear 제어 | `readiness/mod.rs:57`, `:215`–`:223`, `:265`; `host_input/busy/herdr.rs:315` | 기존 control은 복원으로 해제하지 않음. `fence` boolean; clear는 자신의 인증 경로만 사용 |
| E17 사람 draft·화면 불독해 | `composer.rs:104`–`:116`, `:156`–`:171` | 복원/전달 금지. 미전송 정산은 draft를 건드리지 않음. `screen=false` |
| E18 복구 대기열 재생 | `src/services/discord/runtime_bootstrap/queued_recovery.rs:48`–`:52`; `input_owner/recovery.rs:15`–`:38` | 원본 deadline·refusal 복원 후 drive. body 재큐 0. `replayed` boolean |
| E19 안내·영수증·중복 억제 | `v2/refused.rs:21`, `:44`; `input_owner/notice.rs:35`–`:99`; `src/services/discord/queue_io/retained_notice.rs:18`–`:57` | 사유·fingerprint·안내만 남기고 origin 실패 정산. `phase=refused,explained`, `notices/requeues/busyKeys=0..1` |
| E20 probe의 읽기 가용성·교체 관측 | E02/E03/E08의 `busy/herdr.rs:233`, `:324`; `held_watch.rs:118`–`:127` | `visible/changed` boolean은 첫 Unknown 뒤 증거 가용/한 번 source 교체의 모델 projection. 새로운 운영 플래그 아님 |
| E21 driver의 증명 소비 시점 | E10의 `readiness/mod.rs:355`, `:412`; `composer.rs:173`–`:193`; 1116h `claude_meta.rs:110` | `reserved/keyValid/fallbackIdle` boolean은 예약 여부/키 시점 유효성/running fallback의 Idle 생성 여부. `proof`도 boolean(E01) |
| E22 거절 사유의 모델 범위 | E03/E12–E19의 실패 분류와 `v2/refused.rs:17`, `:47` | `reason`은 `none` 또는 fixture 조건명: `reply,lost-start,nonce,native,inode,prompt,tool,stop-context,pending,compact,continuation,missing,legacy,new-incarnation,old-reply,draft,pane-unreadable,race,replay,cancel,clear,terminate,stop`. 실제 제품 문자열은 §5.3으로 매핑하며 모델은 비어 있지 않은 사유와 안내 1개를 검사 |

`readiness/…` 단축 경로는 `src/services/claude_tui/readiness/…`, `host_input/…`는 `src/services/claude_tui/host_input/…`, `input_owner/…`는 `src/services/discord/queue_io/input_owner/…`, `v2/…`는 `src/services/turn_orchestrator/queued_head/v2/…`, `owned_busy.rs`는 E05의 경로다. 모델에서 `done`은 전달 1회, `explained`는 미전송 정산+사유 안내 1회다. 중간 `sent`/실제 provider receipt 지연은 기존 모델이 맡고 새 refinement는 전달 완료를 `done` 한 단계로 접는다.

### 입력 출처 분류

정본 분류는 `src/services/discord/queue_io/input_owner/origin.rs:5`–`:14`, `:27`–`:50`, `:101`–`:105`다. `is_person`은 Discord/Imessage만 참이다. 이 범위를 넓혀 routine/API를 Busy injection에 넣지 않는다.

| 출처 | 실제 경로 / owner | 복원·정산 범위 |
|---|---|---|
| 사람 Discord·iMessage | E05 early, E06 late, E07 managed running → V2 owner | 이번 typed Busy-Held의 직접 대상. 동일 원본 GUID/message·queued_generation 보존 |
| 사람이 pane에 친 draft/Enter | owner 원본이 아님. 화면(E17)·prompt(E15)로만 관측 | 보호 대상. 내용 삭제/전송/재큐 없음. Enter 전후 숨은 창은 §7의 기존 한계 |
| Routine·scheduled | `InputOrigin::Routine` → 같은 TUI owner → Idle preparation | 새로운 Busy 경로는 열지 않음. 동일 readiness 복원 helper와 준비 단계의 typed pre-key hold 정책 적용 |
| 외부 API | `HttpStart` → TUI owner, origin receipt E19 | 사람 입력으로 취급하지 않음. 같은 최종 실패·안내 및 API 실패 receipt. provider process/non-TUI 실행은 범위 밖 |
| daemon continuation / RestartSeed | `EffortContinue`, `RestartSeed` → 같은 TUI owner | readiness 복원을 받되 기존 continuation fence가 우선. 입력을 복제하지 않음 |
| 재시작 뒤 대기열 재생 | E18, 원 출처/QueueIdentity 그대로 | 새 출처도 새 입력도 아니다. typed hold의 due를 이어받고, 거절 기록은 안내만 재생. 종전 possibly-sent head는 별도 recovery 소유 |

## 3. 두 안과 선택

| 안 | 흐름 | 비용 추정 | 위험·판정 |
|---|---|---|---|
| A: evidence 경계의 제한 복원 + typed hold 정산 | `idle_ready`가 current proof 다음 scoped restored proof 확인 → probe Wake → Idle driver 재예약. 실패는 같은 원본의 고정 due에서 owner 최종 거절 | 세 조각 합계 상한 추정 prod 23파일 / 순증 +1080. 별도 proof 저장소 없음; owner snapshot에 deadline/attemptless refusal 추가 | source 증거·Stop audit·actor CAS를 명시해야 함. 정상적으로 쉬는 pane의 원본 전달과 복원 불가의 정산을 모두 만족. **채택** |
| B: watcher 유한 정산 우선 | proof 없는 Held를 10초 뒤 일괄 최종 미전송. 이후 별도 레그에서 복원 | 정산/배선 두 조각, prod 약 12파일 / +680 | 무기한 Keep은 없지만 복원 가능한 원본도 거절하므로 #8222의 같은 probe Wake→원본 전달 DoD를 충족하지 못함. 기각 |

일반 watcher의 모든 Unknown에 timeout을 붙이지 않는다. typed readiness hold는 **키를 보내기 전 원본**과 사유를 owner가 식별한 경우에만 생긴다. lock/modal/handoff·boot merge failure 등 다른 hold의 권한과 처분은 유지한다.

## 4. 복원 조건과 증거 출처

새 child `readiness/restored_reply.rs`에 읽기·판정을 둔다. `idle_ready()`는 일반 `observe()` 실패 때 **current-reply 전용 `observe_restored_reply()`**를 호출한다. 기존 composer용 `observe_restored()` 전체를 새 Busy fallback으로 노출하지 않는다. 일반 `observe()`/running observer의 계약은 확장하지 않고, None만으로 Idle을 만들지 않는다.

proof에는 내부 `ProofOrigin`(Current/Resume/RestoredHard/RestoredReply)을 붙인다. 새 Busy API는 §4의 전체 조건으로 만들어진 RestoredReply만 추가 허용한다. composer의 기존 API는 기존 RestoredHard와 새 RestoredReply를 다룬다. `reserve(proof)`는 같은 origin의 조건을 다시 평가하고 origin/source/Stamp/revision을 모두 비교한다. 이 구분 없이 기존 bool `restart_ok=true`만 켜면 pin/native 검사가 다른 옛 hard-terminal 복원까지 Busy에 새로 노출될 수 있으므로 금지한다. 이 origin은 메모리 proof의 provenance이며 영속 ready 플래그가 아니다.

| 필수 조건 | main의 증거 출처 file:line | 새 helper의 의무 |
|---|---|---|
| 살아 있는 Herdr 실행·정확한 nonce/논리 세션 | `busy/herdr.rs:281`–`:285`; `readiness/source.rs:15`–`:29`; `src/services/tui_prompt_dedupe/binding_context.rs:26`–`:43`, `:216`–`:222` | execution_alive·spawn marker·context의 nonce/session/channel 일치. missing/unreadable context는 복원 불가 |
| native ID 일치 | `readiness/source.rs:48`–`:76`; `busy/herdr.rs:324`–`:345`; `readiness/evidence.rs:100`–`:107` | runtime binding, nonce별 complete history, pinned native, 첫 record native, 완료 record native가 모두 같아야 함. filename fallback만으로 인증하지 않음 |
| transcript dev+inode 일치 | `busy/herdr.rs:346`–`:362`; `src/services/claude_tui/source_verify.rs:225`–`:287`; `readiness/evidence.rs:14`–`:33` | nonce별 검증된 pin을 요구. unpinned·legacy stat 부재·다른 dev/inode·truncation은 거부. 기존 device renumber 승인과 별개로 이 복원은 exact identity만 허용 |
| 현재 execution의 완료 응답 | `claude_meta.rs:50`–`:53`; `readiness/evidence.rs:153`–`:155`, `:256`; context `created_at`는 `binding_context.rs:29` | `end_turn`이며 tool_use 없음. 완료 record의 native·timestamp를 직접 읽어 `created_at <= timestamp <= 관측시각` 요구. 앞 incarnation의 resume history는 완료로 쓰지 않음 |
| 새 prompt·tool continuation 없음 | `readiness/evidence.rs:136`–`:189`; `claude_meta.rs:170`; `readiness/mod.rs:185`–`:205`, `:611`–`:636` | 마지막 완료 응답부터 EOF까지 허용된 비대화 metadata만. user(isMeta 포함)·tool_use/tool_result·progress·낯선 레코드·찢긴 줄·해석 불가 거부 |
| Stop 추가 context·지속 의도 없음 | `claude_meta.rs:76`–`:103`; `readiness/closed_turn.rs:52`–`:72`; `readiness/mod.rs:588`–`:602`, `:624`–`:636` | ledger latch 부재만으로 통과 금지. suffix의 Stop stdout additionalContext, summary hookAdditionalContext/preventedContinuation/stopReason/errors를 엄격 검사. 알 수 없는 Stop 출력은 거부. quiet Stop 형식 검증은 기존 helper와 공유 가능한 작은 child로 분리 |
| pending·compact·clear 장벽 없음 | `readiness/mod.rs:57`–`:63`, `:137`–`:140`, `:215`–`:223`, `:265`–`:276` | restart floor 이외의 유효 장벽을 해제하지 않음. source replacement floor는 대상 제외 |
| 온전하고 현재인 읽기 | `readiness/evidence.rs:67`–`:99`, `:199`; `readiness/mod.rs:261`, `:351`–`:353`, `:355`–`:384` | 최대 256KiB 안에 완료와 suffix 전부가 있어야 함. 열린 fd의 Stamp와 경로 Stamp를 전후 비교. 범위를 벗어나면 넓게 추측하지 않고 거절 사유 |
| idle·빈 composer·modal 없음 | `composer.rs:104`–`:116`, `:150`–`:175` | observer 위임과 Idle driver 모두 이 screen 검사를 시행. busy indicator/UPS 대기·draft·capture 실패는 전달 금지 |

`claude_meta.rs`는 `src/services/tui_turn_state/claude_meta.rs`다. 새 reply 복원은 `created_at < STARTED`를 필수로 삼지 않는다. SessionStart가 빠졌어도 **현재 launch 이후의** 정확한 응답이 있으면 충분하다. 기존 hard-terminal 복원(`readiness/mod.rs:316`–`:335`)의 launched-before 규칙은 그대로다. 새 incarnation에 옛 응답만 남았으면 timestamp/identity 조건에서 막힌다. 새 nonce의 실행이 실제로 완료한 새 응답은 별개의 유효 증거다.

RestoredReply proof는 기존 RestoredHard의 `Proof.restart()`와 동등한 screen 의무를 가진다. recovery floor를 전역 삭제하거나 ready=true를 영속화하지 않는다. `reserve(proof)`가 동일한 proof origin의 restore 분기를 다시 평가하고, 전달자는 `Reservation::current()`와 기존 first-key gate를 거친다. BusyObservation은 `idle_verified` 위임만 하며 `admits()`는 이 결과를 Busy write 권한으로 쓰지 않는다. source authority/ledger 잠금 중 pane IPC나 전체 transcript 읽기를 추가하지 않는다: 읽기 snapshot → 짧은 잠금 비교 → 재확인 순서를 유지한다.

## 5. 유한 정산 계약

### 5.1 대상·시간·상태 수명

owner가 (namespace, QueueIdentity, original members, queued_generation)에 붙이는 `ReadinessHold`를 새 child `v2/readiness_hold.rs`에 둔다. 저장 내용은 typed 원인·started_at_ms·due_at_ms·기대 실행 식별자·owner gate 세대이며 본문 복사나 readiness proof는 없다. 기존 snapshot의 원본만 참조한다. WATCHES의 세대는 프로세스 로컬이므로 복구 시 새 세대로 재arm하고 owner의 durable key와 대조한다.

- 최초 **typed** pre-key readiness hold 시 `due=now+10초`. 다른 read·metadata append·새 probe·early→late→composer·re-grant는 같은 원본의 due를 바꾸지 않는다. 정상 proof를 얻어도 첫 키 이전까지 due 기록을 지우지 않는다.
- 2초 LOOK는 관측만 한다. due 전 복원에 성공한 read는 먼저 Idle 위임을 시도한다. due 시 owner의 현재성 검사와 first-key commit 중 먼저 성공한 한쪽만 이긴다. 과거 Ready가 due 이후 무기한 전송권을 갖지 않는다.
- probe가 panic/None/Gone이면 기존 generic watcher는 End다. typed hold는 watcher와 별개인 owner deadline backstop이 남아 유한 정산한다. probe·mailbox read 오류도 성공/전달로 간주하지 않는다.
- actor가 접근 가능하고 snapshot/outbox 저장이 진행된다는 조건에서 owner refusal은 10초 deadline 다음 actor turn 안에 commit한다. pane I/O와 분리된 backstop이므로 느린 capture가 정산을 막지 않는다. 관측 루프는 다음 LOOK(최대 2초)까지 정산을 보게 된다. 공개 API 호출·disk 장애를 포함한 실시간 12초 보장은 아니다.
- 재시작은 저장된 due를 이어받는다. 이미 지났으면 boot recovery에서 drive 전에 정산. `now < started_at`(시계 역행)이면 새 10초를 부여하지 않고 만료로 처리한다. 구 snapshot에 field가 없으면 첫 관측에서 1회 부여한다. 판독/commit 실패를 성공으로 표시하지 않는다.
- scope: 이 입력의 key/payload가 시작되지 않았음이 owner 상태로 증명될 때만. 이전 possibly-sent/Unknown head·inflight는 기존 recovery의 소유이며 새 budget으로 폐기하거나 재전송하지 않는다.

### 5.2 attempt 이전의 정확한 경계

현재 `Owner::refuse_not_sent`는 `AttemptRef`와 leaf가 만든 `NotSentProof`를 요구한다(`v2/refused.rs:12`–`:20`). `hold_front`는 이 둘이 없다(`v2.rs:510`). watcher가 가짜 attempt나 `NotSentProof`를 만들어 이 API를 부르면 안 된다.

새 actor 명령 `refuse_readiness_front(expected_hold, now)`의 원자적 전제:

1. 저장한 QueueIdentity·queued_generation·members와 지금 front가 동일하다. 새 front/세대에 오래된 callback이 닿으면 no-op.
2. 해당 입력의 payload-start/키 기록이 없고 다른 head/claim이 실행 권한을 이미 얻지 않았다. actor turn에서 grant/claim을 회수한 뒤 제거한다. first-key와의 경합은 같은 writer gate와 owner CAS로 직렬화한다.
3. due 도달 또는 기존 lifecycle의 확정된 미시작 처분이며, 그 원본은 아직 최종 처분되지 않았다.
4. snapshot CAS 성공 한 번에 members 제거, origin Failed 영수증 의무, identity/fingerprint tombstone, reason, notice intent를 commit한다. 실패하면 원본을 손실시키지 않고 오류로 남긴다.

새 `RefusedFront`는 attempt 없는 body-free 기록이다. 기존 `RefusedInput`의 필수 `AttemptRef`를 nullable로 바꾸지 않는다(`v2_types.rs:194`–`:207`). snapshot에 `#[serde(default)] readiness_holds/refused_fronts`를 추가하고 새 child 타입을 참조한다. 기존 admission receipt·holds/dedup·clear·forget-told·notice drain·owed_drop projection이 새 항목도 소비하도록 작은 호출 연결을 한다. giant `provider_dispatch/owner_input.rs`에는 새 로직을 넣지 않고 child 호출 교체만 한다.

예약된 attempt가 이미 있는 Idle preparation의 typed hold는 그 leaf가 증명한 NotSent만 사용하여 기존 `refuse_not_sent` 경로로 정산한다. active leaf가 키를 보낼 수 있는 동안 watcher가 그 head를 attemptless 경로로 제거하면 안 된다. 별도 cancellation/writer gate를 먼저 확보하거나 actor가 첫 키 미시작을 확인한 뒤 효과를 끝내도록 한다. 이를 구현 시험에서 생략하면 설계 미충족이다.

### 5.3 사유와 안내

구조화된 사유는 최소 `missing_launch`, `source_identity_changed`(nonce/native/inode 세부), `reply_not_current`, `transcript_unreadable`, `prompt_or_tool_open`, `stop_continuation`, `control_pending`, `composer_unavailable`, `readiness_deadline`를 구별한다. 입력 본문·비밀값은 사유/로그에 넣지 않는다.

#8217과 같은 최종 거절 처분: **본문은 저장·재큐하지 않음**, 원본/흡수 sources의 중복 입장 차단, origin Failed, 사유 안내 1개. outbox dedupe identity는 namespace+original QueueIdentity+처분 종류로 고정하고 attempt/watch 세대에 종속시키지 않는다. crash가 outbox enqueue와 ack 사이에 나도 같은 key로 재시도한다. 안내 발행 실패는 의무를 남긴다. 외부 수신 시스템의 exactly-once delivery를 주장하지 않고, durable notice intent·outbox record가 1개임을 PG로 검증한다. 일반 Busy hold의 진행 알림이 이미 있다면 그 카드 갱신/기존 dedupe를 써서 별개의 최종 사유 안내가 중복되지 않게 한다.

### 5.4 lifecycle 경합

| 사건 | 동일 owner 입력에 대한 결과 |
|---|---|
| Cancel·clear가 먼저 commit | 기존 취소/clear 정산이 유일한 처분. hold 제거·gate 철회; 늦은 Wake/refusal no-op. 새로운 refusal 안내를 중복해서 만들지 않음 |
| deadline이 먼저 commit | body-free refusal 1개. 뒤 clear/cancel은 이미 처리된 identity로 본다 |
| Terminate | 기존 termination_fence/retire가 실행을 종료. 새 정책은 키/close/kill을 보내지 않음. 남은 pre-key original만 owner가 종료 사유 정산 |
| `/stop` | cancel flag를 세운다고 가정하지 않음. 기존 user_stop gate가 미시작을 정산하면 deadline은 no-op; 그렇지 않으면 due에 untouched original만 정산 |
| proof 직후 새 prompt/tool/Stop context 또는 nonce/native/inode 교체 | driver reserve/current/first-key 검사 실패. 원본 due 유지; 새 증거 없이 이전 proof로 전달하지 않음 |
| 다른 front·watch 세대 | 옛 observer는 End. 새 front는 자신의 budget. 만료 callback은 QueueIdentity CAS 때문에 새 front를 제거하지 못함 |
| 이미 C-s/content/Enter 또는 불확실한 write | 새 final-not-sent 대상이 아님. 기존 uncertain/hold recovery 유지, 원본 재큐 0 |

## 6. 조각 계획과 수용 시험

공식 측정기 `/home/adk/skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py`의 현재 상수는 `FILES=30`, `NET=1500`이며 PLAN_READY 정지선은 brief의 +1200이다. 다음은 **구현 추정**이고 이번 문서 커밋의 실측과 구별한다. 각 조각도 +1200 아래로 둔다. 테스트 파일/생성 manifest는 공식 측정기의 제외 규칙을 따른다.

| 조각 | 범위·예상 prod 파일 | prod 추정 / 수용 시험 |
|---|---|---|
| S1 제한 reply 복원 | 새 `readiness/restored_reply.rs`, `mod.rs`, `closed_turn.rs`의 작은 Stop 판정 공유, `busy/herdr.rs`, 새 작은 source 검증 child. 기존 generic hard/observe_running 불변 | **5파일 / +280**. restart floor+end_turn+metadata, SessionStart 유실 현재 응답 GREEN. nonce/native/dev+ino 각각 교체, launch 이전 응답, tool result/user/isMeta/progress/Stop 추가 context·찢긴 줄·256KiB 바깥 증거는 복원 차단. 1116h 기존 running observer 시험과 old hard/resume/clear/compact 전부 보존. composer에만 허용되는 기존 RestoredHard fixture가 새 Busy API로 통과하지 않는 부정 control 포함 |
| S2 owner deadline·body-free 최종 정산 | 새 `v2/readiness_hold.rs`, `v2.rs`/`v2_types.rs`, 새 snapshot projection child, 기존 clear·admission·notice·recovery 연결. 기존 RefusedInput 형식 유지 | **10파일 / +480**. 가짜 attempt/proof 없이 정확한 pre-key front만 정산. CAS·first-key 양쪽 순서, members 복수·origin 실패 영수증, old snapshot serde default, due 재시작/역행/metadata churn 불변, told/untold replay, notice enqueue-ack crash에서 1 record·재큐0. Cancel/clear/Terminate와 경쟁에서 처분1 |
| S3 실제 경로 배선·수명 통합 | `owned_busy.rs`, `held_watch.rs`+새 typed child, `first_input.rs`, `composer.rs`, `drive.rs`의 호출 교체. S2 회복/안내 helper 재사용. giant `owner_input.rs`는 필요하면 기존 block 이동·호출 교체만 | **8파일 / +320**. **필수 PG**: 실제 early Busy Held → 같은 probe Keep → fixture로 current 완료 증거 제공 → Wake → Idle driver → 원본 payload+Enter1. late/running·routine/API 준비·replay negative control. 복원 불가·probe panic/오류·capture 멎음도 due 정산+사유1·body재큐0. barrier로 Wake↔Cancel/clear/Terminate/stop, native 변경↔reserve, deadline↔first-key 양쪽 경합 고정 |

합계 상한 추정은 **23파일 / +1080**(조각 사이 중복 파일은 중복 계산한 보수적 합계)이다. parent mod/reexport는 `FILES_AUTO`로 보고하며, 생성 목록은 공식 생성기로만 갱신한다. 독립 책임은 새 child에 두고 1,000줄 넘는 기존 파일에는 얹지 않는다. dormant production helper는 만들지 않고 각 조각에서 실제 호출자를 연결한다.

재현/검증 순서: 원래 main에서 proof 없는 **실제** Busy 진입·watch Keep RED를 먼저 고정한다. S1 뒤에는 해당 증거를 노출한 같은 probe가 Wake하고 원본 1회 전달 GREEN. S2/S3 뒤에는 time advance/barrier로 timeout·source 교체·lifecycle를 검증한다. 각 원인 제거 mutant(복원 nonce/native/inode/Stop/launch guard 제거, deadline 제거/재설정, driver 재검사 제거, body 재큐, Busy 직접 전송)는 해당 수용 시험 RED를 확인하고 바이트 동일 복구한다. 시험 등록은 PG lane manifest/non-PG filter/lib inventory를 공식 생성기로 추가하며 required PR job의 실제 선택을 확인한다.

구현 레그의 완료 조건: targeted PG(nonproduction throwaway)·적절한 일반 회귀·source_scan_readers failed0·자기 파일 clippy warning0·prepush static 통과. deploy 관측은 coordinator만 수행한다. 이 설계 레그가 배포 수용을 완료했다고 표시하지 않는다.

## 7. 상태 차트, 가정과 리뷰 집중 항목

기존 색인에서 #8161 `ClaudeReadiness`/restart-idle, #8202 `HerdrBusyReobserve`, #8217 `ReadinessRequiet`를 확인했다. #8222는 실제로 composer 전에 Held가 되므로 **기존 `HerdrBusyReobserve.tla`에 `RHSpec`를 추가**한다. 기존 `Spec`/cfg/expected RED를 바꾸지 않는다. 새 모델 파일을 만들지 않는다. readiness의 source/launch/continuation 조건과 watcher deadline을 같은 refinement에서 연결하고 #8217의 대기 안 모델과 구분한다. [상태 차트 결과](../statecharts/input-single-path/restart-held-readiness.report.md)를 참조한다.

- INVARIANT: `RHOneDisposition`(전달0..1 또는 거절0..1, 최종 사유 안내1), `RHNoRequeue`, `RHNoBusyDirect`, `RHRunningContract`, `RHProofNeedsEvidence`, `RHKeyNeedsRecheck`, `RHBoundedHold`(첫 typed hold의 due 고정), `RHStaleNoop`(옛 callback의 새 front 제거 0), `RHOneNoticeRecord`(enqueue-ack crash 뒤 record 1). 경합 상태 도달은 witness invariant의 예상 RED로 확인한다.
- PROPERTY: `RHReachedKeep`(실제 Held/Keep 도달), `RHNoInfiniteKeep`, `RHEndExactlyOnce`; 유효 복원 cfg는 `RHHealthyDelivers`도 검사한다(driver 단계가 시계보다 먼저 도는 전제). 시계가 독립인 cfg는 `RHHealthyOrDeadline`(전달 또는 due 경합 패배만)으로 검사한다. 단순히 held notice를 최종 해결로 계산하지 않는다.
- before는 restart→Busy Held→probe Unknown→Keep 반복의 liveness RED가 나와야 한다. fixed는 source/continuation 실패도 최종 사유 정산으로 GREEN. safety mutant를 예상 RED로 등록하더라도 유효 cfg의 실패를 정상화하지 않는다.
- 공정성은 owner·watch read·Idle driver·clock·notice 처리가 계속 진행된다는 의미다. 외부 pane/tool turn 종료, 새 사용자 입력, 추가 SessionStart는 가정하지 않는다. 영구 mailbox/disk/PG 장애에서 실시간 정산·안내를 증명하지 않는다. 모델의 lifecycle action은 기존 취소 처분을 접은 것으로 실제 Cancel/clear의 안내 형식을 새로 규정하지 않는다.

| 리뷰 집중 항목 | 막는 방법 / 남은 반례 |
|---|---|
| 경계 시각 | reply timestamp==created_at 허용, <created_at 거절. first hold/due 직전·동시·직후, clock 역행, STARTED 초기화 순서를 시험. 미래 timestamp·누락은 거절 |
| 동시성 경합 | immutable source snapshot + reserve/current + actor original identity CAS + writer first-key gate. probe가 Ready를 낸 직후 source 변경 mutant RED. payload-start와 만료의 gate 연결은 구현 리뷰 필수이며 문서 선언만으로 완료 아님 |
| 옛 형식 복원 | pin/native/timestamp 없는 기록으로 readiness 추측하지 않음. old snapshot field 누락은 first hold 한 번만 부여하고 after-refusal replay는 안내만. 기존 body 있는 retained 경로와 새 body-free 거절을 구분 |
| 문서로만 선언한 가드 | nonce/native/inode·Stop suffix·pre-key owner gate의 실제 production caller와 부정 PG 시험을 S1–S3 수용 조건에 각각 연결. 가짜 NotSentProof 생성 금지 |
| 잘못된 0/PASS | TLC 비어 있는 선택·syntax error·timeout은 실패. expected.tsv checker로 safety와 각 liveness를 따로 검사. PG는 실제 실행 수·outbox record 수·payload 키 수를 확인하고 skip을 PASS로 세지 않음 |
| 기존 미보고 키 창 | transcript와 화면이 모두 아직 새 Enter/continuation을 숨기는 순간은 main도 식별하지 못함. `restart-idle-readiness.design.md` §9 및 `composer.rs:170`–`:193`의 기존 check-to-key 창. 새 authority/kill/cleanup으로 덮지 않으며 새 복원의 screen/Stop guard를 반드시 유지 |
| Stop persistence 누락 | ledger latch가 재시작으로 사라지고 추가 context도 transcript에 아직 없으면 파일만으로 그 부재를 증명할 수 없음. 화면 busy guard가 기존 방어다. fixture의 화면까지 거짓 idle인 경우는 기존 LateBlind 위험이며 이 설계의 안전성 가정 밖. 독립 pro는 이 제한을 명시적으로 평가 |

## 8. 구현 뒤 운영 수용 (이번 레그 미실행)

coordinator가 배포 뒤 idle compaction이 켜진 상태에서 daemon restart 직후 **살아남은** 쉬는 Herdr 채널에 원본 입력 1개를 넣어 관측한다. 본문 없는 추적 ID로 held→restored delegate→Idle reservation→submit1을 확인한다. 복원 불가 fixture/상태는 사유 있는 최종 미전송과 안내1·재큐0을 확인한다. pane/설정/DB에 접근하거나 배포를 수행하는 권한을 이 설계가 lane에 추가하지 않는다.
