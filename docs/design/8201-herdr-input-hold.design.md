# #8201 herdr 입력 보류 표지: 불확실 Enter 뒤 실행이 살아 있는 동안 안전하게 푸는 판정

- 코드 기준: `934a49997e95ffb0d54ae8eed2d7653ea6c1e4bf` (origin/main, 2026-11-16). 읽기 전용 조사이며 코드·시험은 바꾸지 않았다.
- 상태 차트: [herdr-input-hold.report.md](../statecharts/input-single-path/herdr-input-hold.report.md), 모델 `docs/statecharts/input-single-path/HerdrInputHold.tla`.
- 개정 2 (2026-11-16 20시): pro 설계 리뷰 ISSUES(P1 4·P2 2) 반영.
- 개정 3 (2026-11-16 21시): pro 재리뷰 2 ISSUES(P1 3·P2 4) 반영.
- 개정 4 (2026-11-16 22시): xh 재리뷰 3(P1 1·P2 2) 반영. 송신 결과를 모르는 `in_flight` 단계, 정리 뒤 receipt 창의 모델화, 알림 의도·enqueue·확보 기록의 분리. 
- 개정 5 (2026-11-16 22시): xh 재리뷰 4(P1 1·P2 1) 반영. 코디네이터 결정에 따라 이슈 DoD("입력창에 글이 남은 경우에만 보류 유지")에 설계를 맞춘다. 판독 가능한 빈 입력창에서 증거가 없으면 알림 row를 확보한 뒤 "전달 미확인"으로 해제한다. receipt 창은 시간만 보고, 정리 단계 해제의 표시를 바로잡는다. 항목별 대응은 9장에 있다.
- S1 수리 1 (2026-11-17): pro 리뷰 P1-1·P2-3 반영. 감시의 비교 기준은 판정이 쓴 근거이며 Ready는 Wake나 감시 종료까지 유지한다(3.5). busy 주입 뒤 재판정은 현재 대상 실행 nonce의 보류만 본다. 모델에 판정 끝 → 감시 등록 틈(`Arm`)과 대조 `MutFreshBasis`(RED)를 더했다.
- S1 구현 첫 커밋 (2026-11-16 23시): xh 재리뷰 5 P1-1 반영. 보류 알림 문구 하나가 보류 지속·전달 불확실·응답이 없으면 다시 보내라는 안내를 함께 담는다(3.5). 모델에 `UnconfirmedNoticeGuides`와 cfg 2개(`PreNoticedInFlight` GREEN, `MutHeldOnlyNotice` RED)를 더했다. 리뷰 5 P2-1(재시작 직전 서버가 받은 Enter의 늦은 적용)은 Herdr 서버 순서 계약 확인이 필요해 S1 범위 밖이다.
- 운영: 사건 표지 `runtime/herdr_input_holds/bf28830d9ac94338bbe904c8a0c4eb0a`는 사용자 승인(11-16 19:4x)을 받아 메인 세션이 19:41에 치웠다(삭제하지 않고 이동). 그 뒤 같은 보류는 관측되지 않았다.

## 0. 요약

- 11-16 18:05:15 KST의 `Indeterminate { confirmed: 1, cause: Refused(Herdr(PaneUnverified)) }`는 "붙여넣기는 들어갔고 **Enter는 게이트가 보내기 전에 거절했다**"는 뜻이다(`host_input.rs` `admit_after`). 원 입력은 입력창에 붙여넣은 채로 남았고, `WriteUncertain`로 retained 처리되어 다시 보내지지 않는다.
- 그 입력은 두 가지를 남긴다. 하나는 **파일 표지**(`hold`, 프로세스 재시작에도 남음)이고, 다른 하나는 **readiness 예약**(`readiness::reserve`가 만든 `Entry.pending`, 프로세스 메모리)이다. `Indeterminate`는 둘 다 정산하지 않는다(`composer.rs` `when_idle`의 `_ => ()`). 표지만 풀면 남은 예약 때문에 다음 입력이 여전히 시작하지 못한다(리뷰 P1-1).
- 설계: 표지를 읽는 자리를 **owner 효과 안, 키 직전**(`first_input.rs` `write`, composer 잠금 안)으로 옮기고 `resolve_hold`로 판정한다.
  - 전달 판정은 v2 표지의 digest가 transcript user 기록과 일치할 때만 한다.
  - 판독 가능성(capture·레이아웃·stash·footer·modal)과 빈 입력창 판정을 나눈다. 읽을 수 있는 본문은 정확 일치 검사까지 간다.
  - 삭제는 입력창 본문을 손실 없이 복원할 수 있고, 그 본문이 원 프레임과 **바이트 단위로 같을 때만** 한다. 공백을 접은 비교는 쓰지 않는다.
  - 원 송신 결과(`send_outcome`)와 미전달 증거(`unsent_proof`)는 정리 단계(`cleanup_phase`)와 따로 기록하며 덮어쓰지 않는다.
  - 표지는 첫 키 전에 `send_outcome=in_flight`로 만들고, `InputRun` 결과를 받은 뒤 최종 결과를 원자적으로 기록한다. 결과 기록 전에 재시작하면 Enter가 갔을 수 있는 것으로 보고, transcript나 원문 잔존으로 증명될 때만 정리한다. 증명이 안 되는 경우를 입력창으로 나눈다. 판독 가능한 빈 입력창이면 그 보류의 알림 row를 먼저 확보하고 "전달 미확인"으로 해제해 다음 입력이 시작되게 한다(개정 5). 글이 남았거나 판독할 수 없으면 보류를 유지하고 알림을 1회 보낸다. 어느 쪽이든 자동 재전달은 없다.
  - 해제는 **예약 정산 → 표지 삭제**를 한 단위로 묶는다.
  - 판정할 수 없으면 front를 Held로 두고 근거 변화를 감시해 다시 판정한다. 판정 실패가 2회 쌓이면 **그 보류에 대해** 채널 알림을 1회 보낸다. 알림 키는 보류 identity(`hold_id`)이고, outbox row가 확보될 때까지 같은 키로 다시 enqueue한다.
- 불확실 자체도 줄인다(S2). Enter admit에서 일시 `PaneUnverified`가 나면 시간 상한 안에서 **전체 re-admit**(게이트 judge 전체를 다시 수행하고 새 pin을 만듦)을 한다.
- 상태 차트(38 cfg): 수정 전은 liveness RED이고 설계는 GREEN이다. 결과 기록 전 재시작 cfg 4개도 GREEN이다. 대조군 17개가 각각 정해진 규칙을 깬다. 개정 5에서 더한 대조군은 "in_flight 재시작 시 영구 보류"(liveness RED), "정리 단계 빈 입력창을 미전달로 판정 + 늦은 transcript"(`UndeliveredIsTrue` RED), "판정이 쓰기 잠금을 기다리지 않음"(`OriginalNotOvertaken` RED)이다.

## 1. 진입점 전수표

표지 = `runtime/herdr_input_holds/<execution_nonce>`(`src/services/claude/herdr_turn.rs` `hold_path`, Codex도 같은 함수를 쓴다). main의 파일 내용은 RFC3339 시각 한 줄뿐이다.

### 1.1 쓰기 (W)

| # | 위치 | 상태 변화 |
|---|---|---|
| W1 | `src/services/claude/herdr_turn/first_input.rs:90` `write` → `herdr_turn.rs` `hold` | Claude 턴 시작(Bound·Pending·새 launch, owner `submit_owned`와 legacy `submit` 공통). 붙여넣기 전에 기록하고 fsync한다 |
| W2 | `src/services/codex/herdr_turn.rs:622` `followup_write` → `hold` | Codex Bound 후속 입력 |
| W3 | `src/services/codex/herdr_turn/first_input.rs:147` → `hold` | Codex 첫 입력(launch·Pending 뒤) |
| W4 | `src/services/claude/herdr_turn/composer.rs` `when_idle` → `src/services/claude_tui/readiness/mod.rs` `reserve` | 키 전에 readiness 예약: `Entry.pending = (transcript floor, revision)`. 이 예약이 있는 동안 같은 source의 `observe`는 floor 뒤에 새 활동과 terminal이 없으면 준비를 인정하지 않는다 |

### 1.2 지우기 (C)

