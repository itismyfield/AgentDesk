# #8201 herdr 입력 보류 표지: 불확실 Enter 뒤 실행이 살아 있는 동안 안전하게 푸는 판정

- 코드 기준: `934a49997e95ffb0d54ae8eed2d7653ea6c1e4bf` (origin/main, 2026-11-16). 읽기 전용 조사이며 코드·시험은 바꾸지 않았다.
- 상태 차트: [herdr-input-hold.report.md](../statecharts/input-single-path/herdr-input-hold.report.md), 모델 `docs/statecharts/input-single-path/HerdrInputHold.tla`.
- 개정 2 (2026-11-16 20시): pro 설계 리뷰 ISSUES(P1 4·P2 2)를 반영했다. 항목별 대응은 9장에 있다.
- 운영: 사건 표지 `runtime/herdr_input_holds/bf28830d9ac94338bbe904c8a0c4eb0a`는 사용자 승인(11-16 19:4x)을 받아 메인 세션이 19:41에 치웠다(삭제하지 않고 이동). 그 뒤 같은 보류는 관측되지 않았다.

## 0. 요약

- 11-16 18:05:15 KST의 `Indeterminate { confirmed: 1, cause: Refused(Herdr(PaneUnverified)) }`는 "붙여넣기는 들어갔고 **Enter는 게이트가 보내기 전에 거절했다**"는 뜻이다(`host_input.rs` `admit_after`). 원 입력은 입력창에 붙여넣은 채로 남았고, `WriteUncertain`로 retained 처리되어 다시 보내지지 않는다.
- 그 입력은 두 가지를 남긴다. 하나는 **파일 표지**(`hold`, 프로세스 재시작에도 남음)이고, 다른 하나는 **readiness 예약**(`readiness::reserve`가 만든 `Entry.pending`, 프로세스 메모리)이다. `Indeterminate`는 둘 다 정산하지 않는다(`composer.rs` `when_idle`의 `_ => ()`). 표지만 풀면 남은 예약 때문에 다음 입력이 여전히 시작하지 못한다(리뷰 P1-1).
- 설계: 표지를 읽는 자리를 **owner 효과 안, 키 직전**(`first_input.rs` `write`, composer 잠금 안)으로 옮기고 `resolve_hold`로 판정한다.
  - 전달 판정은 v2 표지의 digest가 transcript user 기록과 일치할 때만 한다.
  - 삭제는 입력창 본문이 우리 붙여넣기 digest와 **정확히 일치**할 때만 한다.
  - 해제는 **예약 정산 → 표지 삭제**를 한 단위로 묶는다.
  - 판정할 수 없으면 front를 Held로 두고 근거 변화를 감시해 다시 판정한다. 판정 실패가 2회 쌓이면 채널에 알림을 1회 보낸다(횟수는 표지에 기록).
- 불확실 자체도 줄인다(S2). Enter admit에서 일시 `PaneUnverified`가 나면 시간 상한 안에서 **전체 re-admit**(게이트 judge 전체를 다시 수행하고 새 pin을 만듦)을 한다.
- 상태 차트(26 cfg): 수정 전은 liveness RED이고 설계는 GREEN이다. 대조군 12개가 각각 정해진 규칙을 깬다. 새 대조군인 "모양만으로 삭제", "시각 기반 전달 판정", "예약 미정산 해제"도 포함한다.

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
| C8 | `src/services/herdr_launch/retired_hold.rs:79` ← `src/services/herdr_launch.rs:446` `before_fresh` | 새 incarnation 전에 retired 표지를 해제 |
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

`Indeterminate`는 P1·P2 어디에도 해당하지 않는다(`composer.rs`의 `_ => ()`). 따라서 원 Enter가 안 나간 사건 경로에서는 같은 프로세스 안에서 P3이 오지 않고, 예약이 영구히 남는다.

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
| R8 | `src/cli/herdr.rs:363` `status` | 실행별 `recorded_at` 표시 | `phase`·`unclear_passes`·`noticed_at` 추가(원문 없음) |

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

원문은 저장하지 않는다(CLI `status`의 "no path or input text is shown" 원칙). `input_holds()`·`input_held()`는 v1(RFC3339 한 줄)과 v2(JSON)를 모두 읽는다.

