# 준비 대기 중 transcript가 한 번 움직인 뒤 멈춘 입력의 정산 (#8217)

설계 기준: `origin/main` fdd44b59c7. 코드 좌표는 이 커밋 기준이다. 같은 판정 파일을 바꾸는 #8214 리뷰 head 462046ed0b를 함께 읽었고, 그 위에서 성립하는지는 §7.4에 적었다.
구현 뒤: `Watched`와 예산 끝 판정(이 문서의 `readiness/mod.rs:353-421`)은 `src/services/claude_tui/readiness/watched.rs`로 옮겨졌다. 그 범위와 그 뒤 줄의 좌표는 옮기기 전 기준이며, 구현 좌표는 상태 차트 보고서에 있다.
범위: Claude tmux(claude-tui)와 Herdr 두 경로의 준비 대기 종료 판정. 설계 레그에서는 코드를 바꾸지 않았다.
상태 차트: [readiness-requiet.report.md](../statecharts/input-single-path/readiness-requiet.report.md).

## 0. 요약

준비 대기는 시작할 때 본 transcript의 Stamp(파일 정체·길이·수정 시각)를 간직했다가, 예산이 끝나면 "그대로면 미전송 정산, 달라졌으면 보류"로 가른다.
대기 중에 `ai-title` 같은 메타 기록이 한 줄 붙으면 Stamp가 달라져 보류가 되는데, 그 보류를 깨울 수 있는 것은 준비 증거뿐이고 쉬는 pane은 증거를 만들지 않는다.
그래서 입력이 키도 보관도 안내도 없이 다음 활동까지 남는다.

고친 계약: 예산 끝에서 Stamp만 달라졌고 나머지 조건이 모두 "쉬는 중"이면, **같은 시도 안에서** 새 Stamp를 기준으로 같은 대기를 짧게(창 하나 최대 2초, 최대 3번) 더 돈다.
그 사이 증거가 오면 기존 경로로 전달하고, 조용해지거나 창을 다 쓰면 키 없이 최종 미전송 거절(§4.6: 본문 없는 정산 기록 1회 → 사유 안내 1회)로 끝낸다.
턴이 진행 중이거나 fence가 열려 있거나 화면이 바쁘면 정산하지 않고 지금처럼 보류한다.

## 1. 경로 확인

Rust 시험은 이 레그에서 돌리지 않았다. 아래는 코드를 읽어 확인한 것이고, 앞 절반은 기존 시험 `a_timed_out_hold_tells_an_unmoved_idle_source_from_a_running_turn`(`src/services/claude_tui/readiness/readiness_tests.rs:544-567`)이 이미 단언한다.

| 단계 | tmux | Herdr |
|---|---|---|
| 대기 시작, Stamp 보관 | `src/services/claude_tui/input.rs:375` → `readiness/mod.rs:359-369` | `src/services/claude/herdr_turn/composer.rs:145` |
| 대기 | `readiness/wait.rs:7-48` (`observe`, 100ms) | `composer.rs:146-173` (`observe_restored`, 200ms, 예산 10초 `:13`) |
| 대기 중 `ai-title` 한 줄 | 턴 상태는 Idle 그대로다: `src/services/tui_turn_state.rs:629`가 `None`을 돌려 walk가 직전 `turn_duration`(`:626`)까지 내려간다. 증거 읽기도 중립이다(`readiness/evidence.rs:184-194`) | 같음 |
| 예산 끝 | `input.rs:391` `watched.unproven()` | `composer.rs:192` |
| Stamp 불일치로 false | `readiness/mod.rs:385-389` | 같음 |
| 보류로 반환 | `input.rs:392-399`(warm), `:429-435` → `Held(idle_evidence_pending, unproven=false)` | `composer.rs:193` `Refused(Unknown)` → `herdr_turn/first_input.rs:232-240`, `:308-318` |
| owner 정산 선택 | `src/services/discord/queue_io/input_owner/effect.rs:184-189` `held()`가 `Completion::Held` → `:116-117` `hold_not_sent` (`unproven=true`였다면 `:118` `retain_not_sent`) | `effect.rs:233` 같은 `held()` |
| 보류 감시 | `input_owner/drive.rs:113-115` → `held_watch.rs:88-149`. probe는 `readiness/mod.rs`의 `watch()`: `observe()`가 `Some`이어야 Ready | probe `first_input.rs:204-231`: `observe_restored()`가 `Some`이어야 Ready |
| 깨움 없음 | `held_watch.rs:62-68`에서 Unknown은 Keep. owner가 있는 채널에는 시간 기반 깨움이 없다(`src/services/discord/turn_presence/supervisor.rs:688-695`: `due`는 `!owned`일 때만) | 같음 |