| # | 위치 | 상태 변화 |
|---|---|---|
| C1 | `first_input.rs` `write`의 `composer_settled(&run)` 뒤 `remove_file` | `Applied`·`Refused`·`Cancelled{0}`이면 해제. `Indeterminate`·`Cancelled{>0}`이면 **남는다** |
| C2 | `first_input.rs` `Closed` → `src/services/provider/herdr_before_start.rs` `release_claude_hold` | 첫 키 전에 멈춘 턴 → 해제 |
| C3 | `src/services/codex/herdr_turn.rs:647` / `release_closed_hold` | Codex 후속: 제출됐거나 건드리지 않음 → 해제 |
| C4 | `src/services/codex/herdr_turn/first_input.rs:158·167·220` | Codex 첫 입력: 닫힘·건드리지 않음 → 해제. 제출 뒤에는 `observe`가 session start를 찾고 bind한 뒤에만 해제한다. 못 찾으면 **남는다** |
| C5 | `src/services/discord/herdr_terminate.rs:321` `settle_and_release` | 종료: retire CAS 뒤 해제 |
| C6 | `src/services/discord/recovery_engine/herdr_row_end.rs:495` `commit` | 행 끝 retire 뒤 해제 |
| C7 | `src/services/discord/recovery_engine/herdr_row_end_live.rs:72` | 살아 있던 턴의 실행이 사라짐 → retire 뒤 해제 |
| C8 | `src/services/herdr_launch/retired_hold.rs:79` ← `src/services/herdr_launch.rs:463` `before_fresh` | 새 incarnation 전에 retired 표지를 해제 |
| C9 | `src/cli/herdr.rs:294` `release_left_hold` | 이미 Retired인 행에 남은 표지 |
| C10 | `src/cli/herdr.rs:330` `retire_on_reading` | pane에 종료가 보일 때만 retire 뒤 해제 |

### 1.3 readiness 예약 정산 (P)

| # | 위치 | 상태 변화 |
|---|---|---|
| P1 | `readiness/mod.rs` `Reservation::not_sent` ← `composer.rs` `when_idle` | 키 전 거절(`Refused`·`Cancelled{0}`)일 때만 그 revision의 예약을 해제한다 |
| P2 | `readiness/mod.rs` `Reservation::sent` ← `when_idle` (`Applied`) | 제출됨 표시. 예약은 그 턴의 terminal이 정산한다 |
| P3 | `readiness/mod.rs` `observe`의 `entry.pending.take()` | floor 뒤에 새 활동과 terminal이 있으면 자연 정산. 원 Enter가 안 나갔으면 **오지 않는다** |
| P4 | `readiness/mod.rs` `note_hook` (UserPromptSubmit) | 다른 실제 제출이 pending을 자기 floor로 바꾼다. 그 제출의 terminal이 P3로 정산한다 |
| P5 | 프로세스 재시작 | ledger가 비고 예약은 사라진다. 대신 기존 재시작 계약인 `recovery` 울타리가 생긴다(이 이슈에서 바꾸지 않음) |

`Indeterminate`와 `Cancelled { confirmed > 0 }`는 P1·P2 어디에도 해당하지 않는다(`composer.rs`의 `_ => ()`). 두 결과 모두 표지도 남긴다. 따라서 원 Enter가 안 나간 경로에서는 같은 프로세스 안에서 P3이 오지 않고, 예약이 영구히 남는다.

### 1.4 읽기 (R)

| # | 위치 | 상태 변화 (main) | 설계 |
|---|---|---|---|
| R1 | `src/services/claude/herdr_turn.rs:264` Bound → `not_held` | 거절 → Opaque start failure 2회 → retained | 거절하지 않고 `write`의 키 직전 판정으로 넘긴다(3.2) |
| R2 | `src/services/claude/herdr_turn.rs:268` Pending → `not_held` | 위와 같음 | 위와 같음(`pending_source`가 키를 보내지 않는다는 것은 S1 수용 시험으로 확인) |
| R3 | `src/services/codex/herdr_turn.rs:153` Bound | 위와 같음 | S3 |
| R4 | `src/services/codex/herdr_turn.rs:157` Pending | 위와 같음 | S3 |
| R5 | `src/services/claude/herdr_return.rs:69` | 표지가 있으면 tmux 복귀를 거절 | 변화 없음(후속) |
| R6 | `src/services/discord/recovery_engine/herdr_reader.rs:249` | 재연결 패스에서 표지 수만 센다 | 변화 없음(키 없음) |
| R7 | `src/server/routes/health_api/runtime_profile.rs:74` | health 표시 | v1·v2를 모두 읽음 |
| R8 | `src/cli/herdr.rs` `status` | 실행별 `recorded_at` 표시 | `send_outcome`·`cleanup_phase`·`unclear_passes`·알림 기록 추가(원문 없음) |

**표지를 읽지 않는 곳**: Claude busy 주입(`src/services/claude_tui/host_input/busy/herdr.rs` `Adapter::submit`)과 Codex busy 주입. busy 주입은 사람 초안을 C-s로 stash한다. 제출 뒤 Claude가 stash를 입력창에 돌려준다(`busy_inject.rs` `Sent::parked`). 그래서 우리 글도 stash됐다가 돌아올 수 있다.

전수: W 4 + C 10 + P 5 + R 8 = **27개**(시험 전용 호출 제외).

## 2. 불확실 판정의 원인과 S2 재판독 정책

1. `host_input.rs` `run_plan`은 동작마다 게이트를 거친다. `MutationGate::admit`은 Herdr에서 `HerdrTarget::pin`(`herdr_gate.rs`)이다. `pin`은 `fence.acquire` + `judge`를 수행하고, 판정(`Judged`, witness·permit 포함)을 다음 send 한 번에 쓰도록 보관한다. 거절하면 이전 pin도 버린다.
2. `judge`는 admission(`herdr_admission::check`) → host marker → policy witness → `verify_pane`(`verify_pane_within(witness, ZERO)`) 순서다. 대기 0초이므로 `pane_probe::probe`는 재시도하지 않는다.
3. `EvidenceGap` 중 `ServerChanged`·`NonceMissing`·`OtherNonce`를 뺀 나머지는 모두 `PaneUnverified`로 접힌다. 부하에서는 `ReadFailed`(소켓 지연)·`ProcessUnreadable` 같은 일시 gap이 유력하다. 하지만 입력 게이트는 gap을 `debug!`로만 남겨 운영 로그로는 확인되지 않는다(현재 `dcserver.stdout.log`에 "pane unverified" 0건).
4. 기존 `execution_confirmed`의 `CONFIRM_WINDOW`나 `pane_probe::before_provider` 재시도 목록을 그대로 재사용하면 안 된다(리뷰 P2-2). `verify_pane_within`은 pane 확인뿐이라 admission·marker·policy·permit이 빠지고 pin을 만들지 않는다. 또 `before_provider` 목록에는 `ReadFailed`가 없고, Enter 직전에 재시도하면 안 되는 `NonceMissing`·`Replaced`가 들어 있다.

**S2 정책 (Enter 경계에만 적용)**

| 항목 | 값 |
|---|---|
| 적용 위치 | `host_input.rs` `Plan::step`의 `TuiInputAction::Enter`. paste·Escape·CtrlU 등 다른 동작과 공유 helper(`verify_pane_within`)는 바꾸지 않는다. busy 주입의 Enter도 같은 `Plan`을 지나므로 함께 적용된다 |
| 재시도 조건 | 첫 admit이 `Herdr(PaneUnverified)`이고, 접기 전 gap이 `ReadFailed`·`Unreported`·`NoneYet`·`Ambiguous(_)`·`ProcessUnreadable(_)`·`EnvUnreadable` 중 하나일 때. 이를 위해 `HerdrGateRefusal`에 일시 gap 표지(예: `PaneUnverified { transient: bool }`)를 더한다 |
| 재시도 금지 | `ServerChanged`·`OtherNonce`·`NonceMissing`·`Replaced`·`RootReplaced`·`ProviderReplaced`·`AdmissionStopped`·`MarkerOtherHost`/`MarkerUnverified`·`RestoreOn`/`RestoreUnverified`·`PolicyUnverified`·`MutationBusy`·`ExecutionClosing` |
| 각 시도 | **전체 re-admit** = `gate.admit(session)` = `pin(Input)`. fence permit, admission, marker, policy witness, pane 증거를 모두 새로 확인한다. 성공하면 새 pin이 생기고 Enter 한 번이 그 pin을 소비한다(`send_pinned`) |
| witness 고정 | 붙여넣기를 보낸 서버의 witness를 `PaneGate`에 기록하고(마지막 send의 witness), 재시도 판정의 witness가 다르면 즉시 중단한다(Enter 0회) |
| 횟수·간격·상한 | 최대 3회, 간격 250 ms, 첫 거절부터 총 1 s를 넘기지 않는다(`CONFIRM_WINDOW` 2 s 안) |
| 취소 | 각 시도 전과 재시도 성공 뒤 송신 직전에 `check_cancel` |
| 상한 초과 | 지금과 같이 `Indeterminate { confirmed, Refused(PaneUnverified) }`로 끝나고 S1 판정이 이어받는다 |
| 관측 | 입력 게이트의 gap과 재시도 결과를 `info!`로 남긴다 |

## 3. 안전한 해제 판정 (실행이 살아 있는 동안)

### 3.1 표지 v2 기록

원문은 저장하지 않는다(CLI `status`의 "no path or input text is shown" 원칙). `input_holds()`·`input_held()`는 v1(RFC3339 한 줄)과 v2(JSON)를 모두 읽는다. v2 표지는 원자적으로 다시 쓴다(임시 파일 → fsync → rename → 디렉터리 fsync).