| 필드 | 쓰임 |
|---|---|
| `schema: 2`, `recorded_at`, `provider`, `logical_key`, `execution_nonce` | 다른 실행·채널은 판정하지 않는다 |
| `frame_digest` (공백을 접은 본문 sha256) | 입력창 본문과 transcript user 기록을 대조한다. **정확히 일치할 때만** 우리 글로 본다. 칩 번호·줄 수·`[User:` 머리는 증거가 아니다 |
| `transcript`, `dev`/`ino`, `offset_before`, `started_at` | 전달 판정은 같은 파일 identity의 `offset_before` 뒤에서만 찾는다 |
| `phase` | `before_keys` → `pasted` → `enter_attempted` → 결과 `enter_refused`(마지막 키가 보내기 전에 거절됨) / `enter_send_failed` / `paste_send_failed`(Enter 시도 없음). 이후 정리 단계 `clearing` → `cleared` |
| `reservation` | `{ boot_id, source(session·nonce·native·path), revision }`. `boot_id`는 dcserver 프로세스 시작 시각이다(`readiness` `STARTED`와 같은 값). 같은 프로세스에서만 정산 대상과 연결한다 |
| `attempt_nonce`·`message_id` | retained 원 입력과 연결(알림 문구용) |
| `unclear_passes`, `noticed_at` | 판정 실패 횟수와 알림 여부. 재시작 뒤에도 남는다 |

같은 프로세스에서 `Indeterminate`가 나면 `when_idle`은 `Reservation`을 버리지 않는다. 대신 프로세스 안 레지스트리 `HELD_RESERVATIONS[nonce]`에 맡긴다(제안). 레지스트리는 메모리라 재시작하면 비며, 그때는 정산할 예약도 없다(P5).

### 3.2 판정 자리와 owner 경계

- **P1 (키 직전)**: R1/R2의 `not_held(nonce)?`를 지우고, `first_input.rs` `write`에서 `hold()` 앞, `with_composer_mutation_lock` 안에서 `resolve_hold`를 부른다. 이 자리는 owner 효과(`submit_owned`의 `submit` 클로저, `capture_preparation`) 안이다. 그래서 보류를 유지할 때 `before_payload(..., "herdr_input_hold_pending")`를 써서 `SubmissionError::Held`를 내고, `owner_input::record_preparation_watch`도 owner scope 안에서 등록된다. front는 Held로 남고 `start_failed`로 가지 않는다. 새 non-Opaque 오류 종류를 `start_failure.rs`에 더하지 않는다. 그렇게 하면 `kind != Opaque` 규칙으로 즉시 retained가 되기 때문이다(`queued_head/v2/start_failure.rs` `start_failed`).
- owner가 아닌 legacy `submit` 경로(`owner_input::active()`가 false)는 지금처럼 거절한다. 운영 herdr Claude는 owner 경로다.
- **P2 (busy 주입 뒤)**: busy `Adapter::submit`이 `Applied`로 끝나고 같은 nonce의 표지가 있으면, receipt 스레드가 그 프레임의 transcript 기록을 확인한 다음 P1과 같은 판정을 한 번 수행한다.
- **P3 (깨움)**: 3.5.

### 3.3 판정 규칙 (`resolve_hold`, 제안)

선행 조건: composer 잠금 안에서, 게이트가 실행을 확인해야 한다(같은 nonce·root·provider, `HerdrTarget::execution_confirmed`). 표지의 `execution_nonce`·`logical_key`가 현재 행과 같아야 한다. 아니면 판정하지 않는다(보류 유지, 키 0).

