# #8201 herdr 입력 보류 표지: 불확실 Enter 뒤 실행이 살아 있는 동안 안전하게 푸는 판정

- 코드 기준: `934a49997e95ffb0d54ae8eed2d7653ea6c1e4bf` (origin/main, 2026-11-16). 읽기 전용 조사이며 코드·시험은 바꾸지 않았다.
- 상태 차트: [herdr-input-hold.report.md](../statecharts/input-single-path/herdr-input-hold.report.md), 모델 `docs/statecharts/input-single-path/HerdrInputHold.tla`.
- 운영 표지 파일(`runtime/herdr_input_holds/bf28830d9ac94338bbe904c8a0c4eb0a`)은 그대로 둔다. 지우는 일은 사용자 승인 사항이다.

## 0. 요약

- 11-16 18:05:15 KST의 `Indeterminate { confirmed: 1, cause: Refused(Herdr(PaneUnverified)) }`는 "붙여넣기는 들어갔고 **Enter는 게이트가 보내기 전에 거절했다**"는 뜻이다(`host_input.rs` `admit_after`: 확인된 키가 1개 이상일 때 거절이 `Indeterminate`가 된다). Enter는 나가지 않았으므로 원 입력은 입력창에 붙여넣은 채로 남아 있다. "불확실"이라는 이름과 달리 이 경우의 실제 상태는 확정적이다.
- 원 입력은 `WriteUncertain`로 retained 처리되어(18:05:52 retained_input_notice) 다시 보내지지 않는다. 그런데 표지는 `composer_settled`일 때만 지워지고, 그 밖에는 실행이 끝날 때만 풀린다. Bound 실행이 살아 있는 동안 다음 시작은 모두 `not_held`에서 거절된다. 거절은 Opaque start failure이므로 2회 뒤 retained가 된다(`queued_head/v2/start_failure.rs` `start_failed`의 `retain` 규칙).
- 설계: 표지를 **읽는 곳에서 판정한다**. 근거는 transcript 기록, 입력창 판독, 표지에 새로 기록하는 입력 식별자다. 남은 글이 우리 붙여넣기라는 것이 증명되면 게이트를 거쳐 지운 뒤 해제한다. 비어 있는 것이 증명되면 해제한다. 판정할 수 없으면 보류를 유지하되, 입력을 retained로 넘기지 않고 깨움 조건과 운영자 표시를 둔다.
- 불확실 자체도 줄인다. `PaneUnverified`는 Enter 직전 게이트가 pane 프로세스를 대기 0초로 한 번 읽다가 실패해서 나온다. 같은 증명을 짧은 창(이미 쓰는 `CONFIRM_WINDOW` 2초) 안에서 다시 읽으면 부하 중 일시 실패를 흡수할 수 있다.
- 상태 차트: 수정 전 cfg는 liveness 위반(RED: `HoldEventuallyReleased`·`HoldReleasedOrSurfaced`·`LaterInputStarts`)이고, 설계안 cfg는 통과(GREEN)다. 음성 대조군 6개가 각 안전 규칙을 깬다.

## 1. 진입점 전수표

표지 = `runtime/herdr_input_holds/<execution_nonce>`. 파일 내용은 RFC3339 시각 한 줄뿐이다. 어느 입력의 보류인지 알 수 있는 정보는 없다. 경로를 만드는 곳은 `src/services/claude/herdr_turn.rs` `hold_path`이다(Codex도 같은 함수를 쓴다).

### 1.1 쓰기 (W)

| # | 위치 | 상태 변화 |
|---|---|---|
| W1 | `src/services/claude/herdr_turn/first_input.rs:90` `write` → `herdr_turn.rs` `hold` | Claude 턴 시작(Bound·Pending·새 launch, owner 경로 `submit_owned`와 legacy `submit` 공통). 붙여넣기 전에 기록하고 fsync한다. "입력이 입력창에 있을 수 있음"으로 바뀐다 |
| W2 | `src/services/codex/herdr_turn.rs:622` `followup_write` → `hold` | Codex Bound 후속 입력. W1과 같다 |
| W3 | `src/services/codex/herdr_turn/first_input.rs:147` → `hold` | Codex 첫 입력(launch·Pending 뒤). W1과 같다 |