| 필드 | 쓰임 |
|---|---|
| `schema: 2`, `recorded_at`, `provider`, `logical_key`, `execution_nonce` | 다른 실행·채널은 판정하지 않는다 |
| `hold_id` | 이 보류의 identity. owner 경로는 `attempt_nonce`, 그 밖에는 기록 때 만든 UUID. 같은 실행의 다음 보류는 다른 값을 갖는다. 알림 키에 쓴다(3.5) |
| `frame_sha256`, `screen_exact` | 원 프레임 바이트의 sha256과, 화면에서 그 프레임을 손실 없이 복원할 수 있는지 여부. `screen_exact`가 false인 프레임(줄 끝 공백, 탭, 제어 문자, 접힘 칩이 생기는 크기 등)은 삭제 증거를 만들 수 없다(규칙 8 불가) |
| `transcript`, `dev`/`ino`, `offset_before`, `started_at` | 전달 판정은 같은 파일 identity의 `offset_before` 뒤에서만 찾는다. user 기록 본문 바이트의 sha256이 `frame_sha256`과 같아야 한다 |
| `send_outcome` | `in_flight`(표지를 만든 쓰기가 아직 결과를 기록하지 않음. 붙여넣기·Enter가 갔을 수 있음)에서 시작해 **최종 결과로 한 번만** 바뀌고, 그 뒤로는 바뀌지 않는다. 최종 결과: `enter_refused`(Enter admit 거절, 키 미송신), `cancelled_before_enter`(`Cancelled { confirmed: 1 }`: paste 뒤 Enter 전 취소), `paste_send_failed`(Enter 시도 없음), `enter_send_failed`(Enter가 나갔을 수 있음) |
| `unsent_proof` | 한 번 생기면 지우지 않는다. `none` / `send_outcome`(앞의 세 최종 결과. 결과 기록과 같은 쓰기에서만 생김) / `exact_text_seen`(정착 창과, `enter_send_failed`·`in_flight`이면 receipt 창까지 지난 뒤 우리 본문이 정확히 보였음. Enter가 원문을 제출했다면 입력창에 남을 수 없다). `in_flight`는 언제나 `none`으로 시작한다 |
| `cleanup_phase`, `cleanup_at` | `none` → `clearing`(지우기 직전) → `cleared`(지운 뒤 재판독 확인). 정리 단계는 증거 필드를 건드리지 않는다 |
| `reservation` | `{ boot_id, source(session·nonce·native·path), revision }`. 같은 프로세스에서만 정산 대상과 연결한다 |
| `unclear_passes`, `notice_intent_at`, `notice_enqueued_at` | 판정 실패 횟수, 알림 의도, 그 보류 키의 outbox row 확보. 재시작 뒤에도 남는다 |

**기록 순서 (개정 4, 리뷰 P1-1)**: 지금 `hold()`는 첫 키보다 먼저 표지를 쓰고 fsync한다(`first_input.rs` `write`, `herdr_turn.rs` `hold`). 설계는 이 순서를 유지하고 그 시점의 내용을 정한다.

1. `hold()`: `send_outcome=in_flight`, `unsent_proof=none`, `cleanup_phase=none`과 identity·transcript 위치 필드를 원자적으로 기록한다.
2. `when_idle`이 예약을 잡고 입력 계획(붙여넣기 → Enter)을 실행한다.
3. `InputRun`을 받으면 최종 `send_outcome`과 (해당하면) `unsent_proof=send_outcome`을 **한 번의 원자적 재기록**으로 확정한다. `Applied`는 지금처럼 표지를 지운다.
4. 1~3은 같은 composer 잠금 안에서 끝낸다(제안). 지금은 `hold()`가 잠금 밖에 있고 `run_ready`만 `with_composer_mutation_lock` 안에 있다. S1은 3.2의 판정과 `hold()`·계획 실행·결과 기록을 한 잠금 구간으로 옮긴다. 그래서 같은 프로세스의 판정은 쓰기가 진행 중인 `in_flight` 표지를 보지 않는다. 잠금이 풀렸는데도 `in_flight`이면(재시작, 또는 결과 기록이 실패한 오류 경로) 결과를 모르는 표지로 다룬다.

`in_flight` 표지를 만나면 판정은 Enter가 갔을 수 있다고 본다. 그래서 `enter_send_failed` + `unsent_proof=none`과 같은 규칙(3.3의 2·7·8)을 쓴다. 규칙 7은 개정 5부터 보류를 유지하지 않고 알림 row 확보 뒤 "전달 미확인"으로 해제한다. 표지에 Enter 시도 여부가 남지 않으므로, 키 전이나 붙여넣기 직후 끊긴 경우도 똑같이 다룬다. 정착·receipt 창은 쓰기가 살아 있으면 그 쓰기의 붙여넣기·Enter 키부터 재고, 쓰기가 없으면(재시작 뒤) 표지 기록 시각부터 잰다. 더 보낼 키가 없기 때문이다. `in_flight`를 `enter_refused`처럼 미전달로 읽으면, Enter가 pane에 닿고 transcript 기록 전에 재시작한 경우 빈 입력창을 미전달로 잘못 판정한다(대조군 `MutInFlightAsRefused`, `UndeliveredIsTrue` RED). 미전달로 판정한 뒤 원 입력을 다시 보내는 경로는 처음부터 없으므로(3.6) 같은 입력이 두 번 제출되지는 않지만, 사용자에게 "미전달"이라고 잘못 알리게 된다. 설계의 "전달 미확인"은 이런 거짓 단정을 하지 않는다.

**예약 맡김 (P2-B)**: 표지를 남기는 모든 "건드린" 결과, 곧 `Indeterminate`와 `Cancelled { confirmed > 0 }`에서 `when_idle`은 `Reservation`을 버리지 않고 프로세스 안 레지스트리 `HELD_RESERVATIONS[nonce]`에 맡긴다(제안). `Cancelled { confirmed: 1 }`은 `send_outcome=cancelled_before_enter`, `unsent_proof=send_outcome`으로 기록한다. 레지스트리는 메모리라 재시작하면 비며, 그때는 정산할 예약도 없다(P5).

### 3.2 판정 자리와 owner 경계

- **P1 (키 직전)**: R1/R2의 `not_held(nonce)?`를 지우고, `first_input.rs` `write`에서 `hold()` 앞, `with_composer_mutation_lock` 안에서 `resolve_hold`를 부른다. 이 자리는 owner 효과(`submit_owned`의 `submit` 클로저, `capture_preparation`) 안이다. 그래서 보류를 유지할 때 `before_payload(..., "herdr_input_hold_pending")`를 써서 `SubmissionError::Held`를 내고, `owner_input::record_preparation_watch`도 owner scope 안에서 등록된다. front는 Held로 남고 `start_failed`로 가지 않는다. 새 non-Opaque 오류 종류를 `start_failure.rs`에 더하지 않는다. 그렇게 하면 `kind != Opaque` 규칙으로 즉시 retained가 되기 때문이다(`queued_head/v2/start_failure.rs` `start_failed`).
- owner가 아닌 legacy `submit` 경로(`owner_input::active()`가 false)는 지금처럼 거절한다. 운영 herdr Claude는 owner 경로다.
- **P2 (busy 주입 뒤)**: busy `Adapter::submit`이 `Applied`로 끝나고 같은 nonce의 표지가 있으면, receipt 스레드가 그 프레임의 transcript 기록을 확인한 다음 P1과 같은 판정을 한 번 수행한다.
- **P3 (깨움)**: 3.5.

### 3.3 판정 규칙 (`resolve_hold`, 제안)

선행 조건: composer 잠금 안에서, 게이트가 실행을 확인해야 한다(같은 nonce·root·provider, `HerdrTarget::execution_confirmed`). 표지의 `execution_nonce`·`logical_key`가 현재 행과 같아야 한다. 아니면 판정하지 않는다(보류 유지, 키 0).

판독은 두 단계로 나눈다(리뷰 P1-A). 기존 `draft_sighting`의 의미는 바꾸지 않는다.

- **판독 가능 관문**: capture 실패, 레이아웃 인식 실패(`screen::typed`가 `None`), stash 있음·확인 불가(`Stash` ≠ `AbsentInRecognizedLayout`), 비정상 footer(`PasteExpand` 등), modal이면 판독 불가다. 입력창에 글이 있는지는 여기서 보지 않는다.
- **빈 입력창**: `screen::Composer::Empty`이고 `draft_sighting` = `Settled`. 규칙 5·6·7과 삭제 뒤 재판독에만 쓴다.

