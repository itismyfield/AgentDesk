# AgentDesk Test Plan

> 패치 시 regression 방지를 위한 전체 기능 테스트 케이스 목록
> 총 250+ 핵심 케이스 (우선순위별 정리)

---

## 실행 레이어

| Layer | 목적 | 기준 파일/경계 | 실행 명령 |
| --- | --- | --- | --- |
| `unit` | 직렬화, 저장, mailbox state 같은 로컬 불변식 검증 | `src/services/discord/*` 개별 테스트 모듈 | 모듈별 `cargo test --bin agentdesk <filter>` |
| `state-transition integration` | DB + policy + dispatch 상태 전이 검증 | `src/integration_tests.rs` | 기본 gate는 `cargo test --all-targets` |
| `failure-recovery` | restart / reconcile / outbox / delayed-worker 복구 경계 검증 | `src/integration_tests/tests/high_risk_recovery.rs` | `cargo test --bin agentdesk high_risk_recovery::` |

### 현재 고정 실행 경로

- broad regression: `cargo test --all-targets`
- high-risk recovery gate: `cargo test --bin agentdesk high_risk_recovery::`
- restart / boot reconcile만 재현: `cargo test --bin agentdesk high_risk_recovery::failure_recovery::`
- outbox boundary만 재현: `cargo test --bin agentdesk high_risk_recovery::outbox_boundary::`
- delayed worker / watchdog만 재현: `cargo test --bin agentdesk high_risk_recovery::delayed_worker::`

고위험 coverage inventory와 `existing vs missing` 매핑은 `docs/high-risk-recovery-lane.md`가 기준 문서다.

---

## P0: 재시작/복구 시나리오 (반드시 통과)

### Restart During Active Turn
- `restart_during_turn_saves_inflight_state` — turn 진행 중 재시작 → inflight JSON 저장됨
- `restart_recovery_resumes_completed_turn` — 재시작 후 완료된 turn 출력 Discord에 전달
- `restart_recovery_reattaches_watcher_if_tmux_alive` — tmux 살아있으면 watcher 재연결
- `restart_generation_gating_skips_old_state` — 이전 generation inflight 건너뛰기
- `restart_pending_drains_all_turns_first` — restart 대기 중 모든 turn 완료 후 재시작
- `restart_report_saved_on_graceful_shutdown` — graceful shutdown 시 report 저장
- `restart_deferred_until_active_turn_completes` — 활성 turn 완료까지 재시작 지연

### Inflight State Lifecycle
- `inflight_save_atomic_write` — temp 파일 → rename으로 atomic write
- `inflight_serialization_all_fields` — 모든 필수 필드 직렬화 (channel_id, provider, generation, offset, placeholder_id)
- `inflight_load_by_provider_and_channel` — provider + channel_id로 정확한 파일 로드
- `inflight_stale_cleanup_over_5min` — 5분 이상 오래된 inflight 파일 자동 제거
- `inflight_malformed_json_graceful_skip` — 손상된 JSON → 건너뛰기 (crash 안 함)
- `inflight_provider_mismatch_skip` — 다른 provider의 inflight 무시

### Handoff State
- `handoff_save_and_load_roundtrip` — 저장 → 로드 일관성
- `handoff_dedup_prevents_double_execution` — 같은 handoff 중복 실행 방지
- `handoff_ttl_10min_auto_cleanup` — 10분 후 자동 정리

---

## P0: 메시지 라우팅 (핵심 기능)

### Bot Message Filtering
- `ignore_bot_messages_unless_allowed` — 봇 메시지 기본 무시
- `accept_allowed_bot_messages` — allowed_bot_ids에 포함된 봇 메시지 허용
- `allowed_bot_bypass_auth_check` — 허용된 봇은 auth 우회

### Mention Filtering
- `ignore_mentions_to_other_users` — 다른 유저 멘션 메시지 무시
- `accept_direct_mention_to_self` — 자기 멘션만 처리
- `accept_no_mention_messages` — 멘션 없는 일반 메시지 처리

### Provider Routing
- `cc_suffix_routes_to_claude` — `-cc` 채널 → Claude provider
- `cdx_suffix_routes_to_codex` — `-cdx` 채널 → Codex provider
- `dm_routes_to_any_provider` — DM → 모든 provider 지원
- `role_binding_overrides_suffix` — role binding이 suffix보다 우선
- `unsupported_provider_silently_skips` — 미지원 provider는 무시

### Auth
- `first_user_imprinted_as_owner` — 첫 사용자 = owner
- `owner_allowed_user_ids_authorized` — allowed_user_ids 인증 통과
- `unauthorized_user_rejected_with_message` — 미인증 → 거절 메시지

### Thread Handling
- `thread_inherits_parent_channel_role` — 스레드 → 부모 채널 role binding 상속
- `thread_session_bootstrap_from_parent` — 부모 세션 경로 복사