### 1.2 지우기 (C)

| # | 위치 | 상태 변화 |
|---|---|---|
| C1 | `first_input.rs` `write`의 `composer_settled(&run)` 뒤 `remove_file` (`herdr_turn.rs` `composer_settled`: `Applied`·`Refused`·`Cancelled{confirmed:0}`) | 제출 완료, 또는 키가 하나도 안 나감 → 해제. `Indeterminate`·`Cancelled{confirmed>0}`이면 **남는다**(결함의 출발점) |
| C2 | `first_input.rs` `observed_input` 결과 `Closed` → `src/services/provider/herdr_before_start.rs` `release_claude_hold` | 첫 키 전에 멈춘 턴 → 해제 |
| C3 | `src/services/codex/herdr_turn.rs:647` (`Applied` 또는 `composer_untouched`) / `release_closed_hold` | Codex 후속 입력이 제출됐거나 입력창을 건드리지 않음 → 해제. 나머지는 남는다 |
| C4 | `src/services/codex/herdr_turn/first_input.rs:158·167·220` | Codex 첫 입력: 닫힘·건드리지 않음 → 해제. 제출 뒤에는 `observe`가 session start를 찾고 bind한 다음에만 해제한다. `SOURCE_WAIT` 안에 session start가 없으면 **남는다**("later prompts stay held") |
| C5 | `src/services/discord/herdr_terminate.rs:321` `settle_and_release` | 종료 명령: retire CAS 뒤 해제 |
| C6 | `src/services/discord/recovery_engine/herdr_row_end.rs:495` `commit` | 행 끝 처리: 실행이 끝난 행을 retire한 뒤 해제 |
| C7 | `src/services/discord/recovery_engine/herdr_row_end_live.rs:72` | 살아 있던 턴의 실행이 사라짐: retire 뒤 해제 |
| C8 | `src/services/herdr_launch/retired_hold.rs:79` `release` ← `src/services/herdr_launch.rs:446` `before_fresh` | 새 incarnation: retired 실행의 표지를 후계 launch 전에 해제 |
| C9 | `src/cli/herdr.rs:294` `release_left_hold` | `agentdesk herdr retire`: 이미 Retired인데 표지가 남은 행 |
| C10 | `src/cli/herdr.rs:330` `retire_on_reading` | `agentdesk herdr retire`: pane에서 실행 종료가 보일 때만 retire 뒤 해제 |

C5~C10은 모두 **실행이 끝났을 때만** 동작한다. 실행이 살아 있는 동안 표지를 푸는 경로는 없다.

### 1.3 읽기 (R)

| # | 위치 | 상태 변화 |
|---|---|---|
| R1 | `src/services/claude/herdr_turn.rs:264` `execute` Bound → `not_held` | 표지가 있으면 턴 시작을 거절한다(`input held after an unclear prompt`). 거절은 Opaque start failure로 처리되어 2회 뒤 retained가 된다 |
| R2 | `src/services/claude/herdr_turn.rs:268` `execute` Pending → `not_held` | R1과 같다 |
| R3 | `src/services/codex/herdr_turn.rs:153` Bound → `not_held` | R1과 같다(Codex) |
| R4 | `src/services/codex/herdr_turn.rs:157` Pending → `not_held` | R1과 같다(Codex) |
| R5 | `src/services/claude/herdr_return.rs:69` | 표지가 있으면 tmux 복귀를 거절한다 |
| R6 | `src/services/discord/recovery_engine/herdr_reader.rs:249` | 재시작 재연결 패스에서 표지 수를 센다(health 표시, 판정 없음) |
| R7 | `src/server/routes/health_api/runtime_profile.rs:74` | health `input_holds` 표시 |
| R8 | `src/cli/herdr.rs:363` `status` | 실행별 `input_hold.recorded_at`, 행이 없는 표지는 `other_input_holds`로 표시 |