다음 활동(새 입력, 턴 경계)이 drive를 깨우면 새 시도가 새 Stamp로 대기하고, 그때는 움직임이 없으므로 `unproven=true`로 정산된다. 즉 결함은 "다음 활동까지 설명 없는 보류"다.

Herdr에서 재시작 증명(`readiness/mod.rs:315-333`)이 서는 pane은 `ai-title`이 붙어도 전달된다(메타는 `end`·`barrier`를 바꾸지 않는다).
이 결함이 Herdr에서 나는 것은 재시작 증명이 서지 않는 경우다: 재시작 뒤에 띄운 실행(`launched_before_start` 거짓), 끝이 hard terminal이 아닌 transcript, 또는 증명은 있으나 화면이 idle로 읽히지 않아(`composer.rs:166`) 예산을 다 쓴 경우.

## 2. 진입점 전수표

### 2.1 준비 대기 Stamp를 기록·비교·소비하는 곳

| 역할 | file:line | 이 설계에서 |
|---|---|---|
| Stamp 정의·읽기 | `src/services/claude_tui/readiness/evidence.rs:8-34` | 그대로 |
| 기록: `watched()` | `readiness/mod.rs:354-369` | child로 이동, 창마다 Stamp만 갱신 |
| 기록 호출(tmux) | `claude_tui/input.rs:375` | `prepare()` 안으로 이동 |
| 기록 호출(Herdr) | `claude/herdr_turn/composer.rs:145` | 그대로 |
| 비교: `Watched::unproven()` | `readiness/mod.rs:373-420` (Stamp 비교 `:385-389`) | 3분류 판정으로 교체(§4.1) |
| 대기 루프(tmux) | `readiness/wait.rs:7-48` (timeout `:34-36`) | 창 연장 지점 |
| 대기 루프(Herdr) | `composer.rs:137-195` (`wait_for` `:46-71`은 공용, 그대로) | 창 연장 지점 |
| 소비(tmux) | `input.rs:391`, `:392-399`, `:429-435` | 호출 교체 |
| 소비(Herdr) | `composer.rs:192-193`, `first_input.rs:232-240` | 호출 교체 / 그대로 |
| 판정 운반 | `claude_tui/input/preparation_hold.rs:19`, `:54-59`, `claude_tui/host_input.rs:43` (`InputRefusal::Unproven`) | 주석만 |
| owner 분기 | `input_owner/effect.rs:184-189`, `:215`, `:233` | 그대로 |
| held probe 기록 | `readiness/mod.rs`의 `watch_draft`·`watch_modal`·`watch_current`, `first_input.rs:182-183`, `:204-231`, `claude_tui/input/submission.rs:158-180` | 그대로 |
| held probe 실행 | `input_owner/held_watch.rs:62-68`, `:88-149` (`LOOK` 2초 `:15`) | 그대로 |
| tmux 대기 호출자 | `claude_tui/input/submission.rs:35`, `claude_tui/hosting/warm_followup.rs:248`, `src/services/discord/router/message_handler/intake_turn.rs:1592`, `claude_tui/input.rs:349-360` ← `effect.rs:201`, `claude_tui/guarded_outcome.rs:319`, `warm_followup.rs:382` | 그대로(같은 내부 함수를 지난다) |
| Herdr 대기 호출자 | `first_input.rs:170` (`composer::run_ready` `composer.rs:120-134`) | 그대로 |
| Stamp를 쓰지만 대기 판정이 아닌 곳 | `readiness/mod.rs:259`, `:422-435`(`reserve`), `:479-488`(`Reservation::current`), `claude_tui/host_input/busy/herdr.rs:294-300`, `src/services/discord/turn_presence/activity/claude_activity.rs:82` | 그대로 |

### 2.2 미전송 정산 경로

| 역할 | file:line |
|---|---|
| 보류(front 유지) | `src/services/turn_orchestrator/queued_head/v2/retained.rs:142-164` `hold_not_sent` |
| 보관(front 해제) | `retained.rs:166-204` `retain_not_sent` (`remove_members` `:177`, 안내 의도 `retained_retry_required` `:192-195`) |
| 안내 발행 | `input_owner/drive.rs:120-122` → `input_owner/notice.rs:7-43`, `:60-93` → `queue_io/retained_notice.rs`의 `enqueue` |
| 안내 뒤 종결 | `retained.rs:231-255` `notice_enqueued` (실패 종결 부채 `:245-247`, continuation 장벽 닫기 `:248-253`) |

이 설계는 이 경로를 바꾸지 않는다. `unproven=true`가 되는 경우만 늘린다.

### 2.3 같은 상태를 소비하는 인접 경로

