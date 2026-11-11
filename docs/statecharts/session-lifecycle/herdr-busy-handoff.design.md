# #8077 Claude tmux → Herdr busy 넘김 설계 1

기준 코드: `1fe3a57d266e1e7fe384796d22a925076f1d99fc` (`origin/main`, 조사 시작 HEAD). 브랜치 `fix/i8077-handoff-busy`, worktree `/home/adk/adk/wt/i8077`. 설계 레그이며 production 수정은 0이다. 이슈는 `gh issue view 8077 --repo itismyfield/AgentDesk --json title,body,url,comments`로 읽었다. 아래 좌표는 모두 이 SHA다. 제공된 운영 화면만 읽었으며 설정·운영 runtime/DB/O-store·실제 tmux/Herdr에는 접근하지 않았다.

## 0. 상태 차트와 계약

먼저 `docs/statecharts/README.md`의 Herdr 행과 `HerdrSession.tla/.cfg`, `HerdrRootRecovery.tla` 및 root/S2 보고서, `HerdrPendingRegistration.tla` 및 보고서, retired-marker cfg들을 읽었다. 관련 forward/return 모델 `docs/statecharts/host-migration/HostMigration.tla`도 대조했다. 기존 모델의 Pending/Bound 복구 및 Codex 등록부 수리는 변경하지 않는다. 이번 분기는 session-lifecycle의 파생 모델 `HerdrBusyHandoff.tla`로 추가했다. HostMigration의 `ForwardEndsTmux`가 성공을 원자적으로 가정했던 부분을 Running·busy/unread·종료 실패·입력 custody로 나눈다.

규칙은 다음과 같다.

- `InputAtMostOnce`: 각 원본 identity의 provider 입력 effect ≤ 1. `NoSilentLoss`: 입력은 durable custody 또는 실제 effect와 연결된 terminal에 있으며, 실행 0을 완료로 만들지 않는다. 둘과 `MessageProgress`를 합쳐 명시적 Cancel/clear가 없는 입력의 실행을 정확히 1회로 요구한다. 본문이 같은 서로 다른 Discord ID는 각각 1회다.
- `NoUserResend`: 이번 정책이 새로 만드는 재전송 요구는 0. 과거 이미 안내한 retained의 안내 이력은 지우지 않되, 원본을 자동 회수한다.
- `DraftNeverExecuted`, `DraftLossTold`: 사람 초안은 자동 실행하지 않는다. 강제 전환에서 보존을 약속하지 않으며, 폐기 가능성을 사용자에게 먼저 알린 뒤 종료한다. 단순 ANSI 캡처를 숨은 stash/첨부까지 보존한 백업이라고 부르지 않는다.
- `OneProvider`, `SameSession`: 이전 Claude 부재 확인 전 같은 ID의 Herdr provider를 만들지 않는다. resumable인 forward 전환은 같은 ID를 이어간다.
- PROPERTY `MessageProgress`, `HandoffProgress`: 원본이 결국 실행되고 전환이 완료된다. 명시적 Cancel/clear는 권한 있는 terminal이며 재실행하지 않는다. 실패 fallback으로 이전 실행에 전달했다면 그 입력이 실제 처리된 뒤 전환을 재개한다.

모델 한계도 명시한다. 유한 재시작, 읽기/저장/출력 서비스 복구, 신원이 증명된 provider의 종료 또는 fallback 실행 가능성, exact receipt/NoKeyYet 증거의 회복을 가정한다. 영구적인 EPERM·읽을 수 없는 모든 호스트·소실된 native receipt를 fairness로 해결했다고 주장하지 않는다. `LostReceipt` cfg는 이 마지막 반례를 RED로 남긴다. backend 입력 effect와 owner settlement는 별도 전이이며 둘 사이 Crash를 허용한다. `RecoverNoKey`는 writer의 양성 NoKeyYet 증거이며 transcript에서 안 보인다는 뜻이 아니다.

### 상태 차트 실행

지정 frontend `/home/adk/.claude/skills/statechart/scripts/statechart`가 없어 `SC_RUN=mac-needed`다. Java 21과 정본 TLC 1.7.4 jar가 있어 저장소 공식 `scripts/ci/statecharts_tlc.py`를 이 레인의 15개 cfg만 담은 격리 root에 실행했다. 최종 실행은 15/15 기대 위반 집합 일치, 28.0초, exit 0이다. 이는 RED cfg가 GREEN이라는 뜻이 아니다. frontend chart 생성은 Mac 후속이며 `.chart.mmd`는 손으로 작성한 설계 투영이라고 표시했다. 모델링은 시작 후 약 20분에서 동결하여 30분 정지선 안이다.

| cfg 접미사 (`HerdrBusyHandoff*.cfg`) | 입력/조건 | 기대 및 직접 TLC 확인 | 깨지는 규칙 |
|---|---|---|---|
| BeforeBusy | main, busy, N=1 | RED | NoUserResend, MessageProgress, HandoffProgress |
| BeforeUnread | main, 잘린 대기 화면, N=1 | RED | 동일 |
| AfterBusy | 제안, busy, N=2, restart 1 | GREEN | 없음 |
| AfterUnreadDraft | 제안, unread+잠재 초안, N=2, restart 1 | GREEN | 없음 |
| AfterStopFailure | 제안, 사람 초안, 종료 실패 1회, old fallback | GREEN | 없음 |
| AfterFailureUnread | 제안, 종료 실패+unread, 화면 복구 후 fallback | GREEN | 없음 |
| AfterRetained | 과거 retained 5개, restart 1 | GREEN | 없음 |
| Controls | N=2, Cancel/clear before content 경합 | GREEN | 없음 |
| Duplicate | 완료한 원본을 재전달하는 mutant | RED | InputAtMostOnce, NoSilentLoss, MessageProgress |
| SilentDrop | custody 삭제 mutant | RED | NoSilentLoss, MessageProgress, HandoffProgress |
| DraftNotice | 사전 안내 guard 삭제 mutant | RED | DraftLossTold |
| WrongSession | resume ID guard 삭제 mutant | RED | SameSession |
| NoWake | boot의 owner wake 삭제 mutant | RED | MessageProgress, HandoffProgress |
| RetainedRecovery | 과거 보관 회수 삭제 mutant | RED | MessageProgress, HandoffProgress |
| LostReceipt | effect 후 증거를 영원히 얻지 못함 | RED, 잔여 경계 | MessageProgress |

