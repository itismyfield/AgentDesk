# TUI Relay 안정화 — 이중 트랙 계획 (rev2)

> 목적: Claude TUI / Codex TUI direct relay의 긴꼬리 버그를 통제. **Rust 단위/통합 트랙**으로 내부 로직(classifier/dedupe/chunking/offset/rehydrate)을 잠그고, **Python E2E smoke 트랙**으로 실제 Discord+tmux+restart 한 바퀴 관통만 검증.
> 통과 기준: 두 트랙 각각의 grade 조건 모두 충족.

## 0. 직전 검토(Codex) 반영 요약

| 지적 | 반영 |
|---|---|
| 시나리오 ID 카운트 불일치, 회귀 누락 | E2E 12 + Unit 18 로 재구성, 누락 7건 추가 |
| 2.3 control char "strip"은 잘못, 실제는 reject | reject path + notification strip 양쪽 분리 |
| 2.4 attachment 경로는 TUI input과 무관 | Rust unit + test-only assembler로 이전, E2E 폐기 |
| 3.1 cc 대상인데 rollout_tail은 codex | cdx로 변경, 모델 의존 줄임 |
| 3.2 정책 없이 snapshot은 anti-pattern | `docs/tui-thinking-policy.md` 선행 |
| 5.1 "재방출 0"은 rehydrate 의도와 충돌 | grace window 내 의도된 replay 허용, 그 외 0 |
| 10.1 ERROR 0은 false fail 빈발 | run_id 스코프 + allowlist 적용 |
| destructive 격리 약함 | dev(8799) 전용 + lease + pane/PID/cwd 재검증 + inflight zero guard |
| 드라이버 혼재 | 분리 (아래 §3) |
| timing-flaky 기준 약함 | sentinel-driven 전환, 별도 soak 트랙 |
| 8 Open Q | P0~P2 답안 확정 (§9) |

## 1. 코드 지도 (요약)