**표지를 읽지 않는 곳**: Claude busy 주입(`src/services/claude_tui/host_input/busy/herdr.rs` `Adapter::submit`)과 Codex busy 주입. 그래서 작업 중 입력은 영향을 받지 않는다(이슈 관찰과 일치). 대신 busy 주입은 사람 초안을 C-s로 stash하고 그 위에 붙여넣으므로, 표지가 가리키는 우리 붙여넣기가 stash될 수 있다. 판독기는 stash가 보이면 `Unread`로 본다(`busy_inject/ownership.rs` `composer_owner_for`).

전수: 쓰기 3 + 지우기 10 + 읽기 8 = **21개 진입점**(시험 전용 호출 제외).

## 2. 불확실 판정의 원인 (`PaneUnverified`)

1. `host_input.rs` `run_plan`은 동작마다 게이트를 통과시킨다. `PasteBuffer`는 admit → `load_buffer` → admit → `paste_buffer`(확인 1)이고, `Enter`는 admit → `send_keys`(확인 2)다.
2. 게이트 `src/services/session_host/herdr_gate.rs` `judge`는 변경마다 marker → policy witness → `verify_pane`을 다시 수행한다. `verify_pane`은 `verify_pane_within(witness, Duration::ZERO)`, 즉 **재시도 없이 한 번** `pane.process_info`와 OS 프로세스 판독(`pane_probe::probe`)을 한다.
3. 판독 실패 원인 `EvidenceGap`(`src/services/session_host/herdr/pane_probe.rs`) 중 `ServerChanged`·`NonceMissing`·`OtherNonce`를 뺀 나머지(`ReadFailed`·`ProcessUnreadable`·`NoneYet`·`Ambiguous`·`EnvUnreadable`·`Unreported` 등)는 모두 `PaneUnverified`로 접힌다(`verify_pane_within`의 `_ => HerdrGateRefusal::PaneUnverified`).
4. 부하 150대에서는 herdr 소켓 응답 지연(`ReadFailed`)과 `ps` 계열 판독 실패(`ProcessUnreadable`)가 일어나기 쉽다. 붙여넣기 admit은 수백 ms 전에 같은 판독을 통과했으므로 일시 실패일 가능성이 높다. 다만 **어떤 gap이었는지는 운영 로그로 확인되지 않는다**: 입력 게이트는 `tracing::debug!`로만 남긴다(현재 `dcserver.stdout.log`에 "pane unverified" 0건). 18:05~19:06 사이 같은 실행에 거절이 계속 기록된 것(09:32, 10:06 UTC)으로 보아 실행 자체는 살아 있었다.
5. 이미 일시 실패를 흡수하는 장치가 있다. `HerdrTarget::execution_confirmed`는 `CONFIRM_WINDOW`(2초) 동안 같은 서버에서 다시 읽는다. 각 시도가 독립적인 완전한 증명이다.

**Enter 직전 재확인으로 줄이기 (조각 S2)**: `Plan::mutate`가 Enter admit에서 `PaneUnverified`(그리고 그 gap이 위 일시 계열일 때만)를 받으면, 같은 witness로 `verify_pane_within(CONFIRM_WINDOW)` 재판독을 허용한다. `ServerChanged`·`OtherNonce`·`RootReplaced`·`ProviderReplaced`·`MutationBusy`·`ExecutionClosing`·`AdmissionStopped`는 재시도하지 않는다. Enter는 아직 한 번도 보내지 않았으므로 재전송이 아니다. 재시도 사이마다 취소를 확인한다. 실패가 2초를 넘기면 지금처럼 `Indeterminate`로 끝나고 S1 판정이 이어받는다. 입력 게이트의 gap은 info 수준으로 남긴다(지금 원인을 확인할 수 없었던 공백).

## 3. 안전한 해제 판정 (실행이 살아 있는 동안)

### 3.1 표지에 더 기록할 것 (hold v2)