| 순서 | 관측 | 판정 | 원 입력 |
|---|---|---|---|
| 1 | provider가 턴을 실행 중(원 입력이든 다른 입력이든) | 대기 | — |
| 2 | **v2만**: 같은 파일 identity의 `offset_before` 뒤 user 기록 본문 digest = `frame_digest` | **전달됨 → 해제** (예약은 그 턴의 terminal이 P3로 이미 정산함) | 다시 보내지 않음. "전달 확인" 표시 |
| 3 | 판독 불확실: capture 실패, `ComposerOwner::Unread`, `draft_sighting` ≠ `Settled`(stash 보임·PasteExpand 꼬리), modal | 보류 유지, `unclear_passes` + 1 | — |
| 4 | 붙여넣기 정착 창이 지나지 않음 (`recorded_at` + 정착 창) | 대기. `enter_refused`도 이 대기는 건너뛰지 않는다. Enter 미송신은 붙여넣기 반영 완료를 증명하지 않는다 | — |
| 5 | 입력창이 `draft_sighting` = `Settled`(빈 입력창, stash 없음, 정상 꼬리)이고 **미전달 증명**이 있음: v2이며 `phase` ∈ {`enter_refused`, `paste_send_failed`} 또는 `phase` = `cleared` | **해제 단위**(3.4) | 다시 보내지 않음. "미전달 확인" |
| 6 | 입력창 `Settled` + v1 표지 | 해제 단위(v1은 배포 재시작 뒤에만 만나므로 정산할 예약이 없다) | 전달 여부를 말하지 않음(unknown) |
| 7 | 입력창 `Settled` + v2 `enter_send_failed` + 증명 없음 | receipt 창 안이면 대기. 지나면 보류 유지와 `unclear_passes` + 1(전달 불명을 빈 화면으로 확정하지 않음) | — |
| 8 | 입력창 본문이 `frame_digest`와 **정확히 일치**(v2만, 칩은 본문을 볼 수 없으니 일치 불가) | `phase=clearing`을 기록하고 fsync → 키 직전 재판독(여전히 정확히 일치할 때만) → 게이트를 거친 지우기 → 재판독해서 `Settled`이면 `phase=cleared` → 같은 패스에서 5로 | 손실 없음(retained에 같은 원문) |
| 9 | 그 밖: 사람 초안, 우리 글에 사람이 덧붙인 글, 칩, v1 표지에 남은 글 | 보류 유지, `unclear_passes` + 1, 키 0 | — |

- v1 표지는 2(전달 판정)와 8(삭제)을 쓰지 않는다(리뷰 P1-2·P1-4). 시각은 검색 범위 힌트로도 쓰지 않는다.
- 다른 입력 B가 기록 시각 뒤에 제출되어도 digest가 다르므로 전달 증거가 아니다. stash 복귀 전에는 3(stash 보임)이 막고, 복귀 뒤에는 8이 정확한 일치로 처리한다.

### 3.4 해제 단위: 예약 정산 → 표지 삭제 (리뷰 P1-1)

모두 같은 composer 잠금 안에서 한다.

1. 증거를 다시 확인한다(3.3의 5 또는 6).
2. **예약 정산**: `HELD_RESERVATIONS[nonce]`가 있고 그 `boot_id`가 현재 프로세스와 같으면, 그 예약만 `Reservation::settle_unsent(UnsentProof)`(제안, `not_sent`과 같은 revision 일치 규칙)로 해제한다. revision이 다르면(다른 실제 제출이 P4로 바꿨으면) 아무것도 하지 않는다. 다른 source·다른 입력의 예약과 `recovery` 울타리는 건드리지 않는다. 전역 ledger 초기화는 하지 않는다.
3. **표지 삭제**: `release_hold`(unlink + 디렉터리 fsync). `NotDurable`/`Kept`면 보류 유지로 보고 다음 패스에서 다시 시도한다.
4. 잠금을 놓고 owner에 Wake를 보낸다. 다음 front 시도가 `when_idle`에서 새 예약을 잡는다.

| 끊기는 지점 | 결과 |
|---|---|
| 2 전에 crash | 메모리 예약은 사라짐(P5). 표지가 남아 부팅 뒤 owner가 front를 다시 몰면 5/6에서 재판정(이미 `cleared`면 바로) |
| 2와 3 사이 crash | 위와 같음. 표지가 남아 있으므로 다른 입력이 그 사이에 들어갈 수 없다 |
| 2 성공, 3 실패(fsync 등) | 예약은 정산됐고 표지는 남음 → 다음 입력은 P1 판정에 머무르고, 같은 증거로 3만 다시 수행. 키 0 |
| 3 뒤 crash | 정상 해제. 부팅 뒤 기존 재시작 readiness 계약(P5)을 따른다 |

순서를 반대(삭제 → 정산)로 하지 않는 이유가 있다. 삭제 직후 잠금 밖에서 다른 경로가 표지를 "없음"으로 읽을 수 있다. 잠금 안이라도 3이 2보다 먼저 성공하고 2가 빠지면(구현 실수) 모델의 `NoSettleReservation` 반례처럼 다음 입력이 계속 막힌다.

### 3.5 깨움과 알림 1회 (리뷰 P1-3)