| 순서 | 관측 | 판정 | 원 입력 |
|---|---|---|---|
| 1 | provider가 턴을 실행 중(원 입력이든 다른 입력이든) | 대기 | — |
| 2 | **v2만**: 같은 파일 identity의 `offset_before` 뒤 user 기록 본문 sha256 = `frame_sha256` | **전달됨 → 해제** (예약은 그 턴의 terminal이 P3로 이미 정산함) | 다시 보내지 않음. "전달 확인" 표시 |
| 3 | 판독 가능 관문 실패 | 보류 유지, `unclear_passes` + 1 | — |
| 4 | 붙여넣기 정착 창이 지나지 않음 | 대기(`enter_refused`도 건너뛰지 않음) | — |
| 5a | 빈 입력창 + `cleanup_phase=clearing` + 이 패스에서 지움 키를 보냈음 | `cleanup_phase=cleared` 기록 후 5b | — |
| 5b | 빈 입력창 + v2 + `unsent_proof` ≠ `none` + (`cleanup_phase` ≠ `none`이면 정리 단계에서 빈 입력창을 처음 본 뒤 receipt 창 경과·digest 기록 없음) | receipt 창 안이면 대기. `cleanup_phase` ∈ {`none`, `cleared`}이면 **해제 단위**(3.4). `clearing`(비었지만 우리 키로 비운 것이 확인되지 않음: 재시작 뒤이거나 사람이 보냈을 수 있음)이면 7과 같은 **미확인 해제** | `none`·`cleared`는 "미전달 확인", `clearing`은 "전달 미확인"(알림 1회). 어느 쪽도 다시 보내지 않음 |
| 6 | 빈 입력창 + v1 표지 | 해제 단위(v1은 배포 재시작 뒤에만 만나므로 정산할 예약이 없다) | 전달 여부를 말하지 않음(unknown) |
| 7 | 빈 입력창 + v2 `send_outcome` ∈ {`enter_send_failed`, `in_flight`} + `unsent_proof=none` + digest 기록 없음 | receipt 창 안이면 대기. 지나면 **미확인 해제**: ① `notice_intent_at` → ② enqueue → ③ `notice_enqueued_at`(3.5)을 마친 뒤 해제 단위(3.4). 이미 이 보류의 알림 row가 있으면 바로 해제 단위(개정 5) | 다시 보내지 않음. "전달 미확인 — 응답이 없으면 다시 보내 주세요"(알림 1회) |
| 8 | 읽을 수 있는 본문을 복원한 바이트가 원 프레임과 같음(v2, `screen_exact`, `enter_send_failed`·`in_flight`이면 receipt 창 경과) | ① `unsent_proof`가 `none`이면 `exact_text_seen`으로 올리고 `cleanup_phase=clearing`을 같은 쓰기로 기록·fsync → ② 키 직전 재판독(여전히 같을 때만) → ③ 게이트를 거친 지우기 → ④ 재판독해 빈 입력창이면 5a | 손실 없음(retained에 같은 원문) |
| 9 | 그 밖: 사람 초안, 우리 글을 고친 글(덧붙임·공백·줄바꿈 변경), 칩, v1 표지에 남은 글, `screen_exact=false` | 보류 유지, `unclear_passes` + 1, 키 0 | — |

- 규칙 8의 비교는 화면 표시용 요소(프롬프트 표식 `❯`, 연속 줄 들여쓰기, 줄바꿈된 표시 줄)만 검증된 규칙으로 걷어 내고, 본문의 공백과 개행은 그대로 둔다. 공백을 접은 비교(기존 `own_draft`의 `split_whitespace`)는 삭제 증거로 쓰지 않는다(리뷰 P2-A). 화면에서 원문을 손실 없이 복원할 수 없으면 삭제하지 않는다.
- v1 표지는 2(전달 판정)와 8(삭제)을 쓰지 않는다. 시각은 증거로 쓰지 않는다.
- 다른 입력 B가 기록 시각 뒤에 제출되어도 digest가 다르므로 전달 증거가 아니다. stash 복귀 전에는 3(stash 보임)이 막고, 복귀 뒤에는 8이 정확한 일치로 처리한다.
- 5b의 "정리 뒤 receipt 창"은 우리가 읽은 직후(①~③ 사이) 사람이 같은 원문에 Enter를 누른 경우를 위한 것이다. 창은 정리 단계에서 빈 입력창을 처음 본 시각(우리 지움 키 뒤이거나, 사람이 보내서 비었거나)부터 잰다. 빈 입력창에는 더 보낼 원문이 없으므로 그 뒤에 새 제출이 생기지 않는다. 그 사이 transcript digest 기록이 생기면 2가 전달로 처리한다. 시각은 프로세스 메모리에 둔다. 재시작하면 다시 재므로 기다리는 시간만 길어진다.
- **창은 시간만 본다(개정 5, 리뷰 P2-1)**: 판정기는 transcript 기록이 아직 오는 중인지 알 수 없다. 그래서 정착·receipt 창의 경과를 transcript 상태와 묶지 않는다. 창이 transcript보다 먼저 끝나도 거짓 "미전달"이 생기지 않게 하려면, "미전달"을 빈 입력창을 우리 쪽에서 설명할 수 있는 경우로만 좁힌다. 첫째는 `send_outcome`이 Enter 미송신을 증명할 때(`cleanup_phase=none`)이고, 둘째는 우리 지움 키 뒤 재판독으로 비었음을 확인했을 때(`cleared`)다. 정리 단계(`clearing`)에서 비었는데 우리 키가 확인되지 않으면 사람이 보냈을 수 있으므로 "전달 미확인"으로 해제한다. 개정 4 규칙(정리 단계 빈 입력창도 미전달)은 대조군 `MutLateTranscriptAfterClearReceipt`에서 `UndeliveredIsTrue` RED가 된다. 남는 경계는 둘이다. ②와 ③ 사이 같은 순간에 사람이 Enter를 누르고 우리 키가 빈 입력창에 가는 경합(A15)과, 사람이 `clearing` 기록 전에 우리 원문을 보낸 경우(`send_outcome` 증거로 "미전달"이 될 수 있음, A3)다. 둘 다 표시만 틀릴 뿐 재전달·삭제는 없다.
- `in_flight`는 2(digest)와 8(원문 잔존 → `exact_text_seen`)로만 "전달"·"미전달"이 증명된다. 둘 다 아니고 판독 가능한 빈 입력창이면 7의 미확인 해제다. 글이 남았거나(9) 판독할 수 없으면(3) 보류를 유지한다. 키 0, 자동 재전달 0.
- **미확인 해제 뒤 Enter가 실제로 갔던 경우**: 그 턴은 provider에서 정상으로 진행한다. 다음 입력은 기존 `when_idle` readiness와 busy 주입 규칙을 따른다. 원 입력을 다시 보내는 경로가 없으므로 같은 입력이 두 번 제출되지 않는다. 판정은 쓰기가 composer 잠금을 놓은 뒤에만 하므로(A16) 원 입력의 Enter가 다음 입력보다 늦게 닿지 않는다. 모델에서는 `RestartInFlightSend`·`RestartInFlightTwice`의 `OriginalAtMostOnce`·`OriginalNotOvertaken`이 이를 확인하고, 잠금을 빼면(`MutNoWriteLock`) 깨진다.

### 3.4 해제 단위와 정리 단계의 끊김 (리뷰 P1-1·P1-B·P2-D)

해제 단위는 같은 composer 잠금 안에서 수행한다.

1. 증거를 다시 확인한다(3.3의 5b 또는 6).
2. **예약 정산**: `HELD_RESERVATIONS[nonce]`가 있고 `boot_id`가 현재 프로세스와 같으면, 그 예약만 `Reservation::settle_unsent(UnsentProof)`(제안, `not_sent`과 같은 revision 일치 규칙)로 해제한다. revision이 다르면 아무것도 하지 않는다. 다른 source·다른 입력의 예약과 `recovery` 울타리는 건드리지 않는다.
3. **표지 삭제**: unlink 뒤 디렉터리 fsync. `release_hold`의 반환값별 처리는 아래 표를 따른다.
4. 내구성까지 확인되면 잠금을 놓고 owner에 Wake를 보낸다.

