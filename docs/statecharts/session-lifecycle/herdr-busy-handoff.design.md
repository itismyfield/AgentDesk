# #8077 Claude busy handoff 설계 r2

Production 기준 SHA: **`1fe3a57d266e1e7fe384796d22a925076f1d99fc`**. 검토한 d1 artifact SHA: `fae2d9ee41bf57897dcef1e56869b1a19b1265cc`. 브랜치 `fix/i8077-handoff-busy`, worktree `/home/adk/adk/wt/i8077`. 이 레그는 설계만 수행하며 production 수정 0이다. 리뷰 원문 `/home/adk/adk/lane-runs/i8077/review-design-1-pro.out.md`를 읽었고, 이전 보고서와 ps에서 이전 작업의 진행 중 프로세스가 없음을 먼저 확인했다.

결정: mapped local Claude의 **forward tmux→Herdr**만 다룬다. 준비 화면과 무관하게 소유한 이전 pane을 끝내며, 새 실행의 첫 입력 준비는 자기 resume SessionStart/nonce와 owner 상태로 판단한다. 첫 payload 전 양성 no-key 거절은 custody로 돌려 자동 재시도한다. **과거 retained 자동 회수는 전부 제외**한다. 사람 초안 판독 함수의 의미·보존/폐기 UX 정책은 이번에 변경·확정하지 않는다. 새 clear/stash/Enter로 초안을 처리하는 동작을 추가하지 않는다. generic Return, Codex, process provider에는 이 권한을 주지 않는다.

## 1. 리뷰 처리와 코드 근거

| 지적 | main/d1 근거 | r2의 구체적 변경 | 모델 cfg |
|---|---|---|---|
| P1-1 종료 1회 실패+고정 unread 뒤 old 선택에서 정지 | d1 `HerdrBusyHandoff.tla:53`, `:109`, `:141`, `:171`; `src/services/claude/herdr_turn/composer.rs:125` | **old 선택은 첫 키 전까지만 provisional**. exact owner no-key 증거와 key_count=0으로 old 선택/시도만 철회하고 같은 원본을 `RevalidatingOld`로 이동한다. 새 연결에서 현재 소유권/신원을 재검증한 뒤 종료 재시도. screen-ready 이벤트를 기다리지 않음. C-s를 포함한 첫 키 뒤에는 이 전이를 금지 | BeforeFailureUnread RED → AfterFailureUnread GREEN; OldUnread=TRUE 상수이며 RecoverScreen 전이/fairness 없음 |
| P1-2 새 첫 입력 pre-key 거절도 문자열을 거쳐 retained | `src/services/claude/herdr_turn/first_input.rs:94`, `:112`; `src/services/claude_tui/host_input.rs:341`; `src/services/discord/queue_io/input_owner/effect.rs:360`, `:139`; `queued_head/v2/retained.rs:58` | own-resume first-input capability로 일반 composer ready gate를 대체. 거절은 **SubmissionError::Held(PreparationHold{BeforePayload})**를 끝까지 유지한다. owner scope에 veto/first_key_held 증거를 기록해 실제 NotSentProof가 생성되게 하고, effect::created는 Held로 매핑. hold_not_sent→queued 원본+native/host-ready wake. 문자열 prefix나 confirmed=0만으로 무전송을 추정하지 않음 | BeforeFirstRefusal RED → AfterFirstRefusal GREEN, NoFirstWake/NoNativeReady RED |
| P1-3 active 첫 메시지는 marker가 없고 Bound row도 없어 Begin에 도달하지 않음 | `.../provider_dispatch/owner_input.rs:217`, `:238`, `:325`, `:76`, `:91`, `:359` | active 분기의 **hand_busy 호출 전**에 Claude/local/mapped/Legacy preflight를 호출한다. 같은 front grant를 actor에서 재검증·claim하여 durable Begin으로 이동한다. marker 존재 여부를 진입 전제로 삼지 않음. queue 접수 당시 marker를 읽은 결과도 grant/commit 시 현재 journal과 다시 비교 | BeforeActiveEntry RED → AfterActiveEntry GREEN; BeforeActiveRace RED → AfterActiveRace GREEN(N=2, probe/commit 분리) |
| P1-4 kill helper identity 확인 뒤 PID/PGID 재사용 창 | `src/services/process.rs:527`, `:546`–`:550`; `src/services/claude/session_handoff.rs:101`은 live pane만 확인 | helper 재사용을 철회한다. **한 tmux control 연결에서 얻은 pane ID capability**로 그 연결에 `kill-pane -t %N`을 보낸다. 연결이 끊기면 권한 폐기, socket 경로에 재접속하여 숫자 ID 재전송 금지. SIGTERM/SIGKILL/kill_pid_tree/kill-session fallback 없음 | PidSignal/ReconnectNumber RED; AfterPidReuse/AfterServerReplacement/AfterPaneReplacement GREEN |
| 과거 retained 회수 제외, journal-only 복구 유지 | `queued_head/v2/retained.rs:283`, `:249`; `queued_head/compat.rs:20`; `pending_queue_persistence/queued_recovery.rs:87` | historical classifier, atomic retained migration, Opaque ticket 재결합, eligible_handoff_retained census 모두 없음. **unfinished_handoff**만 Claude boot census에 추가. 입력이 모두 정산돼도 journal을 복구·wake하고 전환을 마침 | AfterRetained/RetainedRecovery cfg 삭제. BeforeJournalCensus/NoWake RED → AfterJournalOnly GREEN |
| P2 초안 실행 규칙 자명 | d1 draftExecuted를 바꾸는 전이 없음 | old fallback의 실제 Submit이 park되지 않은 초안을 함께 실행하면 draftExecuted=TRUE. DraftEnter mutant에서 그 경로를 열어 규칙이 깨지는지 확인. 초안 정책을 새로 결정하는 모델로 쓰지 않음 | DraftExecution RED, AfterStopFailure GREEN |
| P2 최초 argv와 현재 native ID 혼동 | `src/services/claude_tui/hook_server/adoption_retry.rs:395`; `.../runtime_binding/adopt_skip.rs:81`; `claude_tui/session.rs:75` | 시작 argv A, 현재 binding B, 종료 직전 continuation C를 구분. 이전 provider 종료 확인 후 최신 durable binding/source/auth에서 **C**를 다시 읽어 resume. argv는 실행 유래 확인에만 사용 | BeforeContinuation/WrongSession RED → AfterContinuation GREEN |

