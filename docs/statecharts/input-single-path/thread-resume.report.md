# 검증 바인딩 없는 Claude 스레드 cold resume 상태 차트 — #8372

코드 기준: **main `4c3ff0b0b7` + 구현 커밋 `44be986d8e`·`c36873023a`·`42f266b870`**(`wt/i8372`). `ThreadIngress.tla`에 `ResumeSpec` 하위 명세를 붙였다. 기존 `Spec`·`HerdrSpec`에는 `UNCHANGED rvars`만 더했고 새 CONSTANT는 없다. 모델 GREEN은 구현·운영 수용 완료가 아니다.

범위: 예약 정책 아래에 있는 late-adopt Claude 스레드 child(`claude_thread.rs` `launch_policy`: Deferred 또는 parked) 하나와 사람 입력 2건. adoption이 Committed로 넘어간 child는 예약 대상이 아니라 이 투영 밖이다. 답 게시·세션 저장·훅 검증 기록은 `RAnswer` 한 전이로 묶여 있어, 재시작 중 중복 게시 부재까지는 이 모델이 보이지 않는다.

## 진입점과 모델 값

| 모델 값·행동 | 코드 근거 | 뜻 |
|---|---|---|
| `InitialSession="unboundStored"` (`rstored=process`, `rbinding=none`) | 루틴 process 실행이 스레드 세션 행을 자기 세션으로 바꿈: `completion_postlude.rs` → `channel_writeback.rs` `persist_provider_session` | 헤드리스 루틴 결과 스레드(E1) |
| `InitialSession="boundStored"` (`tui`/`hook`) | TUI pane 훅이 남긴 검증 기록: `claude_fold.rs` `verified_claude_sources` | 사람·Tmux 루틴 스레드(E4) |
| `InitialSession="noneStored"` | 새 스레드 첫 입력, `/clear` 뒤 | 저장 세션 없음(E6) |
| `RLaunch` → `RRefuse` | `tui_session_launch.rs` `reserve(.., ResumePane::Cold)` → `claude_fold.rs` `reserve_thread_resume` "has no verified binding" → 원문 보관 알림 | 현재 코드의 거절 |
| `RLaunch` → `RFresh` (`RFix="fresh"`) | `unverified_thread_resume.rs` `apply`(intake `session_preparation.rs` `restore` 끝, `headless_turn.rs` 복원 블록 뒤) → `claude_thread.rs` `cold_resume_unverified` | 수리: 세션을 비우고 fresh 시작, `rnotice="fresh"` 알림 1회 |
| `rprompt="withRecent"` | 세션이 None일 때만 최근 쌍을 싣는 `channel_recent_context.rs` `load_channel_recent_context` | fresh 첫 프롬프트의 최근 대화 |
| `RWarm` | `claude.rs` `resolve_live_claude_tui_session`의 live pane 경로. 판정은 pane이 없다고 관측될 때만 참(`probe_tmux_session_pane_liveness`) | 살아 있는 pane 입력은 바뀌지 않음 |
| `RRoutineNext` | 다음 루틴 process 실행이 세션 행을 다시 바꿈 | 예약 대상 child에서만 다시 fresh |
| `RRestart` → `uncertain` → `RShowFailure` | 재시작 때 제출된 입력은 재전송 없이 실패 알림으로 끝남 | 보이는 실패 |
| `RClear`, `RIdleReap` | `/clear`, idle 회수 | 인접 경로 |

## 규칙

- INVARIANT `RNoSilentLoss`: 보관이면 다시 보내라는 알림이 있다.
- INVARIANT `RNoSilentFailure`: 실패로 끝난 입력에는 사람에게 보이는 실패 알림이 있다(`rshown`).
- INVARIANT `RAnswerOnce`: 입력당 답 게시 ≤1, 답 게시 ⇔ 1.
- INVARIANT `RVerifiedResumeKept`: 검증 바인딩이 있으면 fresh로 바꾸지 않는다.
- INVARIANT `RVerifiedMeansObserved`: 검증 기록은 훅 관측만 만든다.
- INVARIANT `RFreshCarriesRecent`: 저장 세션을 대체한 fresh 시작(`rnotice="fresh"`)의 첫 프롬프트는 `withRecent`다.
- PROPERTY `RPersonServed`: 보관 대기 입력은 결국 답 게시 또는 알림이 보인 실패로 끝난다. "다시 보내 주세요" 보관은 끝으로 치지 않는다.

## 결과 (TLC 1.7.4, statechart 도구)

| cfg | distinct 상태 | 결과 |
|---|---:|---|
| ThreadResumeBefore (현재 코드, E1) | 191 | **RED** `RPersonServed` |
| ThreadResumeFresh (수리, E1) | 297 | GREEN |
| ThreadResumeBoundRoutineBefore (현재, E5) | 275 | RED `RPersonServed` |
| ThreadResumeBoundFresh (수리, E5) | 330 | GREEN |
| ThreadResumeBoundNoRunCurrent / Fresh (E4) | 120 / 120 | GREEN / GREEN |
| ThreadResumeNoneFresh (E6·`/clear`) | 208 | GREEN |
| ThreadResumeRecord (버린 안 b) | 초기 상태 | RED `RVerifiedMeansObserved` |
| ThreadResumeRecordLegacy (안 b, 배포 전 스레드) | 267 | RED `RVerifiedMeansObserved`, `RPersonServed` |
| ThreadResumeRecordLegacyLive | 267 | RED `RPersonServed` |
| ThreadResumeFreshAll (변이: 검증 바인딩이 있어도 fresh) | 6 | RED `RVerifiedResumeKept` |
| ThreadResumeFreshLate (변이: launch 층에서만 fresh) | 6 | RED `RFreshCarriesRecent` |
| ThreadResumeFreshNoRecent (변이: 최근 대화 주입 제거) | 6 | RED `RFreshCarriesRecent` |
| ThreadResumeSilentFail (변이: 거절이 알림 없이 실패로 끝냄) | 191 | RED `RNoSilentFailure`, `RPersonServed` |
| ThreadResumeFreshSilentRestart (변이: 재시작 실패 알림 없음) | 297 | RED `RNoSilentFailure`, `RPersonServed` |

차트: [현재 코드](thread-resume-before.chart.mmd), [수리](thread-resume-fresh.chart.mmd). view: `thread-resume.view.mjs`.

## 반례와 코드 대조

`ThreadResumeBefore`의 `RPersonServed` 반례(9걸음): process/none 저장 → 재시작·루틴 재실행 → 입력1 보관 대기 → cold 실행 → **원문 보관(다시 보내라는 알림)** → `/clear` → 입력2만 fresh로 답 → 입력1은 끝까지 보관. 코드에서 같은 전이는 `tui_session_launch.rs`의 cold 예약 → `claude_fold.rs`의 "has no verified binding" → `start_failure.rs`의 owner_start_failed → `retained_notice.rs`의 보관 알림이고, 운영 로그의 스레드 `1574279895679504388` 입력 세 건과 같다. 출구가 `/clear`뿐인 것은 `session_preparation.rs`의 cleared 분기와 `tui_session_launch.rs`의 `if resume && …`로 확인했다.

새 변이 두 개의 반례: `ThreadResumeSilentFail`은 입력1이 cold 실행에서 알림 없이 `failed`가 되는 3걸음, `ThreadResumeFreshNoRecent`는 입력1의 fresh 시작이 `rprompt="none"`인 채 `rnotice="fresh"`가 되는 3걸음이다. 두 규칙을 넣기 전 모델은 이 변이를 GREEN으로 통과시켰다.