| 끊기는 지점 | 남는 기록 | 결과 |
|---|---|---|
| `hold()` 뒤, 첫 키 전 | `in_flight`, `unsent_proof=none` | 부팅 뒤 표지 기록 시각부터 정착·receipt 창을 잰다. 빈 입력창이면 7: 알림 row 확보 → "전달 미확인" 해제 → 다음 입력 시작. 키 0, 재전달 0 |
| 붙여넣기 뒤, 결과 기록 전 | 같음 | 원문이 남았으면 창 뒤 8(① `exact_text_seen`)로 정리하고 5b로 해제("미전달"). 붙여넣기가 화면에 닿지 않았으면 7(미확인 해제) |
| Enter 뒤, 결과 기록 전 | 같음 | 창 안에 transcript digest가 보이면 2(전달). 원문이 남았으면 8. 빈 입력창이면서 digest가 없으면 7(미확인 해제, 알림 1회). "미전달"로 단정하지 않는다. Enter가 실제로 갔으면 그 턴은 정상으로 진행되고, 다음 입력은 busy 규칙을 따른다 |
| 미확인 해제의 ①·②·③ 사이 | `notice_intent_at`(·`notice_enqueued_at`) | 부팅 뒤 같은 규칙. ②는 보류 키 dedupe로 같은 row를 받는다 |
| ③ 뒤, 해제 단위 2·3 사이 | 표지와 `notice_enqueued_at` | 부팅 뒤 해제 단위만 다시. 알림은 다시 만들지 않는다 |
| 규칙 8 ① 전 | `send_outcome`, `unsent_proof`(있으면) | 부팅 뒤 같은 규칙으로 재판정 |
| ① 뒤, ③ 전(아직 키 없음) | `cleanup_phase=clearing`, `unsent_proof`(≠ none) | 부팅 뒤 원문이 그대로면 8 ②부터. 사람이 그새 고쳤으면 9(키 0) |
| ③ 뒤, ④ 전(키는 갔고 `cleared` 기록 전) | `clearing`, `unsent_proof` ≠ none | 부팅 뒤 빈 입력창이면 5b의 `clearing` 갈래로 미확인 해제(알림 1회). 우리 키였는지 사람의 Enter였는지 기록으로 구분할 수 없기 때문이다. 재판독이 한 번 실패하면 3에서 보류했다가 근거 변화로 다시 5b |
| ④ 뒤, 해제 단위 전 | `cleared` | 5b |
| 해제 단위 2와 3 사이 | 표지 그대로 | 메모리 예약은 사라짐(P5). 부팅 뒤 5b/6에서 3만 다시 |
| 3 `Kept`(unlink 실패) | 파일이 남음 | 예약은 정산됐고 표지가 남아 다음 입력은 판정에 머무른다. 다음 패스에서 unlink 재시도, 키 0 |
| 3 `NotDurable`(unlink 성공, 디렉터리 fsync 실패) | 이 프로세스에서는 파일이 **없음**. crash하면 되살아날 수 있음 | Wake 전에 `fsync_parent_dir`만 다시 시도한다(새 helper `ensure_hold_absent_durable`, 제안). `release_hold`를 다시 부르면 "파일 없음 → Released"로 보고하므로 내구성 재시도로 쓰지 않는다. 성공하기 전까지 front는 Held, 자체 재시도 간격으로 재시도한다. 그 사이 crash로 표지가 되살아나면 부팅 뒤 같은 증거(`unsent_proof`)로 다시 해제한다(멱등) |
| 3 뒤 crash | 없음 | 정상 해제. 부팅 뒤 기존 재시작 readiness 계약(P5)을 따른다 |

증거 보존은 여전히 계약이다. `clearing`을 쓸 때 원 송신 결과를 지우지 않고, `unsent_proof`는 별도 필드에 둔다. `clearing` 자체를 무조건 "미전달" 단계로 두면 `enter_send_failed`까지 미전달로 바꾸게 된다. 개정 3에서는 증거를 덮어쓰면 ③과 ④ 사이 재시작 뒤 영구 보류가 됐다. 개정 5에서는 규칙 7의 미확인 해제가 이를 흡수해, 증거를 잃어도 알림 1회와 "전달 미확인" 해제로 끝난다. 그래서 대조군 `MutOverwriteProof`는 규칙을 깨지 않게 되어 뺐다(5장). 증거가 남아 있어야 `cleanup_phase=none`·`cleared`에서 알림 없이 "미전달"로 조용히 풀 수 있다.

순서를 반대(삭제 → 정산)로 하지 않는 이유는 개정 2와 같다. 정산이 빠지면 다음 입력이 계속 막힌다(`MutNoSettleReservation`).

### 3.5 깨움과 보류마다 알림 1회 (리뷰 P1-3·P1-C·P2-C)

- **감시 탐침**: 보류를 유지할 때 등록하는 `PreparationWatch` 탐침은 **판정 근거 변화**를 본다. 직전 판정의 지문(판독 가능 관문 결과, 입력창 분류 빈/정확 일치/그 밖, stash 유무, 같은 nonce transcript의 digest 기록 유무와 길이, 정착·receipt 창 경과, 게이트 확인 여부)이 바뀌면 `Screen::Ready`를 돌려 `held_watch`가 owner를 Wake하게 한다. 비교 기준은 감시 등록 때 새로 읽은 화면이 아니라 **판정이 실제로 쓴 근거**다. 판정 끝과 등록 사이(알림 enqueue 등)의 변화도 다시 판정하게 하려는 것이다(대조 `MutFreshBasis`). 한 번 Ready면 Wake나 감시 종료까지 Ready로 남는다. Wake는 판정을 다시 돌릴 뿐 입력 권한이 아니다. 실행이 사라지면 `Screen::Gone`.
- **자체 재시도**: 그 보류의 `notice_enqueued_at`이 없는 동안에는 근거 변화가 없어도 탐침이 재시도 간격(예: 30 s) 뒤 Ready를 돌린다. 종료 조건은 `notice_intent_at`이 아니라 **durable outbox row 확보**(`notice_enqueued_at`)다. 그래서 enqueue만 실패해도 같은 프로세스에서 계속 다시 시도한다(리뷰 P2-C).
- **알림 순서**: `unclear_passes`가 2에 닿으면 ① 표지에 `notice_intent_at` 기록·fsync → ② outbox enqueue(키 `input-hold:{execution_nonce}:{hold_id}`, LOOPBACK, 영구 dedupe) → ③ 성공(새 row든 같은 키의 기존 row든)이면 `notice_enqueued_at` 기록. ②가 실패하면 ③을 쓰지 않으므로 재시도가 계속된다. ①과 ② 사이에서 crash하면 부팅 뒤 판정이 `notice_intent_at`을 보고 enqueue부터 한다. ②와 ③ 사이에서 crash하면 부팅 뒤 같은 키로 다시 enqueue하고, 영구 dedupe가 기존 row id를 돌려준다. 그래서 보류마다 row는 정확히 하나다. ③을 기록해도 이미 와 있는 근거 변화 Wake는 지우지 않는다. 자체 재시도는 그다음 판정 패스에서 끝난다. 개정 4 모델은 ①(`Bump`가 한도에서 의도 기록) → ②(`EnqueueNotice`) → ③(`RecordEnqueued`)을 따로 전이하고 그 사이 어디서든 재시작이 끼어들 수 있게 했다(`RestartUnreadable`).
- **보류 단위 키 (리뷰 P1-C)**: 실행 nonce만 키로 쓰면 같은 실행의 두 번째 보류부터 기존 row가 반환되어 알림이 사라진다(대조군 `MutNonceKey`). 기존 `retained_notice.rs`도 입력 identity와 attempt nonce를 키에 넣는다. v1 표지의 `hold_id`는 `sha256(execution_nonce ‖ 파일의 recorded_at 원문)`이다. 파일 내용이 바뀌지 않으므로 그 표지의 생애와 재시작 동안 같다.
- **알림 이후**: 근거 변화 감시만 남는다. 판독이 회복되거나 사람이 글을 비우면 자동으로 이어진다.
- **알림의 성격**: 사용자에게 상태를 알리는 채널 메시지다. 보류 키가 같으므로 한 보류의 알림은 1회뿐이고, 보류 알림이 이미 나간 뒤 미확인 해제가 되면 새 알림을 만들지 않는다. 그래서 문구는 쓰이는 이유(보류 유지·미확인 해제)와 무관한 **하나**이며 처음부터 세 가지를 함께 담는다(리뷰 5 P1-1): ① 앞선 입력 때문에 다음 메시지가 기다리고 있고 입력창이 비거나 다시 읽히면 자동으로 이어진다 ② 앞선 입력이 전달됐는지 확인하지 못했을 수 있다 ③ 보류가 풀린 뒤에도 앞선 입력에 응답이 없으면 그 입력을 다시 보내 달라. 모델 `UnconfirmedNoticeGuides`가 이를 확인한다(대조 `MutHeldOnlyNotice`). 명령·버튼·수동 해제 기능은 없다. 기존 `agentdesk herdr status/retire` 외에 새 수동 조작을 만들지 않는다.
- **알림의 증명 범위**: 모델과 시험이 증명하는 것은 중복 없는 durable outbox row 확보다. 사용자가 실제로 받았는지는 기존 outbox 전달 계약이 맡는다.

### 3.6 재전달 여부

자동 재전달은 하지 않는다(`Completion::WriteUncertain` "never sent again" 유지). 정착 창 없이 "빈 입력창 + transcript 없음"을 미전달로 보고 다시 보내면, 늦게 도착한 Enter와 겹쳐 두 번 제출된다(대조군 `MutResendNoSettle`).

## 4. 입력 출처와 인접 경로