명시적 before trace: queued → Begin(stopping) → BeforeRefuse(retained, resendAsks=1). 새 경로: queued → Begin → TellDraftPolicy → SignalStop → ProviderEnds → VerifyEnd → Launch(Pending) → Create → Attach(동일 ID) → Choose(new) → Arm → Submit → Confirm. 실패 경로: StopFailed → Choose(old) → 필요 시 RecoverScreen/ParkDraft → Arm/Submit/Confirm → **OldCompletes** → FallbackDone → 종료·전환. Confirm만으로 fallback provider를 종료하면 입력이 native 큐에만 있는 상태에서 사라질 수 있으므로 금지한다.

## 1. 원인 확정

제공 파일 `/home/adk/adk/lane-runs/i8077/adkcc-pane-busy-1424.ansi`는 39행, 46열 화면이다. 버전 2.1.322·운영 다섯 시각은 브리프의 관측값이며 CLI/운영을 재조회하지 않았다. 파일 SHA256은 `b4ed2407b6cb970bf7d5538f3f86e9564974ce627aaf7ac19643da5d5bace893`다. **14:17:54 대기 화면 자체는 제공되지 않았다.** 아래 대기 재현은 busy 파일에서 `Synthesizing`·그 아래 Tip 3행을 뺀 합성 화면이며 실제 14:17 화면이라고 주장하지 않는다.

| 단계 | main 좌표 | 실제/합성 화면의 판정과 의미 |
|---|---|---|
| 캡처 | `src/services/claude/session_handoff.rs:115`, `src/services/claude_tui/host_input.rs:583` | `capture-pane -p -e ... -80`, ANSI 포함; unknown을 empty로 바꾸는 코드 없음 |
| 정규화 | `src/services/claude/herdr_turn/composer.rs:54`, `src/services/claude_tui/prompt_readiness.rs:14` | 정규화 대상은 완전한 `🤖 ... │ ...` 형식. 샘플 status 행은 이를 만족하지 않으며 footer/stash를 고치지 않음 |
| 입력 소유자 | `src/services/claude_tui/busy_inject/ownership.rs:74`, `src/services/claude_tui/busy_inject/screen.rs:326` | `typed`는 footer와 독립적으로 빈 `❯ `를 읽어 owner=Empty. **46열이어서 owner가 반드시 Unread인 것은 아님** |
| 준비 marker | `src/services/tmux_common.rs:323` | 격리 실행 결과 busy·합성 idle 모두 marker=true. marker만으로 준비를 확정하지 않음 |
| 최종 준비 | `src/services/claude_tui/input.rs:1257`, `:1267`, `:1285`, `src/services/claude_tui/prompt_readiness.rs:216` | busy 샘플의 structured spinner는 별도 busy=true라 final ready를 막음. 합성 idle은 busy=false·modal=false·빈 composer이므로 이 veto를 넘음 |
| exit 전용 조건 | `src/services/claude/herdr_turn/composer.rs:73` | 일반 prompt와 달리 `/exit`에는 `draft_sighting == Settled`를 추가 요구. 합성 idle도 여기서 false |
| footer/stash | `src/services/claude_tui/busy_inject/screen.rs:157`, `:175`, `:240`, `src/services/tmux_common/claude_footer.rs:5`, `:45` | 실제 마지막 행 `⏵⏵ bypass permissions on · 2 shells · ⧉ se`는 잘렸고 허용된 counted suffix 문법에도 맞지 않아 footer=Unknown, stash=Unknown |
| 두 번째 독립 장벽 | `src/services/claude_tui/busy_inject/screen.rs:298` | 입력창 위 `✔ Update installed · Restart to update`는 blank/기존 measured effort/전체 ASCII 정렬 행이 아니어서 stash=Unknown. footer만 정상 counted 문구로 바꿔도 Unsettled |
| draft 분류 | `src/services/claude_tui/busy_inject/ownership.rs:26` | footer가 Normal이 아니면 즉시 Unsettled. footer 수리만 한 대조도 update 행 때문에 Unsettled. 둘 다 measured 형태로 바꾼 대조만 Settled |
| 거절 | `src/services/claude/herdr_turn/composer.rs:125`, `src/services/claude/session_handoff.rs:87`, `:69` | busy는 polling timeout 경로, idle에서 final_ready가 true이면 **즉시 Break(false)** 경로. 둘 다 Refused(Unknown) → Untouched → earlier claude kept running: Unknown |
| 보관 | `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:226`, `src/services/turn_orchestrator/queued_head/v2/start_failure.rs:95` | opaque 실패는 원본별 횟수 ≥2에서 retained. 한 입력의 첫 Opaque 실패가 무조건 보관되는 것으로 모델링하지 않음; BeforeRefuse는 해당 보관 임계 도달을 축약 |

따라서 대기 화면에서도 실패하는 직접 원인은 **exit 전용 stash 증명 실패**다. busy 화면에는 final busy veto도 있다. 전역 readiness를 느슨하게 하거나 footer를 대충 통과시키면 사람 초안 보호를 다른 모든 입력 경로에서 약화시키므로 그렇게 해결하지 않는다. 강제 넘김은 composer 입력을 하지 않는 별도 경로로 설계한다.

확인용 cargo 격리 probe는 main의 ANSI/ownership/footer 구현과 screen 구현을 재사용했다(screen의 자식 모듈 위치만 scratch 경로에 맞춤). ready/busy/modal 함수는 원문 함수와 그 의존 함수들을 그대로 추출했다. 외부 frame registry와 사용하지 않는 modal adapter만 neutral stub이며 샘플에는 해당 frame이 없다. whole-app 통합 시험이 아니다. 2시험/실패0/skip0. 결과: 두 화면 모두 owner=Empty, footer=Unknown, stash=Unknown, draft=Unsettled; busy만 busy=true, 둘 다 marker=true. 이 시험의 최초 가정(marker가 busy 때 false일 것)은 실제 실행으로 정정했다.