지금 파일에는 시각만 있어 "어느 입력의 보류인지"를 알 수 없다. W1~W3에서 아래 JSON을 기록한다. 원문은 저장하지 않는다(CLI `status`의 "no path or input text is shown" 원칙).

| 필드 | 쓰임 |
|---|---|
| `schema: 2`, `recorded_at` | `input_holds()`는 v1(RFC3339 한 줄)과 v2를 모두 읽는다. R6~R8 표시는 그대로 유지 |
| `provider`, `logical_key`, `execution_nonce` | 다른 실행·다른 채널의 판정 금지 |
| `frame_digest`(공백을 접은 sha256), `frame_chars`, `frame_newlines` | 입력창의 글이 우리 붙여넣기인지 증명한다(`src/services/tui_input/actor/gate.rs` `own_draft`의 본문 일치와 `[Pasted text #N +k lines]` 칩 규칙을 digest 판으로) |
| `attempt_nonce`·`message_id`(owner 경로일 때) | retained 원 입력과 연결(배치 표시·운영자 안내) |
| `transcript`, `dev`/`ino`, `offset_before`, `started_at` | 그 입력의 user 기록을 `offset_before` 뒤에서 찾는다(`busy_inject.rs` `transcript_submission`·`HerdrTurnStart`와 같은 기준) |
| `phase` | `before_keys` → `pasted` → `enter_attempted`. 결과 분류 `enter_refused`(Enter가 확정적으로 안 나감) / `enter_send_failed` / `paste_send_failed`. Enter가 확정적으로 안 나갔으면 transcript 대기를 건너뛸 수 있다 |
| `unclear_passes`, `surfaced_at` | 판정 불가 횟수와 운영자 표시 여부(재시작 뒤에도 표시가 중복되지 않게) |

### 3.2 판정 시점

- **P1 (다음 턴 시작 직전, R1/R2 자리)**: `not_held(nonce)?`를 `resolve_hold(nonce, record, target)`으로 바꾼다. 결과가 `Released`일 때만 `bound_source`/`pending_source`로 진행한다.
- **P2 (busy 주입이 입력창을 비운 뒤)**: Claude busy `Adapter::submit`이 `InputRun::Applied`로 끝나고 표지가 있으면, receipt 스레드가 그 프레임의 transcript 기록을 확인한 다음 같은 판정을 한 번 수행한다(같은 composer 잠금, 같은 실행 확인).
- **P3 (깨움)**: 판정이 "보류 유지(대기)"이면 기존 준비 감시(`owner_input::record_preparation_watch`, `first_input.rs`의 `Screen::Ready` 탐침과 같은 형태)에 "입력창이 비고 판독 가능해짐"을 등록한다. 감시가 깨면 P1을 다시 수행한다.

### 3.3 판정 규칙 (`resolve_hold`, 제안)

선행 조건: composer mutation lock(`composer_lock::with_composer_mutation_lock`) 안에서, 그 실행이 게이트에 확인되어 있어야 한다(`HerdrTarget::execution_confirmed`). 같은 nonce·같은 root·같은 provider여야 하고, 확인되지 않으면 판정하지 않는다. 표지의 `execution_nonce`·`logical_key`가 현재 행과 같아야 한다.