- **감시 탐침**: 보류를 유지할 때 등록하는 `PreparationWatch` 탐침은 "입력창이 비어 Ready"가 아니라 **판정 근거 변화**를 본다. 직전 판정의 지문(판독 가능 여부, 입력창 분류 `Settled`/정확 일치/사람/Unread, stash 유무, 같은 nonce transcript의 digest 기록 유무와 길이, 정착·receipt 창 경과, 게이트 확인 여부)이 바뀌면 `Screen::Ready`를 돌려 `held_watch`가 owner를 Wake하게 한다. Wake는 판정을 다시 돌릴 뿐 입력 권한이 아니다. 같은 `write` P1 판정이 다시 결정한다. 실행이 사라지면 `Screen::Gone`.
- **자체 재시도**: `noticed_at`이 없는 동안에는 판정 실패 뒤 근거 변화가 없어도 탐침이 재시도 간격(예: 30 s) 뒤 Ready를 돌린다. 그래서 두 번째 판정에 반드시 도달한다.
- **알림 1회**: `unclear_passes`가 2에 닿고 `noticed_at`이 없으면 먼저 표지에 `noticed_at`을 기록·fsync한 뒤 채널 알림을 outbox에 넣는다(새 source, 예: `input_hold_notice`, LOOPBACK 정책. `retained_input_notice`와 같은 계열이며 outbox key = nonce로 dedupe). 기록이 알림보다 먼저이므로 재시작해도 두 번 나가지 않는다. 기록 뒤 enqueue 전에 crash하면 알림 0회가 될 수 있으므로, outbox key를 nonce로 두고 부팅 뒤 판정 패스에서 "기록됨·미enqueue"를 다시 enqueue한다. dedupe가 중복을 막는다.
- **알림 이후**: 근거 변화 감시만 남는다. 판독이 회복되거나 사람이 글을 비우면 자동으로 이어진다.
- **알림의 성격**: 사용자에게 상태를 알리는 채널 메시지다. 예: "앞선 입력이 입력창에 남아 있거나 확인할 수 없어 다음 메시지가 기다리고 있습니다. 입력창이 비거나 다시 읽히면 자동으로 이어집니다." 명령·버튼·수동 해제 기능은 없다. 기존 `agentdesk herdr status/retire` 외에 새 수동 조작을 만들지 않는다.

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
| dcserver 재시작 | 표지는 남고 예약은 사라짐(P5), `recovery` 울타리 | 표지 v2(`phase`·`unclear_passes`·`noticed_at`)로 재판정. owner의 front 복구가 P1을 다시 부른다 |
| 부팅 재연결 패스 | R6가 수만 셈 | 키 0 유지 |
| 새 incarnation | C8 | 변화 없음. 행이 없는 표지는 표시만 |
| herdr 서버 재시작/pane 교체 | 게이트 거절, C6/C7 | 판정하지 않음(선행 조건 실패) → 행 끝 경로 |
| tmux 복귀(R5) | 표지가 있으면 거절 | 후속 |

이 경계들은 문서상 기존 동작 유지이며, 모델이 그 교차를 검증한 것은 아니다(6장).

## 5. 상태 차트 결과 (`HerdrInputHold.tla`, 26 cfg)

| 구분 | cfg | 깨지는 규칙 |
|---|---|---|
| 수정 전 | Before | HoldEventuallyReleased, HoldReleasedOrNoticed, LaterInputStarts (**RED**) |
| 수정 전 | BeforeEnds | LaterInputStarts |
| 설계 | Fixed, LoadSpike, EnterSendFail, PasteSendFail, PersonLeaves, PersonEdits, StashOther, Restart | 없음 (**GREEN**) |
| 설계(증명 불가, 예상) | PersonStays, Unreadable, RestartUnreadable, Legacy | HoldEventuallyReleased, LaterInputStarts(보류 유지, `HoldReleasedOrNoticed`·`NoticeAtMostOnce`는 통과) |
| 대조군 | MutShapeDelete | NoHumanTextDeleted |
| 대조군 | MutTimeReceipt | DeliveredIsTrue, NoReleaseOnResidue, LaterInputStarts |
| 대조군 | MutNoSettleReservation | LaterInputStarts |
| 대조군 | MutEmptyOnlyWake | HoldEventuallyReleased, HoldReleasedOrNoticed, LaterInputStarts |
| 대조군 | MutNoSelfRetry | HoldEventuallyReleased, HoldReleasedOrNoticed, LaterInputStarts |
| 대조군 | MutVolatileCounter | NoticeAtMostOnce, HoldEventuallyReleased, LaterInputStarts |
| 대조군 | MutOpaqueFailure | LaterInputStarts |
| 대조군 | MutSkipPasteSettle | NoReleaseOnResidue, NoPasteOverResidue, LaterInputStarts |
| 대조군 | MutResendNoSettle | OriginalAtMostOnce, UndeliveredIsTrue, NoReleaseOnResidue, NoPasteOverResidue, LaterInputStarts |
| 대조군 | MutReleaseOnUnread, MutReleaseOnResidue | NoReleaseOnResidue, LaterInputStarts |
| 대조군 | MutIgnoreTranscript | HoldEventuallyReleased, LaterInputStarts |

