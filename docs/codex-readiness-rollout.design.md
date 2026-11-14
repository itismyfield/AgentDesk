# Codex 준비·작업 중·/stop 판정을 rollout·훅·프로세스 신호로 옮기기 — #8082 설계 d3

<!-- doc-path-refs: proposed docs/herdr-parity-restart.design.md -->

기준 코드: `082c77aa4e06b81c76b614f55b8fed73ce01923f` (#8081 B1 리뷰 CLEAN head, main 미착지).
d3까지는 설계·모델만 다뤘다. 구현은 5절의 조각 순서로 들어가고, 조각 A가 확인한 사실은 **A1** 표시로 적었다.
`src/services/` 접두어는 생략한다. 표기: [C] 이 base의 코드에서 확인, [F] 저장소 fixture에서 확인, [U] 확인하지 못한 운영 전제.
A1(구현 조각 A)은 Codex 0.169.0 fixture 결과를 반영했다. 바뀐 곳: 0절(모델 74 cfg, `EnterGap`), 2절의 Busy (b)·floor·턴 없는 새 실행·`.zst`, 5절의 fixture 결과와 조각 A·C 메모, 8절 R3·R4.
d3는 d2 델타 리뷰(P1 1건, P2 3건)를 반영했다. 바뀐 곳: 0절(모델 68 cfg), 새 절 1.7(기존 pane을 끝내는 모든 길), 2절의 Busy (b) 해제 조건과 시간 초과 문단, 3절 marker 소비자 표의 재전송 행, 5절 조각 C·이관 표, 시험 계획, 8절 R3~R5.
d2는 d1 리뷰(P1 2건, P2 8건)를 반영했다. 바뀐 곳: 0절 전체, 1.1의 Enter 뒤 확인 행, 1.6(새 절), 2절의 Busy 조건·floor·시간 초과 표·단계 4, 3절의 marker 소비자 표, 4절의 [U] 행, 5절 조각 계획과 이관 표 틀, 8절 잔여.

## 결정 요약

- 준비·작업 중·종료·/stop의 권위 신호는 **현재 실행에 묶인 rollout의 최신 턴 상태**다. 이미 owner 입력 경로가 쓰는 `busy_inject::rollout::read_native_end`(`codex_tui/busy_inject/rollout.rs:98`)를 그대로 쓴다.
- 종료 기록이 늦거나 없을 때는 같은 `turn_id`의 **Codex Stop 훅**이 종료를 증명한다. 그것도 없고 턴이 열린 것으로 읽히면 기존 busy 입력 경로로 보낸다. rollout을 아예 읽을 수 없을 때만(`Unknown`) 조용한 상태를 확인한 뒤 새 실행으로 바꾼다.
- **열린 턴은 자동으로 죽이지 않는다.** tmux의 45초 시간 초과 교체는 판정이 `Busy`이면 하지 않는다. 이 확인은 준비 판독에서 나온 교체에만 건다. launch 옵션 변경·binding 불일치 같은 다른 사유의 교체는 지금처럼 한다(1.7). 대가로 "readonly 실행 + 종료 기록·Stop 훅 모두 유실" 한 경우가 사유 있는 보류로 남는다(8절 R1, 코디네이터 결정 항목).
- herdr는 #8128 착지 전까지 `Unknown`을 풀 교체 경로가 없다. 그동안은 600초 뒤 형식 있는 거절로 끝낸다(2절 단계 4).
- 입력을 쓰기 전에 **프로세스 생존**(tmux `pane_liveness`, herdr `process_info`)을 확인한다. rollout만 보면 종료된 pane의 셸에 입력이 들어갈 수 있다.
- 화면 판독은 사람 초안(5번 보류)·모달 veto·Enter 앞뒤의 자기 글 확인·pane 크기에만 남는다. 이 판독이 실패해도 거절·보류가 영구화되지 않는다.
- #8081 B1 계약은 바꾸지 않는다. 훅 ingress 호출부 한 곳(`claude_tui/hook_server.rs:491`~`:511`) 옆에 Codex 관측 호출을 더하고, herdr 자식 관측을 위해 `claude_tui/activity/processes.rs:53` `ancestry`의 가시성 한 줄을 넓힌다(시그니처·동작 불변). `DESIGN_DELTA` 없음.

## 두 안 비교

| | 안 A — source 판독 재사용 (채택) | 안 B — B1 활동 수집기를 Codex로 확장 |
|---|---|---|
| 흐름 | `codex_tui/readiness.rs`(새 child 파일, 순수 함수) 하나가 binding → `read_native_end` → Stop 훅 증거 → 프로세스 생존을 합쳐 `Idle/Busy/Unknown/Gone`을 낸다. 준비 대기·herdr 입력·/stop·행 종료·제출 게이트가 이 함수를 부른다. | `claude_tui/activity`의 `Source`·`Frame`·`poll`을 provider 공용으로 넓히고, Codex rollout parser와 Pre/PostToolUse 짝을 수집기에 넣는다. 소비자는 `activity::observe().foreground`를 읽는다. |
| 비용 | prod +1,690 안팎(조각 4개, 각 +600 이하). 새 trait·Core 없음. | prod +2,000 이상 추정. `Source::read`가 `binding_events::claude_history`·`source_verify::is_top_level_transcript`에 묶여 있어(`claude_tui/activity/source.rs:38`~`:83`) 타입부터 갈라야 한다. |
| 위험 | 호출마다 rollout 꼬리를 읽는다(최신 턴 시작까지만, 상한 64MiB `rollout.rs:9`). 파일 stamp가 같으면 다시 읽지 않도록 캐시한다. | main에 아직 없는 B1 파일을 다시 열어 B1 불변식을 재리뷰해야 한다. B1 계약 변경이라 `DESIGN_DELTA` 사유다. |
| 얻는 것 | owner 경로(`owner_input.rs:823`)와 판정이 한 곳으로 모인다. | 도구 단위 busy, PermissionRequest 짝맞추기(29번)를 한 번에 얻는다. |

안 A를 택한다. 안 B의 이점인 29번(모달)은 훅 payload가 fixture로 고정된 뒤 선택 조각 E로 따로 다룬다.

## 0. 상태 차트

모델: [`CodexReadiness.tla`](statecharts/input-single-path/CodexReadiness.tla), 결과: [보고서](statecharts/input-single-path/codex-readiness.report.md).

`docs/statecharts/README.md` 색인에서 Codex 입력 준비를 다루는 모델은 없었다. Codex 모델은 `session-lifecycle`(marker·handoff·Pending 재사용)과 `turn-presence`(retired marker)뿐이고, 입력 준비 가정은 `ClaudeReadiness`·`HerdrBusyInput`에만 있다. `ClaudeReadiness`에 Codex 모드를 더하는 것을 먼저 검토했으나, 증거 축이 겹치지 않아(Claude: SessionStart·terminal whitelist·continuation·clear, Codex: rollout 턴 상태·Stop 훅 turn_id·압축·프로세스) 공유 변수가 대기열뿐이고 기존 81 cfg의 상태 공간을 건드리게 된다. 같은 폴더에 형제 모델을 두고 규칙 이름만 맞췄다.

d2에서 모델에 더한 것(리뷰 P1-2·P2-4·P2-5):

- **호스트 축**(`Host`): tmux의 수정 전은 "사전 확인 → 45초 화면 대기 → 시간 초과 시 pane 교체·argv 재기동"(`Admit`·`TimeoutReplace`·`ArgvLaunch`), herdr의 수정 전은 화면 대기 뒤 거절이다.
- **사람의 Enter와 `task_started` 기록 사이**(`PersonEnter`·`RecordStart`): 그 사이에는 rollout이 아직 앞 턴의 종료를 보여 준다. 수정 후는 UserPromptSubmit 훅 증거(`ups`)로 `Busy`를 낸다.
- **`Quiet` 오라클 제거**: 단계 4가 실제로 볼 수 있는 것만 쓴다(`QuietObserved` = rollout이 조용함, `NoChildObserved` = 자식 없음). 둘 다 "끝난 턴"과 "조용히 실행 중인 턴"에서 같은 값을 낼 수 있다(`Silent`).

d3에서 더한 것(델타 리뷰 P1-N1·P2-A·P2-B·P2-C):

- **교체 사유 축**(`Config`, `ConfigChange`·`ConfigReplace`): 준비 판독과 무관한 warm fallback(launch 옵션 변경·binding 불일치·runtime kind 불일치)이다. 수정 전과 후 모두 사전 확인이 busy가 아니면 바로 교체한다. 뮤턴트 `after-guard-all`은 d2 문장대로 "Gone 또는 단계 4"를 모든 사유에 건다.
- **느슨한 판독 보정**: 수정 전 tmux의 사전 확인은 `turn_aborted`와 모르는 종료 이벤트를 busy로 읽는다(`tui_turn_state.rs:731`~`:735`). `LenientBusy`에 반영했다.
- **UserPromptSubmit 증거의 해제**: `task_started`가 rollout에서 읽히면 푼다(`RecordStart`). 읽히지 않으면 단계 4와 같은 가드(조용함 + 자식 없음) 아래에서 만료한다(`UpsExpire`). 턴이 뒤따르지 않는 UserPromptSubmit(`PersonBlocked`)도 넣었다. 뮤턴트 `after-ups-until-end`는 d2처럼 종료 신호로만 푼다.

A1에서 더한 것(0.169.0 fixture):

- **Enter 뒤 순서**(`EnterGap`): d3는 UserPromptSubmit 훅이 `task_started` 기록보다 먼저 온다고 뒀다(`"hook"`, 0.157.1 기준). 0.169.0은 Enter 뒤 약 13ms에 `task_started`를 먼저 쓰고, 훅은 그 0.05~0.7초 뒤에 온다([F] `codex-0.169.0.json`). `"record"`는 Enter와 함께 기록이 쓰인 것으로 보고, `"silent"`는 기록 전의 짧은 순간(아무 신호도 없다)을 남긴다. d3의 68 cfg는 `"hook"`으로 그대로 두었고 결과도 같다.

74 cfg: GREEN 40, 의도한 RED 34, 예상 밖 0. 호스트별 전/후 표와 반례의 코드 대조는 보고서에 있다. 설계에 반영한 반례는 다음과 같다.

| 반례 | 모델 cfg | 코드 대조 | 설계 반영 |
|---|---|---|---|
| herdr: 화면이 안 읽히면 유휴 Codex에 영영 입력하지 못한다 | `CodexReadyBeforeNarrow` | [C] herdr `composer_ready` 시간 초과(`codex/herdr_turn.rs:316`~`:336`), owner 경로도 같다 | 2절 준비 판정 |
| tmux: 화면이 안 읽히면 45초 뒤 pane을 교체해 전달한다. 영구 보류는 아니고 비용은 재기동이다 | `CodexReadyBeforeTmuxNarrow` (GREEN) | [C] `warm_followup.rs:542`~`:557` → `absence_launch.rs:248`~`:286` | 0절 표의 tmux "수정 전" |
| tmux: 대기 중 사람이 TUI에서 연 턴이 45초 뒤 교체로 죽는다 | `CodexReadyBeforeTmuxPerson` | [C] `Replacement::begin`은 turn row와 nonce만 본다(`discord/codex_absence/replacement.rs:63`~`:103`) | 1.7, 2절 |
| tmux: 종료 기록이 유실되거나 모르는 종료로 끝나면 느슨한 판독이 계속 busy라 후속 입력이 큐에 남는다 | `CodexReadyBeforeTmuxLostRecord`·`…LostBoth`·`…Unnamed` | [C] 사전 확인 `intake_turn.rs:1549`~`:1575`, 느슨한 판독 `tui_turn_state.rs:343`, `:731`~`:735` | 2절 대체 단계 2·3 |
| tmux: launch 옵션이 바뀐 직후 사람이 연 턴이 교체로 죽는다 | `CodexReadyBeforeTmuxConfigPerson` | [C] eligibility fallback(`warm_followup.rs:92`~`:98`)도 같은 `Replacement::begin`으로 간다 | 1.7, 8절 R5 |
| 새 판정에서 시간 초과 교체를 그대로 두면 `Busy` 턴을 죽인다 | `CodexReadyTimeoutReplace` | 구현 가드 | 1.7 |
| 교체 전 확인을 모든 사유에 "Gone 또는 단계 4"로 걸면, 옵션이 바뀐 유휴 pane의 입력이 영영 전달되지 않는다 | `CodexReadyGuardAllReasons` | d2 설계 문장의 결함. 구현 가드 | 1.7 |
| 열린 턴을 조용하다는 이유로 재기동하면 실행 중인 턴을 죽인다 | `CodexReadyReclaimOpen` | 구현 가드. readonly 실행의 긴 응답이 해당 | 2절 단계 4 |
| UserPromptSubmit 증거가 없으면 Enter 직후 구간에 Idle 경로 키가 들어간다 | `CodexReadyNoUps` | 구현 가드 | 2절 Busy 조건 |
| UserPromptSubmit 증거를 종료 신호로만 풀면, 종료가 이름 없이 끝나거나 턴이 시작되지 않았을 때 입력이 영영 보류된다 | `CodexReadyUpsUntilEnd`, `CodexReadyUpsUntilEndBlocked` | d2 설계 문장의 결함. 구현 가드 | 2절 Busy (b) |
| 종료 신호가 영영 없으면 보류가 영구화된다 | `CodexReadyNoFallback` | [C] owner 경로가 `NoStart\|Unknown → Screen::Unknown → Wait::Watch`로 무한 재판독(`owner_input.rs:829`, `:771`) | 2절 대체 단계 |
| 재개된 rollout의 끊긴 턴이 새 실행에서 계속 Open으로 읽힌다 | `CodexReadyNoFloor` | [C] `read_native_end`는 파일의 최신 `task_started`까지만 거슬러 읽고 실행 경계를 모른다(`rollout.rs:98`~`:105`) | 2절 floor |
| 종료 기록이 늦고 Stop 훅도 없으면 유휴 composer에 Esc가 간다 | `CodexStopAfterLateNoHook` | 설계 잔여. 화면 판독도 같은 창을 가진다 | 2절 /stop, 8절 |

수정 후에도 RED로 남는 cfg는 일곱이고 잔여로는 넷이다(R2·R3·R4에 cfg가 둘씩). 숨기지 않고 8절 잔여로 적었다.

| cfg | 깨지는 규칙 | 뜻 | 8절 |
|---|---|---|---|
| `CodexReadyAfterLostBothHeld` | `CodexPendingEventuallyDelivered` | 열린 것으로 읽히는 턴 + 종료 신호 둘 다 유실 + busy 경로 불가 → 보류가 풀리지 않는다 | R1 |
| `CodexReadyAfterCompressedHerdr`, `CodexReadyAfterUnnamedHerdr` | `CodexPendingEventuallyDelivered` | herdr + rollout 판독 불가(`.zst`, 이름 없는 종료에 Stop 훅도 없음) + #8128 전 → 전달되지 않는다(거절). `…Herdr8128` 둘은 GREEN | R2 |
| `CodexReadyAfterEnterRestart`, `CodexReadyAfterEnterGapSilent`(A1) | `CodexNoEarlyInput` | 사람의 Enter 구간 안에서 dcserver가 재시작해 UserPromptSubmit 증거가 사라진다. 0.169.0에서는 Enter와 `task_started` 기록 사이(약 13ms)에 신호가 없다 | R3 |
| `CodexReadyAfterArchivedSilentTurn`, `…RecordFirst`(A1) | `CodexNoRunningKill` | 살아 있는 턴의 rollout이 `.zst`이고, 그 턴이 자식 없이 조용하다 | R4 |

d2의 P2-B 경로(사람 턴 + 이름 없는 종료 + Stop 훅 없음)는 `CodexReadyAfterUnnamedPerson`·`…PersonHerdr8128`로 GREEN을 확인했다. 턴이 뒤따르지 않는 UserPromptSubmit은 `CodexReadyAfterBlockedPrompt`·`…Herdr`로 GREEN이다. 같은 상수에서 조용한 도구 실행은 `CodexReadyAfterSilentTool`로 GREEN이고, 자식 확인을 뺀 `CodexReadyNoChildCheck`만 `CodexNoRunningKill`로 깨진다(R3와 분리했다).

모델 범위 밖: 사람 초안과 모달(5번·29번은 바꾸지 않는다), 입력 owner의 예약·영수증(기존 `InputOwnership*` 모델), 턴 출력 릴레이, Enter 뒤 확인(1.6. 판정이 아니라 초안 판독이다), 1.7의 사유 가운데 `StrandedDraft`·`SubmitFailed`와 교체 없이 세션을 정리하는 길, 사전 확인과 교체 사이의 짧은 틈(8절 R5), 사전 확인을 거치지 않는 호출자, herdr의 launch 옵션 변경(지금도 형식 있는 거절이다. `codex/herdr_turn.rs:108` `PromptRefused::LaunchOptionsChanged`).

## 1. 진입점 전수표

### 1.1 화면 판독 호출부 (이슈 대상 26·27·28·30·32·33, 29)

| 조사 | 판독 함수 | 판정 | 호출자 (생산 코드) |
|---|---|---|---|
| 26 | `pane_looks_ready_for_codex_prompt` `codex_tui/input.rs:1246` | 준비 | `input.rs:1302`(ANSI 변형), `codex/herdr_turn.rs:319`, `discord/recovery_engine/herdr_row_end.rs:591`, `provider.rs:644` |
| 26 | `pane_looks_ready_for_codex_prompt_with_ansi` `input.rs:1290` | 준비 | `input.rs:432` |
| 26 | `prompt_readiness_from_ansi_pane` `input.rs:430` | 준비+초안 | `input/submission.rs:131` → `prompt_readiness_snapshot_with_pane` `:119` |
| 30 | `prompt_readiness_snapshot_with_pane` `input/submission.rs:119` | **Enter 뒤 확인·재전송·취소 정리** | `input.rs:427`(`prompt_readiness_snapshot`), `submission.rs:149`(`submission_readiness_from_ansi_pane` 안), `input.rs:1188`·`:1197`(Enter 뒤 두 번) → `classify_prompt_submit_confirmation` `:980`(호출 `:1204`) → `warm_followup/submit.rs:6` `settle_submit`(`RetrySafeDraft` `:48`, `Unconfirmed` `:93`), 취소 정리 `input.rs:943`~`:971`. 전수는 1.6 |
| 26 | `prompt_readiness_snapshot` `input.rs:426` | 준비+초안+생존 | `input.rs:530`·`:533`·`:564`·`:580`·`:634`·`:664`·`:670`·`:737`·`:790`(대기), `:951`(취소 뒤 초안 정리), `warm_followup.rs:532`·`:550`·`:561`, `discord/router/message_handler/tui_followup.rs:516`, 진단 `server/routes/agents.rs:360`·`:380`·`claude.rs:2103` |
| 26 | `pane_has_codex_prompt_draft` `input.rs:1476` | 초안 | `input.rs:434` |
| 26 | `pane_has_codex_active_turn_in_pane` `input.rs:1305`, `line_is_codex_active_turn_marker` `:1459` | 작업 중 | `input.rs:1294`, `input/composer_status.rs:274`(초안 판독기 안), `input/submission.rs:252` |
| 27 | `composer_status.rs:46` `model_first_status`, `:145` `short_composer_footer`, `:183` `pane_has_dim_composer`, `:197` `recent_has_compact_composer`, `:247` `composer_draft_with_start_in_pane` | 상태줄 모양·흐린 입력창 | `input.rs:1036`·`:1439`·`:1473`, `input/submission.rs:165`, `input/inline_banner.rs:35`, `input/composer_content.rs:81`, `input/herdr_cancel.rs:39` |
| 28 | `wait_until_codex_tui_input_ready` `input.rs:463` → `_with_policy` `:476`, `decide_rollout_ready_snapshot` `:254` | 준비 대기 | `warm_followup.rs:542`, `codex/tui_session_launch.rs:434`(PostTurnHandoff, 200ms), `discord/router/message_handler/intake_turn.rs:1576` |
| 28 | `decide_warm_input` `warm_followup.rs:363`, `snapshot_has_stranded_draft` `:375` | **준비 대기 시간 초과 → pane 교체** | `warm_followup.rs:533`(대기 전), `:551`(시간 초과 사유), `:562`(대기 뒤) → `Fallback` → `codex/absence_launch.rs:248`~`:261` → `Replacement::begin` `:274` → `cleanup_existing_codex_tui_session` `:280`(`codex.rs:1213`) → argv 재기동 |
| 30 | `submission_readiness_from_ansi_pane` `input/submission.rs:169`, `submission_busy_in_pane` `:248` | 제출 직전 준비·작업 중 | `input/submission.rs:151`(`admit_codex_prompt_under_lock` `input.rs:1082`), `tui_input/actor/gate.rs:85`·`:109`·`:199`(`judge_codex`·`own_draft`·`submission_busy`), `busy_inject/screen.rs:108` |
| 30 | `prompt_submission_fits_pane` `input/submission.rs:83` | pane 크기 | `host_input.rs:534` (tmux `pane_size`, 유지) |
| 30 | `active_composer_content_in_pane` `input/composer_content.rs:45` | 초안 | `codex/herdr_turn.rs:341`, `herdr_row_end.rs:594`, `busy_inject/screen.rs:112` |
| 32 | `composer_ready` `codex/herdr_turn.rs:316`, `empty_composer` `:339`, `owned_composer` `:354` | herdr 준비·초안·모달 | `herdr_turn.rs:536`·`:574`, `herdr_turn/first_input.rs:135`·`:137` |
| 32 | `composer` `codex/herdr_turn/busy.rs:46` | herdr busy 입력의 초안·모달 | `busy.rs:69`·`:106` (바꾸지 않음) |
| 33 | `herdr_turn_in_progress` `input/herdr_cancel.rs:12` | herdr /stop | `discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs:773` |
| 29 | `pane_shows_codex_interactive_modal` `input.rs:1041`, `submission_modal_in_pane` `submission.rs:207`, `inline_banner.rs:8`·`:12`·`:44` | 모달·배너 | `herdr_turn.rs:322`, `busy_inject/screen.rs:59`·`:78`·`:79`, `tui_input/actor/gate.rs:87`, `input.rs:1085`~`:1091` |
| 이슈 댓글 | `CodexPaneBusySignalTracker` `input.rs:1322` | pending-tool 회복 보류 | `rollout_tail/handoff.rs:9` → `rollout_tail.rs:235`, `handoff.rs:28`, `following_source.rs:57` → 소비 `following_source.rs:506`~`:530` |

이슈가 적은 줄 번호(1fe3a57d)는 이 base에서 `input.rs`만 85줄 앞당겨졌다(`:1331`→`:1246`, `:1375`→`:1290`, `:1544`→`:1459`, `:1561`→`:1476`, 댓글의 `:1407`→`:1322`). 나머지 파일은 그대로다.

### 1.2 이미 rollout로 판정하는 곳 (재사용 대상)

| 좌표 | 하는 일 |
|---|---|
| `busy_inject/rollout.rs:35` `NativeEnd{Open, Ended, NoStart, Unknown}`, `:98` `read_native_end` | 최신 턴의 자기 `task_complete`/`turn_aborted`만 종료로 본다. 이름 없는 종료·끊긴 줄은 `Unknown`(`:389`~`:407`) |
| `busy_inject/rollout.rs:14` `TurnVerdict`, `:75` `read_turn` | busy 입력이 열린 모델 턴인지 본다(`busy_inject.rs:306`) |
| `discord/router/message_handler/tui_followup/codex_observation.rs:52` `codex_native_end` | binding이 가리키는 rollout만 읽는다. 다른 rollout은 `Unknown` |
| `provider_dispatch/owner_input.rs:786` `native_read`, `:823` `codex_read` | owner 시작: `pane_liveness` + `NativeEnd`. `Ended`만 Ready |
| `codex_stop_delivery.rs:22` `CodexStopTurnIdentity`, `:717` | /stop이 자기 턴인지 rollout로 확인 |
| `herdr_row_end.rs:662` `codex_last_turn_closed` | 행 종료가 rollout로 턴 닫힘 확인 |
| `rollout_tail/parser.rs:455`~`:466` → `input.rs:312` `record_rollout_composer_ready` | tail이 `composer_ready`(명시·합성)를 세션 이름 래치로 기록 |

### 1.3 같은 상태를 소비하는 인접 경로

| 경로 | 좌표 | 이번 변경과의 관계 |
|---|---|---|
| Cancel | 대기 중 취소 `input.rs:492`~`:498`, `codex.rs:1260` `wire_cancel_token_to_tmux_session`, 취소 뒤 부분 초안 정리 `input.rs:943`(화면에서 자기 초안 확인 뒤 `C-u`) | 취소 우선순위(취소 > 기한 > 세션 종료) 보존. `:943`은 자기 초안 판독이라 유지 |
| /stop | herdr `codex_stop_delivery.rs:693` `deliver`, 늦은 재시도 `:487` `late_stop`, tmux `judged_stop.rs:60`·`session_stop.rs:98`(binding log로 턴 식별. 1.1의 판독 함수 호출 0) | herdr만 바꾼다 |
| Terminate | `codex.rs:789` `record_codex_tmux_termination`, `herdr_row_end.rs:556`~`:620` | 행 종료의 화면 준비 확인만 뺀다 |
| clear | Codex `/clear`는 새 session id·새 rollout 파일로 바뀌고, SessionStart `source=clear`는 다음 프롬프트와 함께 온다 [F] `tests/fixtures/hook_payload/codex-0.157.1.json` `tui_bypass_startup_clear` | 그 사이 binding은 옛 rollout(`Ended`)을 가리킨다. 프로세스는 같으므로 입력은 유휴 composer에 들어간다. 새 source가 게시되면 floor를 다시 잡는다 |
| provider reset·재기동 | `codex/absence_launch.rs:235`~`:286`(warm 실패 → 교체), `warm_followup.rs:29` `InputReadinessFailed`, `discord/codex_absence/replacement.rs:63` `Replacement::begin` | 교체 전에 판정을 확인한다(2절 시간 초과 표). 대체 단계 4가 이 기존 경로를 호출 |
| Discord 사전 확인 | `intake_turn.rs:1549`~`:1575`(느슨한 판독이 busy면 대기 없이 큐), 시간 초과 뒤 busy 알림 `:1637`~`:1656`, `busy_retry::defer_owned_front` `:1666` | 사전 확인을 2절 판정으로 바꾼다. 사유 있는 보류는 이 알림·재시도 경로를 쓴다 |
| 부팅 재수화 | `discord/recovery_engine/rebind_runtime/codex_spawn.rs:92` `tail_recovery_rollout_for_tmux` → `following_source.rs:57`(pane busy probe), `codex.rs:552` `register_codex_tui_idle_relay_binding` | probe 교체 대상. 재시작 뒤에는 메모리 증거가 비어 있다 |
| idle 정리·회복 | `tmux_turn_liveness.rs:159`~`:166`, `discord/relay_recovery/idle_tmux.rs:221`~`:231` → `provider.rs:643` | Codex 분기의 화면 준비 판독을 뺀다 |
| 준비 래치 | `input.rs:298`~`:362`. 지우는 곳은 `take`(`:342`)와 시험 전용 `:326`뿐이다 | 래치는 깨우는 용도로만 남긴다 |

### 1.4 재시작·부팅·incarnation

| 사건 | 좌표 | 사라지는 것 / 남는 것 |
|---|---|---|
| dcserver 재시작 | 래치 `input.rs:303`(메모리), 훅 버퍼 `claude_tui/hook_registry.rs:52`(TTL 30초, 메모리) | 래치·훅 증거 소실. rollout·binding log·spawn nonce marker는 남는다 |
| 새 Codex 실행(tmux) | `codex/tui_session_launch/first_input.rs:225`~`:340`, `launch_policy.rs:147` | 새 execution nonce, 새 rollout 또는 resume. SessionStart 훅과 rollout 헤더는 첫 프롬프트 때 온다 [F] |
| 새 Codex 실행(herdr) | `codex/herdr_turn.rs:200` `fresh_launch`, `:228` `pending_launch`, `:268` `hook_logged`, `:279` `logged_source` | 실행 nonce 자신의 Startup binding event가 source |
| 프로세스 종료 | tmux `platform/tmux.rs:1005` `pane_liveness`, herdr `session_host/herdr_gate.rs:758` `read_execution` → `PaneProvider::Exited` `:78` | 턴은 끊기고 rollout에는 종료 기록이 없다 |

### 1.5 입력 출처 분류표

| 출처 | 진입 좌표 | 소유자 | 준비 판정 소비 |
|---|---|---|---|
| Discord 사람·봇 메시지(유휴) | `intake_turn.rs:1549`~`:1600` → tmux `codex/absence_launch.rs:235` `try_codex_tui_warm_followup`, herdr `provider_dispatch.rs:630` → `herdr_turn.rs:515` `followup` | 채널 mailbox, owner 입력이면 `owner_input.rs:835` `start_idle` | 준비 대기·제출 게이트 |
| Discord 메시지(작업 중) | `discord/queue_io/input_owner/effect/codex.rs:116`(tmux), `:143`(herdr `busy.rs:17`) | 입력 owner | `read_turn`(이미 rollout). 화면은 초안·모달만 |
| API·health 전달 | `discord/health/turn_deliver/inject.rs:329`·`:944` | 요청자 ticket | 위와 같음 |
| 루틴·예약·HTTP 시작 | `owner_input.rs:847`~`:852`(provenance ticket 유지) | owner | `native_read` |
| 사람이 TUI에 직접 입력 | AgentDesk가 쓰지 않는다. UserPromptSubmit 훅·rollout으로 관측 | 사람 | 이 턴이 열려 있으면 `Busy` |
| 첫 입력(새 실행) | tmux `first_input.rs:225`(argv), herdr `herdr_turn/first_input.rs:113`(composer에 입력) | launch | 프로세스 전경 실행 + 축소된 화면 확인 + 초안 판독 |

### 1.6 `composer_marker_detected`와 준비 snapshot의 소비자 전수

설계가 정의를 바꾸는 값은 `PromptReadinessSnapshot.composer_marker_detected`(`input.rs:366`) 하나다. 지금은 `pane_looks_ready_for_codex_prompt_with_ansi`(`input.rs:432`)가 채우고, 화면에 작업 중 표시나 모달이 보이면 false다(`:1294`~`:1295`). `git grep composer_marker_detected -- 'src/**/*.rs'`에서 시험과 Claude 쪽 동명 필드를 뺀 전부를 적는다.

| 소비자 | 좌표 | 쓰임 | 분류 | 옮긴 뒤 |
|---|---|---|---|---|
| `PromptDraftPolicy::accepts` | `input.rs:236` | 준비 대기가 준비를 인정 | 판정 | 삭제(판정 대기로 대체, 조각 A) |
| `decide_warm_input` | `warm_followup.rs:364` | warm 재사용 또는 교체 | 판정 | 판정 사유를 받는다(조각 C) |
| `snapshot_allows_guarded_submission` | `input.rs:999`(정의 `:996`, 사용 `:1084`·`:1090`) | lock 안 제출 허용 | 판정 | `proof.current()` + 초안 판독(조각 C) |
| `judge_codex` | `tui_input/actor/gate.rs:84`·`:90` | owner 제출 게이트의 Ready | 판정 | 위와 같음(조각 C) |
| `submission_readiness_from_ansi_pane` | `input/submission.rs:149`·`:152` | 제출 직전 marker 덮어쓰기 | 판정 | 위와 같음(조각 C) |
| `hosted_tui_draft_should_enter_provider_recovery` | `tui_followup.rs:518` → `:151`~`:158`(사용 `:537`) | "살아 있고 composer가 보이고 초안이 있다" → provider 회복 | 초안 판독 | marker 새 정의 그대로 |
| `snapshot_has_retry_safe_prompt_draft` | `input.rs:901`(사용 `:910`·`:952`·`:977`·`:984`·`:1189`) | Enter 뒤 자기 글이 composer에 남았는가 | 초안 판독(Enter 뒤 확인) | marker 새 정의 + rollout 길이 가드 |
| `classify_prompt_submit_confirmation` | `input.rs:980`(호출 `:1204`), `snapshot_allows_tail_only_no_replay` `:974` | `Submitted`·`RetrySafeDraft`·`Unconfirmed` 선택 | 초안 판독(Enter 뒤 확인) | 위와 같음 |
| `settle_submit` | `warm_followup/submit.rs:48`~`:92`(`prompt_draft_matches` `:50`·`:51`, `submit_failure_allows_fallback` `warm_followup.rs:380`), 로그 `:99` | 재전송을 위한 교체 | 초안 판독 + 교체 | 교체 전에 판정이 `Busy`가 아님을 확인(1.7, 조각 C) |
| `clear_cancelled_partial_prompt_draft` | `input.rs:943`~`:971`(`:952`) | 취소 뒤 자기 초안을 `C-u`로 지움 | 초안 판독 | marker 새 정의 |
| `log_prompt_ready_timeout` | `input.rs:1615`·`:1618` | 시간 초과 로그 | 진단 | 판정 사유를 싣는다 |
| 진단 API | `server/routes/agents.rs:384`·`:386` | `ready_for_input` 표시 | 진단(38) | 판정에 쓰지 않음. 값은 2절 판정으로 채운다 |
| `snapshot_allows_warm_followup_submit` | `input.rs:1007` | `#[cfg(test)]` 전용 | 시험 | 시험과 함께 정리 |

**결정(권고안 A)**: marker는 "초안 판독기가 활성 composer를 찾았는가"로만 정의한다. Enter 뒤 확인·재전송·취소 정리는 판정 경로가 아니라 자기 초안 판독(5번 범위)이며, 지금 동작을 그대로 둔다. 지금 marker가 하던 "작업 중 표시가 보이면 false"만 빠진다. 그 역할은 rollout이 대신한다.

- 재전송(`RetrySafeDraft`): 이미 `rollout_len()`이 Enter 전보다 자라지 않았을 때만 교체한다(`submit_failure_allows_fallback`). 여기에 교체 전 확인 하나를 더한다. 조건은 1.7의 표와 같다: 판정이 `Busy`이면 교체하지 않고, 지금의 "refusing replay" 종료(`submit.rs:55`~`:57`)로 끝낸다. 유휴 pane에 자기 글이 남은 정상 복구는 지금처럼 교체한다.
- `Unconfirmed`: 지금처럼 Enter를 다시 보내지 않는다.
- 취소 정리(`C-u`): 자기 초안 글자가 composer에서 **읽혔을 때만** 지운다. 새 정의에서는 작업 중 표시가 있는 화면에서도 자기 초안이 읽히면 지운다. 조건은 지금과 같다: composer를 건드렸고 Enter는 시도하지 않았으며(`:948`), 읽힌 초안이 자기 프롬프트의 앞부분이다(`:952`~`:955`). 사람이 덧쓴 글은 앞부분 비교에서 달라 지우지 않는다. 턴이 도는 중에 남은 자기 글 조각은 다음 Enter에 섞여 나가므로 지우는 쪽이 맞다.

Enter 뒤 확인을 rollout 영수증(`busy_inject/rollout.rs:419` `submitted`)으로 바꾸는 안 B는 더 정확하지만 조각 C가 커지고 5번 초안 판독의 기대값을 바꾼다. 이번에는 하지 않는다.

### 1.7 기존 tmux pane을 끝내는 모든 길 (사유 × 호출자)

교체 전 판정 확인을 어디에 거는지는 이 표 하나로 정한다. 1.6·2절·3절은 이 표를 가리킨다. `git grep`으로 확인한 범위: `Replacement::begin`의 생산 호출자는 둘(`codex/absence_launch.rs:274`, `codex_tui/warm_followup/submit.rs:68`), `CodexWarmFallbackReason`은 여섯(`warm_followup.rs:25`~`:32`), `cleanup_existing_codex_tui_session`의 호출자는 하나(`absence_launch.rs:280`), `kill_legacy_pane`의 생산 호출자는 하나(`submit.rs:72`), `try_codex_tui_warm_followup`의 생산 호출자는 하나(`absence_launch.rs:235`. `codex.rs:3029`는 시험), `settle_submit`의 생산 호출자는 하나(`warm_followup.rs:605`)다. `verified_warm`(`warm_followup.rs:1218`)은 교체하지 않고 오류로 끝난다.

| 사유 | 생기는 곳 | pane을 끝내는 곳 | 지금 | d3의 교체 전 확인 |
|---|---|---|---|---|
| `InputReadinessFailed` | 준비 대기 시간 초과 `warm_followup.rs:553`, 대기 뒤 거절 `:562` → `:371` | `absence_launch.rs:274` → `:280` | 무조건 교체 | **확인한다.** `Busy` → 교체하지 않고 `Deferred`. `Unknown` → 단계 4 조건(2절)이면 교체, 아니면 `Deferred`. `Gone`·`Idle` → 교체(실행 중인 턴이 없다) |
| `StrandedDraft` | 대기 전 `:534`, 시간 초과 뒤·대기 뒤 `:366` | `absence_launch.rs:274` → `:280` | 교체 | **확인한다.** `InputReadinessFailed`와 같은 매핑이다. `Idle`·`Gone`이면 지금처럼 교체한다(5번 동작 불변) |
| `SubmitFailed` (Enter 뒤 자기 글이 남음, `RetrySafeDraft`) | `submit.rs:48`~`:59` | `submit.rs:68` → `:72` | 초안 두 번 일치 + rollout 길이 가드 뒤 교체 | **`Busy`만 확인한다.** `Busy` → 교체하지 않고 "refusing replay" 종료. 그 밖에는 지금처럼 교체 |
| `SubmitFailed` (Enter 전 전달 실패, `NotSubmitted`) | `submit.rs:28`~`:39` | `absence_launch.rs:274` → `:280` | rollout 길이 가드 뒤 교체 | 확인하지 않는다. 지금처럼 교체 |
| `LaunchOptionsChanged` | eligibility `warm_followup.rs:98`, `:513`~`:519` | `absence_launch.rs:274` → `:280` | 교체 | 확인하지 않는다. 지금처럼 교체 |
| `RolloutBindingMismatch` | eligibility `:95`, rollout 파일 없음 `:582` | `absence_launch.rs:274` → `:280` | 교체 | 확인하지 않는다. 지금처럼 교체 |
| `RuntimeKindMismatch` | eligibility `:92` | `absence_launch.rs:274` → `:280` | 교체 | 확인하지 않는다. 지금처럼 교체 |
| (사유 없음) `LegacyPath`: 새 세션 요청(`force_fresh`), 죽은 pane, resume 대상 없음 | `warm_followup.rs:84`~`:90` | `absence_launch.rs:280`(`Replacement::begin` 없이 정리) | 정리 뒤 새로 띄움(`codex.rs:749` 종료 사유 셋) | 확인하지 않는다. 그대로 |
| (사유 없음) warm 재사용이 꺼져 있음, `prepared`가 있음 | `absence_launch.rs:234` | `absence_launch.rs:280` | 정리 뒤 새로 띄움 | 확인하지 않는다. 그대로 |

**확인을 거는 기준**: (가) 사유 자체가 준비 판독이다(`InputReadinessFailed`). (나) 사유를 만드는 판독이 이 설계가 정의를 바꾸는 marker를 읽는다(`StrandedDraft`는 `decide_warm_input`, `RetrySafeDraft`는 `snapshot_has_retry_safe_prompt_draft`). 지금 marker는 작업 중 표시가 보이면 false라서 실행 중인 pane에서는 이 두 사유가 생기지 않는다. 새 정의에서는 생길 수 있으므로 교체 전 확인으로 그 차이를 메운다. 두 기준 어디에도 들지 않는 사유는 손대지 않는다. d2의 "`absence_launch.rs:274` 앞에서 Gone 또는 단계 4를 확인한다"는 문장은 이 자리를 지나는 모든 사유에 걸려, launch 옵션이 바뀐 유휴 pane의 정상 교체를 막는다(`CodexReadyGuardAllReasons` RED). 그 문장은 버린다.

**구현 위치**: `absence_launch.rs:251`~`:252`에서 받은 `warm_fallback_reason`으로 갈라, `:274` 앞에서 위 표의 "확인한다" 사유(`InputReadinessFailed`, `StrandedDraft`)일 때만 판정을 읽는다. 재전송(`submit.rs:68`)은 `Busy`만 본다. 방금 `Idle` 증명 아래에서 Enter를 보낸 직후이기 때문이다.

**거부했을 때의 반환 계약**: `CodexWarmFollowupOutcome`(`warm_followup.rs:58`)에 `Deferred(CodexWarmFallbackReason)`를 더한다. `absence_launch`는 이를 준비 시간 초과 오류(`input.rs`의 `is_prompt_ready_timeout_error`가 참인 형식)로 올리고, pane·세션 파일·binding은 건드리지 않는다. turn 층은 Claude가 같은 상황에서 쓰는 길로 입력을 다시 큐에 넣는다: Enter 전 시간 초과를 재시도 가능한 오류로 올리고(`claude_tui/hosting/warm_followup.rs:334`~`:341`), turn bridge가 다시 넣는다(`discord/turn_bridge/followup_requeue.rs:98` `requeue_if_needed` → `:120`). [C] 이 재큐는 지금 Claude 전용이다. 후보 분류가 provider `Claude`와 runtime `ClaudeTui`를 요구한다(`discord/turn_bridge/streaming_edit_text.rs:185`~`:198`). Codex의 준비 시간 초과 오류(`input.rs:165` 접두어, `:411` `is_prompt_ready_timeout_error`)는 지금 turn 실패로 끝난다. 그래서 조각 C가 이 분류와 재큐에 Codex 분기를 더한다. [U] 재시도 상한에 닿았을 때의 끝은 조각 C의 첫 작업에서 좌표로 고정한다.

**`Deferred`가 거절로 굳지 않게 하는 조건**: busy 재시도 상한은 6회다(`discord/busy_followup_retry_store.rs:24` `MAX_BUSY_RETRY_COUNT`). 45초 대기를 여섯 번 돌아도 270초라, `Unknown`의 단계 4(600초)가 열리기 전에 상한에 닿을 수 있다. 그래서 `Deferred`는 사유를 싣고, turn 층은 이렇게 다룬다. `Busy`로 미룬 입력은 지금의 busy 큐처럼 턴이 끝날 때 다시 시도한다. `Unknown`으로 미룬 입력은 재시도 횟수를 쓰지 않고, 단계 4가 열리는 시각(마지막 rollout 변화 + 600초)에 맞춰 한 번 예약한다(`followup_requeue.rs:79` `schedule_deferred_idle_queue_kickoff`와 같은 예약). 이 조건이 없으면 d3의 확인이 새 거절 경로가 된다.

**Discord 사전 확인과의 관계**: Discord 메시지는 turn을 시작하기 전에 사전 확인을 거친다(`intake_turn.rs:1549`). 조각 A가 이것을 2절 판정으로 바꾸므로, 판정이 `Busy`인 동안에는 입력이 turn을 시작하지 않고 큐에 남는다. 그래서 "확인하지 않는다" 사유가 실행 중인 턴을 만나는 경우는 사전 확인과 교체 사이에 턴이 시작된 때와 사전 확인을 거치지 않는 호출자뿐이다. 지금도 같고, 8절 R5로 적는다.

## 2. 판정별 신호 대응표

`codex_tui/readiness.rs`의 판정 하나가 아래를 낸다. 값 범위는 1절의 좌표에서만 가져왔다.

| 판정 | 조건 | 신호 좌표 |
|---|---|---|
| Gone | 프로세스가 없다 | tmux `PaneLiveness::DeadOrAbsent`(`owner_input.rs:790`), herdr `PaneReading::Missing`·`PaneProvider::Exited`(`herdr_gate.rs:61`·`:78`) |
| Unknown | 프로세스 판독 실패, source 없음·교체 중, rollout 없음·`.zst`·끊긴 줄·이름 없는 종료 | `PaneLiveness::ProbeError`, `PaneReading::Unreadable`, `codex_verified_input_blocked`(`tui_prompt_dedupe/runtime_binding/codex_policy.rs:136`), `NativeEnd::Unknown` |
| Busy{turn} | (a) 현재 실행 floor 뒤의 최신 턴이 열려 있고, 그 `turn_id`의 유효한 Stop 훅이 없다. 또는 (b) 현재 실행의 UserPromptSubmit 훅이 마지막 종료 증거(종료 기록·Stop 훅)보다 늦게 왔고, 그 뒤의 `task_started`가 아직 rollout에서 읽히지 않았으며, 만료되지 않았다 | `NativeEnd::Open`, `read_turn`, UserPromptSubmit payload `turn_id` [F] |
| Idle(proof) | 프로세스 생존 + source 현재 + Busy (b) 아님 + (`Ended` 또는 열린 턴과 같은 `turn_id`의 유효한 Stop 훅) | `read_native_end`, Stop 훅 payload `turn_id` [F] |
| 턴 없는 새 실행 | floor 뒤 `NoStart`. 준비 판정이 아니라 첫 입력 경로가 맡는다(아래 5) | tmux는 프롬프트를 argv로 넘긴다(`launch_policy.rs:146` 주석), herdr는 `herdr_turn/first_input.rs:113` |

**Busy (b)**: 사람이 TUI에서 Enter를 누른 뒤 `task_started`가 rollout에 쓰이기 전에는 rollout이 앞 턴의 `Ended`를 보여 준다. UserPromptSubmit 훅은 턴이 진행되기 전에 오므로, 그 훅을 받은 시점부터 `Busy`로 둔다. 이 조건을 빼면 그 구간에 Idle 경로 키가 실행 중 턴에 들어간다(`CodexReadyNoUps` RED). 훅이 없는 실행과 재시작으로 증거가 사라진 경우는 8절 R3이다.

**A1**: 위 순서는 0.157.1의 것이다. 0.169.0은 `task_started`를 먼저 쓰고 UserPromptSubmit 훅을 그 뒤에 낸다([F] `codex-0.169.0.json`의 `rollout_identity.record_order`). 그래서 0.169.0에서 Enter 구간을 닫는 것은 훅이 아니라 시작 기록이고, 훅이 도착했을 때는 그 턴이 이미 열린 턴으로 읽혀 Busy (b)가 곧바로 풀린다(Busy (a)가 맡는다). Busy (b)는 시작 기록을 읽을 수 없을 때(`.zst`)와 훅이 먼저 오는 버전에서만 일한다. 기록 전의 약 13ms는 R3이다.

Busy (b)가 막을 것은 Enter와 `task_started` 사이뿐이다. 그래서 다음 셋 중 하나로 푼다.

1. 같은 `turn_id`의 `task_started`가 rollout에서 읽혔다(훅보다 먼저 쓰였어도 같다. A1: 0.169.0에서 `turn_id`가 같음을 확인했다), 또는 훅 뒤에 시작한 다른 턴이 읽혔다. 그 뒤로는 열린 턴 자체가 Busy (a)의 증거다.
2. 같은 `turn_id`의 종료 기록이나 유효한 Stop 훅을 봤다.
3. 만료: `task_started`가 읽히지 않은 채 rollout stamp가 600초(`STALE_USER_SUBMITTED_RECLAIM_SECS`) 동안 그대로이고 provider 프로세스에 자식이 없다. 단계 4와 같은 가드다. 만료하면 증거를 버리고 rollout만으로 다시 판정한다. 읽을 수 있고 `Ended`이면 `Idle`이라 교체 없이 전달하고, 읽을 수 없으면 `Unknown`이라 단계 4로 간다.

d2는 2번으로만 풀었다. 그러면 사람 턴이 이름 없는 종료로 끝나고 Stop 훅이 없을 때, 또는 UserPromptSubmit 뒤에 턴이 시작되지 않을 때(사용자층 훅의 block 등 [U]) 증거가 영영 남아 단계 4까지 막는다(`CodexReadyUpsUntilEnd`·`CodexReadyUpsUntilEndBlocked` RED). 리뷰는 만료 때 `Unknown`으로 내리기를 권했는데, 읽을 수 있는 rollout이 `Ended`인 경우까지 교체할 이유가 없어 "증거를 버리고 다시 판정"으로 했다. 가드는 같다.

**유효한 Stop 훅**: nonce·`transcript_path`·`turn_id`가 모두 현재 턴과 같고, 훅을 받은 뒤 같은 턴의 rollout 기록이 더 자라지 않았다. 사용자층 `~/.codex/hooks.json`의 다른 Stop 훅이 block을 돌려주면 같은 `turn_id`로 턴이 이어질 수 있기 때문이다(4절 [U]). 확인 위치는 `readiness.rs`의 `proof.current()`다.

### 준비

1. 호출자는 `Idle(proof)`를 받을 때까지 기다린다. 깨우는 신호는 지금과 같다: rollout `composer_ready` 래치 알림(`input.rs:360`), Stop 훅 알림(`hook_server.rs:42`), 그리고 100ms~1s 재판독(`input.rs:629`~`:706`의 기한·취소 계약 유지).
2. 래치는 **깨우기 전용**이다. 래치만으로 준비를 인정하지 않는다. 래치는 `task_started`에서 지워지지 않고 세션 이름으로만 구분되므로(`parser.rs:467`~`:480`, `input.rs:312`) 다음 턴·다음 실행에 남는다(`CodexReadyLatchAuthority` RED).
3. 첫 키 직전, composer lock 안에서 `proof.current()`를 다시 확인한다. 실행 nonce·파일 `(dev, ino)`·최신 턴 id·종료 상태가 그대로일 때만 쓴다(`CodexReadyNoRevalidate` RED). 위치는 tmux `admit_codex_prompt_under_lock`(`input.rs:1066`), herdr `followup_write` 직전(`herdr_turn.rs:614`)이다.
4. **floor**: `NativeEnd`는 이 실행이 뜬 뒤의 기록만 읽는다. 근거는 운영 코드가 실제로 남기는 것만 쓴다.
   - 1순위: 이 실행의 launch context가 만들어진 시각. `BindingContext.created_at`(`tui_prompt_dedupe/binding_context.rs:24`)은 spawn 전에 정해져 디스크에 남고(`:345`, 읽기 `:207` `execution_context`), 재시작 뒤에도 읽힌다. rollout 레코드의 최상위 `timestamp`가 이 시각보다 이른 `task_started`는 앞 실행의 턴이므로 열린 턴으로 세지 않는다. 같은 호스트의 같은 시계다(`BindingContext.host`).
   - 2순위: 이 실행의 첫 훅을 받았을 때의 파일 길이(메모리). 레코드에 `timestamp`가 없는 옛 형식에서 쓴다. **A1**: 구현하지 않았다. 0.157.1·0.169.0 모두 모든 레코드에 `timestamp`가 있어 쓸 곳이 없다. 시작 기록에 `timestamp`가 없으면 floor를 적용하지 않고 읽는다.
   - 둘 다 없으면 0으로 읽는다. 그때 끊긴 턴이 Open으로 보이면 단계 3으로 간다. **A1**: "없다"는 pane에 실행 nonce marker가 없는 경우만이다. marker가 있는데 그 launch context를 읽지 못하면 0으로 읽지 않고 `Unknown(ProbeError)`로 둔다(앞 실행의 종료를 이 실행의 준비로 읽지 않기 위해서다).
   - d1이 1순위로 적은 rollout marker의 `rollout_start_offset`은 뺀다. `write_codex_tui_rollout_marker`(`codex_tui/session.rs:151`)는 `#[cfg(test)]`이고 `_with_start_offset`(`:164`)의 호출자도 모두 시험이라 운영에서는 쓰이지 않는다. 리뷰가 권한 "binding 관측의 파일 길이"도 쓸 수 없다. binding이 내구화하는 source 식별자는 `SourceId{session_id, path, dev, ino}`(`tui_o/shadow/mod.rs:47`)뿐이고 길이가 없다.
   - **A1** 확인: launch context는 훅 주입 여부를 정하기 전에 만들어진다([C] `codex/tui_session_launch/first_input.rs:225`의 launch context 준비가 `:261` `hooks_enabled` 분기보다 앞). 0.169.0 rollout은 캡처한 두 파일의 모든 레코드(18·30건)에 최상위 `timestamp`가 있다([F]).
5. **턴 없는 새 실행**: Codex 0.157.1은 rollout 헤더를 launch 때 쓰지만, SessionStart 훅은 첫 프롬프트와 함께 낸다([F] `tests/fixtures/hook_payload/codex-0.157.1.json` `tui_bypass_startup_clear`의 note: "thread created at launch …; SessionStart fired only with the first prompt …, after the rollout header was written"). 파일은 있어도 그 파일을 이 실행에 묶는 훅이 아직 없으므로, 부팅 직후에는 묶인 source가 없다. **A1**: 0.169.0은 rollout 파일 자체를 첫 프롬프트 때 만든다([F] `file_birth`가 `launched_at`보다 12초 늦고 첫 `task_started`와 같은 시각. `session_meta.payload.timestamp`만 thread 생성 시각이다). 부팅 때 나는 훅은 여전히 없다. 그래서 부팅 직후에는 파일도 없고, 결론(묶인 source 없음 → 첫 입력 경로)은 같다. 이 구간의 입력은 첫 입력 경로뿐이고, 증거는 "프로세스가 pane의 전경 실행이다"(herdr `PaneProvider::Execution`) + "화면이 비어 있지 않고 모달이 읽히지 않는다"다. owner 경로가 이미 쓰는 규칙이다(`herdr_turn.rs:354`~`:366`). 상태줄·폭을 해석하지 않는다. owner 시작(`owner_input.rs:829`)의 `NoStart → Unknown`은 그대로 둔다.
6. 훅이 주입되지 않은 실행(`first_input.rs:298`~`:302`의 "using rollout transcript tail")은 Stop 훅 단계가 없다. rollout 판정과 대체 단계 3·4로만 간다.

### 작업 중

- `Busy{turn}` = 판정표의 (a) 또는 (b). `tui_busy_followup_diagnostic`(`tui_followup.rs:455`)의 Codex 분기도 이 판정으로 바꾼다. 지금 쓰는 느슨한 판독은 `turn_aborted`를 Streaming으로 분류한다([C] `tui_turn_state.rs:727`~`:736`의 기본 분기). 중단된 턴 뒤 다음 입력이 busy로 분류될 수 있는 후보다(시험·운영 재현 아님).
- pending-tool 회복 보류(`following_source.rs:506`~`:530`)의 pane 경과 시간 표시는 "rollout stamp가 보류 시작 뒤 자랐는가 또는 provider 프로세스에 자식이 있는가"로 바꾼다. 자식 관측은 tmux에서는 B1의 `claude_tui/activity/processes.rs:19` `observe`를 그대로 쓰고, herdr에서는 아래 단계 4의 어댑터를 쓴다(`Descendants::Present` = Fresh, `NoneObserved` = Idle, `Unknown` = Unavailable). 보류 상한(`pane_busy_veto_cap`)은 그대로다.

### 종료

- 자기 턴의 `task_complete`/`turn_aborted`(이름 일치)가 종료다. 이름 없는 종료는 `Unknown`이다(`rollout.rs:402`~`:405`).
- Stop 훅이 종료 기록보다 먼저 올 수 있다(`rollout_tail.rs:693` 주석). 같은 `turn_id`의 Stop 훅은 종료 증거다.

### /stop (herdr)

- 진행 중 = 프로세스 생존 + source 현재 + `CodexStopTurnIdentity`가 자기 턴을 Open으로 읽음(`codex_stop_delivery.rs:717`~`:724`, 이미 있음) + 그 턴의 Stop 훅 없음. `herdr_turn_in_progress`의 화면 표시 요구(`herdr_cancel.rs:14`)를 뺀다.
- 모달·사람 초안 veto는 남긴다(`herdr_cancel.rs:15`~`:17`). 판독이 안 되면 veto하지 않는다. Esc는 작업을 멈출 뿐이라 gate도 통과시킨다(`herdr_gate.rs:86`~`:92`).
- rollout을 읽지 못하면 "유휴"가 아니라 기존 `unobserved()`(`:217`)로 답한다. `late_stop`(`:487`)이 다시 시도한다.
- 잔여: 종료 기록이 아직 안 쓰였고 Stop 훅도 없는 창에서는 유휴 composer에 Esc 한 번이 갈 수 있다(`CodexStopAfterLateNoHook`). pin 뒤 `identity.current()` 재확인(`:788`)이 이 창을 줄이지만 없애지 못한다. 화면 판독도 스피너가 사라지기 전 같은 창을 가진다.

### 신호가 없거나 늦을 때

| 상황 | 단계 | 동작 | 모델 |
|---|---|---|---|
| 종료 기록이 늦다 | 1 | 재판독이 기록을 본다. Stop 훅이 먼저 오면 2 | `CodexReadyAfterPerson` |
| 종료 기록 유실·이름 없음, Stop 훅 있음 | 2 | 같은 `turn_id`의 유효한 Stop 훅으로 `Idle` | `CodexReadyAfterLostRecord`, `…UnnamedHook` |
| 둘 다 없음, 프로세스 생존, 턴이 Open으로 읽힘 | 3 | 기존 busy 입력 경로로 보낸다(`effect/codex.rs:116`·`:143`). Codex는 열린 턴이면 큐에 넣고 유휴면 새 턴을 시작하며, nonce 영수증이 어느 턴에 기록됐는지 알려 준다(`rollout.rs:419` `submitted`). 대상 턴과 다른 턴에 기록된 영수증의 처리(`busy_inject.rs:484`~`:508`)는 [U]라 조각 C에서 먼저 확인한다 | `CodexReadyAfterLostBothBusy`, `CodexReadyAfterRestart` |
| Open으로 읽히는데 busy 경로를 쓸 수 없다(`TurnVerdict::NonSteerable`: readonly 실행 등, `rollout.rs:14`) | — | **준비 판독을 이유로는 교체하지 않는다**(1.7). 사유 있는 보류로 두고(`intake_turn.rs:1637`~`:1666`의 busy 알림·재시도), 프로세스가 사라지면(`Gone`) 재기동한다. 8절 R1 | `CodexReadyAfterLostBothHeld` (RED) |
| rollout을 읽을 수 없다(`.zst`·없음·이름 없는 종료·끊긴 줄), Busy (b) 아님 | 4 | 아래 조건에서만 새 실행으로 바꾼다 | `…Unnamed`, `…Compressed`, `…CompressedHerdr8128` |
| 프로세스 종료 | — | `Gone` → 기존 재기동 | `CodexReadyAfterExit`, `…ExitHerdr` |
| dcserver 재시작 | — | 디스크의 rollout·binding·launch context로 같은 판정. 메모리의 훅 증거가 없으면 3·4 | `CodexReadyAfterRestart` |

**단계 4의 조건**: 판정이 `Unknown`이고, rollout stamp가 `STALE_USER_SUBMITTED_RECLAIM_SECS`(600초, `tui_turn_state.rs:76`) 동안 그대로이고, provider 프로세스에 자식이 없다(`Descendants::NoneObserved`). 자식 판독이 `Unknown`이면 교체하지 않는다. `Open`에는 쓰지 않는다. d1은 `Open + busy 경로 불가`에도 단계 4를 열었는데, 그러면 readonly 실행이 자식 없이 600초 넘게 응답을 만드는 턴을 죽인다(`CodexReadyReclaimOpen` RED). 자식 확인을 빼면 조용한 도구 실행 중인 턴을 죽인다(`CodexReadyNoChildCheck` RED).

**기존 준비 대기 시간 초과(tmux 45초, `input.rs:154`)와의 관계**: 지금은 시간 초과면 무조건 `Fallback(InputReadinessFailed) → Replacement::begin → pane 종료 → argv 재기동`이다. 새 판정에서는 시간 초과 시점의 판정에 따라 갈린다.

| 시간 초과 시점의 판정 | 동작 |
|---|---|
| `Idle(proof)` | 시간 초과가 아니다. 그대로 전달한다 |
| `Busy{turn}` | **교체하지 않는다.** `Deferred`로 돌려 다시 큐에 넣는다. 다음 시도에서 사전 확인이 단계 3(busy 경로)으로 보내거나 사유 있는 보류로 둔다 |
| `Unknown` | 단계 4의 조건을 만족할 때만 교체한다. 아니면 `Deferred` |
| `Gone` | 기존 재기동 |

이 매핑은 `InputReadinessFailed` 사유에만 적용한다. 같은 자리(`absence_launch.rs:274`)를 지나는 다른 사유와 Enter 뒤 재전송(`submit.rs:68`)의 조건은 1.7의 표가 정본이고, 거부했을 때의 반환 계약도 거기에 있다. `decide_warm_input`(`warm_followup.rs:551`~`:557`)은 판정을 받아, 초안이 읽혔으면 `StrandedDraft`, 판정이 `Idle`이면 `ReusePane`, 그 밖에는 판정을 실은 `InputReadinessFailed`를 낸다. `InputReadinessFailed`에 이 확인이 없으면 `Busy` 턴을 죽인다(`CodexReadyTimeoutReplace` RED: `CodexNoRunningKill`).

**herdr의 단계 4**: herdr에는 살아 있는 pane을 끝내고 새 실행으로 바꾸는 경로가 아직 없다.

- #8128 착지 전: 단계 4 조건이 되면 `followup`(`codex/herdr_turn.rs:515`)이 형식 있는 거절로 끝낸다. `PromptRefused`(`:108`)에 새 사유 `SourceUnprovable`을 더한다. 키는 쓰지 않는다. 이 동안 그 채널 입력은 전달되지 않는다(`CodexReadyAfterCompressedHerdr`·`CodexReadyAfterUnnamedHerdr` RED, 8절 R2).
- #8128 착지 뒤: B2a의 "기존 detached pane exact 종료"와 B2b의 "새 incarnation launch → successor Ready"를 단계 4의 교체로 부른다(`CodexReadyAfterCompressedHerdr8128`·`CodexReadyAfterUnnamedHerdr8128` GREEN). 설계 정본은 #8128의 `docs/herdr-parity-restart.design.md`이고 이 base에는 없다. 조각 B는 거절까지만 넣고, 교체 연결은 #8128 착지 뒤의 작은 후속 조각으로 둔다.
- 자식 관측: B1의 `claude_tui/activity/processes.rs:19` `observe`는 `tmux::pane_pid`로 루트를 잡아(`:31`) herdr pane에서는 항상 `Unknown`이다. herdr는 `read_execution`(`session_host/herdr_gate.rs:758`)이 주는 실행 `ProcessStamp{pid, start}`(`db/dispatched_sessions/hosted_execution.rs:58`)의 pid를 루트로 삼아 같은 `ancestry(root, text)`(`processes.rs:53`)를 부르는 어댑터를 `codex_tui/readiness.rs` 쪽에 둔다. B1 파일에서 바뀌는 것은 `ancestry`의 가시성 한 줄이다.

`.zst`: 운영 Codex CLI(0.169.0)는 오래된 rollout을 `.jsonl.zst`로 바꾸고 원본을 지운다(#8051). 원본이 없고 같은 경로의 `.zst`가 있으면 `Unknown(SourceArchived)`로 구분해 로그에 남기고 단계 4로 간다. `.zst`를 풀어 읽지는 않는다. 살아 있는 TUI의 rollout이 압축되는지, 그 뒤 기록이 어디에 쓰이는지는 [U]다. **A1** 관찰(읽기 전용): 캡처 호스트의 plain 1,942개·`.zst` 5,882개에서 가장 새 `.zst`가 11-02, 가장 오래된 plain이 11-03이라 약 10~12일 지난 파일이 압축된다. 같은 세션의 plain과 `.zst`가 함께 있는 경우는 0이고, 가장 오래 산 TUI(4일 22시간)의 rollout은 plain이었다. 압축 나이보다 오래 산 세션이 없어 "살아 있는 rollout은 압축되지 않는다"를 확정하지 못했다. 바이너리에 `skipped_busy`·`skipped_not_cold` 계수가 있어 건너뛰는 조건이 있다는 것까지만 안다. R4는 문장 그대로 둔다.

### 남는 화면 판독과 실패 처리

| 남는 판독 | 좌표 | 판독 실패 시 |
|---|---|---|
| 새 실행의 첫 입력(herdr) | `herdr_turn/first_input.rs:135`·`:137` → `owned_composer`·`empty_composer` | 화면이 비어 있지 않고 모달이 안 읽히면 쓴다. capture를 못 얻으면 사유 있는 보류 뒤 다시 읽는다. 프로세스가 사라지면 `Gone`으로 재기동한다 |
| 사람 초안(5번 보류) | `composer_draft_with_start_in_pane` `composer_status.rs:247`, `active_composer_content_in_pane` `composer_content.rs:45`, `busy_inject/screen.rs:51`·`:101`·`:243` | 초안이 **읽혔을 때만** 보류한다. 못 읽으면(`ComposerContent::Unread`, capture 없음) source가 `Idle`인 한 전달하고 기존 "초안 보존 미확인" 알림을 낸다(`submission.rs:34` `notify_owned_draft`). owner 경로는 이미 이렇게 한다(`herdr_turn.rs:354`~`:366`) |
| 모달·배너(29) | `input.rs:1041`, `submission.rs:207`, `inline_banner.rs:8`~`:60` | 모달 문구가 읽혔을 때만 veto. 못 읽으면 veto 없음. 권한 모달은 launch 인자상 드물다(4절) |
| Enter 직전 자기 글 확인 | `tui_input/actor/gate.rs:98` `own_draft`, `tui_input/submission.rs:110`~`:116` | 지금처럼 `Indeterminate`로 끝낸다. Enter를 다시 보내지 않는다 |
| Enter 뒤 확인·재전송·취소 정리(1.6) | `input.rs:1188`·`:1197` → `:980`, `warm_followup/submit.rs:48`~`:104`, `input.rs:943`~`:971` | 자기 글이 **읽혔을 때만** 재전송·정리한다. 못 읽으면 `Unconfirmed`로 끝내고 Enter를 다시 보내지 않는다. 재전송을 위한 교체는 판정이 `Busy`면 하지 않는다(1.7) |
| pane 크기 | `host_input.rs:524`~`:535` → `submission.rs:83` | tmux `pane_size` 상태값. 그대로 |
| Codex busy_inject 초안(31) | `busy_inject.rs:206`, `busy_inject/screen.rs` | 바꾸지 않음 |
| 진단 표시(38) | `server/routes/agents.rs:360`·`:380`, `claude.rs:2103` | 판정에 쓰지 않음 |

## 3. 삭제 목록

조각 D에서, 아래 "판정 호출자"가 모두 2절 판정으로 옮겨진 뒤 지운다.

| 대상 | 판정 호출자 (옮긴 뒤 0) | 남는 호출자 | 처리 |
|---|---|---|---|
| `pane_looks_ready_for_codex_prompt` `input.rs:1246`, `_with_ansi` `:1290`, `recent_has_codex_compact_prompt` `:1465`, `recent_has_codex_active_turn` `:1450` | `herdr_turn.rs:319`, `herdr_row_end.rs:591`, `provider.rs:644`, `input.rs:432` | 없음 | 삭제. `:432`의 marker는 "초안 판독기가 활성 composer를 찾았는가"로 바꾼다. marker 소비자는 아래 표 |
| `CodexPaneBusySignal`·`Tracker` `input.rs:1309`~`:1379`, `normalize_codex_active_turn_marker` `:1396`, `codex_elapsed_time_component` `:1417` | `rollout_tail/handoff.rs:9` | 없음 | 삭제. 시험 `input.rs:2306`·`:2347` 삭제 |
| `PromptDraftPolicy::accepts`의 marker 요구 `input.rs:235`, `decide_rollout_ready_snapshot` `:254`, 폴링 루프의 `draft_policy.accepts` `:593`·`:683`·`:741` | 대기 함수 자신 | 초안 차단(`should_block_rollout_ready` `:240`)만 | 준비 인정 분기 삭제, 초안 차단 유지 |
| `composer_ready` `herdr_turn.rs:316` | `herdr_turn.rs:340` | 없음 | 삭제(source 대기로 대체) |
| `herdr_turn_in_progress`의 활성 표시 요구 `herdr_cancel.rs:14` | `codex_stop_delivery.rs:773` | veto 함수로 축소 | `recent_codex_active_turn_marker` import 삭제 |
| `submission_busy_in_pane` `submission.rs:248` | `submission.rs:180`, `tui_input/actor/gate.rs:199` | 없음 | 삭제(Enter 직전 `proof.current()`로 대체) |
| Codex 분기 `provider.rs:643`~`:648` | `tmux_turn_liveness.rs:166`, `idle_tmux.rs:228` | wrapper 준비 마커 분기 | Codex TUI 화면 판독만 삭제 |

marker 정의를 바꿀 때 영향을 받는 소비자(1.6의 전수에서 "판정"이 아닌 것)와 처리:

| marker 소비자 | 좌표 | 정의 교체 뒤 동작 | 조각 |
|---|---|---|---|
| Enter 뒤 확인 | `input.rs:901` → `:980` | 같다. "작업 중 표시가 보이면 초안 없음"만 사라지고, 그 경우는 rollout 길이 증가가 `Submitted`를 가른다 | D |
| 재전송 교체 | `warm_followup/submit.rs:48`~`:92` | rollout 길이 가드 그대로 + 판정이 `Busy`가 아님(1.7) | C |
| warm 재사용 결정 | `warm_followup.rs:364` `decide_warm_input` | 초안이 읽히면 `StrandedDraft`. 교체 전 확인은 `InputReadinessFailed`와 같다(1.7) | C |
| 취소 정리 | `input.rs:943`~`:971` | 자기 초안 조각이 읽히면 지운다 | D |
| provider 회복 진입 | `tui_followup.rs:151`~`:158` | 같다 | D |
| 진단 API·시간 초과 로그 | `agents.rs:384`·`:386`, `input.rs:1615` | 판정 값과 사유를 싣는다 | A |

증명 시험(조각 D, `warm_followup.rs`의 시험 묶음 `:1030`~`:1110`과 같은 fixture 형식): 세 pane에서 Enter 뒤 분류 결과를 수정 전/후 표로 고정한다.

| pane | rollout | 수정 전 | 수정 후 |
|---|---|---|---|
| 좁은 pane + 자기 글이 composer에 남음 | 자라지 않음 | 조각 D 착수 때 실제 값을 기록 | `RetrySafeDraft` |
| 작업 중 표시 + 초안 없음 | 자람 | 위와 같음 | `Submitted` |
| 작업 중 표시 + 자기 글이 composer에 남음 | 자라지 않음 | 위와 같음 | `RetrySafeDraft`이되 판정이 `Busy`라 교체 0회, "refusing replay"로 끝남 |

수정 전 칸은 이 설계 레그에서 시험을 돌리지 않아 비워 둔다. 조각 D는 코드를 바꾸기 전에 세 pane의 현재 결과를 먼저 고정한다. 뮤턴트: `:1294`의 작업 중 표시 거절만 뺀 marker(판정 확인 없음) → 셋째 pane에서 교체가 1회 일어나 RED.

`composer_status.rs`의 함수는 지우지 않는다. 옮긴 뒤 호출자 표는 다음과 같다.

| `composer_status.rs` 함수 | 지금 호출자 | 옮긴 뒤 호출자 | 용도 |
|---|---|---|---|
| `:46` `model_first_status`·`:12` `line_is_codex_model_first_status` | `:219`(모달), `:279`(초안 판독기) | 같음 | 초안·모달 위치 찾기 |
| `:145` `short_composer_footer` | `:204`, `:219`, `:282`, `:329` | 같음(`:204`는 남는 `recent_has_compact_composer` 안) | 초안·모달 |
| `:183` `pane_has_dim_composer` | `input.rs:1439` → `:433`(초안), `:1299`(준비) | `input.rs:433` | 흐린 placeholder는 초안이 아님 |
| `:197` `recent_has_compact_composer` | `input.rs:1473` → `input.rs:933`(초안), `herdr_cancel.rs:37`(veto), `input.rs:1466`(준비) | `input.rs:933`, `herdr_cancel.rs:37` | 초안·veto |
| `:208` `pane_has_interactive_modal` | `input.rs:1036` → `submission.rs:209`, `herdr_cancel.rs:15`, `input.rs:1258`·`:1295`(준비) | `submission.rs:209`, `herdr_cancel.rs:15` | 모달 veto |
| `:247` `composer_draft_with_start_in_pane` | `submission.rs:165`, `:184` | 같음 | 초안 판독(5번) |

준비·작업 중·/stop 판정에서 `composer_status`로 들어가는 호출은 0이 된다(DoD 3). Enter 뒤 확인·재전송·취소 정리는 판정이 아니라 자기 초안 판독으로 분류해 이 0의 범위 밖에 둔다(1.6).

시험·fixture: `composer_status.rs`의 준비 단언(`:458` `cut_113_column_idle_ansi_is_ready`, `:495`, `:506`, `:702`, `:718`, `composer_status_tier_tests.rs`)과 `input.rs`의 준비 분류 시험(`:2300`, `:2380`~`:2404`, `:2468`~`:2480`, `:2583`~`:2712`, `:2901`, `:2925`)은 판정 단언을 지운다. 같은 pane fixture는 "초안 판독"과 "화면과 무관하게 source 판정이 맞다"는 시험의 입력으로 남긴다. `:2778` `rollout_composer_ready_signal_beats_dead_pane_capture`는 "래치만으로는 준비가 아니다"로 뒤집는다.

## 4. 운영 전제

| 전제 | 코드 좌표 | 확정 범위 |
|---|---|---|
| Codex CLI 버전 | `claude_tui/hook_bundle.rs:695` `probe_codex_cli_version_with_path`, `:452` `codex_hook_capability` | [F] 0.157.1에서 SessionStart·UserPromptSubmit·Stop payload와 `turn_id` 일치를 실측. SessionStart는 첫 프롬프트와 함께 온다. 운영은 0.169.0이다(맥북·맥미니·WSL, 코디네이터 확인). [U] 0.169.0의 같은 payload. 조각 A의 첫 작업이 `tests/fixtures/hook_payload/codex-0.169.0.json` 캡처다(5절) |
| Stop 훅이 종료 증거라는 전제 | AgentDesk는 Codex Stop에 턴을 잇는 응답을 내지 않는다(`hook_server.rs:446`의 flush는 `provider == "claude"`만) | [C] AgentDesk 쪽. [U] 사용자층 `~/.codex/hooks.json`의 Stop 훅(moshi-hook, herdr)은 block하지 않는다. 판정은 "Stop 훅 뒤 같은 턴의 rollout이 더 자라면 증거 무효"로 이 전제에 기대지 않게 한다(2절). 확인: 조각 A |
| 유휴 Codex pane은 자식 프로세스가 0개다 | `claude_tui/activity/processes.rs:19` `observe`, `:53` `ancestry` | [U] 리뷰가 이 호스트의 e2e pane 두 개에서 읽기만으로 확인했다(0.169.0 launcher가 `process.execve`로 native codex가 된다). `execve`가 없는 Node에서는 node가 부모로 남아 항상 `Present`가 되고 단계 4가 열리지 않는다(죽이지는 않는다). 확인: 조각 C의 probe 시험 |
| rollout 기록 이름 | `rollout.rs:339`(`task_started`+`root_turn_id`), `:389`(`task_complete`·`turn_aborted`) | [C] 모르는 이름·이름 없는 종료는 `Unknown`으로 떨어져 대체 단계로 간다 |
| rollout 경로 | `rollout_tail/discovery.rs:16`·`:22`(CODEX_HOME), `tui_prompt_dedupe/runtime_binding/binding_access.rs:150` | [C] 판정은 binding이 가리키는 파일만 읽는다. cwd 최신 파일을 고르지 않는다 |
| 훅 설치 | `hook_bundle.rs:31`~`:40`(8종), `codex.rs:411` 스위치, `tui_session_launch.rs:289`(`--dangerously-bypass-hook-trust` 광고 필요), herdr는 override 없으면 launch 거절(`launch_policy.rs:204`~`:207`) | [C] tmux는 훅 없이도 뜰 수 있다. 그때 단계 2가 없고 3·4만 남는다 |
| 훅 수신 | `hook_server.rs:297` persist-before-ACK, `:321` `activity_accepted` | [C] binding 관측이 내구화된 훅만 증거로 쓴다 |
| herdr server | `herdr_gate.rs:758` `read_execution`, `session_host/herdr/contract.rs:179`·`:361` | [C] `Unreadable`은 `Unknown`이지 `Gone`이 아니다 |
| launch 인자 | `codex.rs:448`~`:454`(`--dangerously-bypass-approvals-and-sandbox`, readonly면 `--sandbox read-only`), `launch_policy.rs:155`~`:158`(`check_for_update_on_startup=false`), `:203`(`--no-daemon`) | [C] 일반 실행은 승인 모달이 없다. readonly 실행과 trust 창은 모달 veto가 맡는다 |
| 준비 대기 예산 | `input.rs:153`~`:163`, `config` `followup_prompt_ready_timeout_secs` `:188` | [C] 그대로. PostTurnHandoff 200ms 안에 파일 판독이 끝나야 한다(리뷰 항목) |

## 5. 구현 계획

새 trait·Core를 만들지 않는다. B1에서 다시 쓰는 것: 훅 ingress 연결 자리(`hook_server.rs:491`~`:511`의 `activity::note_hook` 옆, 같은 `activity_accepted` 조건), binding header·`published_at` 검증 방식(`claude_tui/activity/hooks.rs:17`~`:42`), 자식 프로세스 관측(`activity/processes.rs`의 `observe`와 `ancestry`).

| 조각 | 내용 | 주요 파일 | prod 추정 |
|---|---|---|---|
| A | **첫 작업: 0.169.0 fixture 캡처**(아래). `codex_tui/readiness.rs`: 판정·proof·floor(launch context 시각)·Stop/UserPromptSubmit 훅 증거(메모리, source 키). 훅 ingress 연결. `wait_until_codex_tui_input_ready_with_policy`를 판정 대기로 교체(초안 차단 유지). `owner_input.rs:823` `codex_read`, `tui_followup.rs:455`, Discord 사전 확인(`intake_turn.rs:1549`), PostTurnHandoff 연결. 화면 판정과 불일치 로그 | `codex_tui/readiness.rs`(신규), `codex_tui/mod.rs`, `input.rs`, `busy_inject/rollout.rs`(floor 인자), `hook_server.rs`, `owner_input.rs`, `tui_followup.rs`, `codex_observation.rs`, `intake_turn.rs`, fixture 1 | 9파일 / +560 −150 |
| B | herdr: `composer_ready`·`empty_composer`·`first_input::write`를 판정+초안 판독으로. `read_execution` 연결과 자식 관측 어댑터. 단계 4 조건에서 `PromptRefused::SourceUnprovable` 거절. `/stop` `deliver`의 진행 중 판정. 행 종료의 준비 확인 제거 | `codex/herdr_turn.rs`, `herdr_turn/first_input.rs`, `input/herdr_cancel.rs`, `codex_stop_delivery.rs`, `herdr_row_end.rs`, `readiness.rs`, `claude_tui/activity/processes.rs`(가시성 한 줄) | 7파일 / +380 −90 |
| C | **첫 작업: 이관 표 채우기와 재시도 상한의 끝 확인**(1.7의 [U]). turn bridge 재큐에 Codex 분기(`streaming_edit_text.rs:185`, `followup_requeue.rs:98`). tmux 제출 게이트: `judge_codex`·`submission_busy`·`admit_codex_prompt_under_lock`·`decide_warm_input`에서 준비·작업 중을 proof로. **1.7 표대로 사유별 교체 전 확인**(`absence_launch.rs:274`는 `InputReadinessFailed`·`StrandedDraft`만, `warm_followup/submit.rs:68`은 `Busy`만)과 `Deferred` 반환. 초안 미판독 정책. pending-tool probe 교체. `provider.rs` Codex 분기. 단계 4 연결과 자식 probe 시험 | `tui_input/actor/gate.rs`, `tui_input/submission.rs`, `input/submission.rs`, `input.rs`, `warm_followup.rs`, `warm_followup/submit.rs`, `codex/absence_launch.rs`, `rollout_tail/handoff.rs`, `rollout_tail.rs`, `following_source.rs`, `provider.rs`, `tmux_turn_liveness.rs`, `discord/turn_bridge/streaming_edit_text.rs`, `discord/turn_bridge/followup_requeue.rs`, `followup_requeue/attempt.rs` | 15파일 / +600 −270(`discord/turn_bridge/streaming_edit_text.rs`, `followup_requeue.rs` 포함) |
| D | 3절 삭제, marker 정의 교체와 Enter 뒤 확인 3 pane 시험, 시험·fixture 정리, 불일치 로그 제거 | `input.rs`, `input/submission.rs`, `composer_status.rs`(시험), `herdr_cancel.rs`, `warm_followup.rs`(시험), `warm_followup/submit.rs` | 6파일 / +150 −520 |
| B'(#8128 착지 뒤) | herdr 단계 4의 거절을 B2a exact 종료 → B2b 새 incarnation으로 교체 | `codex/herdr_turn.rs`, `readiness.rs` | +120 |
| E(선택) | 29번: PermissionRequest 훅 소비. payload fixture에 turn·tool 식별자가 있을 때만 | `readiness.rs`, `hook_server.rs` | +180 |

조각마다 30파일·+1,500 상한 안이고 +1,200 정지선 아래다. A가 +900을 넘을 조짐이면 "판정 모듈+훅 연결"과 "소비자 연결"로 나눈다. C가 +900을 넘을 조짐이면 "교체 전 확인 + `Deferred` 재큐"와 "제출 게이트 + probe 교체"로 나눈다. 순서는 A → B·C(서로 독립) → D다. D는 A~C가 운영에서 불일치 로그 0을 보인 뒤 착수한다.

**조각 A의 첫 작업 — 0.169.0 fixture**: `tests/fixtures/hook_payload/codex-0.169.0.json`을 0.157.1과 같은 스키마로 캡처하고, fixture 로드 시험이 아래를 단언한다.

| 확인 항목 | 단언 | 어긋나면 |
|---|---|---|
| Stop의 `turn_id` == 같은 턴 rollout `task_complete.turn_id` | 같다 | 멈춤 조건 3 |
| UserPromptSubmit의 `turn_id` == 뒤따르는 `task_started`의 턴 | 같다 | Busy (b)를 "훅 뒤 첫 `task_started`"로 약화 |
| rollout 헤더 시점과 SessionStart 시점 | 헤더는 launch, SessionStart는 첫 프롬프트 | 2절 5의 문구와 floor 2순위를 고친다 |
| `/clear` 뒤 session id·rollout 파일·SessionStart(`clear`) 시점 | 0.157.1과 같다 | 1.3 clear 행을 고친다 |
| 레코드 최상위 `timestamp` | 모든 레코드에 있다 | floor 1순위를 2순위로 내린다 |
| 오류로 끝난 턴의 rollout 꼬리와 Stop 훅 | 이름 있는 종료 또는 Stop 훅이 있다 | 단계 3·4로 가는 빈도 추정에 반영 |
| 살아 있는 TUI의 rollout이 `.zst`로 압축되는가(읽기 전용 관찰) | 압축되지 않는다 | R4를 설계 대상으로 올린다 |

**A1 결과**([F] `codex-0.169.0.json`, 로드 시험 `codex_0_169_hooks_and_rollout_records_name_the_same_turns`): 1행 같다(Stop 2건). 2행 같다. 다만 `task_started`가 훅보다 먼저 쓰인다(위 Busy (b) A1). 3행 어긋남: 파일이 launch가 아니라 첫 프롬프트 때 생긴다(2절 5 A1). 4행 0.157.1과 같다. 5행 모든 레코드에 있다. 6행 중단된 턴은 `turn_aborted`, 모델 오류 턴은 `task_complete`로 이름 있는 종료가 있고 두 경우 모두 Stop 훅은 없다. 7행 미확정(2절 `.zst` A1). 멈춤 조건 3·4는 해당 없다.

**조각 A의 과도기 규칙**: 판정이 `Idle`·`Busy`·`Gone`이면 판정이 정하고, `Unknown`이면 지금의 화면 판독(준비 대기)·느슨한 판독(진단)·session id 판독(owner)이 정한다. 단계 3·4와 교체 전 확인이 조각 C라서, 그 전에 `Unknown`을 "준비 아님"으로 두면 화면이 읽히는 pane의 `.zst`·unbound 입력이 45초 대기와 교체로 퇴행한다. 조각 C가 단계 4를 넣으면서 이 대체를 지운다. 같은 이유로 조각 A만으로는 lock 안 제출 확인(`admit_codex_prompt_under_lock`)이 아직 화면을 요구한다.

**조각 C 메모**(d3 델타 리뷰 P2-1·P2-2): Enter 전 거절의 분류와 `Deferred` 재시도의 시간 상한은 조각 C의 첫 작업에서 정한다.

멈춤 조건: (1) #8128이 착지하기 전에는 herdr 단계 4를 거절까지만 넣는다. 교체 연결(B')은 #8128의 B2a·B2b가 main에 있을 때만 착수한다. (2) 조각 D가 5번(사람 초안) 시험의 기대값을 바꿔야 하면 멈춘다. (3) 0.169.0 fixture에서 Stop 훅에 `turn_id`가 없거나 rollout과 다르면 단계 2를 빼고 3·4만으로 진행할지 코디네이터에 묻는다. (4) 훅 주입 없는 실행에 launch context가 없으면 floor를 2순위로만 쓸지 묻는다.

### legacy 준비 대기 호출자 → owner 경로 이관 가능 여부 (틀)

리뷰의 새 방향 제안은 tmux legacy 준비 대기와 45초 교체를 새 판정으로 다시 잇지 말고, 이미 rollout으로 판정하는 owner 경로(`owner_input.rs:767` `native_wait`, `input.rs:1097` `admit_owned_codex_prompt`)로 모두 보내 조각 C를 삭제 조각으로 바꾸자는 것이다. **지금은 갈아타지 않는다.** 조각 C 착수 전에 아래 표를 채우고, 모든 행의 마지막 열이 "포함"이면 C를 삭제 조각으로 바꾸는 후속을 제안한다. 한 행이라도 "미포함"이면 이 설계의 C를 그대로 한다.

| 호출자(legacy 준비 대기·제출) | 좌표 | 입력 출처(1.5) | owner scope(`owner_input::active()`) 포함 여부 | 근거 좌표 |
|---|---|---|---|---|
| `try_codex_tui_warm_followup` | `warm_followup.rs:542` ← `absence_launch.rs:235` | Discord 메시지(유휴) | 조각 C 착수 전 확인 | |
| ↳ 이 행이 "포함"이면 사라지는 것 | `warm_followup.rs:423`~`:617`, `warm_followup/submit.rs`, `absence_launch.rs:234`~`:262` | — | legacy warm 경로와 함께 eligibility fallback 세 사유, `InputReadinessFailed`·`StrandedDraft`·`SubmitFailed`, 두 `Replacement::begin` 호출과 1.7의 교체 전 확인·`Deferred` 계약이 모두 삭제 대상이 된다. 남는 것은 `LegacyPath`의 세션 정리뿐이다 | 1.7 |
| Discord 사전 대기 | `intake_turn.rs:1576` | Discord 메시지(유휴) | 조각 C 착수 전 확인 | |
| PostTurnHandoff(200ms) | `codex/tui_session_launch.rs:434` | 턴 종료 직후 넘김 | 조각 C 착수 전 확인 | |
| API·health 전달 | `discord/health/turn_deliver/inject.rs:329`·`:944` | API·health | 조각 C 착수 전 확인 | |
| 루틴·예약·HTTP 시작 | `owner_input.rs:847`~`:852` | 루틴·예약·HTTP | 포함(이미 owner) | `owner_input.rs:835` `start_idle` |
| lock 안 제출 | `input.rs:1066` `admit_codex_prompt_under_lock` | 위 legacy 호출자 전부 | 조각 C 착수 전 확인 | |
| owner 제출 게이트 | `tui_input/actor/gate.rs:84` `judge_codex` | owner | 포함(이미 owner) | `gate.rs:84` |

### 시험 계획

| DoD | 시험 (실제 진입점) | 수정 전 | 수정 후 |
|---|---|---|---|
| 1 좁은 pane·버전별 상태줄 | `composer_status.rs` 시험의 46열·113열·0.166 상태줄 pane과 새로 캡처한 0.169.0 상태줄 pane, 빈 pane, 반대 상태를 그린 pane을 spy transport(`host_input::spy`)에 넣고 rollout fixture(`task_started`/`task_complete`)를 준 뒤 `wait_until_codex_tui_input_ready`·`try_codex_tui_warm_followup`·herdr `followup` 호출 | 안 읽히는 pane에서 시간 초과(RED) | 화면과 무관하게 Ready/Busy가 rollout대로 |
| 1 | 같은 pane + 열린 턴 rollout → 입력 0, 턴 종료 기록 추가 → 한 번 전달 | — | 전달 수 = 1 |
| 2 herdr /stop | `codex_stop_delivery_tests`: 활성 표시가 없는 pane + 자기 턴 Open rollout → Esc 1회 | `Idle`로 거절(RED) | 전송 |
| 2 | 활성 표시가 남은 pane + 종료된 rollout → Esc 0, `Idle` | Esc 전송(RED) | 0 |
| 3 | `composer_status` 판정 호출자 0: 삭제 뒤 컴파일 + 호출자 표 갱신 | — | 표 |
| 신호 유실·늦음 | 종료 기록 지연 뒤 추가, 종료 기록 없음+Stop 훅, 이름 없는 종료, `.zst`로 교체, 재시작(증거 맵 비움), 프로세스 종료 | — | 각각 한 번 전달 또는 형식 있는 대체 경로 1회, 무한 보류 0 |
| 재개 floor | 끊긴 턴이 든 rollout을 재개한 실행 → 준비. 재시작 시뮬레이션(메모리 증거 비움) 뒤에도 같다 | Open으로 보류(RED) | Ready |
| 시간 초과 교체(P1-2) | `absence_launch` 시험 묶음(`try_codex_tui_warm_followup` 진입점. Discord 사전 확인을 거치지 않는 호출): rollout Open + pane 읽힘 + 45초 경과 | `Replacement::begin` 1회(RED) | `Replacement::begin` 0회, `Deferred` 1회, pane·세션 파일 그대로 |
| 사유 × 판정(P1-N1) | 같은 진입점: `LaunchOptionsChanged` + `Idle` | 교체 1회, 전달 1회 | 같다(교체 1회, 전달 1회) |
| 사유 × 판정 | `LaunchOptionsChanged` + `Busy`, `RolloutBindingMismatch` + `Idle`, `RuntimeKindMismatch` + `Idle`, `SubmitFailed`(Enter 전 실패) + `Idle` | 각각 교체 1회 | 같다(확인 없음. 지금 동작 고정) |
| 사유 × 판정 | `InputReadinessFailed` + `Unknown`(`.zst`): 600초 전 / 600초 뒤 자식 없음 / 자식 판독 실패 | 모두 교체 1회 | `Deferred` / 교체 1회 / `Deferred` |
| 사유 × 판정 | `StrandedDraft` + `Idle` / `StrandedDraft` + `Busy`(작업 중 표시 + 초안이 읽히는 pane) / `StrandedDraft` + `Unknown`(600초 전) | 교체 1회 / (지금 marker로는 `InputReadinessFailed`로 45초 뒤 교체) / 교체 1회 | 교체 1회 / `Deferred` 1회 / `Deferred` 1회 |
| `Deferred`의 재큐 | turn bridge 진입점: `Deferred` → 입력이 큐 앞에 다시 들어가고 다음 시도가 한 번 돈다 | — | 재큐 1회, 유실 0, 중복 0 |
| `Deferred`와 재시도 상한 | `Unknown`으로 미룬 입력: 600초 전에 시도가 7번 넘게 일어나도록 시계를 돌림 | — | 입력이 버려지지 않는다. 600초 뒤 교체 1회, 전달 1회 |
| 시간 초과 교체 | 같은 진입점: rollout Open + readonly launch 옵션(busy 경로 불가) + 600초 정지 + 자식 없음 | 교체 1회(RED) | 교체 0회, 사유 있는 보류 1회 |
| Enter 구간(P2-5) | UserPromptSubmit 훅 기록 뒤 `task_started`가 아직 없는 rollout(앞 턴 `Ended`) → 준비 대기 | 화면이 읽히면 대기, 안 읽히면 시간 초과 | `Busy`, 키 0. `task_started`·종료 기록을 추가하면 한 번 전달 |
| UserPromptSubmit 해제(P2-B) | UserPromptSubmit → `task_started` → 모르는 이름의 종료, Stop 훅 없음, 600초, 자식 없음(tmux) | — | 교체 1회, 전달 1회 |
| UserPromptSubmit 만료 | UserPromptSubmit 뒤 `task_started` 없음, rollout `Ended` 그대로, 600초, 자식 없음 | — | 교체 0회, `Idle`로 전달 1회. 600초 전에는 키 0 |
| 느슨한 판독(P2-A) | `tui_turn_state` 시험: 모르는 `event_msg` 종료 뒤 `observe_codex_jsonl_turn_state` | `Streaming`(지금 동작 고정) | 새 판정은 `Unknown` |
| Stop 훅 무효화(P2-6) | Stop 훅 뒤 같은 턴의 `item_completed` 추가 | — | `Busy` |
| herdr 단계 4(P2-1) | herdr spy: rollout `.zst` + 600초 경과 + 자식 없음 | 화면 시간 초과 | `SourceUnprovable` 거절 1회, 키 0회 |
| herdr 자식 관측 | `read_execution`의 pid를 루트로 한 ps fixture: 자식 있음 / 없음 / ps 실패 | — | `Present` / `NoneObserved` / `Unknown`, `Unknown`이면 거절·교체 0 |
| 자식 전제(P2-7) | 운영 읽기 전용 probe: 유휴 Codex pane에서 `observe` | — | `NoneObserved`. 아니면 단계 4를 "마지막 `Idle` 증명 때의 자식 집합과 같다"는 비교로 바꾼다 |
| Enter 뒤 확인(P1-1) | 3절의 세 pane 표 | 착수 때 기록 | 표대로 |

원인 뮤턴트(각각 해당 시험이 RED가 되어야 한다): 래치만으로 준비 인정, 프로세스 확인 제거, floor 제거(0으로 고정), 첫 키 직전 재확인 제거(barrier로 사람 턴을 끼워 넣음), Stop 훅의 `turn_id` 비교 제거(다른 턴의 훅으로 준비됨), 자식 확인 제거, /stop에 화면 표시 요구 복원, 교체 전 판정 확인 제거(`InputReadinessFailed`·`StrandedDraft`·재전송 각각), `Unknown`으로 미룬 입력이 재시도 횟수를 쓰게 하기(상한 시험이 RED), 확인을 "Gone 또는 단계 4"로 모든 사유에 적용(`LaunchOptionsChanged` + `Idle` 행이 RED), 단계 4를 `Open`에도 열기, UserPromptSubmit 조건 제거, UserPromptSubmit을 `task_started`에서 풀지 않기(P2-B 첫 행이 교체 0회로 RED), 만료 제거(둘째 행이 전달 0회로 RED), Stop 훅 뒤 rollout 증가 확인 제거.

필터별 `--list` 선택 수 = 통과 수, skip 0. 구현 조각은 `CodexReadiness` cfg를 저장소 checker로 다시 돌려 전/후 표를 보고서에 적는다.

### 운영 관측

- 구조화 로그: `codex_ready_verdict`(idle/busy/unknown/gone), `tier`(level/hook/busy_route/reclaim), `why`(source_archived, torn, unnamed_end, no_source, probe_error), `floor`, `turn_id`.
- 조각 A~C 동안 화면 판정과 source 판정이 다르면 `codex_ready_disagreement`를 남긴다. D 착수 조건이다.
- 대체 단계 3·4는 사용할 때마다 info 로그와 카운터를 남긴다. 단계 4가 하루 1회를 넘으면 신호 유실 원인을 조사한다.
- 기존 시간 초과 로그(`input.rs:1605` `log_prompt_ready_timeout`)는 판정 사유를 싣는 형태로 유지한다.

## 6. 리뷰 집중 항목

1. **경계 시각**: 사람의 Enter와 `task_started` 기록 사이, 턴 종료와 종료 기록 사이. `proof.current()`가 composer lock 안에서, 첫 키와 Enter 직전 두 번 도는가.
2. **동시성**: 훅 증거 맵과 대기 루프의 lock 순서. source authority lock(`tmux_common::with_tmux_source_authority`)을 쥔 채 파일을 읽지 않는가. PostTurnHandoff 200ms 안에 끝나는가.
3. **옛 형식 복원**: 훅 없는 실행, marker 없는 floor, `turn_id` 없는 옛 rollout, 훅 미주입 resume. 각각 `Unknown`으로 떨어지고 대체 단계가 도는가.
4. **문서로만 선언한 가드**: 래치를 준비로 쓰지 않는다는 것, Stop 훅 `turn_id` 일치, 자식 확인, floor가 코드 조건과 원인 뮤턴트로 고정됐는가.
5. **잘못된 0/PASS**: 시험이 화면 spy가 우연히 맞는 pane을 돌려줘서 통과하지 않는가(반대 상태 pane으로 확인). rollout fixture가 실제 0.169.0 기록 모양인가.
6. 다른 턴·다른 실행의 Stop 훅이 현재 턴을 끝낸 것으로 읽히지 않는가(nonce·transcript_path·turn_id 셋 다 비교).
7. 단계 4가 살아 있는 턴을 죽이지 않는가. 자식 판독 `Unknown`에서 교체가 일어나지 않는가. 교체 전 확인이 1.7 표의 사유에만 걸리고, 표에서 "확인하지 않는다"인 사유의 교체가 그대로 도는가. 표에 없는 사유나 호출자가 생기지 않았는가(`git grep Replacement::begin`, `CodexWarmFallbackReason::`, `cleanup_existing_codex_tui_session`).
8. 초안을 못 읽었을 때 전달하는 정책이 사람 초안이 **읽힌** 경우의 보류를 바꾸지 않는가.
9. 첫 입력의 축소된 화면 확인이 셸 화면(launch 직후 exec 전)을 composer로 받아들이지 않는가. 프로세스 전경 실행 확인과 함께 도는가.
10. marker 정의를 바꾼 뒤 1.6의 소비자가 표의 "옮긴 뒤"대로 동작하는가. 1.6에 없는 소비자가 생기지 않았는가(`git grep composer_marker_detected`).
11. floor의 시각 비교가 launch 직전·직후 경계의 레코드를 잘못 가르지 않는가(같은 초 안의 `task_started`).
12. `Deferred`가 pane·세션 파일·binding을 건드리지 않고 돌아오는가. 다시 큐에 들어간 입력이 유실·중복 없이 한 번 더 시도되는가. 재시도 상한에 닿았을 때의 끝이 형식 있는 응답인가.
13. UserPromptSubmit 증거가 `task_started`·종료 신호·만료 셋 중 하나로 반드시 풀리는가. 만료 가드(600초 + 자식 없음)가 단계 4와 같은 값을 쓰는가.

## 7. 범위 밖

- 사람 초안 판독 자체(5번)와 Codex busy_inject 초안(31번).
- tmux /stop(`judged_stop.rs`·`session_stop.rs`): 1.1의 판독 함수를 부르지 않는다.
- Claude 경로 전부(#8080·#8081).
- `.zst` 해제 판독, wrapper·Qwen·Gemini 준비 판정(34번).

## 8. 알려진 잔여

모델이 RED로 보여 주는 넷과 그 밖의 잔여다.

- **R1 열린 턴의 보류**(`CodexReadyAfterLostBothHeld`): readonly 실행처럼 busy 경로를 쓸 수 없는 pane에서 종료 기록과 Stop 훅이 모두 유실되면, 그 턴은 계속 열린 것으로 읽히고 입력은 사유 있는 보류로 남는다. 신호만으로는 "끝났는데 기록이 없다"와 "조용히 실행 중이다"를 가를 수 없다. 자동으로 풀면 실행 중인 턴을 죽인다(`CodexReadyReclaimOpen`). 풀리는 길은 프로세스 종료(`Gone`), 사람의 /stop·재시작, 그 pane에서 사람이 새 턴을 여는 것이다. 이슈 DoD의 "영구 보류 0"과 어긋나는 한 점이라 **코디네이터 결정 항목**으로 올린다. 대안은 (가) 이 설계대로 보류 + 알림, (나) 상한(예: 60분) 뒤 자식 없음 조건으로 교체다. 권고는 (가)다. 죽인 턴은 되돌릴 수 없고 보류는 사람이 풀 수 있다.
- **R2 herdr의 `Unknown`**(`CodexReadyAfterCompressedHerdr`, `CodexReadyAfterUnnamedHerdr`): #8128 착지 전까지 600초 뒤 거절로 끝난다. 착지 뒤 B'로 닫는다.
- **R3 Enter 구간의 증거 유실**(`CodexReadyAfterEnterRestart`, 훅 없는 실행 포함): 사람의 Enter와 `task_started` 기록 사이에 dcserver가 재시작하거나 훅이 주입되지 않은 실행이면, 그 구간에 Idle 경로 키가 실행 중 턴에 들어갈 수 있다. Codex가 큐에 넣으므로 유실은 아니고 턴 귀속이 틀어진다. 구간은 `task_started`가 쓰일 때까지다. Enter 직전 `proof.current()`가 한 번 더 줄인다. **A1**: 0.169.0에서는 훅이 시작 기록보다 늦어 이 구간을 덮지 못한다(`CodexReadyAfterEnterGapSilent`). 구간은 약 13ms이고 시작 기록이 닫는다. 훅이 주입된 실행에서도 같다.
- **R4 살아 있는 턴의 rollout 압축**(`CodexReadyAfterArchivedSilentTurn`): rollout이 `.zst`인 채로 사람이 턴을 열고, 그 턴이 자식 없이 600초 넘게 조용하면 UserPromptSubmit 증거가 만료되고 단계 4가 실행 중인 턴을 죽인다. d2에서는 증거가 종료 신호 전까지 남아 재시작이 겹쳐야만 일어났다. d3는 그 증거가 만드는 영구 보류를 없애는 대신 이 잔여가 재시작 없이도 열린다. 살아 있는 TUI의 rollout이 압축되는지가 [U]다. A1 관찰에서도 확정하지 못했다(2절 `.zst`). 압축되지 않는다면 일어나지 않는다. 압축된다면 `.zst`의 mtime·크기 변화를 "조용함" 판독에 넣는다.
- **R5 준비 판독과 무관한 교체**(`CodexReadyBeforeTmuxConfigPerson`은 수정 전 RED): launch 옵션 변경·binding 불일치·Enter 전 전달 실패·`LegacyPath` 정리는 지금처럼 실행 중 턴을 확인하지 않고 pane을 끝낸다(1.7). Discord 입력은 사전 확인이 `Busy`를 걸러 주므로, 남는 경우는 사전 확인과 교체 사이에 턴이 시작된 때와 사전 확인을 거치지 않는 호출자다. 이 설계가 만든 것이 아니라 그대로 둔 것이다. 닫으려면 이 사유들에도 "`Busy`면 `Deferred`"를 걸면 되고(리뷰가 권한 형태), 그때는 "옵션이 바뀐 pane에서 열린 턴의 종료 신호가 유실되면 보류"가 R1에 더해진다. 코디네이터 결정 항목이다.
- `/stop` 늦은 기록 창(2절, `CodexStopAfterLateNoHook`). 유휴 composer에 Esc 한 번. 화면 판독과 같은 수준이며 훅이 있으면 닫힌다.
- 단계 4는 600초를 기다린다. 그동안 입력은 사유가 붙은 보류로 남는다.
- dcserver 재시작은 메모리의 Stop·UserPromptSubmit 훅 증거를 지운다. 그 뒤에는 단계 3·4에 기댄다(`CodexReadyAfterRestart`는 busy 경로가 있어 GREEN이고, 없으면 R1이다).
- 부팅 직후에는 묶인 source가 없어 첫 입력(herdr)에 축소된 화면 확인이 남는다. Codex가 부팅 시점 훅을 내는 버전이 확인되면 그 훅으로 바꾼다.