| 출처/경로 | 표지·예약 | 설계 뒤 |
|---|---|---|
| Discord 사용자 메시지, idle 시작 | W1·W4 / P1 판정 | 3.2~3.5 |
| 작업 중 입력(busy 주입) | 표지를 읽지 않음, C-s stash 가능 | P2 판정. stash가 보이면 3에서 보류 |
| 봇 루프백(예약·배치) | idle 시작과 같음 | 같음 |
| 사람이 pane에 직접 입력·수정 | 판독에서 `Person`, 덧붙임은 digest 불일치 | 키 0, 보류 유지 + 알림 |
| `/clear` (Bound + `awaited_clear`) | main은 R1이 먼저 막음 | P1 판정 뒤 `cleared_source` 그대로 |
| stop / Cancel | 표지 변화 없음, 삭제 권한 없음 | 변화 없음 |
| Terminate / 행 끝 / retire | C5~C10 | 변화 없음 |
| dcserver 재시작 | 표지는 남고 예약은 사라짐(P5), `recovery` 울타리 | 표지 v2(`send_outcome`·`unsent_proof`·`cleanup_phase`·`unclear_passes`·알림 기록)로 재판정. owner의 front 복구가 P1을 다시 부른다 |
| 부팅 재연결 패스 | R6가 수만 셈 | 키 0 유지 |
| 새 incarnation | C8 | 변화 없음. 행이 없는 표지는 표시만 |
| herdr 서버 재시작/pane 교체 | 게이트 거절, C6/C7 | 판정하지 않음(선행 조건 실패) → 행 끝 경로 |
| tmux 복귀(R5) | 표지가 있으면 거절 | 후속 |

이 경계들은 문서상 기존 동작 유지이며, 모델이 그 교차를 검증한 것은 아니다(6장).

## 5. 상태 차트 결과 (`HerdrInputHold.tla`, 40 cfg)

모든 v2 cfg는 `in_flight` 표지에서 시작하고, 붙여넣기 키 → Enter 키 → 결과 기록을 지난다. 재시작 범위(상수 `CrashWindow`)는 다음과 같다.
- `Restart`: 결과 기록 뒤 재시작만
- `RestartInFlight*`: 결과 기록 전 재시작
- `RestartInFlightTwice`: 결과 기록 전 재시작 + 어디서나 두 번째 재시작
- `RestartUnreadable`: 어느 지점이든 재시작 1회

개정 5에서 안전 규칙 `UnconfirmedNoticed`(미확인 해제에는 그 보류의 알림 row가 있음)와 `OriginalNotOvertaken`(원 입력이 다음 입력 뒤에 닿지 않음)을 더했다.

| 구분 | cfg | 깨지는 규칙 |
|---|---|---|
| 수정 전 | Before | HoldEventuallyReleased, HoldReleasedOrNoticed, ReadableOursCleaned, LaterInputStarts (**RED**) |
| 수정 전 | BeforeEnds | LaterInputStarts |
| 설계 | Fixed, LoadSpike, EnterSendFail, PasteSendFail, PersonLeaves, PersonEdits, PersonSubmits, StashOther, Restart, SecondHold | 없음 (**GREEN**) |
| 설계(결과 기록 전 재시작) | RestartInFlight, RestartInFlightPaste, RestartInFlightSend, RestartInFlightTwice | 없음 (**GREEN**, 개정 4에서는 HoldEventuallyReleased·LaterInputStarts가 깨졌음) |
| 설계(증명 불가, 예상) | PersonStays, Unreadable, RestartUnreadable, EnqueueFail, Legacy | HoldEventuallyReleased, LaterInputStarts(글이 남았거나 판독 불가라 보류 유지. `HoldReleasedOrNoticed`·`NoticeAtMostOnce`는 통과) |
| 설계(S1, 리뷰 5 P1-1) | PreNoticedInFlight | 없음 (**GREEN**. main 정책으로는 HoldEventuallyReleased·HoldReleasedOrNoticed·ReadableOursCleaned·LaterInputStarts RED) |
| 대조군(S1) | MutHeldOnlyNotice | UnconfirmedNoticeGuides |
| 대조군(S1 수리) | MutFreshBasis | HoldEventuallyReleased, HoldReleasedOrNoticed, ReadableOursCleaned, LaterInputStarts |
| 대조군(개정 5) | MutInFlightKeep | HoldEventuallyReleased, LaterInputStarts |
| 대조군(개정 5) | MutLateTranscriptAfterClearReceipt | UndeliveredIsTrue |
| 대조군(개정 5) | MutNoWriteLock | OriginalNotOvertaken, NoPasteOverResidue, NoReleaseOnResidue, LaterInputStarts |
| 대조군(개정 4) | MutInFlightAsRefused | UndeliveredIsTrue |
| 대조군(개정 3) | MutSightingGate | HoldEventuallyReleased, ReadableOursCleaned, LaterInputStarts |
| 대조군(개정 3) | MutNonceKey | HoldReleasedOrNoticed |
| 대조군(개정 3) | MutStopOnIntent | HoldEventuallyReleased, HoldReleasedOrNoticed, LaterInputStarts |
| 대조군 | MutShapeDelete | NoHumanTextDeleted |
| 대조군 | MutTimeReceipt | DeliveredIsTrue, NoReleaseOnResidue, LaterInputStarts |
| 대조군 | MutNoSettleReservation | LaterInputStarts |
| 대조군 | MutEmptyOnlyWake | HoldEventuallyReleased, HoldReleasedOrNoticed, ReadableOursCleaned, LaterInputStarts |
| 대조군 | MutNoSelfRetry | HoldEventuallyReleased, HoldReleasedOrNoticed, LaterInputStarts |
| 대조군 | MutOpaqueFailure | LaterInputStarts |
| 대조군 | MutSkipPasteSettle | NoReleaseOnResidue, NoPasteOverResidue, LaterInputStarts |
| 대조군 | MutResendNoSettle | OriginalAtMostOnce, UndeliveredIsTrue, NoReleaseOnResidue, NoPasteOverResidue, LaterInputStarts |
| 대조군 | MutReleaseOnUnread, MutReleaseOnResidue | NoReleaseOnResidue, LaterInputStarts |

개정 5에서 뺀 대조군:
- `MutIgnoreTranscript`(transcript 확인 생략)와 `MutOverwriteProof`(clearing이 증거 덮어씀): 규칙 7의 미확인 해제가 두 결함을 흡수한다. 빈 입력창은 어느 쪽이든 알림 1회와 "전달 미확인" 해제로 끝나므로 규칙이 깨지지 않는다. 남는 비용은 불필요한 알림과 덜 정확한 표시다. 증거 보존(P1-B)과 transcript 판정(규칙 2)은 설계 계약으로 유지하며, 6장 A5·A9 시험이 맡는다.
- `MutSkipClearReceipt`: 리뷰 4 권고에 따라 창이 transcript를 모르는 `MutLateTranscriptAfterClearReceipt`로 바꿨다.
- 개정 2의 `MutVolatileCounter`: 알림 키 dedupe가 보류마다 row를 하나로 묶으므로, 계수를 메모리에만 두어도 중복 알림이 생기지 않는다.

## 6. 모델 밖 계약 → S1 수용 시험 (리뷰 P2-1)

모델 GREEN은 아래 계약을 **가정**한다. 각 계약은 S1(또는 표시한 조각)의 Rust 시험으로 증명한다. 시험 이름은 후보다.