모든 축약 `.../provider_dispatch/`는 `src/services/discord/router/message_handler/provider_dispatch/`, `.../runtime_binding/`는 `src/services/tui_prompt_dedupe/runtime_binding/`다. 아래 전수표에는 전체 경로를 쓴다.

원인 자체는 d1에서 확인한 그대로다. 제공 46×39 busy 화면과 스피너를 뺀 **합성 idle**은 owner=Empty, ready marker=true이지만 footer/stash=Unknown, draft_sighting=Unsettled다. busy 화면은 final busy veto도 있고, idle은 `composer.rs:75`의 exit-only stash 조건에서 즉시 거절된다. 기존 46열 fixture는 있지만 `/exit` 시험은 해당 배치를 다루지 않았다. 실제 14:17 idle 화면을 확보한 것으로 주장하지 않는다. d1 격리 predicate 시험 2개는 이 baseline의 원인 증거이며 재실행하지 않았다. 기존 opaque 시작 실패는 첫 실패마다 즉시 보관하는 것이 아니라, `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:226` 및 `src/services/turn_orchestrator/queued_head/v2/start_failure.rs:95`의 기존 임계 처리로 보관될 수 있다. BeforeBusy/BeforeUnread는 그 보관 결정에 도달한 경로를 추상화한다.

## 2. 종료 수단과 안전 계약

### 선택: 서버 연결에 묶인 tmux pane close

`src/services/tui_input/submission_tmux/guarded.rs:186`은 현재 server/session/window/pane/pane_pid를 관측하지만 매 명령이 다시 연결되는 snapshot만으로는 서버 교체 뒤 같은 `%N`을 보호하지 못한다. 새 `claude/session_handoff/pinned_tmux.rs`가 범위를 좁힌 연결 capability를 소유한다.

1. 기존 mapping/admission/local ownership을 확인한다. 동일 계정·현재 native ID·transcript/O cursor를 읽을 수 있는지 사전 확인하되, **ID를 이때 최종 고정하지 않는다**. 다른 원본의 MayHaveSent/미소비 native queued frame이 있으면 기존 owner settlement를 먼저 수행한다.
2. `tmux -N -S <검증한 socket> -C attach-session -E -f ignore-size,no-output -t =<logical>`의 전용 control client를 연다. `-N`으로 서버 자동 생성 금지, `-E`로 update-environment 금지, ignore-size/no-output으로 크기·화면 캡처 영향 제한. 전체 tmux 설정이나 pane 입력을 바꾸지 않는다. attach hook이 있는 환경의 관측 영향은 격리 수용 시험으로 확인한다.
3. **그 연결 안에서** exact pane metadata를 읽고 owner/channel/logical key, session/window/pane ID, spawn nonce/context, 현재 binding/source authority와 대조한다. `%begin`↔동일 command number의 `%end`/`%error`를 파싱한다. pane id는 `%[0-9]+`만 받는다. 빈 metadata, 출력 누락, 형식 오류, timeout은 권한을 만들지 않는다.
4. owner의 claim/cancel generation·mapping·host row가 여전히 같은지 재검증한다. 짧은 종료 명령 직전에 같은 connection의 exact pane metadata를 다시 확인하고 **그 pane ID만** 닫는다. `kill-pane -a`, `kill-session`, OS PID signal, process-tree 정리는 호출하지 않는다. pane가 마지막 pane이면 tmux가 해당 window/session을 자연히 없애는 부수효과는 수용 범위이며 다른 pane는 닫지 않는다.
5. reply loss면 `Unobserved`로 둔다. 같은 열린 연결에서 `%N` 존재 여부를 다시 읽고, 원래 provider의 process identity가 부재인지 확인한다. **kill-pane 성공은 provider 종료·transcript flush 증명이 아니다.** pane 없이 provider가 살아 있으면 새 provider를 만들지 않고 EvidenceWait로 유지한다.
6. 연결 종료/daemon restart 때 capability는 폐기한다. journal에는 증거를 기록하되 OS 연결 객체를 직렬화했다고 취급하지 않는다. 새 연결은 현재 logical/nonce/source를 **새로 검증**해서만 새로운 capability를 만든다. 옛 `%N`을 socket 경로에 무조건 재전송하지 않는다. 새 서버의 같은 번호 pane는 다른 object다.