### Intake Dedup
- `dispatch_id_based_dedup` — dispatch_id 기반 중복 제거
- `hash_based_dedup_for_regular_messages` — 일반 메시지 hash dedup
- `dedup_cache_ttl_cleanup` — TTL 만료 후 캐시 정리

---

## P0: Turn Bridge (메시지 → AI → 응답)

### Turn Lifecycle
- `turn_creates_cancel_token` — turn 시작 시 cancel token 생성
- `turn_increments_global_active_counter` — 활성 카운터 +1
- `turn_creates_placeholder_in_discord` — Discord에 "..." placeholder 생성
- `turn_edits_placeholder_with_final_response` — 완료 후 placeholder 수정
- `turn_decrements_counter_on_completion` — 완료 시 카운터 -1

### CLI Execution
- `claude_cli_path_resolution` — `which claude` → fallback 체인
- `codex_cli_path_resolution` — `which codex` → fallback 체인
- `stream_parser_init_event` — Init 이벤트에서 session_id 추출
- `stream_parser_text_accumulation` — Text 이벤트 누적
- `stream_parser_result_completion` — result 이벤트 = 완료

### Response Formatting
- `chunk_respects_2000_char_limit` — Discord 2000자 제한
- `chunk_preserves_code_block_boundaries` — 코드 블록 분할 방지
- `chunk_handles_multibyte_korean` — 한국어 다중바이트 정상 처리
- `long_response_multi_message` — 긴 응답 → 여러 메시지

### Timeout & Cancel
- `watchdog_cancels_stuck_turn` — 타임아웃 → turn 취소
- `stop_command_sends_cancel_signal` — /stop → cancel token 발동
- `intervention_queue_during_turn` — 진행 중 메시지 큐잉
- `intervention_dedup_same_message` — 같은 메시지 intervention dedup

---

## P1: Slash Commands

### /start
- `start_explicit_path_creates_session` — 경로 지정 → 세션 생성
- `start_empty_path_auto_workspace` — 빈 경로 → workspace 생성
- `start_tilde_expansion` — `~/path` 확장
- `start_validates_path_exists` — 없는 경로 → 에러
- `start_restores_existing_session` — 기존 세션 복구
- `start_worktree_conflict_creates_new` — git worktree 충돌 → 새 worktree

### /stop
- `stop_cancels_active_turn` — 활성 turn 취소
- `stop_kills_tmux_session` — tmux 세션 종료
- `stop_no_active_returns_message` — 활성 없으면 메시지

### /clear
- `clear_cancels_ai_and_wipes_history` — AI 취소 + history 삭제
- `clear_removes_session_files` — 세션 파일 제거
- `clear_drains_queue` — intervention 큐 비우기

### /down
- `down_absolute_path_downloads` — 절대 경로 다운로드
- `down_relative_resolves_from_cwd` — 상대 경로 → cwd 기준
- `down_rejects_directory` — 디렉토리 거절

### /shell
- `shell_executes_command` — 커맨드 실행
- `shell_inherits_cwd` — 작업 디렉토리 상속
- `shell_captures_stdout_stderr` — stdout/stderr 캡처

### /cc (Skill)
- `cc_builtin_health_no_ai` — builtin skill은 AI 호출 없음
- `cc_skill_with_args` — skill 인자 전달
- `cc_requires_active_session` — 세션 필수
- `cc_rejected_during_turn` — turn 중 거절

### /adduser, /removeuser
- `adduser_owner_only` — owner만 가능
- `removeuser_owner_only` — owner만 가능
- `adduser_prevents_duplicate` — 중복 방지

### /model
- `model_get_shows_current` — 현재 모델 조회
- `model_set_overrides` — 모델 override
- `model_clear_resets` — override 제거

---

## P1: Settings & Config

### Config Domains
- `patch_config_entries_rejects_readonly_metadata` — read-only runtime metadata like server_port cannot be written
- `put_settings_pg_is_full_replace_and_strips_retired_company_keys` — `kv_meta['settings']` is full-replace; callers must merge any hidden keys they intend to preserve, while retired legacy keys are stripped server-side (see `src/server/routes/settings_tests.rs`)

### bot_settings.json
- `token_hash_sha256_correct` — SHA256 해시 계산 정확
- `token_hash_reproducible` — 같은 토큰 → 같은 해시
- `load_settings_parses_all_fields` — 모든 필드 파싱
- `save_settings_atomic_write` — atomic write
- `string_encoded_ids_accepted` — 문자열 ID 파싱 ("123" → 123)

### org.yaml
- `load_org_schema_parses_agents` — agents 파싱
- `suffix_map_provider_lookup` — suffix_map → provider 매핑
- `tilde_expansion_in_paths` — ~ 확장
- `missing_org_yaml_falls_back` — 없으면 fallback