## 6. 모델 밖 계약 → S1 수용 시험 (리뷰 P2-1)

모델 GREEN은 아래 계약을 **가정**한다. 각 계약은 S1(또는 표시한 조각)의 Rust 시험으로 증명한다. 시험 이름은 후보다.

| # | 모델이 가정하는 것 | 증명할 시험 |
|---|---|---|
| A1 | 실제 `readiness::reserve`가 만든 예약이 `Indeterminate` 뒤 레지스트리에 맡겨지고, 해제 단위가 그 예약만 정산한다 | `hold_resolve_enter_refused_from_real_reserve_clears_paste_settles_reservation_next_input_submits_once` (같은 프로세스에서 reserve → paste 성공 → Enter 거절 → 정리 → 해제 → 후속 입력 1회). 대조: `..._without_settle_next_input_stays_blocked` |
| A2 | 정산은 revision이 맞는 이 입력의 예약만 건드린다 | `hold_resolve_settle_ignores_other_source_and_superseded_revision` |
| A3 | 붙여넣기 정착 창 ≥ 화면 반영, receipt 창 ≥ transcript 기록 지연 | `hold_resolve_enter_refused_waits_paste_settle_before_reading_empty`, `hold_resolve_enter_send_failed_waits_receipt_window` |
| A4 | "우리 글"은 본문 digest 정확 일치뿐이다 | `hold_resolve_v1_prefix_with_person_suffix_sends_no_key`, `hold_resolve_v2_chip_or_same_line_count_other_body_sends_no_key`, `hold_resolve_v2_exact_body_clears_then_rereads_empty` |
| A5 | 전달 판정은 v2 digest 일치뿐이다 | `hold_resolve_v1_later_user_record_is_never_delivery`, `hold_resolve_stash_submit_other_then_return_is_not_receipt` |
| A6 | 근거 변화가 Wake를 만들고, 판정 실패 1회 뒤에도 자체 재시도로 두 번째 판정에 닿는다 | `hold_resolve_unread_then_readable_wakes_without_new_message`, `hold_resolve_persistent_unread_notices_once_retained_zero_keys_zero` |
| A7 | `unclear_passes`·`noticed_at`이 재시작 뒤에도 남고 알림이 중복되지 않는다 | `hold_resolve_notice_survives_restart_without_duplicate`, `hold_resolve_noticed_unenqueued_reenqueues_once_after_boot` |
| A8 | 보류 유지는 Held front + 감시이지 start failure가 아니다 | `hold_resolve_kept_hold_is_held_front_not_start_failed`, `hold_resolve_watch_registered_inside_owner_scope` |
| A9 | 해제 단위의 순서와 crash 재개 | `hold_resolve_crash_between_settle_and_unlink_resumes_after_boot`, `hold_resolve_unlink_failure_keeps_hold_and_sends_no_key` |
| A10 | 게이트 확인 실패·다른 nonce·Pending에서는 키를 보내지 않는다 | `hold_resolve_unconfirmed_execution_judges_nothing`, `hold_resolve_other_nonce_hold_untouched`, `hold_resolve_pending_source_sends_no_key_before_resolve` |
| A11 | busy 주입 뒤 판정(P2) | `busy_inject_applied_then_resolve_releases_on_digest_receipt` |
| A12 | 재시작 뒤 readiness는 기존 `recovery` 계약을 따르고 판정이 그것을 바꾸지 않는다 | `hold_resolve_after_restart_leaves_recovery_fence_untouched` |
| A13 (S2) | Enter 전체 re-admit 정책 | `enter_readmit_read_failed_then_success_sends_one_enter`, `enter_readmit_stops_on_admission_marker_closing_or_witness_change`, `enter_readmit_forbidden_gap_never_retries`, `enter_readmit_cancel_during_probe_sends_no_enter`, `enter_readmit_budget_expiry_keeps_indeterminate` |
| A14 (S3) | Codex 첫 입력: `expected_session_id`/`expected_transcript`가 null로 시작하고 session start 뒤 source가 확정된다. 확정 전에는 전달 판정을 하지 않는다 | `codex_hold_first_input_without_source_never_judges_delivery`, `codex_hold_bound_source_then_digest_receipt_releases` |