기존 46열 fixture는 **있다**: `tests/fixtures/claude_tui/truncated-footer-empty.ansi`, `truncated-footer-draft.ansi`, `subagent-list-cut-footer-empty.ansi`. `src/services/claude_tui/busy_inject/screen_tests.rs:78`은 46×37 box를 검사하고 `busy_inject_tests.rs:596`은 busy paste/Enter를 검사한다. `/exit` 시험 `src/services/claude/herdr_turn/composer_tests.rs:285`는 넓은 정상 footer의 stashed/clear만 대조한다. 구현 때 제공 샘플을 익명화한 46×39 fixture와 위 합성 idle fixture를 추가하고, 별도로 확보한 실제 idle capture는 후속 보강으로 구분한다.

## 2. 동작 설계

### 방법 비교와 선택

| 방법 | 동일 provider ID / transcript | 초안·입력 안전 | 출력 경계 / 실패 |
|---|---|---|---|
| Escape/interrupt 후 `/exit` | 정상 exit가 flush할 여지가 있고 기존 resume 체인 재사용 | Escape가 dialog를 닫거나 composer를 바꿀 수 있음. 이후 `/exit`에도 바로 이번 readiness/stash 불확실성이 남음. 무조건 Enter는 초안 실행 위험 | finish 직전/직후 화면 경합과 부분 `/exit` 뒤 Unknown이 남음 |
| **검증된 이전 provider 프로세스 종료** | 이미 저장된 session ID/transcript로 기존 resume; 미저장 현재 turn 부분은 보존 보장 불가 | composer에 키를 넣지 않아 사람 초안·slash를 실행하지 않음. visible/hidden draft는 사전 안내 후 폐기 가능 | 종료 부재 증명이 필요. SIGKILL 시 미완성 JSONL/미게시 생성분 손실 가능; 아래 O 경계로 확대 방지 |

**두 번째를 선택한다.** 강제 동작의 허용 범위는 로컬 Claude의 mapped forward(tmux→Herdr)이며, resumable ID·같은 계정·O cursor 증명이 모두 먼저 통과한 경우다. `end_by_exit`의 공통 기본 정책이나 Herdr→tmux Return은 바꾸지 않는다. Codex, #8057, 다른 호스트 전환에는 권한을 추가하지 않는다.

main의 Running 판정(`session_handoff.rs:101`)은 live pane일 뿐 provider 신원 증명이 아니다. 새 `session_handoff/forced_tmux.rs`는 다음을 하나의 재검증 가능한 증거로 묶는다: logical key, 채널/봇 소유자, socket/server witness, exact pane ID, pane root/provider PID와 start identity, spawn execution nonce/context, native session ID, 기존 auth/source binding. `platform/tmux.rs:542`의 PID만으로 shell·다른 process를 종료하지 않는다. provider는 기존 launch script가 `exec`하는 Claude와 일치해야 한다(`claude_tui/session.rs:491`). PID=0·부재·argv/nonce 불일치·PID 재사용·증거 읽기 실패는 종료 권한을 만들지 않는다.

기존 `process.rs:518`의 `kill_pid_tree_if_identity_matches`를 재사용하되 Linux/macOS의 `has_baseline`, provider=group leader, 그 group이 해당 실행 소유라는 증명을 요구한다. 동작은 기존 helper의 SIGTERM→200ms grace→identity 확인 SIGKILL이다(`process.rs:544`). 임의 descendants census, 전체 tmux session 삭제, 이름 패턴 kill, 고아 background jobs 정리는 추가하지 않는다. 이 조건을 만족하지 않는 wrapper/shell은 kill 대상이 아니며 아래 old fallback을 사용한다. 반환 bool은 신호 dispatch 여부일 뿐 **종료 확인이 아니다**. 원래 PID/start의 부재와 같은 pane의 provider 부재를 다시 관측해야 Ended다. 새 pane/root로 바뀌면 옛 권한은 폐기한다.

사람 초안 정책: 강제 전환은 visible·stashed·읽지 못한 초안을 **보존 보장 없이 종료**한다. 항상 전환별 안내를 먼저 게시하고 그 게시 영수증을 durable handoff에 기록한다. 안내 예: “세션을 전환하면서 진행 중 작업과 입력창·보관 초안을 종료합니다. 새 메시지는 이어서 처리합니다.” 초안 내용은 게시하지 않는다. 게시/영수증 저장 실패면 키·signal 0, 원본은 owner의 queued custody에 두고 서비스 복구 이벤트로 재개한다. 이번 실행에서 실제 Discord 게시를 하지는 않았다. 원본의 실행과 초안의 자동 실행을 혼동하지 않는다.

### owner와 저장 경계

새 상태는 별도 파일이 아니라 기존 V2 snapshot의 optional, `serde(default)`인 Claude forward-handoff record로 둔다. 원본은 기존 `items`/head 한 곳에만 있고 journal에는 identity/receipt 참조만 둔다. record는 channel/owner/session ID/old execution witness/정책 안내 receipt/선택 destination/launch nonce/단계/취소 generation을 포함한다. proposed 단계는 Preparing → Stopping → OldFallback 또는 OldEnded → LaunchPending → Bound → Submitted/ConsumedOnOld → Completed다. unknown outcome은 EvidenceWait이며 보관함으로 이동하지 않는다.

1. owner의 front grant와 per-channel transition lease 아래 준비하고, irreversible effect 직전 owner claim/cancel generation/기존 host row/physical identity를 다시 읽는다. `clear`는 3초 뒤 transition을 추월할 수 있으므로 lock 보유만 guard라고 선언하지 않는다(`commands/control.rs:519`).
2. 이미 전송 중인 **다른 원본**의 first-key/head/hold가 있으면 먼저 기존 owner settlement로 귀속을 확정한다. 현재 수행 중인 turn의 중단은 수용되지만, provider 큐에만 들어간 별도 AgentDesk 원본을 현재 turn이라고 간주하여 버리지 않는다. 그런 queued/MayHaveSent 프레임이 있으면 소비/NoKeyYet 증명을 기다리고 다음 메시지는 동일 owner 뒤에 둔다. 이 guard의 운영 호출과 시험이 없는 구현은 불수용이다.
3. 이번 새 원본의 내용은 old 종료·새 attach까지 0바이트다. old가 이미 Ended면 signal 0으로 다음 단계. Running이면 준비 화면과 관계없이 위 검증/안내/종료를 실행한다.
4. 종료가 확인되면 기존 Herdr Pending CAS/create/evidence/own SessionStart/attach를 그대로 잇고, 기존 owner의 first_input effect에 같은 원본 claim을 **이동**한다. attachment 전에 새 메시지를 prompt에 넣지 않는다. 새 incarnation nonce와 기존 native session ID는 다른 개념이다.
5. 첫 content 전 확실한 실패만 같은 원본을 재시도한다. 부분 paste/Enter·hook timeout·lost create reply는 NotSent라고 바꾸지 않고 exact old/new witness와 receipt를 복구한다. Pending이 있으면 현재 unplaced/pending 복구를 사용하며 두 번째 create를 하지 않는다. Unknown을 success/absence/0건으로 바꾸지 않는다.