| # | 모델이 가정하는 것 | 증명할 시험 |
|---|---|---|
| A1 | 실제 `readiness::reserve`가 만든 예약이 `Indeterminate`·`Cancelled { confirmed > 0 }` 뒤 레지스트리에 맡겨지고, 해제 단위가 그 예약만 정산한다 | `hold_resolve_enter_refused_from_real_reserve_clears_paste_settles_reservation_next_input_submits_once` (같은 프로세스에서 reserve → paste 성공 → Enter 거절 → 실제 판독기를 통과하는 원문 잔존 capture(`draft_sighting` ≠ `Settled`) → 정리 → 해제 → 후속 입력 1회), `hold_resolve_cancel_after_paste_entrusts_reservation_and_resumes`. 대조: `..._without_settle_next_input_stays_blocked`, `..._sighting_gate_never_reaches_cleanup` |
| A2 | 정산은 revision이 맞는 이 입력의 예약만 건드린다 | `hold_resolve_settle_ignores_other_source_and_superseded_revision` |
| A3 | 붙여넣기 정착 창 ≥ 화면 반영. receipt 창은 transcript 기록 지연보다 길게 잡지만, 늦게 와도 "미전달"을 거짓으로 단정하지 않는다(개정 5. 모델의 창은 transcript 상태를 모름) | `hold_resolve_enter_refused_waits_paste_settle_before_reading_empty`, `hold_resolve_enter_send_failed_waits_receipt_window` |
| A4 | "우리 글"은 손실 없이 복원한 본문 바이트가 원 프레임과 같을 때뿐이다 | `hold_resolve_v1_prefix_with_person_suffix_sends_no_key`, `hold_resolve_v2_chip_or_same_line_count_other_body_sends_no_key`, `hold_resolve_whitespace_indent_or_newline_edit_sends_no_key`, `hold_resolve_frame_not_screen_exact_never_deletes`, `hold_resolve_v2_exact_body_clears_then_rereads_empty` |
| A5 | 전달 판정은 v2 digest 일치뿐이다 | `hold_resolve_v1_later_user_record_is_never_delivery`, `hold_resolve_stash_submit_other_then_return_is_not_receipt` |
| A6 | 근거 변화가 Wake를 만들고, 판정 실패 1회 뒤에도 자체 재시도로 두 번째 판정에 닿는다 | `hold_resolve_unread_then_readable_wakes_without_new_message`, `hold_resolve_persistent_unread_notices_once_retained_zero_keys_zero`, `hold_resolve_recovery_during_notice_enqueue_wakes_after_registration`(판정 뒤 등록 전 변화) |
| A7 | 알림은 보류마다 정확히 1회이며, enqueue 실패와 재시작에도 row가 하나로 수렴한다 | `hold_resolve_two_holds_same_execution_two_outbox_rows`, `hold_resolve_notice_survives_restart_without_duplicate`, `hold_resolve_enqueue_failure_retries_in_process_until_row`, `hold_resolve_crash_between_enqueue_and_record_reuses_row`, `hold_resolve_crash_after_notice_intent_before_enqueue`(부팅 뒤 enqueue부터 이어 row 1개, 알림 0개도 2개도 아님) |
| A8 | 보류 유지는 Held front + 감시이지 start failure가 아니다 | `hold_resolve_kept_hold_is_held_front_not_start_failed`, `hold_resolve_watch_registered_inside_owner_scope` |
| A9 | 정리 단계·해제 단위의 순서와 crash 재개, `Kept`/`NotDurable` 구분 | `hold_resolve_crash_after_clear_keys_before_cleared_releases_after_boot`(원 입력 재전송 0, 후속 입력 1), `..._capture_fails_once_after_boot_then_releases`, `hold_resolve_crash_between_settle_and_unlink_resumes_after_boot`, `hold_resolve_unlink_kept_keeps_file_and_sends_no_key`, `hold_resolve_not_durable_retries_dir_fsync_before_wake`, `hold_resolve_crash_after_hold_before_first_key`, `hold_resolve_crash_after_paste_before_outcome_record`, `hold_resolve_crash_after_enter_before_outcome_record`(세 시험 모두 표지가 `in_flight`로 유효하게 다시 읽히고 지움 키 0·원 입력 재전달 0이다. 첫 키 전 crash → 빈 입력창 → 알림 row 1개 확보 뒤 "전달 미확인" 해제와 후속 입력 1회. Enter 뒤 crash → digest 없음 → 같은 동작. digest나 원문 잔존이 나타나면 기존대로 "전달"이나 정리 뒤 "미전달"로 해제), `hold_resolve_unconfirmed_release_crash_between_notice_and_unlink_no_second_row`, `hold_resolve_unconfirmed_release_after_enter_landed_runs_turn_once`(Enter가 실제로 간 경우 원 입력 턴 1회, 다음 입력은 busy 규칙). 대조: `..._in_flight_empty_is_never_judged_undelivered`, `..._in_flight_empty_kept_forever_fails`(증명 가능한 빈 입력창을 무기한 보류하면 실패) |
| A10 | 게이트 확인 실패·다른 nonce·Pending에서는 키를 보내지 않는다 | `hold_resolve_unconfirmed_execution_judges_nothing`, `hold_resolve_other_nonce_hold_untouched`, `hold_resolve_pending_source_sends_no_key_before_resolve` |
| A11 | busy 주입 뒤 판정(P2) | `busy_inject_applied_then_resolve_releases_on_digest_receipt` |
| A12 | 재시작 뒤 readiness는 기존 `recovery` 계약을 따르고 판정이 그것을 바꾸지 않는다 | `hold_resolve_after_restart_leaves_recovery_fence_untouched` |
| A13 (S2) | Enter 전체 re-admit 정책 | `enter_readmit_read_failed_then_success_sends_one_enter`, `enter_readmit_stops_on_admission_marker_closing_or_witness_change`, `enter_readmit_forbidden_gap_never_retries`, `enter_readmit_cancel_during_probe_sends_no_enter`, `enter_readmit_budget_expiry_keeps_indeterminate` |
| A15 | 정리 단계에서 사람이 같은 원문을 제출한 경우 정리 뒤 receipt 창 안에서는 해제하지 않고, 창 안에 digest가 오면 전달, 창 뒤에도 없으면 "전달 미확인"("미전달" 아님)으로 해제한다. 모델(`PersonSubmits`)은 지우기 기록과 키 사이의 제출을 다루고, 키와 제출이 같은 순간에 겹치는 경합은 이 시험이 맡는다 | `hold_resolve_person_submits_our_text_after_read_is_delivered`(receipt 창 안 해제 없음 → digest 뒤 전달), `hold_resolve_person_submit_with_late_transcript_is_unconfirmed_not_undelivered` |
| A7a | 보류 알림이 먼저 나간 뒤의 미확인 해제는 새 row 없이 그 row의 문구로 미확인·재전송 안내를 전한다(리뷰 5 P1-1) | `hold_resolve_pre_noticed_inflight_then_empty_unconfirmed_once` |
| A16 | 결과 기록은 `in_flight` → 최종 결과의 원자적 재기록 한 번이며, 판정·`hold()`·계획 실행·결과 기록이 한 composer 잠금 구간 안에 있다 | `hold_record_outcome_is_single_atomic_rewrite`, `hold_resolve_never_judges_in_flight_while_write_holds_lock`, `hold_resolve_outcome_record_failure_leaves_in_flight_and_is_judged_unknown` |
| A14 (S3) | Codex 첫 입력: `expected_session_id`/`expected_transcript`가 null로 시작하고 session start 뒤 source가 확정된다. 확정 전에는 전달 판정을 하지 않는다 | `codex_hold_first_input_without_source_never_judges_delivery`, `codex_hold_bound_source_then_digest_receipt_releases` |

## 7. 조각 계획

리뷰 상한: `scripts/pr_cap_prod.py` `FILES, NET = 30, 1500`.

| 조각 | 내용 | 예상 prod 파일 / 순증 |
|---|---|---|
| **S1 Claude 판정** | 표지 v2와 원자적 재기록(`herdr_turn.rs`), 건드린 결과의 예약 맡김과 `settle_unsent`(`composer.rs`, `readiness/mod.rs`), 판정 모듈 `src/services/claude/herdr_turn/hold_resolve.rs` (제안), R1/R2 제거와 `write` 키 직전 판정(`first_input.rs`), 판독 가능 관문과 무손실 본문 복원(`busy_inject/screen.rs` 옆), `in_flight` 기록과 결과 원자 확정, 규칙 7 미확인 해제(알림 row 확보 → 해제 단위)와 알림 문구, 판정·`hold()`·결과 기록을 한 잠금 구간으로 옮김, 근거 변화 탐침과 자체 재시도, busy 뒤 P2(`busy/herdr.rs`), 보류 키 알림 source(outbox source·registry·`retained_notice.rs` 계열), 디렉터리 fsync 재시도 helper, CLI·health 표시. 시험 A1~A12, A15, A16 | 약 15~17개 / +1250 |
| **S2 Enter 재판독** | `Plan::step` Enter re-admit(`host_input.rs`), 일시 gap 표지(`herdr_gate.rs`, `pane_probe.rs`), 마지막 send witness 기록, info 로그. 시험 A13 | 약 4~5개 / +250 |
| **S3 Codex** | R3/R4·W2/W3·C3/C4에 같은 판정 적용. 첫 입력의 source 미확정 단계(A14), Codex 판독기(접힌 `[Pasted Content N chars]`는 증명 불가 → 보류·알림), rollout user 기록 digest | 약 8~10개 / +550 |

S1이 운영 증상을 고친다. S2는 공유 게이트를 건드리므로 따로 리뷰한다. S3는 S1의 provider 중립 핵심 위에 Codex source 확정 단계를 더하므로 판독기 추가보다 범위가 넓다.

## 8. 운영 메모

- 사건 표지(v1)는 사용자 승인으로 19:41에 이동 처리됐고, 이후 재발이 관측되지 않았다. 배포 전에 새로 생기는 v1 표지는 S1 배포 뒤 3.3의 6(빈 입력창이면 해제)이나 9(남은 글이면 보류·알림)로 처리된다. v1에서는 자동 삭제나 전달 판정을 하지 않는다.

## 9. 리뷰 대응

### 개정 2

개정 2 당시의 이름이다. 개정 3에서 `frame_digest`는 `frame_sha256`(공백을 접지 않음)으로, 단일 `phase`는 `send_outcome`·`unsent_proof`·`cleanup_phase`로 바뀌었고, `MutVolatileCounter`는 뺐다(5장).