| 순서 | 관측 | 판정 | 원 입력 처리 |
|---|---|---|---|
| 1 | transcript `offset_before` 뒤에 그 입력의 user 기록 또는 UserPromptSubmit이 있음(digest 일치, v1이면 `recorded_at` 이후 첫 user 기록) | **전달됨 → 해제** | 다시 보내지 않는다. retained 원 입력에 "전달 확인" 표시(사용자에게 다시 보내지 말라고 알림) |
| 2 | provider가 그 턴을 실행 중(transcript 기준 busy) | 대기(판정 미룸) | 없음 |
| 3 | **아직 가라앉지 않음**: `recorded_at`이 붙여넣기 정착 + receipt 창(`busy_inject::TIMING.confirm_window`)보다 최근이고 `phase`가 `enter_refused`가 아님 | 대기 | 없음 |
| 4 | 판독 불확실: capture 실패, `ComposerOwner::Unread`, stash 보임, `PasteExpand` 꼬리, modal | **보류 유지**(기본값) + `unclear_passes` 증가 | 없음 |
| 5 | 입력창이 측정 레이아웃에서 정확히 빔(`ComposerOwner::Empty`, `claude_composer_empty_when_present`) | **미전달 → 해제** | 다시 보내지 않는다(`Completion::WriteUncertain` 계약 "never sent again" 유지). retained 상태는 그대로이며 기존 알림이 이미 나갔다 |
| 6 | 입력창이 **우리 붙여넣기**(v2 digest `own_draft` 일치, 또는 v1이면 `ComposerOwner::AgentDesk`의 `[User:` 머리) | 게이트를 거친 지우기(기존 `host_draft.rs` `require_cleanup_owner`와 `draft_hold.rs` `stranded_draft_is_ours`의 "우리 글만 지움" 규칙을 herdr 대상에 적용). 키 직전 소유권을 다시 판독하고, 지운 뒤 판독을 한 번 더 해서 빈 것이 보이면 5로 해제 | 지운 입력은 retained 원 입력과 같으므로 손실 없음 |
| 7 | 사람 초안(`ComposerOwner::Person`), 또는 v1 표지가 증명할 수 없는 접힌 칩 | **보류 유지** + 운영자 표시 | 없음 |

- 4와 7에서 보류를 유지할 때는 다음 입력을 Opaque start failure로 태우지 않는다. 타입 있는 "대기" 거절(제안 `HeldInputPending`)을 `start_failure.rs` `confirmed`/owner front Held 쪽으로 보내, front를 유지하고 P3 감시를 건다. 지금처럼 2회 뒤 retained로 넘기는 것을 막는다.
- **상한**: `unclear_passes >= 2`가 되면(재시작 뒤에도 파일에 남는 값) 그 채널에 한 번만 운영자 알림을 보낸다. 알림에는 nonce, 남은 글 분류(사람 초안 / 판독 불가 / 증명 불가 칩), 복구 명령 안내를 넣는다. 보류 자체는 안전 근거가 생길 때까지 유지한다. "보류가 영원히 조용히 남는" 상태는 없앤다(모델의 `HoldReleasedOrSurfaced`).
- 해제는 `release_hold`(fsync 포함)로만 한다. `NotDurable`/`Kept`이면 판정 실패로 보고 보류 유지한다.

### 3.4 재전달 여부

원 입력은 이미 `WriteUncertain` → retained → 알림 경로를 탔다. 이 설계는 **자동 재전달을 하지 않는다**. 이유는 모델의 `ResendNoSettle` 대조군에 있다. Enter가 들어갔지만 transcript가 아직 안 쓰인 순간에 "입력창 빔 + transcript 없음"을 미전달로 보고 다시 보내면 같은 입력이 두 번 제출된다(`OriginalAtMostOnce` 위반). 미전달 확정 뒤 원 입력을 다시 제안하는 일(예: 알림 문구에 "아직 전달되지 않았음이 확인됨" 추가)은 별도 결정 사항으로 남긴다.

## 4. 입력 출처 분류와 인접 경로

### 4.1 입력 출처 분류표

| 출처 | 쓰는 경로 | 표지 | 소유자 |
|---|---|---|---|
| Discord 사용자 메시지, idle 시작 | owner front → `herdr_turn::execute` → W1/W2/W3 | 씀(W), 읽음(R1~R4) | channel mailbox owner(front claim) |
| Discord 사용자 메시지, 작업 중 | owner busy → `host_input/busy/herdr.rs` | 읽지 않음 | owner front, busy observation |
| 예약·배치 등 봇 루프백 프롬프트 | idle 시작과 같은 경로 | W·R 같음 | owner front |
| 사람이 pane에 직접 입력 | 없음(AgentDesk 밖) | 없음. 판독에서 `Person`으로 보임 | 사람 |
| `/clear`(채널 명령) | Bound + `awaited_clear` → `cleared_source` → W1 | R1이 먼저 막는다(보류 중이면 /clear 뒤 첫 입력도 거절) | owner |
| stop / Cancel(Escape) | `try_prepare_herdr_interrupt`, 게이트 `Mutation::Cancel` | 표지를 바꾸지 않음 | 턴의 cancel token |
| Terminate / 종료 명령 | `herdr_terminate.rs` | C5로 해제 | 종료 명령 소유자 |