### 종료 실패와 정확히 한 번

정지 실패를 평문 `String`의 generic start failure로 넘기지 않는다. typed outcome은 Ended, StillRunning(재검증된 동일 실행), Unobserved(확인 불가)다. pre-input handoff 실패는 start-failure 카운터/`retain_not_sent` 대상이 아니며 durable handoff 단계로 반환한다.

StillRunning이고 새 Pending/Bound/create 불확실성이 전혀 없을 때만, 동일 front claim의 destination을 **Old**로 durable 선택한다. main `Registration::observed`는 mapping으로 Herdr를 다시 고르므로 그대로 부르면 fallback이 되지 않는다(`owner_input.rs:549`). 별도 `claude_handoff` adapter가 검증된 legacy physical target을 한 입력에만 등록하고 기존 BusyClaude owner effect·writer lease·frame nonce·receipt를 사용한다. 설정/`.host_kind`를 tmux로 바꾸거나 generic launcher를 우회해 새 tmux를 만들지 않는다.

fallback composer는 완전한 empty box 또는 실제로 C-s로 park 가능한 사람 초안만 허용한다. existing owned paste는 unread draft에 append할 수 있고 안내도 effect 후다(`busy_inject.rs:499`, `effect.rs:223`); **그 기본 정책은 넓히지 않는다**. 새 handoff adapter는 unread/stashed/attachment/modal/copy-mode면 첫 key 전에 Held로 돌려 화면/turn/probe 이벤트를 감시한다. 별도 strict policy helper로 제한하며 전역 busy behavior는 유지한다. 입력을 몰래 초안 뒤에 붙이거나 새 prompt와 함께 실행하지 않는다.

old delivery의 첫 key 뒤에는 new destination으로 갈 수 없다. owner의 Submitted는 local Enter일 뿐이므로 **정확한 frame의 native user/queue 소비와 처리 terminal**을 확인할 때까지 강제 종료를 재시도하지 않는다. 완료 뒤에는 원본을 재주입하지 않고 migration만 자동 재개한다. fallback 선호 때문에 다음 원본도 old로 들어가도록 무제한 열지 않으며 handoff record가 같은 채널의 새 입력을 owner 큐에 붙잡는다. 모델은 보수적으로 N=2 모두 old로 선택될 수 있는 interleaving도 검사한다.

Unobserved면 old fallback도 new launch도 0회이고 queued custody+증거 복구를 유지한다. host/source/turn/writer-ready 및 boot wake가 다음 시도를 만든다. 모든 provider가 영구적으로 조작 불가능한 경우까지 “즉시 한 번 실행”은 성립하지 않는다. 이를 숨기는 양성 판정은 하지 않는다. 재전송 요구 없이 가능한 한쪽 실행이 회복되면 정확히 그 원본을 진행시키는 계약이다.

### 동일 ID와 출력 경계

이미 main에서 만족한 부분은 재구현하지 않는다: `herdr_turn.rs:590`의 auth/transcript/O 조건, `:639`의 ID 선택, `:666`의 hook 사전 subscribe, `session_handoff.rs:192`의 source=resume+expected ID+execution nonce 일치, `tui_session_launch.rs:138`의 launch-context ID 검증, `claude_tui/session.rs:372`의 `--resume` argv. 이 수리는 fresh fallback을 같은 ID resume라고 부르지 않는다. 기존 force_fresh/다른 계정/없는 transcript 분기는 별도 기존 행동이다.

O는 같은 SourceId(path/session/inode 및 기존 canonical dev 처리)의 cursor를 계속 사용한다. `session_handoff.rs:153`→`tui_o/writer/renumbered.rs:50`이 이미 이를 검사한다. 종료 전/후 source와 cursor를 다시 확인하고 bind를 0으로 reset하거나 임의 현재 EOF로 건너뛰지 않는다. durable spool의 미게시 출력은 기존 단일 O writer가 이어 게시한다(`tui_o/writer/actor.rs:669`). 강제로 끊긴 turn의 메모리 속 미저장 토큰·background 작업은 사용자 수용 손실이고, 이미 transcript에 있는 완전한 record/이미 spooled 출력은 이 수리를 이유로 버리거나 두 번 게시하지 않는다. partial JSONL tail은 `src/services/tui_o/shadow/capture.rs:331`의 newline 경계와 `:340`의 partial 재검증을 유지하며 파일 수정·truncate·새 세션으로 위장 복구하지 않는다. source가 바뀌거나 partial tail을 provider가 이어 쓰지 못하면 EvidenceWait/진단으로 남기며 정상 resume라고 보고하지 않는다. 이 마지막 Claude 실제 flush/resume 동작은 격리 CLI 통합 확인과 운영 수용 확인이 필요하다.

## 3. 진입점 전수표와 과거 retained 소비

`git grep` 전수 결과는 logs의 `handoff-entries.txt`, `retained-entries.txt`, `adjacent-entries.txt`, `boot-incarnation-entries.txt`에 남겼다. 테스트 호출부를 별도로 보존하되 아래 표는 production 진입/소비 지점이다. 표의 모델 행동은 새 모델의 실제 이름이며, 인접 제어의 구현 세부는 아래 명시한 추상화 경계다.