| Surface | 파일:라인 | 핵심 |
|---|---|---|
| Claude input | `src/services/claude_tui/input.rs:88-100` | `plan_prompt_submit`, paste 1800자, control char **reject** |
| Codex input | `src/services/codex_tui/input.rs:1-100` | rounded-box composer, fresh=120s/followup=45s |
| Notification strip | `format_ssh_direct_prompt_notification` @ `tui_prompt_relay.rs:1538`, `strip_terminal_controls` @ `:1563` | 표시용 control char strip — #2699 |
| Claude tail | `src/services/claude_tui/transcript_tail.rs` + `tui_prompt_relay.rs:247,373,584,631` | offset JSON, rehydrate grace |
| Codex tail | `src/services/codex_tui/rollout_tail.rs` | composer_ready, tool deadline 5m |
| Turn state | `src/services/tui_turn_state.rs:64-120` | envelope classifier + partial parser(Claude only) |
| Hook (Claude) | `src/services/claude_tui/hook_server.rs:1-80`, `hook_relay.rs:23-80` | Stop/SubagentStop → PROMPT_READY_NOTIFY |
| Dedupe | `src/services/tui_prompt_dedupe.rs:9-12` | TTL 10s/24h/30m, channel anchor |
| Idle recap (#2702) | `tui_prompt_relay.rs:spawn_*_idle_*_relay` | once-per-idle period |
| Explicit turn-finished (#2625) | `TurnFinishedSignal`, `mark_turn_finished_signal_done`, `ChannelMailboxRegistry::turn_finished` @ `turn_orchestrator.rs:1413,1473,1548`; 이벤트명 `turn_finished` @ `observability/emit.rs:91` | inflight cleanup |
| Stale resume / draft (#2635) | `src/services/discord/turn_bridge/stale_resume.rs:21-62` | unknown transcript recovery |
| tmux watcher | `src/services/discord/tmux_watcher.rs:11-42` | dead marker, ready sentinel |
| Discord I/O | API release 8791 | `POST /api/discord/send`, `GET .../messages` |

## 2. 두 개 트랙

### 트랙 A — Rust 단위/통합 (helper-level subset, 우선)
**위치**: 기존 컨벤션(`#[cfg(test)] mod tests` inline)을 따라 **대상 함수와 같은 모듈에 inline**으로 추가. 신규 디렉터리 `tests/unit_tui/`는 만들지 않음.
**Private 함수 접근**: `claude_tui_rehydrate_start_offset`, `read_persisted_claude_tui_relay_offset` 등이 `pub(super)`라 같은 부모 모듈의 inline test에서 호출 가능. 외부 `tests/` 통합테스트가 필요한 경우만 `pub(crate)` 승격 검토.
**원칙**: 외부 의존(Discord, tmux, launchctl) 없이 fixture jsonl + 함수 단위 호출.

### 트랙 B — Python E2E smoke (12케이스, 후속)
**위치**: `tests/e2e/tui_relay/scenarios/*.yaml`, `scripts/e2e/run_tui_relay.py`.
**원칙**: dev(8799)+전용 채널에서만 실행, 한 바퀴 관통 검증 + soak 별도 측정.

## 3. 트랙 A — Rust Unit/Integration (helper-level subset of U-1~U-19)

**커버 범위 (helper/pure-function level)**: U-1, U-2, U-3, U-4, U-5, U-6 (helper), U-7, U-8 (parser-level fixture), U-10 (advance offset), U-11 (offset edge cases), U-12 (binding accessors), U-13 (classifier negative), U-14 (provider isolation), U-17 (Claude shrink), U-19 (hook hash identity).

**트랙 B로 이동**:
- U-6 dispatch-level "marker 최대 1개" 검증 (process-wide async dispatch 의존)
- U-9 idle-dedupe end-to-end (기존 inline 다수 케이스로 helper-level은 이미 cover)
- U-15 multi-binder runtime overlap (실제 1초 race 재현 필요)
- U-16 partial-line race in tail loop (기존 `claude_partial_*`로 helper level은 이미 cover, race 시점 재현은 트랙 B)
- U-18 envelope→Discord 1:1 mapping (실제 emit 경로 의존)

| ID | 회귀 PR / 의도 | 대상 함수 | Fixture | Assert |
|---|---|---|---|---|
| U-1 paste-chunking | 기본 | `plan_prompt_submit` (멀티라인) | 코드블록 3줄 | multiline은 `[PasteBuffer(full_text), Enter]`, no-newline은 `Literal chunks(≤1800) + Enter` |
| U-2 utf8-chunking | 기본 | 동상 | "안녕👋 코드" | UTF-8 경계 분할 안 됨, 단일 PasteBuffer로 전달 |
| U-3 control-char-reject | #2699 path 분리 | `plan_prompt_submit` | `\x07\x1b[` | `Err` (reject), error variant 정확 |
| U-4 notification-strip | #2699 표시용 | `format_ssh_direct_prompt_notification` | 동상 | 출력에서 strip/escape 적용 |
| U-5 chunking-8KB | 2.4 재정의 | `plan_prompt_submit` | 8192 bytes no-newline | 1800자 이하 chunks + 단일 Enter, SHA256 일치 |
| U-6 thinking-redact | #정책 후 | `redacted_thinking_transcript_event` | thinking-only envelope | 본문 비움, status_line 1개만 |
| U-7 system-meta-filter | 3.4 | turn_state + relay filter | system/turn_duration jsonl | relay candidates에서 제외 |
| U-8 tool-use-emit-policy | 3.1 codex로 이동 | rollout_tail emit | tool_use 5+ envelope fixture | 결정적 emit/skip 정책 일관 |
| U-9 idle-dedupe | 4.1 | `tui_prompt_dedupe` | 같은 라인 N회 | PENDING_PROMPT_TTL 내 1회만 emit |
| U-10 cold-start-window | 5.2 | `register_rehydrated_tmux_runtime_binding` | 비어있는 transcript | replay 0건 |
| U-11 cold-start-grace | 5.1 reframed | `claude_tui_rehydrate_start_offset` | 마지막 user prompt 직후 transcript | grace 안 라인만 replay |
| U-12 stream-resume-offset | 5.3 일부 | Claude: `read_persisted_claude_tui_relay_offset` + `claude_tui_rehydrate_start_offset`(:523). Codex: `TuiRuntimeBinding::last_offset` + `scan_codex_idle_rollout_for_prompt`(:1128) + `tail_rollout_file_from_offset`. 기존 inline test 다수(`claude_rehydrate_start_offset_*`, `codex_idle_rollout_scan_*`) — 신규 보강 케이스만 | 청크 N개 본 jsonl + offset/binding | 미관측 청크만 emit, 중복 0 |
| U-13 stranded-draft-classify | 6.1 | `claude.rs::claude_tui_followup_stranded_prompt_draft_state`, `gently_clear_claude_tui_prompt_draft` (cb83f2ca6 추가). 기존 inline test 3건(`detects_non_busy_transcript_with_stranded_prompt_draft` 등) 존재 — 보강 케이스만 추가 | 미submit draft 흔적 | 'recoverable' 분류 정확 |
| U-14 channel-isolation | 7.2 + 7.1 | `tmux_by_provider_session` | cc/cdx 동시 binding | HashMap 키 충돌 0 |
| U-15 multi-binder-dedupe | 7.1 | dedupe state | 동일 jsonl 1초 겹친 두 reader | 라인당 1 emit |
| U-16 partial-line | 8.1 | `claude_partial_turn_state` | 반쪽 라인 + 다음 라인 | 완성 후 정확 분류 |
| U-17 compact-reanchor | 8.2 | `persist_*_pending_prompt` | /compact 후 잘린 transcript | offset 재계산, 동일 라인 0 |
| U-18 envelope-discord-mapping | 10.2 | dedupe + emit 카운터 | jsonl + 모의 emit log | 1:1 매핑, 중복/누락 0 — **트랙 B로 이동**: 실제 Discord emit 경로(`shared.discord.send_message`)는 process-wide async 의존이 강해 unit fixture로 만들기 부적절. E2E smoke E-1/E-11에서 자연 발생적으로 검증 |

**부속 트랙(이미 합의된 5건)**: 2.5 compact-burst readiness, 4.3 hook vs result race, 4.4 Codex turn.completed 누락, 9.2 fault inject 단위, **추가**: U-19 hook-hash-session-id (#2647).
**범위 명시**: 트랙 A는 **helper-level / pure-function 단위 회귀 가드**. tmux session 복구, Discord emit, 프로세스 재기동 같은 외부 의존 흐름(예: `rehydrate_existing_claude_tui_bindings` 전체 path, `stale_resume` reconnect 흐름, U-18 envelope→Discord 1:1 mapping)은 트랙 B에서 다룬다. PR 본문도 "regression-guard at unit/helper level"로 명시.

**Grade**: 모든 unit test pass + flaky 0 (50회 반복 시 변동 0). 이걸 먼저 잠근다.

## 4. 트랙 B — Python E2E Smoke (12케이스)

| ID | 회귀 PR | 채널 | 핵심 step | sentinel/assert |
|---|---|---|---|---|
| E-1 single-prompt | 기본 | cc, cdx | "ping" 송신 | jsonl `result` 1회, Discord 1메시지 |
| E-2 three-turns | 기본 | cc, cdx | 3턴 (각 result 후 다음) | 3개 분리 메시지 헤더 |
| E-3 utf8-multiline | 2.1/2.2 합침(관통) | cc, cdx | 멀티라인+이모지 | 최종 응답에 입력 echo, 깨짐 0 |
| E-4 direct-input-relay | #2697/#2683/#2669 positive | cc, cdx | SSH 경로 또는 direct send-keys | 프롬프트 알림 1 + 응답 1 |
| E-5 turn-separation | 4.2 + #2625 | cc, cdx | 응답 후 즉시 새 프롬프트 | 새 헤더 emit, inflight cleanup 신호 관측 |
| E-6 compact-recap-once | #2702 + 3.3 | cc | `/compact` → 새 프롬프트 | idle-recap 1회만, 본문 사용자메시지 오인 0 |
| E-7 codex-compact-readiness | #2695 | cdx | `/compact` 후 입력 | 입력 먹힘 0, 다음 응답 정상 |
| E-8 restart-between-turns | 5.1 reframed | cc, cdx | 응답 완료→ dcserver dev restart → 새 프롬프트 | 새 프롬프트만 emit, grace 안 replay만 허용 |
| E-9 restart-mid-stream | 5.3 sentinel | cc, cdx | 응답 sentinel(특정 marker token) 관측 후 restart | 청크 이어 emit, 중복 0 |
| E-10 stranded-draft | 6.1 | cc, cdx | 격리 pane에 send-keys (no Enter) → restart | draft 인지 알림 또는 무시 — 일관 |
| E-11 dual-channel-concurrency | 7.2 + #2691 | both | cc·cdx 동시 idle direct + 응답 | 채널 격리, 교차 오염 0 |
| E-12 tmux-pane-kill | 9.1 | cc, cdx | `tmux kill-pane` mid-stream | "세션 종료" 1회, hang 0 |

**별도 soak 트랙**: 9.2 (kill -9), 6.3 rehydrate 시간 — 10~20회 반복 후 p95 측정, 통과/실패 임계 별도.
**Grade**: 12케이스 1회 통과(green) + 직후 **고정 high-risk 5(E-5, E-8, E-9, E-11, E-12) 재실행 3회 연속** + 나머지 7케이스 랜덤 1회 추가 검증.

## 5. 시나리오 명세 포맷 (YAML, E-시리즈 전용)

```yaml
id: E-9
title: "스트리밍 중 dev 재기동"
channel: cc
isolation: required
sentinel:
  prompt: "응답 첫 줄에 '[E2E:E9:STREAM_OK]'를 출력하고 50줄 시 작성"
  marker: "[E2E:E9:STREAM_OK]"
setup:
  - assert_env: { ADK_PORT: 8799, AGENTDESK_E2E_ALLOW_DESTRUCTIVE: 1 }
  - acquire_lease: e2e-relay
  - resolve_channel: { name: adk-dash-cc, source: settings_binding }
  - send_marker: "### E2E SETUP E-9 run=${RUN_ID}"
  - wait_idle: 3
steps:
  - send_prompt: "${sentinel.prompt}"
  - wait_in_jsonl: { contains: "${sentinel.marker}", timeout_s: 30 }
  - restart_dcserver: { target: dev }
  - wait_in_jsonl: { envelope: result, timeout_s: 120 }
assertions:
  - discord_messages_in_window: { min: 1, max: 5 }
  - no_duplicate_chunks: true
  - jsonl_to_discord_coverage_ratio: { min: 0.9 }
teardown:
  - send_marker: "### E2E TEARDOWN E-9"
  - release_lease: e2e-relay
```

## 6. 격리/destructive 안전성 (보강)

1. **환경 분리**: 트랙 B 전체는 **dev(8799) + 전용 채널**(`adk-dash-cc`, `adk-dash-cdx` *for dev*)에서만 실행. release(8791)는 최종 smoke만, kill 계열 금지.
2. **Env gate**: `AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1` 없으면 destructive step 모두 abort.
3. **Lease file**: `/tmp/agentdesk-e2e-relay.lease`에 RUN_ID 기록, 동시 실행 차단.
4. **Pre-flight 재검증** (kill 직전):
   - `tmux list-panes -F '#{pane_id} #{pane_pid} #{pane_current_path}'`
   - 캡처한 pane_id, PID, cwd가 시나리오 등록값과 정확히 일치할 때만 kill.
5. **Restart 안전성**: dev dcserver inflight 중 테스트 외 채널 trace가 있으면 abort.
6. **Dry-run 모드** (`--dry-run`): 실제 send/kill/restart는 skip, 의도된 절차만 출력.
7. **Channel ID 소스**: **`agentdesk.yaml`이 canonical** source of truth (참고: `docs/source-of-truth.md:25`). `/api/discord/bindings`는 Postgres `agents.discord_channel_*`의 materialization으로 검증용. 실측 ID: `adk-dash-cc=1490141479707086938`, `adk-dash-cdx=1490141485167808532` (release config 기준).
8. **dev(8799) 환경 전제조건**: 현재 8799는 미가동 (release 8791만 가동). 트랙 B 착수 전 dev dcserver를 띄우고 별도 dash 채널/세션 매핑이 필요. **이 셋업 자체를 트랙 B 사전조건**으로 명시.

## 7. Timing 처리

| 케이스 | 변경 |
|---|---|
| 4.1 idle 5초 → E2E에서 빠짐 | dedupe 검증은 U-9 단위로. E2E에서는 다른 sentinel 활용 |
| 5.3 → E-9 | "50줄 응답 중"이 아니라 jsonl에 marker token 도달 직후 restart |
| 6.3 rehydrate 시간 | 기능검증(U-11/E-8)과 perf(soak) 분리 |
| 9.2 kill -9 ordering | soak 트랙으로 이동, p95 알림 ≤ 5s |
| E-5 turn-separation | 응답 후 100ms 대기 후 새 프롬프트(인간적 갭) — 너무 짧으면 hook race 자체가 stress test |

## 8. 산출물

- 이 문서 (`docs/plans/tui-relay-e2e-stabilization.md`)
- **신규** `docs/tui-thinking-policy.md` — 정책 본문 (raw thinking은 Discord 노출 금지, 중립 marker 1개, turn-completion 산정 제외, transcript event content blank)
- 기존 모듈 inline `#[cfg(test)] mod tests` 확장으로 트랙 A helper-level subset(위 §3 명시) 추가. 신규 디렉터리 없음.
- `tests/e2e/tui_relay/scenarios/E-*.yaml` 12파일
- `scripts/e2e/run_tui_relay.py` + `scripts/e2e/tui_relay/{discord,jsonl,tmux,launchctl,assertions,lease}.py`
- `out/e2e/tui_relay/<run_id>/{report.json, artifacts/}`
- `out/soak/{9.2,6.3}/<run_id>/report.json` (별도 soak 결과)

## 9. Open Questions 답안 (확정)

| P | Q | 답 |
|---|---|---|
| P0 | release vs dev | 트랙 B는 **dev(8799)** 전용. release는 최종 smoke만, destructive 금지 |
| P0 | 채널 ID 출처 | **`agentdesk.yaml` canonical** (참고 `docs/source-of-truth.md:25`). `/api/discord/bindings`는 Postgres `agents.discord_channel_*` materialization으로 검증용 |
| P0 | 8KB 주입 | E2E 폐기, **U-5 Rust unit + test-only assembler**로 분리 |
| P1 | thinking policy 위치 | `docs/tui-thinking-policy.md` **먼저 작성**, U-6은 policy 직접 참조 |
| P1 | unit 트랙 분리 | **트랙 A를 선행 PR**로 진행, B는 A grade 통과 후 시작 |
| P2 | ERROR 0 정의 | run_id 스코프 신규 structured ERROR만 fail. restart/kill 예상 로그 allowlist |

## 10. 실행 순서

1. `docs/tui-thinking-policy.md` 초안 작성 (정책 합의 → 잠금)
2. 트랙 A: helper-level subset(§3 명시) inline test 추가 → grade(50회 flaky 0) → PR
3. dev(8799) 전용 dash 채널 존재 확인 또는 생성
4. 트랙 B: 드라이버 + 12 YAML 작성 → baseline 1회 → fix loop → grade
5. soak: 9.2, 6.3 별도 측정
6. release(8791) 최종 smoke E-1, E-3, E-11만 1회

## 11. Stop-gap / 진행보고 규칙

- 동일 fail 3회 fix 후 재발 → 별도 follow-up 이슈 분리, 본 goal에서 분리
- 단일 시나리오 디버깅 90분 초과 → 사용자 멘션
- 트랙 B grade 통과 후 1주 운영, 새 회귀 0건 시 /goal 완수