| 경로 | file:line | 재평가 창과의 관계 |
|---|---|---|
| CancelToken 취소 | `readiness/wait.rs:17-19`, `composer.rs:53-55`, `:66-68`, `src/services/provider.rs:591-593` | `cancel_requested`는 `token.cancelled`만 읽는다. Herdr 첫 입력은 `turn.cancel`을 넘긴다(`first_input.rs:55`, `:170`). tmux owner의 유휴 leaf는 토큰을 넘기지 않으므로(`effect.rs:201-203`) 그 대기는 예산과 제출 fence로만 끝난다. 창도 같은 루프라 같은 지점에서 끝난다 |
| tmux stop(제출 fence) | `readiness/wait.rs:20-22`, `src/services/claude_tui/submission_fence.rs:90-92`, `effect.rs:205-208` | 창의 매 반복이 같은 fence를 먼저 읽는다. 창이 새 키 경로를 만들지 않는다 |
| Herdr 사용자 `/stop` | `src/services/turn_orchestrator/herdr_user_stop.rs:53-75` | `user_stop` 의도만 기록하고 `token.cancelled`를 세우지 않는다. 그래서 대기도 창도 `/stop`으로 바로 끝나지 않고 예산과 창(최대 +6초)을 다 쓴다. 키 없이 끝난 입력은 `FinishedUntouched`로 기록되어(`src/services/provider/herdr_before_start.rs:115-128`, `:287-294`) 기존 stop 종결을 탄다. 이 설계는 그 계약을 바꾸지 않고 대기 상한만 늘린다 |
| Herdr Terminate | `src/services/discord/herdr_terminate.rs:534-539`(물리 실행의 `termination_fence`), `:603-619`(닫기·retire·종결) | tmux의 `submission_fence::refusal()`과 다른 경로이고 대기 루프는 이 fence를 읽지 않는다. 창은 키를 보내지 않으며, 창 안에서 증거가 오면 첫 대기와 같은 예약·현재성 확인(`composer.rs:169-180`)을 지난다. 창 도중 Terminate가 겹치는 순서는 시험하지 않았다(§8) |
| clear | `readiness/mod.rs:380`(`clear::current`), `queued_head/v2/clear.rs:232` | clear 중이면 정산하지 않는다(§4.1 S2). front가 치워지면 감시는 `held_watch.rs:64`에서 End |
| 새 incarnation·source 교체 | `readiness/mod.rs:379`, `:598-609` | source가 달라지면 정산하지 않는다(S1). 창은 source를 다시 읽지 않는다(§4.2) |
| 데몬 재시작·부팅 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:748-759` (`recover` 뒤 `wake`) | 창 상태는 시도의 스택에만 있다. 재시작 뒤 새 시도가 처음부터 다시 센다 |
| 사람 입력 우선 | `queued_head/v2.rs:125-149` `prefer_person` | 보류 front만 대상. 창은 시도 안이므로 무관 |
| 다른 보류 사유 | `v2.rs:488-496` `hold_front`, `v2/claude_handoff.rs:267`, `v2/start_failure.rs:158`, `v2/effort_continue.rs:111` | 무관 |
| drive 깨움 | `owner_input.rs:679-689`, `input_owner/start.rs:77`, `held_watch.rs:139`, `drive.rs:154` | 새 깨움을 만들지 않는다 |

## 3. 입력 출처 분류

출처는 `input_owner/origin.rs:4-13`의 `InputOrigin`이고, TUI 실행이면 출처와 무관하게 채널의 V2 input owner가 받는다(`origin.rs:98-100`).
보관 판정에는 출처 분기가 없다(`retained.rs:166-204`). `is_person()`(`origin.rs:26-28`)은 사람 입력 정책을 적용할 출처의 분류이지 작성자 판정이 아니다.

| 출처 | 작성 주체·전달 경로 | 저장된 원본 identity | 실행 owner | 정산 단위 |
|---|---|---|---|---|
| `Discord` | 사람이 채널에 쓴 메시지. context가 없으면 이 출처다(`origin.rs:35-41`) | 메시지 id와 `queued_generation` | 채널 V2 owner | head의 members 전부 |
| `Imessage` | 외부 전달: 브리지가 사람 입력을 실어 온다(`src/services/turn_orchestrator/queued_head/v2.rs:61-69`) | 채널과 원본 GUID로 만든 메시지 id·영수증. 재전달에도 같다(`origin.rs:49-73`) | 같은 owner | 같음 |
| `HttpStart` | API 호출(`origin.rs:43-44`) | 큐에 저장된 원본. ticket이 있으면 그 ticket에 묶인다(`v2.rs:73-104`) | 같은 owner | 같음. 안내 뒤 시도는 실패로 종결(`retained.rs:245-247`) |
| `Routine` | 루틴·예약(`origin.rs:45`) | 같음 | 같은 owner | 같음 |
| `EffortContinue` | 데몬의 continuation(`origin.rs:32-34`) | 같음 | 같은 owner | 같음. 안내 뒤 장벽이 닫혀 다시 제출되지 않는다(`retained.rs:248-253`) |

이 정산이 그대로 따르는 기존 보존 계약:

- **같은 원본의 재시도**: ticket이 같은 재시도는 저장된 원본을 generation까지 그대로 다시 쓰고, 내용이 달라진 candidate는 거절한다(`v2.rs:73-104`). 재시도는 새 출처가 아니다.
- **여러 원본을 실은 한 프레임**: head 하나가 원본과 합쳐진 후속을 members로 갖는다(`v2.rs:550`). 정산은 members 전부를 한 스냅샷에서 큐 앞에서 빼 보관 항목 하나로 옮긴다(`retained.rs:177`, `:183`, `:417-426`). head만 정산하는 경로는 없다.
- **재시작**: 안내가 아직 나가지 않은 미전송 보관은 안내 drain보다 먼저 다시 큐에 넣는다(`input_owner/recovery.rs:19-25`, `retained.rs`의 `requeueable`). 안내가 나간 원본은 자동으로 다시 제출하지 않는다(`retained.rs`의 `notice_enqueued`). 재평가 창을 거친 정산은 이 보관에 들지 않는다(§4.6): 재시작은 그 안내만 다시 시도한다.
- **늦은 정산**: `retain_not_sent`는 미전송 증명과 현재 head의 시도 일치를 확인한다(`retained.rs:171-175`). clear로 사라진 head를 뒤늦게 정산하면 `stale NotSent attempt`로 거절된다.

출처별 안내 문구와 API·루틴 쪽 실패 표시는 이 설계가 바꾸지 않으며, 문구까지 따라가 확인하지는 않았다(§8).

## 4. 계약

### 4.1 예산 끝의 3분류

지금의 `unproven()`은 아래 S1~S7과 "Stamp가 대기 시작과 같다"를 모두 요구한다. Stamp 조건만 떼어 3분류로 만든다.

정산 가능 조건 S (모두 지금 코드의 검사 그대로다):

- S1 같은 source: session·nonce·native·path가 대기 시작 때와 같다(`readiness/mod.rs:379`).
- S2 clear가 진행 중이 아니다(`:380`).
- S3 현재 증거가 없다(`:378`).
- S4 판정 읽기가 온전하다: 꼬리를 끝까지 읽었고 읽기 앞뒤 Stamp가 같다(`evidence.rs:203-205`, `mod.rs:386`). 덜 쓰인 줄이 있으면 불성립이다(`evidence.rs:94-96`).
- S5 진행 중 턴이 없다: native가 같고 턴 상태가 Idle이다(`mod.rs:390-400`).
- S6 continuation fence가 없다(`:402-413`).
- S7 원장 항목이 있고 pending·compact ticket·compact 완료 대기가 없다(`:415-419`).

| 분류 | 조건 | 결과 |
|---|---|---|
| Unproven | S이고 Stamp가 기준과 같다 | 지금과 같다: `unproven=true` → 보관 → 안내 |
| Requiet | S이고 Stamp가 기준과 다르다 | 재평가 창(§4.2) |
| Waits | S가 아니다 | 지금과 같다: `unproven=false` 보류, 기존 probe |

대기 시작 때의 Stamp와 같은 경우(Unproven)의 동작은 한 글자도 바꾸지 않는다.

### 4.2 재평가 창

- 창은 **같은 대기**다. tmux는 `prepare()`의 같은 루프와 같은 `DialogWait`로, Herdr는 `when_idle`의 같은 관측 클로저로 돈다. 증거가 오면 첫 대기와 같은 검사(예약, 화면)로 전달한다. 창이 새로 여는 키 경로는 없다.
- 기준 갱신: 새 기준 Stamp는 그 판정 읽기(S4)의 Stamp다. 판정 뒤에 쓰인 바이트는 다음 창에서 움직임으로 보인다. source는 다시 읽지 않고 대기 시작 때의 것을 유지한다.
- 파일 정체: 기준과 비교해 `(dev, ino)`가 바뀌었거나 길이가 줄었으면 창을 열지 않고 Waits다. 처음에 파일이 없다가 생긴 경우는 움직임으로 본다.
- 창 길이 `min(2초, 원래 예산)`, 횟수 최대 3. 한 시도가 더 기다리는 시간은 최대 6초다. 2초는 held watch의 관측 간격(`held_watch.rs:15`)과 같다.
- 창 끝:

| 창 끝의 상태 | 결과 |
|---|---|
| 창 중 증거 도착 | 전달(기존 경로) |
| S 불성립 | Waits |
| S이고 창 동안 Stamp 불변 | 화면 조건 P가 서면 Unproven, 아니면 Waits |
| S이고 Stamp가 또 움직였고 창이 남음 | 다음 창 |
| S이고 Stamp가 또 움직였고 창을 다 씀 | 화면 조건 P가 서면 Unproven, 아니면 Waits |

- 화면 조건 P(Requiet를 거친 정산에만 붙는다): pane을 한 번 읽을 수 있고 실행 표시가 없다(`tmux_capture_indicates_claude_tui_busy`가 거짓, ANSI 제거·패널 정규화 뒤). tmux는 `host_input::observe_draft`, Herdr는 `when_idle`이 이미 받는 capture를 쓴다. 읽지 못하면 Waits다. 입력창의 draft 유무는 보지 않는다(키를 보내지 않으므로).

### 4.3 창을 다 쓰고도 정산하는 이유

"안정된 transcript"를 이 설계는 이렇게 정한다: 판정 읽기가 온전하고(S4), 창 하나가 조용했거나 창 3개를 다 썼다.
창을 다 쓴 경우에도 정산하는 것은 의도한 선택이다.

- S가 서면서 Stamp만 움직인다는 것은 붙은 바이트가 턴 상태를 바꾸지 않는 기록뿐이라는 뜻이다. 턴 기록이면 S5가, 덜 쓰인 줄이면 S4가, 훅이 본 제출이면 S7이 먼저 깨진다.
- Stamp 불변은 "증거가 곧 올지 모른다"는 유예의 근거이지 실행 중 판정의 근거가 아니다. 실행 중 판정은 S5·S6·S7과 P가 한다.
- 정산은 키를 보내지 않는다. 잘못 정산하면 재전송 한 번이 들고, 잘못 보류하면 입력이 설명 없이 남는다.
- 창을 다 쓰면 보류하는 안은 "창마다 한 줄씩 네 번 붙고 멈춤"에서 같은 결함을 다시 연다(모델 `ExhaustHolds` RED).

### 4.4 종료 조건과 상한

예산이 끝난 시도는 아래 넷 중 하나로 끝난다.

| 끝 | 조건 | 상한 |
|---|---|---|
| 전달 | 첫 대기 또는 창 안에 증거 | 예산 + 6초 |
| 정산 | Unproven | 예산 + 6초 |
| 보류(Waits) | 턴 진행 중, fence, clear, source 교체, 읽기 불가, 화면 바쁨 | 지금과 같다. 턴 종료·fence 해제·새 SessionStart가 증거를 만들고 probe가 깨운다 |
| 취소·거절 | Cancel, 제출 fence | 지금과 같다 |

재평가 상태는 시도의 스택에만 있다. 원장·큐·디스크에 새 상태를 두지 않고, 새 깨움·새 probe·새 운영자 명령·설정을 만들지 않는다.

### 4.5 지키는 규칙

| 규칙 | 근거 |
|---|---|
| 대기 중 파일이 한 번 바뀌었다는 이유만으로 보류가 무기한 남지 않는다 | §4.1 Requiet, §4.4. 모델 `InputEventuallyResolves` |
| 현재 실행의 제출 증거 없이 키를 보내지 않는다 | 창은 기존 대기 루프다. 정산은 `retain_not_sent`(움직임 없는 분기)와 `refuse_not_sent`(재평가 창을 거친 분기)뿐이고 둘 다 키를 보내지 않는다. 모델 `NoKeyWithoutEvidence` |
| 실제 Running 중에는 정산하지 않는다 | S5·S6·S7, P. 모델 `NoSettleOverOpenTurn`, `NoRequietSettleOverBusyPane` |
| 정산 기록 1회·안내 1회·front 해제 | 움직임 없는 분기는 기존 보관 그대로, 재평가 창을 거친 분기는 §4.6. 모델 `OneEnd`, `RefusedDisposition` |

### 4.6 재평가 창을 거친 정산의 처분 (리뷰 뒤 변경)

재평가 창을 거쳐 끝난 입력은 재실행할 수 있는 보관이 아니라 **최종 미전송 거절**이다. "원문 보관 1회"는 이 정산에 한해 "본문 없는 최종 정산 기록 1회"다.

- 정산 시 members를 모두 큐에서 빼고, 그 시도의 ticket 묶음을 지우고, 출처가 있으면 실행 실패 영수증을 빚진다(`queued_head/v2/refused.rs`의 `refuse_not_sent`).
- 남기는 것은 unit과 시도의 identity, member별 identity와 원문 fingerprint, 미전송 사유, 안내 의도뿐이다(`v2_types.rs`의 `RefusedInput`). 본문·frame·ticket 문맥의 원본 사본은 남기지 않는다. 거절 members와 원본이 일치하는 effort continuation도 같은 정산에서 비우고 cancelled operation 식별자는 유지한다. 다른 입력의 continuation과 아직 거절되지 않은 원본 사본은 그대로 둔다.
- 기록은 재큐 대상이 아니다. 재시작 복구는 안내만 다시 시도한다(`input_owner/notice.rs`의 `owed`가 `refused_inputs`도 읽는다). outbox의 dedupe key가 반복을 한 줄로 묶는다.
- 같은 출처의 재접수는 기록의 identity가 막고(`sources.rs`의 `fenced`, `v2.rs`의 `holds`), 같은 ticket의 재시도는 남은 receipt가 Duplicate로 답한다. catch-up은 이 출처를 다시 가져가지 않는다(`queued_recovery.rs`의 `v2_held_sources`).
- 안내는 "준비됐다는 증거를 확보하지 못해 전달하지 않고 종료했다"는 사유를 쓴다(`retained_notice.rs`, reason code `refused.readiness_unproven`). 사람이 같은 내용을 새 메시지로 보내는 것만이 재실행이다.
- 안내가 나간 기록은 전달된 identity와 같은 24시간 뒤에 지운다(`refused.rs`의 `forget_told_refusals`).
- 움직임 없는 분기(§4.1 Unproven)와 다른 기존 미전송 보관 경로의 보관·재큐는 바꾸지 않았다.

모델 `RefusedDisposition`(§6): `RefusedStaysOut`(거절된 입력은 큐로 돌아오지 않는다), `NoticeExactlyOnce`(안내 정확히 1회).

## 5. 두 안 비교

**(a) `unproven()`의 Stamp 조건을 "대기 중 변경 뒤 안정 구간"으로 바꾼다.**
대기 루프가 폴링마다 Stamp를 표본으로 떠 마지막 변경 시각을 남기고, 예산 끝에 "마지막 변경 뒤 Q 이상 조용했다"로 판정한다.
- 비용: 두 대기 루프에 표본 채집을 넣고 `Watched`를 가변으로 만든다. 순증 약 +60.
- 위험: 예산이 끝나기 직전 Q 안에 움직이면 여전히 false라 같은 무기한 보류가 남는다. 이를 막으려면 예산 뒤 연장이 필요하고 그것이 (b)다. 표본 사이의 왕복 변경은 수정 시각으로만 잡힌다. 움직임이 없는 경우의 판정식도 함께 바뀌어 기존 동작과의 동일성을 다시 보여야 한다.

**(b) 예산 끝 뒤에 제한 재평가 단계를 따로 둔다(선택).**
예산 끝의 판정을 3분류로 나누고, Requiet일 때만 새 기준으로 짧은 창을 돈다.
- 비용: 판정 3분류, 두 대기 루프에 연장 분기, 화면 조건 하나. 순증 약 +95.
- 위험: 시도가 최대 6초 더 걸린다. Herdr 첫 입력의 대기는 composer 변경 잠금(`src/services/claude/herdr_turn/first_input.rs:123`)과 실행의 `state.submission` 잠금(`src/services/provider/herdr_before_start.rs:268-294`) 안에서 돌므로 두 잠금도 그만큼 더 잡힌다. tmux 대기는 잠금 밖이다(`src/services/claude_tui/input/submission.rs:35`). 창을 다 쓴 정산(§4.3)은 Stamp 불변 없이 판정한다.

(b)를 고른 이유: 움직임이 없는 기존 분기를 그대로 두고 결함이 난 분기만 다룬다. 움직인 시점이 예산의 어디든 같은 규칙이 적용된다. 종료가 상수로 묶인다.

(b)의 재평가를 시도 밖(held watch probe가 조용함을 보고 drive를 깨우는 방식)에 두는 변형은 버렸다.
`drive.rs:111-112`의 "보류는 시간으로 깨우지 않는다"를 깨야 하고, 재시도가 같은 사유로 다시 보류되면 깨움이 스스로 도는 고리가 생길 수 있으며(사람 draft로 보류된 front가 그 예다), 횟수 상한을 시도 사이에 넘기려면 새 상태가 필요하다.

## 6. 상태 차트

`ReadinessRequiet.tla` 23 cfg, 전부 `expected.tsv`와 일치(35.3초). 자세한 표는 보고서에 있다.

| 규칙 | 수정 전 | 수정 후 |
|---|---|---|
| `InputEventuallyResolves` (tmux, 메타 1회) | RED `TmuxOnceBefore` | GREEN `TmuxOnceFixed` |
| `InputEventuallyResolves` (Herdr, 메타 1회) | RED `HerdrOnceBefore` | GREEN `HerdrOnceFixed` |
| `NoSettleOverOpenTurn` (턴 진행·fence 중 메타) | — | GREEN `TmuxRunningFixed`·`HerdrRunningFixed`·`TmuxFencedFixed`, 뮤턴트 `TmuxFencedIgnoreRunning` RED |
| `NoRequietSettleOverBusyPane` (기록 전 사람 Enter) | — | GREEN `TmuxPersonFixed`·`HerdrPersonFixed`, 뮤턴트 `TmuxPersonIgnoreScreen` RED |
| `NoKeyWithoutEvidence` | GREEN | GREEN, 뮤턴트 `TmuxOnceQuietSendsKey` RED |
| 메타가 창마다 붙음 | — | GREEN `TmuxChurnFixed`·`HerdrChurnFixed`, 뮤턴트 `NoRoundCap`·`ExhaustHolds` RED |

수정 전 반례(대기 시작 → 메타 한 줄 → 예산 끝 → 깨움 없는 보류)는 §1의 코드 경로와 단계별로 대조했다.

정산 뒤 처분은 `RefusedDisposition.tla` 5 cfg가 다룬다([보고서](../statecharts/input-single-path/refused-disposition.report.md)).

| 규칙 | 보관·재큐(`Before`) | 최종 거절(`Fixed`) |
|---|---|---|
| `RefusedStaysOut` | RED: 안내 전 재시작이 원본을 큐로 되돌린다 | GREEN, 뮤턴트 `RetryAdmits` RED |
| `NoBodyOnceSettled` | RED | GREEN |
| `NoKeyAfterEnd` | RED: 되돌아온 원본이 재전송 없이 제출된다 | GREEN |
| `NoticeAtMostOnce`·`NoticeExactlyOnce` | 안내가 끝내 안 나가는 경로가 있다 | GREEN, 뮤턴트 `NoDedupe`·`DrainSkipsRefused` RED |

## 7. 구현 계획

### 7.1 바꿀 곳

| 파일 | 변경 | 순증(추정) |
|---|---|---|
| `src/services/claude_tui/readiness/watched.rs` (신규) | `mod.rs:353-421`의 `Watched`·`watched()`·판정을 옮기고 `Expiry { Unproven, Requiet, Waits }`, `Watched::expired(pane)`, 창 상수를 둔다 | +45 (이동 약 65는 별도) |
| `readiness/mod.rs` | 옮긴 블록 제거, `mod`·`use` 줄 | −65 +3 |
| `readiness/wait.rs` | `prepare()`가 `Watched`를 만들고, 예산 끝에 `expired()`를 물어 Requiet면 기한만 늘린다. 반환 오류에 `unproven`을 싣는다 | +25 |
| `readiness/idle.rs` | 화면 조건 P 한 함수 | +10 |
| `claude_tui/input.rs` | `:375`, `:391`의 호출을 새 반환값 읽기로 교체(1,362줄 파일이라 로직을 더하지 않는다) | ±0 |
| `claude/herdr_turn/composer.rs` | `when_idle`의 대기를 창 루프로 감싼다 | +15 |
| `claude_tui/input/preparation_hold.rs` | `:18`, `:53` 주석을 새 의미로 고친다 | ±0 |

prod 7파일, 순증 약 +95(이동분을 빼고). `pr_cap_prod.py` 상수(30파일 / +1500, 정지선 +1200) 안이다. 분할은 필요 없다.

### 7.2 시험 계획 (DoD별)

| DoD | 시험 | 수정 전 RED를 보이는 방법 |
|---|---|---|
| 1 진입점 | Herdr: `herdr_turn/first_input/unproven_tests.rs`의 `owner_outcome` 표에 "대기 중 메타 한 줄" 행 — 키 0, 보관 1, front 해제. 안내 1회는 같은 파일의 `_pg` 시험 형태로. tmux: 같은 시나리오를 owner 경계에서(#8214가 더하는 `provider_dispatch` 쪽 시험 틀을 쓴다) | 시험은 base에도 있는 owner 경계만 쓴다. prod 조각만 역적용한 채 실행하면 보관 0·보류 유지로 RED |
| 2 안전 | 같은 표에 행 셋: transcript가 턴을 보이는데 화면은 idle처럼 보이는 경우, continuation fence, transcript는 idle인데 화면이 바쁜 경우. 모두 보관 0, 턴이 끝난 뒤 1회 전달 | 수정 전에도 GREEN이다(Stamp가 달라 보류). 판별력은 뮤턴트 M2~M4로 보인다 |
| 3 뮤턴트 | M1 | 아래 |
| 4 상태 차트 | 이 레그의 23 cfg. 구현 레그는 구현 SHA로 다시 돌려 보고서의 좌표를 갱신한다 | `TmuxOnceBefore`·`HerdrOnceBefore` RED |

대기 중 append를 넣는 방법: 시험 스레드가 "대기의 첫 관측이 원장 항목을 만들었다"(`readiness/mod.rs:208-211`)를 기한 있는 폴링으로 확인한 뒤 한 줄을 붙인다. Stamp 보관이 첫 관측보다 앞이므로 순서가 정해진다. prod에 시험 스위치·훅을 넣지 않는다.

기존 단언 하나가 바뀐다: `readiness_tests.rs:562`의 "움직인 source는 기다린다"는 결함을 고정한 단언이다. 새 계약(움직였으면 Requiet, 조용한 창 뒤 Unproven)으로 고쳐 쓴다.

추가 시험은 prod 변경 줄 수 안에 둔다(표 행 추가 위주, 약 +85).

### 7.3 뮤턴트

| 뮤턴트 | 죽이는 시험 |
|---|---|
| M1 Stamp가 한 번 달라지면 Waits(기존 분기 부활) | DoD 1 두 경로 |
| M2 Requiet 판정에서 S5 제거 | DoD 2 "화면 idle, transcript 진행 중" |
| M3 Requiet 판정에서 S6·S7 제거 | DoD 2 fence |
| M4 화면 조건 P 제거 | DoD 2 "화면 바쁨" |
| M5 창 횟수 상한 제거 | `expired()`를 움직임과 번갈아 부르는 표 시험: 3번 뒤 Requiet가 아니다 |
| M6 창이 source를 다시 읽음 | 창 사이 source 교체 → Waits |
| M7 창이 증거를 무시 | 창 중 증거 도착 → 보관 0, 전달 1 |

M2가 기록된 실행 중 턴에서는 P에 가려진다는 것을 모델이 보였다(보고서). 그래서 M2 시험의 화면은 idle처럼 보여야 한다.

### 7.4 #8214 위에서

#8214 head는 `evidence.rs`(`End::Reply`, 메타 분류 위임), `mod.rs:297` 한 줄, `tui_turn_state/claude_meta.rs`(완료 답변 + 유휴 메타를 Idle로)를 바꾼다.
이 설계가 옮기는 `mod.rs:353-421`과 겹치는 줄이 없다. S5는 `observe_transcript_turn_state`를 그대로 부르므로 #8214의 분류를 따른다.
#8214가 Idle로 읽게 한 꼬리(완료 답변 뒤 유휴 메타)가 대기 중에 붙으면 이 설계에서는 Requiet로 들어가고, 그것이 현재 증거가 되는 경우(S3 불성립)는 창 안에서 전달된다.

## 8. 닫지 않는 것

- **Unknown으로 읽히는 꼬리**: 대기 중 붙은 줄이 단독 `permission-mode`면 턴 상태가 Unknown(`tui_turn_state.rs:624`)이라 S5가 서지 않고 전후 모두 보류로 남는다(모델 `TmuxMarker*` RED). Stamp가 아니라 턴 분류가 원인이고, 새 세션 시작과 구분해야 하므로(`tui_turn_state.rs:602-623`의 주석) 이 설계에서 넓히지 않는다. 후속 후보.
- **움직임 없는 분기는 화면을 읽지 않는다**: 기존 `unproven()`은 화면을 보지 않는다. 훅을 잃고 기록도 아직 없는 사람 Enter 위에서 정산할 수 있다(모델 `TmuxKnownGap*` 전후 RED). 기존 동작이라 바꾸지 않았다. P를 이 분기에도 붙일지는 후속 후보.
- **창 길이 2초·횟수 3**: 운영에서 훅 도착 지연을 재서 정한 값이 아니다. 구현 레그에서 로그(읽기 전용)로 Stop 훅의 기록 대비 지연을 확인해 근거를 보강한다.
- **재시작 뒤 메타 한 줄이 실제로 붙는 계기**: 이 레그에서 운영 transcript를 읽어 재현 시각을 확인하지는 않았다. 이슈 본문과 #8168의 기록을 따랐다.
- **출처별 안내 문구**: §3의 표는 owner 판정까지만 확인했다.
- **입력창 거절이 먼저인 tmux 대기**: warm이 아닌 tmux 대기는 예산 끝에 입력창 거절(사람 draft 등)을 `unproven`보다 먼저 판정해 보류한다(`src/services/claude_tui/input.rs:420-428`). Stamp와 무관한 기존 동작이라 그대로 둔다.
- **창 도중의 Herdr `/stop`·Terminate**: 두 경로의 종료 방식은 §2.3에 구분해 적었다. 창이 열린 뒤 `user_stop`만 기록된 경우와 창 도중 Terminate가 물리 실행을 닫는 경우의 진입점 시험은 만들지 않았다. 후속 후보.
- **owner 밖 호출자**: `warm_followup.rs:248`, `intake_turn.rs:1592`도 같은 내부 함수를 지나므로 Requiet일 때 최대 6초 더 기다린다. 이들은 `unproven`을 읽지 않는다.