## 7. 조각 계획

리뷰 상한: `scripts/pr_cap_prod.py` `FILES, NET = 30, 1500`.

| 조각 | 내용 | 예상 prod 파일 / 순증 |
|---|---|---|
| **S1 Claude 판정** | 표지 v2(`herdr_turn.rs`), `Indeterminate` 예약 맡김과 `settle_unsent`(`composer.rs`, `readiness/mod.rs`), 판정 모듈 `src/services/claude/herdr_turn/hold_resolve.rs` (제안), R1/R2 제거와 `write` 키 직전 판정(`first_input.rs`), 근거 변화 탐침과 자체 재시도, busy 뒤 P2(`busy/herdr.rs`), 정확 digest 판정(`tui_input/actor/gate.rs` 옆), 알림 source(outbox source·registry·`retained_notice.rs` 계열), CLI·health 표시. 시험 A1~A12 | 약 13~15개 / +950 |
| **S2 Enter 재판독** | `Plan::step` Enter re-admit(`host_input.rs`), 일시 gap 표지(`herdr_gate.rs`, `pane_probe.rs`), 마지막 send witness 기록, info 로그. 시험 A13 | 약 4~5개 / +250 |
| **S3 Codex** | R3/R4·W2/W3·C3/C4에 같은 판정 적용. 첫 입력의 source 미확정 단계(A14), Codex 판독기(접힌 `[Pasted Content N chars]`는 증명 불가 → 보류·알림), rollout user 기록 digest | 약 8~10개 / +550 |

S1이 운영 증상을 고친다. S2는 공유 게이트를 건드리므로 따로 리뷰한다. S3는 S1의 provider 중립 핵심 위에 Codex source 확정 단계를 더하므로 판독기 추가보다 범위가 넓다.

## 8. 운영 메모

- 사건 표지(v1)는 사용자 승인으로 19:41에 이동 처리됐고, 이후 재발이 관측되지 않았다. 배포 전에 새로 생기는 v1 표지는 S1 배포 뒤 3.3의 6(빈 입력창이면 해제)이나 9(남은 글이면 보류·알림)로 처리된다. v1에서는 자동 삭제나 전달 판정을 하지 않는다.

## 9. 리뷰 대응 (개정 2)

| 항목 | 바꾼 곳 | 모델 |
|---|---|---|
| P1-1 예약 미정산 | 1.1 W4, 1.3 P1~P5, 3.1 `reservation`, 3.4 해제 단위·crash 표 | `reserved`, `Settle`→`Unlink`(`lock`), `Paste`가 `~reserved` 요구. 대조 `MutNoSettleReservation` RED |
| P1-2 모양 기반 삭제 | 3.1 `frame_digest`, 3.3 규칙 8·9, 8장 | `PersonEdits`(덧붙임 = `edited`), `NoHumanTextDeleted`. 대조 `MutShapeDelete` RED |
| P1-3 Empty-only 깨움·알림 보장 | 3.2 owner 경계, 3.5 근거 변화 탐침·자체 재시도·알림 1회 | `trigger`/`watch`/`Wake`, 표지에 남는 `unclear`·`noticed`, `notices` 계수. `NoticeAtMostOnce`, `HoldReleasedOrNoticed`. 대조 `MutEmptyOnlyWake`·`MutNoSelfRetry`·`MutVolatileCounter`·`MutOpaqueFailure` RED |
| P1-4 시각 기반 전달 | 3.3 규칙 2(v2 digest만), v1 제외 | `StashOther`(C-s → B 제출 → 복귀), `DeliveredIsTrue`. 대조 `MutTimeReceipt` RED |
| P2-1 모델 범위 | 6장 계약 A1~A14와 시험 후보, 4장 끝 문장 | 정착/receipt 창 분리(`pasteSettled`/`receiptSettled`), `enter_refused`의 정착 생략 금지(대조 `MutSkipPasteSettle`), 해제 잔여물을 `Unhold` 한 곳에서 실제 상태로 계산 |
| P2-2 S2 정책 | 2장 표(조건·금지·전체 re-admit·witness·횟수/간격/상한·취소) | 모델 밖(A13) |