### role_map.json
- `role_binding_by_channel_id` — channel ID 매핑
- `role_binding_by_channel_name` — channel name 매핑
- `org_schema_priority_over_role_map` — org.yaml 우선

---

## P1: Tmux Management

### Session Lifecycle
- `tmux_create_with_provider_prefix` — `AgentDesk-claude-channelname` 형식
- `tmux_name_sanitization` — 특수문자 제거
- `tmux_owner_marker_write` — owner marker 파일 생성
- `tmux_ownership_check` — 현재 runtime 소유권 확인

### Output Watcher
- `watcher_tails_output_file` — output 파일 tail
- `watcher_resumes_from_offset` — offset 기반 재개
- `watcher_relays_to_discord` — 새 데이터 Discord 전달
- `watcher_stops_on_session_death` — 세션 죽으면 정지
- `watcher_respects_pause_epoch` — pause epoch 변경 감지

### Cleanup
- `orphan_session_detection_and_cleanup` — orphan 세션 감지/정리
- `restore_watchers_after_restart` — 재시작 후 watcher 복구

---

## P1: Prompt Builder

- `system_prompt_includes_discord_context` — Discord 채널/유저 정보
- `system_prompt_includes_cwd` — 현재 경로
- `system_prompt_includes_file_send_command` — 파일 전송 명령어
- `system_prompt_injects_shared_prompt` — 공유 agent 프롬프트
- `system_prompt_injects_role_prompt` — 역할별 프롬프트
- `system_prompt_includes_longterm_memory` — long-term memory 카탈로그
- `system_prompt_includes_peer_agent_guidance` — peer agent 가이드
- `system_prompt_missing_file_graceful` — 누락된 프롬프트 파일 → crash 안 함

---

## P1: Meeting System

- `meeting_create_and_start` — 회의 생성 + 시작
- `meeting_round_progression` — 라운드 진행
- `meeting_transcript_accumulation` — 발언 기록 누적
- `meeting_completion_saves_summary` — 완료 시 요약 저장
- `meeting_cancel_mid_round` — 진행 중 취소

---

## P2: Health & Metrics

### Health Check Server
- `health_server_binds_to_port` — 포트 바인드
- `health_returns_200_when_ok` — 정상 → 200
- `health_returns_503_when_unhealthy` — 비정상 → 503
- `health_includes_uptime_and_counters` — uptime/counter 포함

### /api/discord/send Endpoint
- `send_routes_via_announce_bot` — announce 봇으로 메시지 라우팅
- `send_validates_json` — JSON 검증

---

## P2: PCD Integration

- `parse_dispatch_id_from_message` — `DISPATCH:uuid` 추출
- `build_session_key_hostname_tmux` — `hostname:tmux-session` 형식
- `derive_session_info_max_60_chars` — 60자 제한
- `post_session_status_graceful_on_failure` — PCD 다운 시 경고만

---

## P2: Error & Edge Cases

### Network
- `discord_api_timeout_handled` — API timeout 처리
- `discord_rate_limit_retry` — rate limit 재시도
- `gateway_disconnect_auto_reconnect` — 자동 재연결

### File System
- `missing_runtime_dir_auto_create` — 디렉토리 자동 생성
- `permission_denied_graceful_error` — 권한 거부 → 에러 메시지
- `atomic_write_partial_recovery` — 부분 write 복구

### Concurrency
- `concurrent_intake_dedup` — 동시 메시지 dedup
- `concurrent_settings_rw` — 설정 동시 읽기/쓰기
- `cancel_token_race_condition` — cancel token 경쟁 조건

---

## 테스트 실행 구조

```
tests/
├── TEST_PLAN.md          ← 이 문서
├── unit/                 ← 단위 테스트 (각 모듈별)
│   ├── router_test.rs
│   ├── settings_test.rs
│   ├── formatting_test.rs
│   ├── provider_test.rs
│   ├── inflight_test.rs
│   ├── handoff_test.rs
│   ├── org_schema_test.rs
│   └── pcd_test.rs
├── integration/          ← 통합 테스트 (실제 Discord API mock)
│   ├── turn_lifecycle.rs
│   ├── restart_recovery.rs
│   ├── multi_channel.rs
│   └── session_persistence.rs
└── e2e/                  ← E2E 테스트 (실제 서버 + API)
    ├── main.rs           ← 테스트 바이너리 엔트리
    ├── smoke_test.rs     ← ✅ 서버 기동 → health → CRUD → settings → cleanup
    └── restart_test.rs   ← 기동 → turn → kill → 재기동 → 복구
```

현재 운영 중인 recovery lane 모듈 경계는 `src/integration_tests/tests/high_risk_recovery.rs`이며, 위 디렉터리 구조는 장기적인 분리 방향을 설명한다.