### 4.2 같은 상태를 소비하는 인접 경로

| 경로 | 지금 동작 | 설계 뒤 |
|---|---|---|
| Cancel/stop | 첫 키 전 닫힘만 C2로 해제한다. 불확실 뒤 stop은 표지를 그대로 둔다 | 변화 없음. stop 뒤 다음 시작에서 P1 판정 |
| Terminate | retire 뒤 C5 | 변화 없음 |
| `/clear` | R1에 막힘 | P1 판정이 먼저 수행됨. 해제되면 `awaited_clear` 경로 그대로 |
| dcserver 재시작 | 표지는 남고(`herdr_terminal_restart_tests.rs` `restart_preserves_execution_nonce_input_hold_pg`) 메모리 상태는 사라짐 | 표지가 판정 근거(v2 필드·`unclear_passes`)를 함께 지니므로 재시작 뒤 첫 P1에서 그대로 판정. herdr pane은 dcserver와 따로 살아 있으므로 같은 nonce = 같은 pane이고, 게이트 확인이 이를 보증한다 |
| 부팅 재연결 | R6가 수만 셈 | 판정하지 않는다(입력이 없을 때 pane에 키를 보내지 않는다). 입력이 올 때 P1 |
| 새 incarnation | C8이 retired 표지를 지움 | 변화 없음. 행이 없는 표지(`other_input_holds`)는 판정하지 않고 표시만 한다 |
| herdr 서버 재시작/pane 교체 | 게이트가 `RootReplaced`/`ProviderReplaced`/`ServerChanged`로 거절, 행 끝 처리 C6/C7 | P1이 게이트 확인 실패로 판정하지 않음 → 행 끝 경로가 해제 |
| tmux 복귀(R5) | 표지가 있으면 거절 | 복귀 전에 같은 판정을 한 번 수행(조각 S1 범위 밖, 후속) |

## 5. 상태 차트 결과

모델 `HerdrInputHold.tla`(값 범위는 1장 표: 결과 분류는 `InputRun` 변형, 해제는 C1/C5~C10, 읽기는 R1·busy 주입). 자세한 내용은 [보고서](../statecharts/input-single-path/herdr-input-hold.report.md).

| cfg | 정책 | 깨지는 규칙 |
|---|---|---|
| Before | main | HoldEventuallyReleased, HoldReleasedOrSurfaced, LaterInputStarts (**RED**) |
| Fixed | 설계 | 없음 (**GREEN**) |
| EnterSendFail / PasteSendFail / PersonLeaves / Restart / LegacyPrefix | 설계 | 없음 |
| PersonStays / Unreadable / LegacyChip | 설계 | HoldEventuallyReleased, LaterInputStarts (예상: 증명 불가 → 보류 유지, `HoldReleasedOrSurfaced`는 통과) |
| BeforeEnds | main + 실행 종료 | LaterInputStarts (표지는 실행 종료로만 풀림) |
| BeforeUnreadable | main | 세 liveness 모두 |
| NoSettle, NoSettleEnter, ResendNoSettle, ReleaseOnUnread, ReleaseOnResidue, IgnoreTranscript | 대조군 | 각각 NoReleaseOnResidue / NoPasteOverResidue / UndeliveredIsTrue / OriginalAtMostOnce |

## 6. 조각 계획

리뷰 상한: `scripts/pr_cap_prod.py` `FILES, NET = 30, 1500`(prod 파일 30개, 순증 +1500줄).