| 경로 | main file:line | 현재 행동 → 제안 모델 행동 |
|---|---|---|
| live/catch-up 원본·재시도 front | `src/services/discord/queue_io/input_owner/drive.rs:87`, `:168` | 채널당 drive 한 개, grant → Begin/Choose; 새 입력도 원본 identity 유지 |
| idle 첫 메시지 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:834`, `:875` | claim 후 provider 시작 → Begin; 새 preflight/typed 결과의 호출 지점 |
| 이미 active인 채널의 새 메시지 | 같은 파일 `:217`, `:238`, `:320`, `:362` | hand_busy → Choose; migration 중이면 이 지점도 marker를 읽어 큐에서 대기 |
| 외부 native turn을 owner start가 발견 | 같은 파일 `:535`, `:549` | into_busy_pane이 host 재선택 → fallback은 explicit old capability로만 Choose(old) |
| synthetic/HTTP/routine 첫 입력 | 같은 파일 `:623`, `:650`, `:858` | synthetic은 busy에서 자기 turn 대기. source policy 유지, 사람 메시지와 합치지 않음 |
| host 선택 | `src/services/turn_host.rs:151`, `:291` | mapped/local/O-ready 검증. Begin의 적용 범위; Codex 선택 분기는 수정하지 않음 |
| provider dispatch | `src/services/discord/router/message_handler/provider_dispatch.rs:631`, `:646` | Claude HerdrTurn→execute → Begin |
| Legacy/Retired row의 forward | `src/services/claude/herdr_turn.rs:256`, `:273`, `:623` | launched_source → 기존 end_by_exit 대신 강제 forward adapter의 SignalStop/VerifyEnd |
| Pending 재시도 | 같은 파일 `:393`, `:401`, `:404`, `:417` | unplaced 복구 후 retired면 launch, 그 외 같은 nonce → Launch/Create/Attach 복구, 중복 생성 금지 |
| Bound 재사용·clear 후 입력 | 같은 파일 `:281`, `:291`, `:363` | confirm_bound / awaited_clear → Attach/Choose(new); forced old stop 대상 아님 |
| 이전 tmux 종료 | `src/services/claude/session_handoff.rs:60`, `:99`, `:115` | Running→send_exit→Unknown → 현재 BeforeRefuse; 제안 별도 SignalStop/ProviderEnds/VerifyEnd |
| ready 판정 | `src/services/claude/herdr_turn/composer.rs:50`, `:119` | BeforeRefuse의 busy/unread 분기; 일반 입력 및 Return guard는 유지 |
| 역방향 Return | `src/services/claude/herdr_return.rs:61`, `:80`, `:83`, `:173` | 같은 공통 end_by_exit의 다른 호출자. **범위 밖**; 이번 SignalStop 권한을 주지 않음 |
| tmux resume 시작 확인 | `src/services/claude.rs:1833` | await_resumed_start; 역방향 기존 identity guard 유지 |
| 새 incarnation/nonce | `src/services/herdr_launch.rs:457`, `:468`, `:723`, `src/services/claude/tui_session_launch.rs:126` | Pending 이후 context 준비 → Launch/Create, native ID와 nonce 혼동 금지 |
| Herdr 첫 prompt, legacy/owner 합류 | `src/services/claude/herdr_turn/first_input.rs:11`, `:22`, `:66` | gate/hold/owner effect → Arm/Submit/Confirm; legacy non-owner는 별도 경로이며 새 old fallback을 복제 호출하지 않음 |
| content 전후 hold/partial | 같은 파일 `:86`, `:94`, `:103` | NoKeyYet와 불확실 content 구분 → RecoverNoKey/Confirm; unknown→새 submit 금지 |
| boot의 V2 census | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:87`, `src/services/turn_orchestrator/queued_head/compat.rs:20` | holds_input만 복구; told-only/완료 후 migration marker 누락 가능 → 아래 별도 census + Boot |
| gateway/REST 공통 boot | `src/services/discord/runtime_bootstrap/queued_recovery.rs:26`, `:48` | v2_channels→owner_recover → Boot/RecoverRetained/Begin |
| owner 복구·wake | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:664`, `:680`, `src/services/discord/queue_io/input_owner/recovery.rs:8`, `:19` | requeue→history→notice→wake. 새 migration은 notice drain보다 먼저 RecoverRetained |
| Herdr boot reconnect | `src/services/discord/recovery_engine/herdr_reader.rs:217`, `:455`, `:463` | Bound만 reconnect, Pending은 next turn에 남김 → Boot/Attach; 새 input-free handoff는 별도 wake 필요 |
| 단순 host reconcile | `src/services/discord/recovery_engine/host_reconcile.rs:94`, `:113` | 관찰/일치 판정만. Boot가 자동 kill/create한다는 가정 금지 |
| start 실패→보관 | `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:226`, `:248`, `:260`, `src/services/turn_orchestrator/queued_head/v2/start_failure.rs:54`, `:95`, `:104` | generic opaque threshold 후 retained 생성 → BeforeRefuse; handoff typed result를 이 분류 전에 처리 |
| effect의 NotSent/Held | `src/services/discord/queue_io/input_owner/effect.rs:107`, `:122`, `src/services/turn_orchestrator/queued_head/v2/retained.rs:142`, `:166` | Held는 front, NotSent는 retained → 새 handoff refusal는 Held/custody, 새 재전송 안내 0 |
| 미확인 입력의 deadline | `src/services/turn_orchestrator/queued_head/v2/retained.rs:58`, `:85`, `src/services/discord/queue_io/input_owner/drive.rs:128` | Unknown→retained tick. 새 handoff content 전 custody는 head로 arm하지 않아 이 timeout을 피함; content 후 LostReceipt는 별도 잔여 경계 |
| 기존 retained 자동 재삽입 | `src/services/turn_orchestrator/queued_head/v2/retained.rs:283`, `:300`, `:386` | untold+attempt 있음만 requeue → RecoverRetained를 별도 좁은 classifier로 추가 |
| notice가 ownership을 끝냄 | 같은 파일 `:225`, `:249`, `src/services/discord/queue_io/input_owner/notice.rs:7` | told known/opaque unsent는 unowed → 과거 handoff의 명시적 정책 예외만 회수 |
| retained/ticket 재결합 | `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:546` | attempt bind 제거/queued receipt 재결합. attempt=None+Opaque bind는 기존 함수만 호출해서 해결되지 않음 |
| Cancel 선택·전체 비우기 | `src/services/discord/commands/control.rs:715`, `src/services/turn_orchestrator/queued_head/v2/control.rs:97`, `:206`, `:401` | original/claim 취소 → CancelBeforeContent; marker도 같은 commit에서 무효화 |
| /clear | `src/services/discord/commands/control.rs:510`, `:519`, `src/services/turn_orchestrator/queued_head/v2/control.rs:277`, `:411` | 3초 후 추월·queue/retained clear → CancelBeforeContent; cleared_notice는 영구 재회수 제외 |
| /stop 및 text stop | `src/services/discord/commands/stop.rs:84`, `:157`, `src/services/discord/commands/text_commands/native_stop.rs:4` | 기존 native stop/queue wake. stop은 자동으로 모든 대기 입력을 취소하는 행동이 아님; NoKeyYet 취소와 이후 effect의 귀속은 별개 |
| Terminate | `src/services/discord/herdr_terminate.rs:401`, `:482`, `:615`, `src/services/termination_audit/host_terminate.rs:116` | 기존 transition/exact fence로 close/retire. ProviderEnds/VerifyEnd 관측과 owner capability 무효화로 추상화; 새 강제 helper를 generic Terminate로 노출하지 않음 |
| 전환 해제 후 wake | `src/services/discord/session_transition.rs:120`, `:132`, `:145` | lock 해제 뒤 owner wake → Boot/Begin. 이 완료 이벤트를 실제 호출하는 시험 필요 |
| busy 입력의 물리 commit | `src/services/claude_tui/busy_inject/submit.rs:35`, `src/services/claude_tui/busy_inject/stash.rs:22`, `:135` | 기존 writer/first-key/pinned key → Arm/Submit; fallback strict policy에서만 사용 |
| busy 입력 receipt | `src/services/claude_tui/busy_inject/receipt.rs:14`, `src/services/discord/queue_io/input_owner/effect/herdr.rs:122` | provider receipt와 local Enter를 구분 → Confirm/OldCompletes. Herdr busy 기본 경로는 유지 |

`stop`·Terminate의 세부 출력 settlement/권한 FSM 전체를 새 모델이 증명하는 것은 아니다. 모델 CancelBeforeContent는 명시적 원본 취소를, ProviderEnds/VerifyEnd는 종료 관측을 표현한다. `clear`의 timeout 추월·late callback generation 검사는 Rust 경합 시험의 의무로 남는다.

### 입력 출처/소유자 표

| 입력 출처 | 판별 코드 | 소유자와 이번 정책 |
|---|---|---|
| Discord 사람 원본 | `queue_io/input_owner/origin.rs:25`, `:30`; context 없음→Discord | 로컬 TUI는 같은 채널 V2 owner. mapped Claude forward가 busy여도 전환. message ID·sources·본문·upload·reply boundary 유지 |
| 허용된 Discord 봇/알림 | `router/intake_gate.rs:329`; `intake_gate/busy_queue.rs:5` | whitelist/announce gate를 통과한 원본만 접수. bot flag와 allowed-automation은 별도 provenance로 보존. context가 없는 Discord를 무조건 실제 사람이라고 재분류하지 않음; 새 설정 읽기/허용 범위 확대 없음 |
| AgentDesk routine/scheduled/HTTP start | `origin.rs:38`, `owner_input.rs:205`, `:650`; `input_owner/target.rs:78` | 같은 owner가 보관하되 synthetic은 busy에서 자기 turn 대기. 이번 사람이 보낸 메시지의 강제 전환을 구실로 synthetic을 중복 주입하지 않음 |
| iMessage 사람 원본 | `origin.rs:29`, `:45` | GUID/채널 identity가 재시도에도 동일. 기존 사람 policy 유지; 운영 수용은 Discord 대상 |
| 사람이 TUI에 쓰는 초안/숨은 stash/첨부 | `busy_inject/ownership.rs:26`, `screen.rs:274`, `:298` | native composer 소유, V2 원본 아님. 강제 전환 사전 안내 뒤 폐기 가능, 자동 submit 금지 |
| AgentDesk가 이미 composer에 넣은 frame | `ownership.rs:6`, `:102`, `claude_tui/owner_input.rs:390` | 문자열 `[User:`만으로 사람/자동 구분하지 않음. owner attempt/nonce/history와 대조, MayHaveSent면 다른 경로로 replay 금지 |
| remote TUI / process / 다른 provider | `input_owner/origin.rs:90`, `target.rs:72` | placement owner 또는 기존 process 경로. 이 강제 Claude local capability를 주지 않음 |

### 이미 보관된 다섯 원본

main은 자동으로 다섯 건을 처리한다고 말할 수 없다. start_failed가 만든 항목은 `attempt=None`이다(`start_failure.rs:109`). 일반 requeue는 `attempt.is_some()`와 `!notice.enqueued`를 요구한다(`retained.rs:288`). notice 이후 ownership도 끝난다(`:249`). 더구나 boot census는 holds_input만 고르므로 told-only 스냅샷은 다른 owed/opaque bind가 없으면 아예 owner_recover를 호출하지 않는다(`compat.rs:30`).

새 `v2/claude_handoff.rs`의 한정 migration은 다음 조건을 모두 요구한다: Claude provider/해당 봇·채널 namespace, 지금 mapped local forward 또는 그 전환 완료 record, KnownNotSent, Reserved, payload 없음, **attempt=None인 start failure**, reason이 정확히 `start_rejected: earlier claude kept running: Unknown`(main의 이 오류 생성 체인으로 고정), start_failure.kind=Opaque, `retained_retry_required`, Discord 원본 provenance. told/untold 모두 대상이며 enqueued=true를 재실행 금지 근거로 쓰던 기존 정책의 명시적 예외다. 해석 가능한 옛 형식의 같은 no-input proof는 fixture로 별도 승인하고, missing/unknown disposition·opaque unsent·Unconfirmed·실제 attempt 있음·부분 키·clear/cancel·타 provider·다른 오류 문자열은 자동 확대하지 않는다. 다섯 시각 모두 같은 오류라는 브리프와 다르게 persisted 증거가 나오면 후보 수/제외 사유를 보고하며 5건 PASS로 세지 않는다.

이 정책을 boot census에도 연결한다. `v2_channels`가 Claude에서만 `holds_input || eligible_handoff_retained || unfinished_handoff`를 읽고, map/local 확인은 owner recovery 안에서 다시 한다. 평소 live intake/전환 후에도 같은 actor migration을 한 번 호출한다. 완료 record만 남고 큐가 빈 fallback migration도 boot에서 누락하지 않는다. 전역 holds_input 의미를 다른 provider에 바꾸지 않는다.

한 actor snapshot commit에서 retained 제거·원본과 followers 재삽입·admission receipt bind 재결합·handoff migration 표시를 함께 한다. 현재 attempted head 뒤, 새 미시도 메시지 앞에 원본 순서를 유지한다. 기존 `source_admission.rs:546`은 Opaque bind를 그대로 bound로 보므로 **attempt=None/BindAt::Opaque의 tk receipt도 정확한 original digest/context가 같을 때만 queued로 재결합**해야 한다. digest/body 충돌은 합치지 않는다. delivery terminal identity·이미 실행된 같은 source member가 있으면 재전송하지 않는다. 명시적 취소/clear terminal은 복구하지 않는다. HTTP/iMessage origin의 Failed outbox(`v2/origin.rs:26`)는 이 Discord 전용 과거 정책에 섞지 않는다. 기존 안내 이력은 감사 정보로 유지하고 새 retry-required 안내는 만들지 않는다. 저장 실패는 변경 0, 재시작 후 같은 candidate를 다시 찾는다.

운영 확인 대상: adk-cc `1479671298497183835`, 13:58:27 `1569939342124646514`, 14:12:18 `1569942806502318151`, 14:17:54·14:21:42·14:23:47의 ID는 제공되지 않았다. 운영 DB를 읽어 채우지 않았다. 구현 시 queue owner의 동일 classifier가 발견한 원본 ID와 결과(5개 기대, 이미 terminal/명시적 취소인 것은 별도 집계)를 coordinator에게 보고한다. 사용자가 재전송한 **새 Discord ID**는 원래 메시지와 본문만으로 dedupe하지 않는다. 이를 동일 입력으로 추정하면 정상 반복 지시가 유실된다.

## 4. 크기 추정과 구현 경계

계획 상한 **production 17파일 / 순 코드 +1150**(테스트/fixture/docs 제외), 예비분 포함. `pr_cap_prod.py` 현 상수 FILES=30/NET=1500, 이번 PLAN 정지선 +1200 이내다. net+1150은 실제 구현 diff를 측정한 값이 아니라 아래 연결 책임을 포함한 상한 추정이다. 이 선을 넘으면 구현 레그에서 범위를 나눠 다시 설계하며 상한을 올리지 않는다.

| 파일/묶음 | 파일 수 | 예상 순 코드 | 책임 |
|---|---:|---:|---|
| `claude/session_handoff/forced_tmux.rs` 신규 | 1 | +210 | exact old execution 증거·안내 확인 후 종료·부재 재관측 |
| `claude/session_handoff.rs` | 1 | +15 | 새 forward adapter 선언/형식 연결; end_by_exit 기본 유지 |
| `claude/herdr_turn/handoff.rs` 신규 + `herdr_turn.rs` | 2 | +40 | 기존 launched_source 분리 이동 포함; 새 전환 결과 연결 |
| `discord/queue_io/input_owner/claude_handoff.rs` 신규 + 모듈 연결 | 2 | +240 | 전환 저널 drive, 사전 안내 receipt, fallback 선택·완료 wake |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs` | 1 | +0 | 기존 관련 helper 이동으로 상쇄하며 호출만 연결 |
| `queued_head/v2/claude_handoff.rs` 신규 + `v2.rs` 연결 | 2 | +193 | typed snapshot mutation, 과거 retained 후보·atomic 소비 |
| `queued_head/v2_types.rs` | 1 | +45 | optional handoff record와 serde 호환 |
| `input_owner/recovery.rs` | 1 | +20 | notice drain 앞 migration/전환 회복 |
| `queued_head/v2/control.rs` | 1 | +25 | 원본 취소와 handoff generation/marker 동시 무효화 |
| `pending_queue_persistence/queued_recovery.rs` + `queued_head/compat.rs` | 2 | +30 | Claude 전용 told-only/unfinished handoff boot census |
| `claude_tui/busy_inject/submit.rs` + `busy_inject.rs` | 2 | +32 | old fallback strict draft policy 한정 adapter |
| `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs` | 1 | +15 | handoff pre-input 결과를 generic retain에서 분리 |
| 여유 | 0 | +285 | 오류/권한 재검증·직렬화 연결 포함, 총 +1150 |

현재 `herdr_turn.rs`는 993행, `owner_input.rs` 1586행, `host_input.rs` 1408행이다. 1000행 넘는 파일에 새 독립 책임을 넣지 않는다. 993행 파일도 새 handoff 책임을 child module로 옮겨 여유를 만든다. `owner_input.rs`는 신규 알고리즘을 넣지 않고 기존 연관 helper를 추출해 net growth를 0 이하로 유지한다. `host_input.rs`, 1000행 넘는 `tmux_common.rs`는 수정하지 않는다. 기존 process helper의 광범위 group semantics나 일반 unread-draft busy paste 결함은 위 main 좌표대로 유지하며 새 경로에는 더 좁은 전제만 적용한다.

## 5. 대상 시험·뮤턴트 계획

구현 레그는 아래 실제 production entry를 통과하는 RED→GREEN을 요구한다. 이번 레그의 격리 predicate probe/TLC를 whole-app GREEN으로 재사용하지 않는다. cargo는 shim 경유, `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none`, CARGO_TARGET_DIR 변경 없음, 기본 병렬. PG가 필요한 것은 `pg_throwaway.sh cargo test ...`만 사용한다.

| 시험 | 실제 관찰/수용 기준 | 원인 제거 mutant |
|---|---|---|
| 46×39 busy/합성 idle | actual observation 경로에서 busy final veto와 idle exit stash veto를 따로 RED로 재현. 새 강제 handoff는 둘 다 신원 확인→종료→기존 first_input으로 1회 | 옛 end_by_exit 호출 복원 → entry RED |
| 46열 업데이트/IDE footer 대조 | footer만 정상화/상단 update만 제거/둘 다 대조. 일반 input ready guard를 느슨하게 하지 않음. fixture가 실제 detector를 통과했는지 중간값도 검사 | exit fixture에서 update/stash 검사를 무조건 Settled로 바꾸면 보호 시험 RED |
| busy→턴 종료 직전/직후 | stop signal 직전 natural exit, signal 후 reply loss, dead pane, 새 shell/root 교체. 이미 종료면 signal0, 새 provider 정확히1 | signal 성공을 Ended로 간주/부재 재관측 삭제 → RED |
| 사람 초안 visible·stashed·unread·attachment | 사전 안내 게시/영수증 저장 뒤 signal, composer key0, 초안 user record0. 실패하면 signal0. fallback은 measured park만 허용 | 안내 guard 제거 / strict old fallback 제거 → RED |
| 중단 실패 | verified same old Live→동일 original claim old1/new0. unknown→old0/new0 custody 유지. old의 native queue 소비/terminal 전에는 signal 추가0 | StillRunning을 Ended로 변경 / local Enter 뒤 바로 kill → RED |
| 동일 native ID | fake host의 own nonce SessionStart(source=resume), 실제 prepare argv `--resume <old-id>`, transcript path/identity·O cursor 유지; 다른 nonce/source/fresh ID는 prompt0 | expected ID/nonce guard 삭제 → RED |
| 두 메시지/같은 본문 | 동일 채널 동시 도착·live/catch-up 중복은 source ID별1; 서로 다른 ID의 같은 본문은 각각1. follower 분리/취소도 검사 | claim 복사/본문 dedupe/second fallback 허용 → RED |
| restart fault matrix | 안내 전/후, signal 전/후, ended 전/후, Pending CAS/create reply/evidence/attach, arm 전/후, Submit 후 Confirm 전, old terminal 전/후. 같은 journal 원본을 이어 받고 두 번째 provider/prompt0 | boot wake/census/record 저장 삭제 → RED |
| 다른 owner의 MayHaveSent/queued frame | pending child가 소비되기 전 강제 종료0, 그 child를 새 입력으로 몰래 재주입0. 회복 증명 뒤 새 원본 전환 | 보호 guard 제거 → RED |
| 과거 retained 5건 | told/untold, attempt=None, 원본/followers/uploads/reply/tk Opaque bind 보존, boot-only 아무 새 메시지 없이5개 실행. current attempted head는 그대로 | migration 또는 told-only census 제거 → RED |
| 과거 retained 음성 대조 | Codex/타 채널·Unknown·OpaqueUnsent·Unconfirmed·attempt 있음·Cancelled·cleared_notice·이미 delivered·다른 reason·digest conflict는 replay0. legacy optional field 없는 JSON도 읽음 | broad starts_with/unknown→zero → RED |
| 취소/clear/stop/Terminate | 매 irreversible 경계에서 pause 후 explicit control, claim generation 재검증, late callback이 새 실행을 kill/send하지 못함. persistence error는 count_unknown/Err | transition lock만 믿기/cancel generation 생략 → RED |
| 출력 O 경계 | pre-stop partial/완전 JSONL, spooled 미게시, 이미 게시된 body, restart+동일 source append를 실제 O fake sink로 검사. complete record와 게시 identity는 각각1, cursor reset/EOF jump0 | cursor0/새 EOF 강제 설정 → RED |

Rust 시험은 새 `*_tests.rs`로 나눠 기본 병렬을 유지한다. run mode/path gate가 새 lib test ID를 실제 선택하도록 공식 inventory 생성/coverage gate를 사용한다. 각 mutant는 test가 실제 선택된 이름/실행 수>0/실패 수>0인지 확인하고 원복 bytes SHA 동일을 남긴다. `#[allow]`, baseline 인상은 없다. 구현 REVIEW_READY 전에 대상 시험/뮤턴트/clippy warning no-growth, 공식 source_scan_readers failed0와 `prepush_gates.sh --static-only`를 완료해야 한다. PLAN 레그에서 이를 실행했다고 보고하지 않는다.

실제 Claude 2.1.322의 signal 종료 후 resume/partial-tail 동작은 fixture만으로 증명할 수 없으므로 격리한 개발 실행에서 확인한다. 운영 DoD의 “adk-cc busy 때 Discord 1건→전환·답 게시·메시지 ID”는 구현/리뷰 후 coordinator가 수행한다. 이 레인은 Discord 송신 및 운영 pane 조작 금지이므로 이를 대체 증거로 꾸미지 않는다.

## 6. 리뷰 집중 항목

| 항목 | 방지 방법 또는 남은 반례 |
|---|---|
| 경계 시각 | PID/start + physical pane/nonce 재관측; local signal success≠종료, local Enter≠처리 완료. 실제 Claude partial flush는 개발/운영 확인이 남음 |
| 같은 채널 동시성 | 기존 single owner grant·writer lease·이동하는 claim + durable destination. active busy path도 marker를 읽음. 두 메시지 race를 entry 시험으로 증명해야 함 |
| 재시작 중 넘김 | snapshot 단계와 Pending nonce 지속, told-only/큐 빈 unfinished marker census, boot/transition/terminal wake. main `queued_head/v2_types.rs:6`에는 unknown-field 보존 장치가 없어 old binary의 재직렬화는 새 journal을 잃을 수 있음. 과거 JSON→새 binary 복원은 지원하되, unfinished handoff를 둔 old-writer rollback의 진행성은 이 설계의 GREEN 범위 밖이며 호환 시험/후속 정책이 필요 |
| 끊긴 출력 유실·중복 | persisted complete record/spool/cursor는 보존. 현재 turn 메모리 속 토큰 손실은 수용된 경계이며 O EOF/cursor 점프로 확대하지 않음 |
| 옛 형식 복원 | attempt=None/told/BindAt::Opaque를 실제 옛 JSON fixture로 검사; missing proof를 KnownNotSent로 승격하지 않음 |
| 문서로만 선언한 guard | strict old fallback, pending-other-input guard, clear 추월 재검증, publish-before-signal, input-free boot census 각각 실제 호출 지점/변이 RED 필요 |
| 잘못된 0/PASS | 대상 cfg15개·시험2개 실측을 구별, main RED/제안 GREEN/receipt 소실 RED 유지. PG 미실행·whole-app 미검증·frontend 누락을 분리 표기. 읽기 실패를 0건으로 기록하지 않음 |

미해결을 숨기지 않는다: 영구적인 provider/receipt 손실에서 exactly-once와 무조건 liveness를 동시에 증명할 수 없다. 이 수리는 content 전 handoff 거절→사용자 재전송이라는 재현된 경로를 없애고, 불확실 content의 자동 복제는 추가하지 않는다. LostReceipt는 기존 `effect.rs:139`→`retained.rs:58`의 main 한계이며 이것을 해결한 것처럼 scope를 넓히지 않는다. 운영 DoD와 실제 signal/resume 출력 경계는 다음 레그의 수용 증거다.