공식 tmux 문서는 server가 살아 있는 동안 object ID를 재사용하지 않으며 다른 서버는 같은 ID를 쓸 수 있다고 설명한다. 따라서 PID나 socket 파일명만이 아닌 **연결된 서버 lifetime + pane object**가 권한 단위다. [tmux 공식 Advanced Use](https://github.com/tmux/tmux/wiki/Advanced-Use/4aaa59af320a585fe488332775a336ad06c1ccfb#unique-identifiers), [3.5a pane ID 할당](https://github.com/tmux/tmux/blob/3.5a/window.c#L886).

숫자 PID/start는 provider **부재 관측**에만 쓴다. 재사용된 PID에 신호를 보내는 fallback은 없으므로 `identity check → exit/reuse → kill(PID)` 창을 만들지 않는다. 소유 pane 안의 프로세스 구성 변화와 AgentDesk clear/relaunch는 owner generation/nonce를 다시 검사한다. 이 계약은 “아무 사용자가 소유 pane 안에 직접 바꿔 넣는 모든 프로세스까지 보존한다”는 권한 모델이 아니다. AgentDesk가 소유권을 잃은 pane에는 close를 보내지 않는다. pane root가 shell/wrapper인지 못 읽으면 Running이라고 넓혀 kill하지 않고 양성 소유 증거를 다시 얻는다.

`if-shell -F`를 별도 one-shot CLI로 보내기만 하는 방법은 선택하지 않았다. 같은 `%N`이라도 새 서버로 연결될 수 있기 때문이다. **열린 연결**을 권한과 함께 보관하는 것이 핵심이다. control protocol과 attach flags는 [tmux 매뉴얼](https://man.openbsd.org/tmux#CONTROL_MODE)에 정의돼 있다. `cmd-kill-pane`는 server의 pane object를 찾아 닫는다([3.5a 구현](https://github.com/tmux/tmux/blob/3.5a/cmd-kill-pane.c#L39)).

### macOS / Linux의 보장 범위

| 항목 | Linux | macOS |
|---|---|---|
| 다른 pane/다른 server의 숫자 ID로 오종료하지 않음 | 동일 연결·exact object 계약. WSL tmux 3.4 격리 probe 3건 확인 | 같은 tmux protocol 계약. 해당 배포 tmux 버전의 동일 격리 수용 시험 필요 |
| OS PID 재사용 뒤 무관 PID signal | 새 경로에 PID/PGID signal 자체가 없음 | 동일 |
| daemon 재시작 후 같은 socket/pane 번호 | capability 폐기 후 새 관측, 무조건 replay 0. 격리 probe에서 새 서버 `%0` 생존 확인 | 같은 규칙, 실제 버전 시험 필요 |
| 원래 provider가 확실히 끝남 | pane close와 별도로 process-start identity/부재 관측. 읽기 실패≠부재 | 기존 Darwin process identity 판독으로 부재 관측; 판독 실패≠부재 |
| PTY close 후 Claude/자식 flush·완전 종료 | API 자체는 보장하지 않음. orphan kill 확대 없음 | 동일 |
| Claude 2.1.322 partial-tail 이어쓰기 | 개발 격리 실제 CLI 수용 시험 필수 | 운영 플랫폼의 같은 버전 수용 시험 필수 |

이번 private probe는 설정을 읽지 않도록 `-f /dev/null`로 별도 서버만 만들었고, 이 레인의 `logs/d2/tc-*/s` socket 및 `i8077-d2-probe` 이름만 사용했다. 운영 AgentDesk tmux·default socket·운영 Herdr는 접근하지 않았다. 실제로 없어진 pane의 `display-message`가 **exit 0 + 빈 값**을 돌려줬다. 이는 missing을 승인하는 proof가 아니며 metadata를 엄격하게 파싱해야 하는 실측 근거다. 새 서버 `%0`은 옛 control 연결로 닫히지 않았다. raw protocol과 소스 링크는 증거 목록에 남겼다.

## 3. 새 Herdr 첫 입력: native 준비와 typed no-key

### 준비 신호

새 execution의 최초 자동 입력에만 `ResumeFirstInput` capability를 만든다. 필요한 증거는 아래 전부다.

- 이번 handoff의 이전 execution Ended와 최종 current native ID/source/auth/O continuity.
- 새로운/재사용 Pending의 정확한 execution nonce와 pane/server witness. bound row/source attach가 그 nonce를 가리킴.
- `SessionStart`, provider=claude, `source=resume`, expected native ID 일치, original_context nonce/launch_mode 일치. `session_handoff.rs:192`와 `herdr_turn.rs:705`의 기존 검증을 재사용한다.
- hook의 source record가 durable하게 기록됐음. `hook_server/observation_ingress/resume_start.rs:27`은 nonce/ID를 확인하고 `:47`–`:58`에서 source 기록을 확인한 뒤 hook에 답한다. 방송 이벤트를 놓친 Pending 복구는 이 기록을 재조회한다.
- 그 첫 입력의 원본 claim/first-key 상태가 아직 무전송이며, 다른 AgentDesk 입력·native prompt/clear/continuation으로 선행 사용되지 않았음. capability는 원본 identity+claim generation+physical pane+nonce+native ID+source record 순번에 결합하고 최초 payload-start에서 소모한다.

SessionStart는 “어떤 화면이든 빈 입력창이다”라는 전역 증명이 아니다. capability는 **AgentDesk가 방금 만든 실행의 아직 사용하지 않은 자동 첫 입력 epoch**에만 적용한다. Bound의 일반 후속 입력, 임의의 기존 pane, 다른 nonce/ID, 사용한 최초 입력에는 적용하지 않는다. 관측된 사람/외부 입력이나 clear/새 source가 있으면 capability를 취소하여 기존 owner 경로에 귀속시킨다. 사람 초안 판독 함수는 수정하지 않는다. 새 pane에서 자동 첫 입력과 사용자의 **아직 제출되지 않은 로컬 편집이 경합하는 경우의 보존·폐기 UX는 보류된 정책 범위**이며 이 모델은 그 편집을 관측할 수 있다고 가정하지 않는다. 이 경계를 모든 사람 초안 안전 증명이라고 확대 보고하지 않는다. 구현 수용 시험에서 미관측 로컬 draft를 자동 실행하는 반례가 나오면 이 capability를 그 경우에 허용하는 구현은 착지하지 않으며, 사람 정책을 별도 결정해야 한다.

선택한 최초 입력 경로는 이 capability로 준비를 판단하고 **일반 `composer::run`/final_prompt_ready/draft_sighting을 준비의 전제로 쓰지 않는다**. 기존 사람 초안 판독의 의미나 일반 busy/후속 입력 규칙을 바꾸는 것은 아니다. 최초 자동 입력 준비와 사람 편집 정책을 같은 boolean으로 합치지 않는다.

`SessionStart`는 provider render/키 수신 성공까지 보장한다고 주장하지 않는다. 실제 2.1.322에서 “own resume hook → 한 first frame → 정확한 native user receipt”가 이어지는지 격리 시험으로 확인해야 한다. `SessionStart.initialUserMessage`를 TUI 주입 수단으로 가정하지 않는다. 공식 문서는 이 필드를 `-p` 비대화형에 한정한다. 이 설계는 provider를 `-p`/SDK로 교체하지 않는다([Claude hooks 문서](https://code.claude.com/docs/en/hooks#sessionstart-decision-control)).

### 쓰기 경계와 typed 거절

새 `herdr_turn/native_first.rs`가 capability·owner first-key fence를 확인하고, bound target의 gate/writer fence 아래에서 **bracketed paste+Enter 한 `pane.send_input` RPC**를 보낸다. `session_host/herdr/model.rs:229`에는 해당 typed request가 있고, `herdr_gate.rs:535`의 send_line은 이미 같은 RPC를 한 줄에 사용한다. multiline용 형제 adapter는 같은 bracketed-paste end-marker 거절 규칙(`:523`)을 지킨다. 기존 send_line을 child module로 옮겨 1000행 넘는 parent에 새 책임을 쌓지 않는다. 이 한정 adapter는 내용을 쉘 명령이나 slash로 직접 실행하지 않고 기존 framed original을 전달한다.

한 RPC도 lost reply에서 exactly-once acceptance를 보장하지는 않는다. `HostMutation::Refused`는 서버/owner가 양성으로 무전송을 증명한 경우이며 `Indeterminate`/transport/timeout/protocol error는 메시지 재전송 권한이 아니다(`session_host/model.rs:127`). Confirmed는 전송 완료이고 실제 처리 영수증/terminal은 기존 reader가 별도로 확인한다.

기존 legacy/string 경로를 다음처럼 좁게 바꾼다.

| 결과 | owner 처리 | 자동 진행 신호 |
|---|---|---|
| cap 없음/own hook 아직 없음/lease 또는 gate의 양성 pre-key refusal | `SubmissionError::Held` BeforePayload, `owner_input::vetoed` 또는 `first_key_held`, `capture_preparation`으로 scope에 남김 | durable own-start/source attach, host-ready, writer lease 해제, transition 완료, boot 복구. subscribe 후 즉시 현재 증거 재조회하여 이미 Ready인 경우도 wake |
| 사람이 명시적으로 Cancel/clear하여 first-key gate 선점 | 해당 control의 취소 terminal. 성공 전송이나 일반 retry로 위장하지 않음 | 다음 원본의 정상 owner drive |
| capture/화면 기반 기존 adapter가 첫 키 전 거절 | 동일 typed Held + 양성 NoKeyYet. 새 최초 경로는 native readiness로 재선택하며 화면 recovery만 감시하지 않음 | native/host-ready의 level check로 원본 재실행; 이것을 LostReceipt 예외로 넘기지 않음 |
| 첫 키 이후 실패 또는 응답 불명 | WriteUncertain/현재 attempt 증거 대기. owner가 다른 destination/새 nonce로 재주입하지 않음 | exact native receipt/terminal 관측; 기존 LostReceipt 한계는 공개 |

핵심은 `first_input::write`/`submit_owned`가 내부에서 `InputRun::into_legacy()`로 증거를 지우지 않는 것이다. `CreatedSubmit`은 이미 typed `Result<(), SubmissionError>`다(`claude_tui/owner_input.rs:325`). `SubmissionError::Held`를 `effect::created`의 `Completion::Held`로 매핑하는 기존 입구(`effect.rs:367`)를 사용한다. 현재 `owner_input.rs:484`–`:491`은 allowed=false, keyed=false, known_before인 **정확한 attempt**만 NotSentProof를 만든다. 반환 enum만 Held로 바꾸고 이 조건을 채우지 않는 수리는 불수용이다. 반드시 `effect::run:122`→`hold_not_sent`로 들어가는 것을 실제 owner 시험으로 확인한다. side effect로 전역 draft guard를 세우는 생성자를 무심코 호출하지 않고, BeforePayload 증거만 구성한다.

첫 입력 hold 파일은 양성 무전송일 때만 종료한다. payload 불확실성의 hold는 유지한다. capability를 아직 사용하지 않은 동일 Pending/Bound의 재시도는 **기존 실행**에서 이 원본만 다시 준비하며 새 pane/create를 반복하지 않는다. 원본의 첫 RPC가 불명확하면 재시도하지 않는다. ordinary Existing 입력에는 capability가 없으므로 기존 정책을 유지하되, 그 pre-key 거절이 typed Held가 되는 연결은 공유한다.

### 최신 native ID

`command_session_id`/최초 argv UUID A는 process launch lineage다. continuation으로 실제 채널/source가 B 또는 C로 바뀔 수 있다(`adoption_retry.rs:395`). Begin에서 account/source가 적합한지 확인하고, old provider 종료 후 같은 logical binding의 마지막 durable source/rotation을 다시 읽어 **C를 resume target**으로 고정한다. hook nonce는 새 execution, C는 provider 대화 ID이므로 혼동하지 않는다. source/auth/O cursor를 C에 대해 다시 검사한다. read 오류·미완료 adoption은 Unknown이며 A로 fallback하지 않는다. 관련 기록 완료/owner recovery 이벤트가 원본을 재개한다. 이미 main에 있는 `--resume`/own-hook/O cursor guard는 재구현하지 않는다.

## 4. owner 상태, 진입점, 남는 화면 판독

### 같은 원본과 journal

V2 snapshot의 optional `unfinished_handoff`는 원본 identity/receipt **참조**만 보관한다. 원문은 기존 items/head에 한 번만 존재한다. 단계: Begin → Stopping → ProvisionalOld 또는 OldEnded → LaunchPending → ResumeReady → FirstAttempt → Complete. ProvisionalOld의 양성 no-key는 RevalidatingOld로 돌아간다. 첫 key가 있었으면 OldProcessingTerminal까지 기다려 전환만 계속하며 그 입력을 Herdr에 재전달하지 않는다.

active owner와 native old turn은 별개다. 이미 수행 중인 old turn은 사용자 결정에 따라 중단하지만, 다른 원본의 미확인/미소비 provider queued frame을 같이 버리는 권한은 추가하지 않는다. 그 입력의 귀속을 먼저 확정하고 새 입력들은 V2 owner 뒤에 쌓는다. old fallback이나 create 실패를 generic opaque start_failure 카운터에 넣지 않는다. pre-content 단계는 owner queued/journal custody이며 확인 deadline의 Unknown→retained tick 대상 head로 두지 않는다.

active preflight는 `!synthetic && provider=Claude && mapped endpoint local && canonical row Legacy`에서 **marker가 없어도** 동작한다. `hand_busy` 호출 직전에 같은 front grant/owner generation을 actor에서 확인하고 Begin을 publish한 뒤 claim을 이동한다. 외부에서 읽은 marker=false를 근거로 legacy busy leaf를 나중에 시작하지 않는다. 동시에 들어온 두 번째 원본은 admission/probe 결과와 관계없이 **commit 시점의 journal** 뒤로 들어간다. Bound Herdr는 기존 hand_busy, Pending은 해당 handoff/nonce 복구, unknown/nonlocal/다른 provider는 기존 경로다. DB 실패를 Legacy로 바꾸지 않는다.

### 갱신한 production 진입점 전수표

전체 git-grep 근거는 d1 logs의 4개 전수 파일을 재사용하고, r2의 추가 검색을 `logs/d2/entrypoints.txt`에 남긴다. 아래 좌표는 production 기준 SHA다.

| 진입/소비 경로 | file:line | r2 모델 / 연결 의무 |
|---|---|---|
| 채널 single owner drive/front grant | `src/services/discord/queue_io/input_owner/drive.rs:87`, `:168` | ProbeIntake/CommitIntake; 같은 원본 이동 |
| live/catch-up active 판정 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:205`, `:217`, `:238` | **hand_busy 전 mapped Legacy preflight→CommitIntake(Begin)**; synthetic 정책 유지 |
| active registration의 현재 거절 | 같은 파일 `:76`, `:91`, `:325`, `:359` | Bound 없음→Absent인 old 경로. 새 preflight 없으면 BeforeActiveEntry RED |
| native busy를 idle start가 나중에 발견 | 같은 파일 `:535`, `:549` | 재차 host 선택만으로 old fallback 불가; 같은 journal/claim으로 handoff 또는 검증된 provisional-old adapter |
| idle 첫 메시지와 retry | 같은 파일 `:834`, `:852`, `:875` | 동일 preflight/journal을 재사용, claim 복사 금지 |
| host 선택 / provider dispatch | `src/services/turn_host.rs:151`, `:291`; `src/services/discord/router/message_handler/provider_dispatch.rs:631` | local/mapping/O admission. Codex selector 수정 없음 |
| Legacy/Retired forward | `src/services/claude/herdr_turn.rs:256`, `:273`, `:623`, `:644` | launched_source에서 `/exit` 대신 ClosePinnedPane/VerifyOldAbsent. 새 책임은 child module |
| 공통 기존 종료/역방향 Return | `src/services/claude/session_handoff.rs:60`, `:115`; `src/services/claude/herdr_return.rs:80`, `:83`, `:173` | forward 호출만 교체. Return의 기존 end_by_exit는 범위 밖 |
| exact tmux pane snapshot | `src/services/tui_input/submission_tmux/guarded.rs:186` | snapshot 형식/identity 일부 재사용. 이것만으로 reconnect-safe close를 승인하지 않음 |
| 기존 broad cleanup / kill helper | `src/services/claude_tui/host_input.rs:216`; `src/services/process.rs:527`, `:546` | 새 경로 호출 0; NoForeignClose mutant 비교 기준 |
| Pending/unplaced 재시도 | `src/services/claude/herdr_turn.rs:393`, `:401`, `:417` | 같은 nonce/source 복구, 중복 create 0 |
| 새 incarnation/launch context | `src/services/herdr_launch.rs:457`, `:468`, `:723`; `src/services/claude/tui_session_launch.rs:126` | Launch/Create. native ID는 최종 현재 ID |
| own resumed SessionStart 대기 | `src/services/claude/herdr_turn.rs:666`, `:705`; `src/services/claude/session_handoff.rs:192` | OwnResumeStartAndAttach → nativeReady capability |
| durable own-start source | `src/services/claude_tui/hook_server/observation_ingress/resume_start.rs:27`, `:47` | nonce/ID record 완료로 복구·wake. 이미 도착한 signal도 level 재조회 |
| continuation adoption | `src/services/claude_tui/hook_server/adoption_retry.rs:395`; `src/services/tui_prompt_dedupe/runtime_binding/adopt_skip.rs:81`; `src/services/claude_tui/session.rs:75` | NativeContinuation→VerifyOldAbsent에서 최종 native ID 재조회 |
| 새 first_input / owner callback | `src/services/claude/herdr_turn/first_input.rs:22`, `:48`, `:94`, `:112`; `src/services/claude/herdr_turn.rs:804` | NewPreKeyRefused→Held/NativeReadyWake. 내부 문자열 변환 제거 |
| CreatedSubmit/typed owner 준비 | `src/services/claude_tui/owner_input.rs:325`, `:312`, `:484`; `src/services/claude_tui/guarded_outcome.rs:128` | exact NotSentProof를 실제 생성. Enum만 Held인 0/PASS 금지 |
| effect 매핑 / settlement | `src/services/discord/queue_io/input_owner/effect.rs:360`, `:122`, `:139` | Held+proof→hold_not_sent. untyped refused→WriteUncertain mutant RED |
| atomic Herdr payload | `src/services/session_host/herdr/model.rs:229`; `src/services/session_host/herdr_gate.rs:523`, `:535`, `:556` | native first-frame 한 RPC, writer/gate 유지. parent giant 파일에는 선언/기존 method 이동만 |
| 기존 composer 화면 검사 | `src/services/claude/herdr_turn/composer.rs:50`, `:81`, `:111`, `:119` | 최초 resume epoch의 준비 의존에서 제외. 일반 후속/Return은 그대로 |
| old fallback busy transaction | `src/services/claude_tui/busy_inject/submit.rs:35`; `src/services/claude_tui/busy_inject/stash.rs:22`, `:135` | ChooseOld/ParkDraft/Arm/Submit. 읽기 실패+no-key면 ReleaseOldNoKey, key 뒤 전환 변경 0 |
| 기존 held screen watch | `src/services/discord/queue_io/input_owner/held_watch.rs:76`, `:82` | 화면 recover만 기다리는 watch를 handoff에 재사용하지 않음. cause/native journal watcher가 담당 |
| source/input hold | `src/services/claude/herdr_turn/first_input.rs:86`, `:103`; `src/services/claude/herdr_turn.rs:555` | 양성 no-key hold 해제 vs unknown hold 유지 |
| generic 시작 실패와 retained 생성 | `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:226`, `:248`; `src/services/turn_orchestrator/queued_head/v2/start_failure.rs:95` | handoff pre-input 결과는 별도 typed custody; generic threshold 확대/삭제 없음 |
| effect NotSent/Held 및 tick | `src/services/turn_orchestrator/queued_head/v2/retained.rs:58`, `:142`, `:166` | pre-key는 Held. 실제 uncertain content의 LostReceipt 한계는 유지 |
| 과거 retained/notice/자동 requeue | 같은 파일 `:225`, `:249`, `:283`, `:386` | **범위 제외·동작 변경 0**. 다섯 운영 보관건의 자동 실행 약속 철회 |
| owner snapshot commit | `src/services/turn_orchestrator/queued_head/v2/commit.rs:17`, `:39` | destination/원본 참조/phase/generation을 한 durable publish로 변경 |
| boot V2 census | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:87`; `src/services/turn_orchestrator/queued_head/compat.rs:20` | **holds_input OR unfinished_handoff**(Claude만). BootCensus에서 actor 선택 |
| gateway/REST 공통 boot / owner 복구 | `src/services/discord/runtime_bootstrap/queued_recovery.rs:48`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:664`, `:680`; `src/services/discord/queue_io/input_owner/recovery.rs:8` | BootWake는 앞의 입력 유무와 독립. journal-only 전환 driver도 시작 |
| Herdr boot reconcile / Pending | `src/services/discord/recovery_engine/herdr_reader.rs:217`, `:455`, `:463`; `src/services/discord/recovery_engine/host_reconcile.rs:94` | 기존 관측/복구 유지. reconcile이 kill/create 또는 journal wake를 해준다고 가정하지 않음 |
| Cancel / 전체 대기열 비우기 | `src/services/discord/commands/control.rs:715`; `src/services/turn_orchestrator/queued_head/v2/control.rs:97`, `:206`, `:401` | CancelBeforeKey, generation/unfinished record를 같은 commit에서 무효화 |
| /clear의 transition 추월 | `src/services/discord/commands/control.rs:510`, `:519`; `src/services/turn_orchestrator/queued_head/v2/control.rs:277` | lock만 믿지 않고 실제 effect 앞 generation 재검증. late callback은 새 실행에 key/close 0 |
| /stop·text stop | `src/services/discord/commands/stop.rs:84`, `:157`; `src/services/discord/commands/text_commands/native_stop.rs:4` | 기존 native stop/queue wake. 모든 waiting original을 자동 취소한 것으로 간주하지 않음 |
| Terminate | `src/services/discord/herdr_terminate.rs:401`, `:482`, `:615`; `src/services/termination_audit/host_terminate.rs:116` | 기존 정확한 fence. 새 tmux close를 generic Terminate/Herdr close로 노출하지 않음 |
| 전환·turn 완료 wake | `src/services/discord/session_transition.rs:132`, `:145`; `src/services/discord/queue_io/input_owner/drive.rs:87` | 원본 또는 journal-only driver wake; lock 해제 뒤 현재 record를 재조회 |
| O/partial tail | `src/services/tui_o/writer/renumbered.rs:50`; `src/services/tui_o/writer/actor.rs:669`; `src/services/tui_o/shadow/capture.rs:331`, `:340` | 동일 source cursor/spool, 완전 record만 전진; partial 수정/EOF jump/0 reset 없음 |

재시작 후 input-free journal에는 front grant가 없을 수 있다. 기존 `drive::grant`가 입력을 발견할 때까지 기다리는 것으로 구현하지 않는다. `claude_handoff` driver가 actor의 unfinished phase를 읽어 **prompt 없이 전환 작업만** 수행할 수 있어야 하며 완료 후 journal을 끝낸다. `holds_input`의 전역 의미를 다른 provider까지 바꾸지 않고 Claude 전용 census predicate를 추가한다.

### 입력 출처와 남는 화면 판독

출처 정책은 d1과 동일하되 과거 회수는 없다. `queue_io/input_owner/origin.rs:25`–`:41`의 Discord/iMessage 사람 policy와 HTTP/routine의 synthetic 대기 policy를 보존한다. 허용 Discord 봇은 `router/intake_gate.rs:329`의 gate를 먼저 통과하며 `intake_gate/busy_queue.rs:5`의 bot/allowed-automation provenance를 잃지 않는다. 사람/봇의 원본 ID·uploads·reply/source member를 같은 owner가 갖고, source 문자열이나 본문만으로 소유자를 추정하지 않는다. native composer draft는 V2 원본이 아니다. 다른 provider/remote TUI/process는 기존 placement/owner를 따른다.

| 화면 판독 지점 | r2에서의 위치 | 실패가 이 handoff를 영구 대기/보관시키지 않는 이유 |
|---|---|---|
| forward `/exit`의 observation/final_ready/draft_sighting | **사용하지 않음** | old 종료는 pinned pane close; unread은 stop guard가 아님 |
| old fallback의 사람 draft/box/modal/copy-mode | 기존 판독 함수 그대로 | 키 전 양성 no-key면 old 선택 철회→fresh identity 재검증→pane 종료 재시도. RecoverScreen fairness 없음. 키 후는 destination 불변 |
| 새 own-resume 자동 첫 입력의 준비 | native capability 사용, 일반 composer gate 제거 | hook/source/owner·host gate 거절은 typed Held+native/host-ready wake. 화면이 읽힐 때까지 기다리는 watcher를 달지 않음 |
| Bound 일반 후속 입력 / 기존 pane / capability 없는 ordinary input | 기존 composer/draft 보호 유지 | 이번 수리로 판독 의미를 바꾸지 않음. 같은 owner의 pre-key Typed Held 연결은 보호. 이 범위의 모든 native modal/사람 편집을 자동 해소한다고 주장하지 않음 |
| Herdr→tmux Return의 `/exit` | 기존 main 그대로, **범위 밖** | `herdr_return.rs:173`의 기존 한계는 남는다. forward 개선을 Return 해결이라고 보고하지 않음 |

사람 초안 정책이 보류됐으므로 d1의 “전환 전 새 draft-discard notice receipt가 필수이며 이후 폐기”를 구현 계약으로 확정하지 않는다. 진행 중 turn/pane가 종료된다는 handoff 상태 통지는 가능하지만, 사람이 쓰던 초안을 몰래 clear하거나 Enter로 실행하는 기능을 추가하지 않는다. pane close가 volatile draft의 보존을 보장하지 않는 사실은 숨기지 않는다. 별도 초안 백업/복원, 허용·폐기 UX는 후속 결정이다.

## 5. 크기 재산정과 분할

계획 상한 **production 22파일 / 순 코드 +1180**. `pr_cap_prod.py` 상수 FILES=30/NET=1500, 이번 PLAN 정지선 +1200 이하다. d1의 retained 이관/원본 ticket 수리 약 200줄 이상을 빼고 active entry·native first·정확한 control connection 종료를 포함한 상한 추정이다. 실제 구현 cap 측정치가 아니다.

| 파일/묶음 | 수 | 순 코드 추정 |
|---|---:|---:|
| `claude/session_handoff/pinned_tmux.rs` 신규 | 1 | +230 |
| `claude/session_handoff.rs` 연결 | 1 | +12 |
| `claude/herdr_turn/handoff.rs` 신규 + `herdr_turn.rs` 기존 launch 이동 | 2 | +25 |
| `claude/herdr_turn/native_first.rs` 신규 | 1 | +120 |
| `claude/herdr_turn/first_input.rs` typed 결과/adapter | 1 | +65 |
| `claude_tui/owner_input.rs` exact no-key 연결 | 1 | +12 |
| `discord/queue_io/input_owner/claude_handoff.rs` 신규 + module | 2 | +248 |
| `discord/router/message_handler/provider_dispatch/owner_input.rs` 연관 helper 이동/호출 | 1 | +0 |
| `turn_orchestrator/queued_head/v2/claude_handoff.rs` 신규 + `v2.rs` | 2 | +128 |
| `queued_head/v2_types.rs` optional journal | 1 | +45 |
| `queued_head/v2/control.rs` generation/취소 | 1 | +20 |
| `discord/queue_io/input_owner/recovery.rs` | 1 | +20 |
| `pending_queue_persistence/queued_recovery.rs` + `queued_head/compat.rs` | 2 | +17 |
| `discord/queue_io/input_owner/effect.rs` typed 매핑 | 1 | +18 |
| `claude_tui/guarded_outcome.rs` typed 전달 glue | 1 | +10 |
| `.../provider_dispatch/start_failure.rs` handoff 결과 분리 | 1 | +10 |
| `session_host/herdr_gate/prompt.rs` 신규 + `herdr_gate.rs` 기존 send_line 이동 | 2 | +30 |
| 여유 | 0 | +170 |
| 합계 | **22** | **+1180** |

1000행 넘는 `owner_input.rs`는 기존 연관 helper를 child로 옮겨 net growth 0 이하, 993행 `herdr_turn.rs`는 launch 분리로 줄인다. 거대한 `herdr_gate.rs`도 기존 send_line의 책임을 child로 옮기고 multiline adapter를 그 child에 넣어 parent 순증 0 이하로 한다. `host_input.rs`, `tmux_common.rs`, `process.rs`에는 새 책임을 얹지 않는다. dormant API/새 allow/baseline 인상은 없다.

구현이 +1200을 넘으면 상한을 올리지 않고 두 조각으로 나눈다. A는 pane close+active Begin+reversible old 선택+journal boot와 **typed pre-key custody/wake까지**(약 16파일/+850)다. 기존 화면 판독을 아직 쓰더라도 알려진 no-key를 retained로 보내지 않으므로 독립적으로 안전하지만 전체 진행성 DoD 완료라고 하지 않는다. B는 own-resume native first+atomic frame adapter/관련 승인 시험(약 8파일/+330, 중복 파일 2개)다. B가 2.1.322 수용 시험을 만족해야 전체 이슈를 완료한다. A만 착지한 상태에서 모델 전체 GREEN을 제품 완료로 사용하지 않는다.

## 6. 시험·수용 계획과 상태 모델

### 실제 구현 시험

| 시험 | 수용 기준 / mutant |
|---|---|
| 고정 unread + 종료 한 번 실패 | 화면을 영원히 같은 46열로 고정. old provisional 선택 뒤 **양성 no-key**로 철회→신원 재검증→종료 성공→같은 원본 Herdr1. read-ready 회복/사람 재전송/retained0. ReleaseOldNoKey 제거 mutant RED |
| old 선택 이후 C-s 또는 payload key 있음 | no-key 철회 금지, 다른 destination0. 정확한 old native 처리 terminal까지 추가 종료0. key_count를 숨기는 mutant RED |
| 새 first-input pre-key refusal | real first_input→CreatedSubmit→effect::created→owner NotSentProof→hold_not_sent를 통과. generic 문자열/settle(false)/retained0. 신호가 watch 등록 전에 도착해도 즉시 level 재조회로 같은 원본1. Typed Held 또는 wake 삭제 mutant RED |
| active 최초 진입 | marker 없는 Legacy+실제 active row에서 owner_drive 시작. 46×39 제공 화면 fixture(익명화) 사용. Registration::Absent/hand_busy 거절 전에 Begin, old 종료, own-resume, 원본1. idle test와 별도 |
| active N=2 stale marker | 두 메시지 각각 probe를 먼저 중단하고 첫 Begin 후 두 번째 commit. 두 번째가 old busy key를 보내지 않고 같은 owner 큐 뒤에 남음. 각각 서로 다른 ID면 동일 본문도 각1. commit 재검증 제거 mutant RED |
| pane/server/PID 재사용 | fake adapter 및 isolated tmux. 옛 pane 삭제·새 pane, 서버 교체 후 같은 `%0`, root PID 재사용, close reply loss. 새 pane/server/무관 PID에 mutation0. metadata 빈 값 exit0은 실패. numeric signal/reconnect-number mutant RED |
| input-free journal boot | old input이 native terminal로 정산된 뒤 전환 전에 crash. queue/head/retained 입력0, unfinished record1. 실제 v2_channels→owner_recover→journal driver→전환완료. census 제거/선택 후 wake 제거 각각 RED |
| native 세션 ID | 최초 argv A, binding B, stop 전 continuation C. 새 provider는 `--resume C`; own source=resume+new nonce 일치. A/B를 고정한 mutant RED. 초기/최종 account/transcript/O continuity를 모두 확인 |
| own-resume 최초 신호 | wrong nonce/native ID/source·선행 native input·cap 이미 소비·clear/supersede를 조합. invalid cap은 양성 Held 또는 current owner의 terminal이며 자동 일반 ready override0. hook을 받았다는 bool 하나로 보내는 mutant RED |
| 사람 draft | 기존 visible/stashed/unread/attachment reader 시험의 의미 유지. old fallback unparked Enter0. 새 초기 pane에 인간 입력이 개입한 수용 반례를 별도 제시하며 보류된 UX를 완료했다고 세지 않음 |
| 실제 Claude 2.1.322 / partial-tail | macOS·Linux의 격리 계정/임시 workspace에서 busy→pane close→provider 종료 확인→현재 native ID resume. newline 미완성 transcript, 완전 persisted record, 미게시 spool, 기존 게시 body를 검사. 완전 record/O cursor 보존, 중복 게시0, native user receipt1. **CLI exit0/SessionStart만으로 PASS 금지** |
| Cancel/clear/stop/Terminate 경합 | owner publish/effect 각 경계 pause→control→late callback. cancel generation과 physical epoch가 달라지면 key/close0. clear가 transition 3초 후 추월해도 새 실행을 지우지 않음 |
| 저장/신호 장애 | journal publish 전/후·fsync 불명·control EOF·Pending create reply loss·hook record 실패·writer lease 거절. 원본 복제/Unknown→0/새 provider 중복0. recover가 같은 exact attempt를 이어받음 |
| 제외 범위 | 과거 retained 5건/told/attempt=None/Opaque bind를 자동 변경하거나 재실행하지 않음. Codex/Return/타 채널/원격/process에는 새 close·cap0 |

실제 Rust 대상 시험은 새 작은 `*_tests.rs`에 나누고 required PR lane의 test ID 선택을 확인한다. 구현 REVIEW_READY 전에 mutant 원복 SHA, 기본 병렬 대상 시험, clippy warning no-growth, source_scan_readers failed0, static prepush를 완료한다. 이 PLAN 레그에서는 repo cargo·PG·clippy·prepush를 실행했다고 보고하지 않는다. cargo를 실행할 때는 shim과 지정 네 profile 변수, CARGO_TARGET_DIR 불변을 지킨다. 운영 adk-cc busy 메시지 1건→전환·답 게시의 DoD는 구현/리뷰 후 coordinator가 수행한다.

### 모델 범위와 실제 결과

`HerdrBusyHandoff.tla`는 **31개 cfg / 136회 TLC / 84.3초**, 저장소 CI 검사기 exit0으로 모든 기대 위반 집합이 일치했다. 모델 작업은 06:17 UTC 재개 후 06:40 전에 동결했고(마지막 모델 mtime 06:39:58 UTC), 최종 검사는 06:41:25 UTC에 끝나 30분 정지선을 지켰다. frontend 유무와 관계없이 r2의 sc=GREEN은 직접 TLC의 제안 cfg 결과를 뜻한다. `.chart.mmd`는 모델 전이의 손작성 투영이고 frontend 생성 결과라고 하지 않는다.

핵심 규칙: InputAtMostOnce, NoSilentLoss, NoUserResend, DraftNeverExecuted, OneProvider, SameSession, NoForeignClose, NoIntakeBypass. PROPERTY MessageProgress, HandoffProgress, **JournalProgress**를 실제 Spec fairness 아래 검사한다. initial incoming→probe→commit을 분리하여 active entry가 journal을 만들기 전의 실패도 HandoffProgress가 볼 수 있다. journal-only cfg는 q=done/effects1·old processing terminal 완료·server=down·actor/awake=false에서 시작하여 MessageProgress가 공허하게 통과하는 것과 별개로 두 journal 속성을 검사한다.

OldUnread은 상수이며 화면 복구 전이/fairness가 없다. 새 준비 거절은 FirstRefusals=1을 소모하고 native capability/host cause wake가 풀며, 화면이 변한다고 가정하지 않는다. 부팅 시 close connection capability는 소실(모델 peer token2)되고 ReacquireOldPeer가 **새 검증**을 수행한다. peer0/1/2는 유한 모델의 lifetime 등가류이며 OS PID가 아니다. B/C는 현재 native session, A는 최초 argv다.

환경 가정: 유한 daemon 재시작과 종료/host pre-key 실패, durable store/host/cause 서비스의 회복, 정상 own hook/receipt 도달. old pane close가 원래 provider를 결국 종료하거나 읽을 수 있는 old fallback에서 처리할 수 있음을 가정한다. 영구적으로 살아 있는 PTY-orphan/모든 API 실패/수신 증거 소실까지 해결한다고 주장하지 않는다. LostReceipt는 content 이후의 기존 한계로 RED이며, pre-key failure를 그 예외에 숨기지 않았다. 사람이 새 pane에 미제출 편집을 넣는 임의 경합이나 tmux attach hook의 외부 side effect 전체는 이 유한 모델의 증명 범위가 아니다.

cfg별 전체 실제 결과는 동반 `.report.md`와 외부 `report-design-2.md`의 표에 적는다. BeforeFailureUnread, BeforeFirstRefusal, BeforeActiveEntry, BeforeActiveRace, BeforeJournalCensus/NoWake, PidSignal/ReconnectNumber, BeforeContinuation, DraftExecution은 각각 지적을 삭제한 반례다. 과거 회수 cfg 두 개는 manifest와 파일에서 제거했다. 사람 draft 폐기 policy를 고정했던 DraftNotice cfg도 제거하고 실제 실행 위험을 보는 DraftExecution cfg로 교체했다.

### 리뷰 집중 항목

| 집중 항목 | 방지 또는 남은 반례 |
|---|---|
| 경계 시각 | close dispatch≠provider end, native hook≠payload receipt, old receipt≠processing terminal. 종료 뒤 최신 native source 재조회. 실제 partial flush/resume는 구현 수용 조건 |
| 동시성 | probe/commit 분리와 actor의 현재 journal 재검증, 이동하는 claim, first-key fence. N=2 stale-marker RED/GREEN. Rust 실제 active entry 시험 필수 |
| 재시작 | input-free unfinished census와 wake를 따로 검사. 연결 capability는 복원하지 않고 재검증. old binary가 새 optional journal을 소실시키는 downgrade/rollback은 지원 범위 밖 |
| 옛 형식 | journal 없는 V2는 기존 owner 경로, 과거 retained는 unchanged. unknown fields/proof를 신뢰로 승격하지 않음 |
| 문서상의 guard | active preflight 호출, no-key proof, old-choice 철회, native-ready level wake, journal-only driver와 pinned control 연결 각각 삭제 mutant/entry 시험 필요 |
| 잘못된 0/PASS | display-message exit0 빈 값 실측, backend receipt1·원본ID1·spool/O 결과로 검증. model31개/136회와 tmux probe3건, d1 predicate2건을 혼합 합산하지 않음 |

이번 레그에서 실제 production/운영 상태를 고치지는 않았다. 리뷰에 필요한 설계·상태 차트·근거만 커밋하며, 새 코드의 GREEN은 다음 구현 레그가 제공해야 한다.