| 조각 | 내용 | 예상 prod 파일 / 순증 |
|---|---|---|
| **S1 (Claude 판정)** | hold v2 기록(W1)과 v1/v2 읽기(`input_holds`·`input_held`), `resolve_hold`(새 모듈 `src/services/claude/herdr_turn/hold_resolve.rs` (제안)), R1/R2 교체, busy 주입 뒤 P2, 대기 거절 타입(`start_failure.rs`), 깨움 감시 P3, 1회 운영자 알림, CLI `status`에 `phase`·`unclear_passes` 표시. 시험: DoD RED(빈 입력창 + 보류 → 새 시작 거절), 수정 후 시작, 남은 글이면 유지, transcript에 이미 있으면 재전달 0, v1 표지 호환, 재시작 뒤 판정 | 약 10~12개 / +700 |
| **S2 (불확실 줄이기)** | Enter admit의 일시 `PaneUnverified`를 `CONFIRM_WINDOW` 안에서 다시 판독(`herdr_gate.rs`·`host_input.rs`), 입력 게이트 gap info 로그. 공유 게이트를 건드리므로 S1과 분리 | 약 3~4개 / +150 |
| **S3 (Codex)** | Codex도 구조가 같다(같은 표지 파일, R3/R4 `not_held`, W2/W3, C3/C4). 추가로 첫 입력 제출 뒤 session start를 못 찾으면 남는 경우(C4)가 있다. S1의 `resolve_hold`를 provider 중립으로 두고 Codex 판독기(`own_draft` Codex 분기: 접힌 `[Pasted Content N chars]`는 증명 불가 → 유지 + 표시)와 rollout user 기록으로 판정한다 | 약 6~8개 / +400 |

S1이 운영 증상을 고친다. S1이 provider 중립 핵심을 만들면 S3는 Codex 판독기만 더하면 된다. 그래서 같은 조각으로 묶지 않고 다음 조각으로 둔다(한 조각의 리뷰 범위를 Claude 경로 하나로 유지).

**운영 표지 복구**(이 레인 범위 밖, 사용자 승인 필요): S1 배포 뒤 `bf28830d…` 표지는 v1이다. 입력창에 `[User:` 머리가 보이면 S1이 지우고 해제한다. 접힌 칩이면 운영자 표시로 남는다. 이때는 사용자 승인을 받아 pane 입력창을 비운 뒤 표지를 지우는 수동 복구가 필요하다.

## 7. 리뷰 집중 항목

- **같은 입력 두 번 제출 / 남은 글 위 붙이기**: 해제는 (1) transcript 전달 확인, (5) 측정 레이아웃에서 정확히 빈 입력창 + 정착 창 경과, (6) 우리 글임을 증명한 뒤 게이트 지우기와 재판독, 이 세 경우뿐이다. 자동 재전달은 없다. 정착 창이 없으면 그려지기 전 붙여넣기를 빈 것으로 읽어 합쳐진다(대조군 NoSettle). 미전달 판정 뒤 재전달하면 늦게 도착한 Enter와 겹쳐 두 번 제출된다(대조군 ResendNoSettle).
- **판독 불확실이 계속되는 경우**: 보류는 유지한다(안전 기본값). 대신 입력을 retained로 태우지 않고 대기·깨움으로 두며, `unclear_passes` 상한 2에서 운영자 알림을 한 번 보낸다. 값은 파일에 남아 재시작 뒤에도 중복 알림이 없다. S2가 게이트 일시 실패 자체를 줄인다.
- **재시작·부팅 뒤 표지와 실행 행**: 표지는 nonce에 묶이고, herdr pane은 dcserver 재시작과 무관하게 살아 있다. P1은 게이트의 같은 nonce·root·provider 확인 뒤에만 판정한다. 부팅 패스는 키를 보내지 않는다. 새 incarnation은 C8이 이전 nonce 표지를 지우고, 행이 없는 표지는 표시만 한다.
- **Codex**: 같은 결함이 있다(R3/R4, W2/W3, C3). Codex 첫 입력은 session start를 못 찾아도 표지가 남는다(C4). S3에서 같은 판정으로 고친다.