| 항목 | 바꾼 곳 | 모델 |
|---|---|---|
| P1-1 예약 미정산 | 1.1 W4, 1.3 P1~P5, 3.1 `reservation`, 3.4 해제 단위·crash 표 | `reserved`, `Settle`→`Unlink`(`lock`), `Paste`가 `~reserved` 요구. 대조 `MutNoSettleReservation` RED |
| P1-2 모양 기반 삭제 | 3.1 `frame_digest`, 3.3 규칙 8·9, 8장 | `PersonEdits`(덧붙임 = `edited`), `NoHumanTextDeleted`. 대조 `MutShapeDelete` RED |
| P1-3 Empty-only 깨움·알림 보장 | 3.2 owner 경계, 3.5 근거 변화 탐침·자체 재시도·알림 1회 | `trigger`/`watch`/`Wake`, 표지에 남는 `unclear`·`noticed`, `notices` 계수. `NoticeAtMostOnce`, `HoldReleasedOrNoticed`. 대조 `MutEmptyOnlyWake`·`MutNoSelfRetry`·`MutVolatileCounter`·`MutOpaqueFailure` RED |
| P1-4 시각 기반 전달 | 3.3 규칙 2(v2 digest만), v1 제외 | `StashOther`(C-s → B 제출 → 복귀), `DeliveredIsTrue`. 대조 `MutTimeReceipt` RED |
| P2-1 모델 범위 | 6장 계약 A1~A14와 시험 후보, 4장 끝 문장 | 정착/receipt 창 분리(`pasteSettled`/`receiptSettled`), `enter_refused`의 정착 생략 금지(대조 `MutSkipPasteSettle`), 해제 잔여물을 `Unhold` 한 곳에서 실제 상태로 계산 |
| P2-2 S2 정책 | 2장 표(조건·금지·전체 re-admit·witness·횟수/간격/상한·취소) | 모델 밖(A13) |

### 개정 3

| 항목 | 바꾼 곳 | 모델 |
|---|---|---|
| P1-A 규칙 3이 우리 본문 차단 | 3.3 머리(판독 가능 관문과 빈 입력창 분리), 규칙 3·8, 6장 A1 | `Shown`이 읽을 수 있는 본문을 그대로 넘김, 새 liveness `ReadableOursCleaned`. 대조 `MutSightingGate` RED |
| P1-B clearing이 증거 덮어씀 | 3.1 `send_outcome`(불변)·`unsent_proof`(단조)·`cleanup_phase` 분리, 3.3 규칙 5a/5b/8, 3.4 정리 단계 끊김 표 | `unsentProof`·`cleanup`(표지에 남음)과 `keysSent`(메모리) 분리, 지우기 기록 → 키 → 확인을 따로 전이하고 그 사이 crash. 대조 `MutOverwriteProof` RED(키 뒤 재시작 → 빈 입력창에서 영구 보류) |
| P1-C 실행 nonce 알림 dedupe | 3.1 `hold_id`, 3.5 키 `input-hold:{execution_nonce}:{hold_id}`, v1 `hold_id` 정의, 6장 A7 | `holdId`, outbox `rows`(영구 dedupe), 보류별 `sentFor`, `SecondHold` 시나리오, `HoldReleasedOrNoticed`를 보류 단위로 강화. 대조 `MutNonceKey` RED |
| P2-A 공백 접은 digest | 3.1 `frame_sha256`·`screen_exact`, 3.3 규칙 8·9와 비교 규칙, 6장 A4 | `PersonEdits`의 `edited`가 공백 편집을 포함. 대조 `MutShapeDelete` |
| P2-B `Cancelled { confirmed > 0 }` | 1.3 끝 문장, 3.1 예약 맡김·`cancelled_before_enter`, 6장 A1 | `enterRefused` 시나리오가 `Cancelled{1}`을 함께 대표 |
| P2-C enqueue 실패 뒤 재시도 | 3.5 자체 재시도 종료 조건 = outbox row 확보, 알림 순서 ①②③ | `intent`/`enqueued` 분리, `EnqFail`. 대조 `MutStopOnIntent` RED |
| P2-D NotDurable 설명 | 3.4 표에서 `Kept`와 `NotDurable`을 나누고 `ensure_hold_absent_durable` 재시도 | 모델 밖(A9) |

### 개정 4

| 항목 | 바꾼 곳 | 모델 |
|---|---|---|
| P1-1 송신 전 상태 없음 | 0장, 3.1 `send_outcome=in_flight`와 기록 순서(① `in_flight`·`unsent_proof=none` → ② 계획 실행 → ③ 결과 원자 확정, 한 잠금 구간), 3.3 규칙 7·8과 끝 문단, 3.4 결과 기록 전 끊김 3행, 6장 A9 시험 3개와 A16 | `sendStep`(held → pasted → entered → recorded)·`writer`, `PasteKey`/`EnterKey`/`RecordOutcome`, `CrashWindow`. `in_flight`는 `EnterMayNow`. `RestartInFlight`·`RestartInFlightPaste`·`RestartInFlightSend`에서 안전 규칙 7개, `ReadableOursCleaned`, `HoldReleasedOrNoticed`가 통과한다. 대조 `MutInFlightAsRefused` → `UndeliveredIsTrue` RED |
| P2-1 5b의 정리 뒤 receipt 창 | 3.3 규칙 5b(정리 단계에서 빈 입력창을 처음 본 뒤 receipt 창)와 끝 문단, 6장 A15 | `clearReceipt`, `ClearReceiptWindow`, 5b 분기에 `ClearWaitOK`, 시나리오 `personSubmits`(`PersonSubmitsOurs`). `PersonSubmits` GREEN, 대조 `MutSkipClearReceipt` → `UndeliveredIsTrue` RED |
| P2-2 알림 단계 합침 | 3.5 알림 순서(①과 ② 사이 crash 재개, ③이 Wake를 지우지 않음), 6장 A7 `hold_resolve_crash_after_notice_intent_before_enqueue` | `Bump`(한도에서 의도 기록) → `EnqueueNotice`(키 dedupe, 실패 가능) → `RecordEnqueued`를 따로 전이. `RestartUnreadable`이 그 사이 재시작을 포함하고, 여기서 `NoticeAtMostOnce`·`HoldReleasedOrNoticed`가 통과한다. `MutStopOnIntent`·`MutNoSelfRetry`·`MutNonceKey`는 계속 RED |

개정 3에서 닫힌 P1-A·P1-B·P1-C는 다시 열리지 않는다. 대조군 `MutSightingGate`·`MutOverwriteProof`·`MutNonceKey`가 개정 3과 같은 규칙을 깨고, 대응 설계 cfg(Fixed·Restart·SecondHold)는 GREEN이다(개정 4 기준. 개정 5에서 `MutOverwriteProof`는 뺐다).

### 개정 5

| 항목 | 바꾼 곳 | 모델 |
|---|---|---|
| P1-1 in_flight 영구 보류가 DoD와 충돌 | 코디네이터 결정에 따라 DoD를 바꾸지 않고 설계를 맞춤. 0장, 3.1 끝 문단, 3.3 규칙 7(알림 row 확보 → "전달 미확인" 해제)과 끝 문단(미확인 해제 뒤 Enter가 실제로 갔던 경우), 3.4 결과 기록 전 끊김 3행 기대값과 미확인 해제 끊김 2행, 3.5 알림 문구와 증명 범위, 6장 A9 기대값과 새 시험 | Choices 규칙 7 → `ReleaseUnconfirmed`(intent → `EnqueueNotice` → `RecordEnqueued` → `Settle("unconfirmed")` → `Unlink`). `Crash`에 `inFlightTwice`(두 번째 재시작). 새 안전 규칙 `UnconfirmedNoticed`·`OriginalNotOvertaken`. `RestartInFlight`·`RestartInFlightPaste`·`RestartInFlightSend`·`RestartInFlightTwice` GREEN(HoldEventuallyReleased·LaterInputStarts 포함). 대조 `MutInFlightKeep`(개정 4 동작) liveness RED, `MutNoWriteLock` `OriginalNotOvertaken` RED |
| P2-1 receipt 창의 미래 정보 의존 | 3.3 규칙 5b(`clearing`이면 미확인 해제)와 "창은 시간만 본다" 문단, 3.4 ③~④ 행, 6장 A3·A15 | `ReceiptWindow`·`ClearReceiptWindow`에서 `landed # "pending"` 조건을 뺐다. 5b 표시: `clearing` → `ReleaseUnconfirmed`, `none`·`cleared` → "미전달". 대조 `MutLateTranscriptAfterClearReceipt`(리뷰 권고 시나리오) `UndeliveredIsTrue` RED, `PersonSubmits` GREEN |

개정 3의 P1(in_flight 누락)은 그대로 해결된 상태다(`in_flight` 시작·결과 원자 확정·`MutInFlightAsRefused` RED 유지). P1-A(`MutSightingGate` RED)와 P1-C(`MutNonceKey` RED)도 그대로다. P1-B는 계약(증거 보존)이 유지되고, 증거를 잃어도 영구 보류가 생기지 않는 쪽으로 더 강해졌다(5장).
