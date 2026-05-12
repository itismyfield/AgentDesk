# AgentDesk 기술부채 종합 감사 — 2026-05-12

**감사 방식:** 6개 영역 병렬 read-only Explore 에이전트
**대상 코드량:** Rust 579 파일 / 약 445,573줄 + 정책/스크립트/대시보드
**감사 영역:** Discord layer, Server/HTTP, Dispatch/Engine/Pipeline, Voice/Recovery, Services/DB/Config, Scripts/Dashboard/Policies

---

## 1. 한눈에 보는 결론

가장 심각한 구조 이슈는 **메가 파일 집중**이다. 6개 영역 모두에서 단일 파일이 5개 이상의 책임을 동시에 지는 패턴이 반복적으로 발견됐다.

| 메가 파일 (Top 10, 줄수) | 영역 |
|---|---|
| `src/server/routes/routes_tests.rs` (31,548) | Server |
| `src/integration_tests.rs` (12,628) | Tests |
| `src/services/discord/router/message_handler.rs` (7,873) | Discord |
| `src/services/discord/mod.rs` (6,181) | Discord |
| `src/services/discord/tmux.rs` (5,996) | Discord |
| `src/server/routes/docs.rs` (5,587) | Server |
| `src/services/onboarding.rs` (5,287) | Services |
| `src/services/discord/health.rs` (5,110) | Discord |
| `src/services/discord/recovery_engine.rs` (5,029) | Discord |
| `src/cli/doctor/orchestrator.rs` (4,603) | CLI |

**전 영역 공통 안티패턴 5종**
1. **메가 파일 SRP 위반** — 단일 파일에 라우팅 + 비즈니스 + 영속성 + 직렬화 동거
2. **`unwrap()`/`expect()` 남용** — 특히 credential, observability, voice, server middleware
3. **하드코딩 산재** — 타임아웃, 백오프, 경로, 채널 ID, codec feature
4. **상태 머신 암묵화** — recovery, voice auto-join, dispatch cancel 등 명시 enum 없음
5. **에러 타입 불일치** — `String` 에러 vs `Result<_>`가 같은 모듈 안에서 혼재

---

## 2. 통합 우선순위 액션 매트릭스

### P0 (즉시 / 기술부채 폭증의 진원지)

| # | 액션 | 영역 | 시작점 | 노력 | 예상 효과 |
|---|---|---|---|---|---|
| 1 | `routes_tests.rs` 31,548줄 8개 도메인별 분할 | Server | `src/server/routes/routes_tests.rs` | L (2주) | CI 빌드 40% 단축, 병렬화 가능 |
| 2 | `policies/auto-queue.js`(1,582) + `merge-automation.js`(2,501) + `kanban-rules.js`(839) 분할 (#1078) | Policies | `policies/lib/` | M (1-2주) | 정책 변경 속도 2배, 테스트성 +50% |
| 3 | `services/onboarding.rs` 5,287줄 → `onboarding/{draft, provider, channel_mapping, validation}` | Services | `src/services/onboarding.rs` | L (2주) | 단일 파일 5,287→1,500줄, 책임 명확화 |
| 4 | `services/observability/mod.rs` 3,946줄을 `events/`+`metrics/`+`sinks/`+`queries/`로 계층 분리 | Services | `src/services/observability/mod.rs` | L (2주) | sink 교체 용이, 글로벌 state 제거 |

### P1 (다음 1-2 스프린트)

| # | 액션 | 영역 | 시작점 | 노력 |
|---|---|---|---|---|
| 5 | `dispatch_context.rs` 4,033줄 → `ReviewContextBuilder` trait + `WorktreeResolver`/`TargetRepoResolver` 추출 | Dispatch | `src/dispatch/dispatch_context.rs` | L |
| 6 | `dispatch_create.rs:739-950` 170줄 mega 함수 4단계 분할 (`validate`/`resolve`/`build`/`persist`) | Dispatch | `src/dispatch/dispatch_create.rs:739` | M |
| 7 | `router/message_handler.rs:228-310` watchdog deadlock 로직 → `watchdog_prealert.rs` 추출 | Discord | `src/services/discord/router/message_handler.rs:228` | M |
| 8 | `discord/mod.rs:2035-2200` mailbox 보일러플레이트(140줄) 매크로/trait로 통합 | Discord | `src/services/discord/mod.rs:2035` | M |
| 9 | High-Risk Recovery 상태 머신 명시화 (`enum RecoveryPhase { Pending, Investigating, Claimed, Resolved, Orphaned }`) | Voice/Recovery | `src/high_risk_recovery.rs` | L |
| 10 | Voice Receiver 모듈 분리 (EventHandler / WavWriter / SessionManager) | Voice | `src/voice/receiver.rs:286` | L |
| 11 | Voice provider 매핑 가드(#2054 v6) 분산 lock 기반 전환 | Voice | `src/voice/...` | M |
| 12 | Server `compose_api_router` `.nest()` 그룹화 + `AppError → StructuredError{code,message,context}` | Server | `src/server/routes/mod.rs:253` | M |
| 13 | Server middleware: Bearer token 검증 → `TokenValidator` struct + Extractor 기반 validation | Server | `src/server/auth.rs:71` | S-M |
| 14 | Escalation/Kanban handler에서 비즈니스 로직 → Service 계층 추출 | Server | `escalation.rs`, `kanban.rs` | M |
| 15 | Config validator 모듈 추출 + `test_config_builder()` 헬퍼로 43회 unwrap 제거 | Config | `src/config.rs` | M |
| 16 | Credential `unwrap()` 9회 제거 (path traversal 검증 후 panic 가능 경로) | Security | `src/credential.rs` | S |
| 17 | Dashboard 상태 통일: `useAgentManagerController` → `AgentManagerContext` (Context API) | Dashboard | `dashboard/src/components/agent-manager/` | M |
| 18 | Pipeline 캐시: YAML 파싱 결과 in-memory 캐싱 | Pipeline | `src/pipeline.rs` | S |

### P2 (백로그)

| # | 액션 | 영역 |
|---|---|---|
| 19 | `engine/mod.rs` 2,590줄 → `PolicyEngine` / `HookExecutor` / `TransitionDrainer` 분리 | Engine |
| 20 | `cli/doctor/orchestrator.rs` 4,603줄 → `doctor/{checks, fixes, reporters}` 모듈화 | CLI |
| 21 | `tmux.rs` stall detection → `tmux_stall_detection.rs` 분리 | Discord |
| 22 | Voice TTS retry/timeout/sleep 상수 → `RetryPolicy` enum + config | Voice |
| 23 | Dispatch cancellation cascade: 부모→자식 취소 전파 | Dispatch |
| 24 | DB row 파싱: `kanban_cards/mod.rs:283-310`의 `unwrap_or(None)` 28회 → 스키마 typed parser | DB |
| 25 | `_sqlite_test`/`_pg` 이중 경로 통합 (dispatch_context.rs:1600-1800) | Dispatch |
| 26 | 환경변수 중앙 registry (`envvars.rs`) — `AGENTDESK_ROOT_DIR`, `AGENTDESK_CONFIG` 등 산개 통합 | Config |
| 27 | Scripts: `_http-client.sh`(retry/timeout) + `_service-manager.sh`(launchctl wrapper) | Scripts |
| 28 | `scripts/deploy-release.sh` 하드코딩 제거 (`com.itismyfield.agentdesk`, `$HOME/ObsidianVault/...`) | Scripts |
| 29 | OpenAPI: `routes/docs.rs` 5,587줄 OpenAPI spec 전용 파일로 분리 | Server |
| 30 | turn_bridge `json_has_bool_key` → `serde_json::pointer()` 사용 | Discord |

### P3 (장기 / 정리성)

| # | 액션 | 영역 |
|---|---|---|
| 31 | Voice symphonia mp3 codec → `feature(voice-mp3)` gate | Voice |
| 32 | `expand_tilde` 중복 제거 (stt.rs / tts/mod.rs / tts/edge.rs) → `voice::utils` | Voice |
| 33 | Migration backwards compat 정책 문서화 (`migrations/POLICY.md`) | DB |
| 34 | Routines 진입점 통일 (7개 routine 구조 일관화) | Routines |
| 35 | `pending_queue_item_serde_backward_compatible()` 제거 시점 확정 | Discord |

---

## 3. 30/60/90일 권장 로드맵

### 30일 (P0 4건 집중)
- **W1-2:** `routes_tests.rs` 분할 (Server) + Policies giant files 분할 (#1078)
- **W3-4:** `onboarding.rs` 모듈 분리 + `observability` 계층 분리

→ **결과:** 메가 파일 4개 제거, CI 빌드 시간 단축, 정책 변경 속도 2배

### 60일 (P1 핵심 8건)
- **W5-6:** `dispatch_context.rs` ReviewContextBuilder 추출 + `dispatch_create_internal` 4단계 분할
- **W7-8:** Discord watchdog 추출 + mailbox 보일러플레이트 매크로화
- **W9-10:** High-Risk Recovery 상태 머신 + Voice Receiver 모듈 분리
- **W11-12:** Server router 그룹화 + StructuredError 도입 + Credential `unwrap()` 제거

→ **결과:** Top 10 메가 파일 중 8개 정리, 상태 머신 2개 명시화, 보안 핫스팟 차단

### 90일 (P1 잔여 + P2 시작)
- Dashboard Context API 통일
- Engine/Doctor/Tmux 모듈화
- Scripts 추상화 (`_http-client.sh`, `_service-manager.sh`)
- 환경변수 registry, Pipeline 캐시

---

## 4. 영역별 상세 보고

### 4.1 Discord Layer

**SRP 위반 Top 5**

| 파일 | 줄수 | 책임 | 분리 제안 | 우선순위 |
|---|---|---|---|---|
| `router/message_handler.rs` | 7,873 | 메시지 라우팅, dispatch trigger, response format, watchdog deadlock alert | watchdog 추출 + response 위임 | High |
| `mod.rs` | 6,181 | bot lifecycle, session, queue, mailbox, metrics | `runtime_lifecycle.rs` 추출, queue persistence 분리 | High |
| `tmux.rs` | 5,996 | watcher lifecycle, output parsing, tmux session, stall detection | watcher submodule + `tmux_output_parser.rs` | Med |
| `recovery_engine.rs` | 5,029 | recovery dispatch, terminal notify, session restore, mailbox recovery | terminal delivery 분리, dispatch 로직 문서화 | Med |
| `turn_bridge/mod.rs` | 4,625 | turn completion guard, memory lifecycle, output lifecycle, recovery text | memory + output 형제 모듈로 분리 | Med |

**핵심 부채:** `unwrap()`/`expect()` 14건, mailbox 보일러플레이트 140줄+, 상태 머신 문서화 부재. 미해결 이슈 #1074 / #1446 / #2011 코드에 산재.

### 4.2 Server / HTTP Layer

**SRP 위반 Top 5**

| 파일 | 줄수 | 책임 분산 | 분리 제안 | 우선순위 |
|---|---|---|---|---|
| `routes_tests.rs` | 31,548 | 단일 파일에 306개 test fn | 도메인별 8개 파일로 분할 (payment/workflow/auth 등) | P0 |
| `escalation.rs` | 2,391 | 스케줄 + DB + 웹훅 | `Escalation{Settings,Webhook,DB}Service` | P1 |
| `routes/mod.rs` | 3,818 | PolicyTick + 잠금 관리 + Router 조합 | `compose_api_router` 모듈화, PolicyTick → BG service | P0 |
| `kanban.rs` | 3,311 | 상태 + 분석 + 영속성 혼재 | `KanbanService` 고도화 | P1 |
| `docs.rs` | 5,587 | OpenAPI 생성 + 라우트 정의 혼재 | OpenAPI spec 전용 파일 | P2 |

**핵심 부채:** 48개 routes 파일 전반에 `unwrap()`/`expect()`, error envelope이 단순 JSON wrapping에 머무름, review_verdict 상태 전이 로직 3개 파일에 중복.

### 4.3 Dispatch / Engine / Pipeline / Supervisor

**SRP 위반 Top 5**

| 파일 | 줄수 | 책임 | 분리 제안 | 우선순위 |
|---|---|---|---|---|
| `dispatch_context.rs` | 4,033 | 워크트리 해석, 리뷰 컨텍스트, 이슈/PR 해석, DB, git 검증, provider 해석 | `ReviewContextBuilder`(800), `WorktreeResolver`(600), `TargetRepoResolver`(400), `ExecutionTargetValidator` | P1 |
| `dispatch_create.rs` | 3,785 | 디스패치 생성, 카드 검증, 윤곽선 결정, 세션 친화성, outbox, 부모 체인 | `DispatchValidation`(300), `SessionAffinityStrategy`(200), `OutboxOrchestration` | P1 |
| `engine/mod.rs` | 2,590 | JS runtime, 정책 엔진, hook 실행, intent drain, transition, actor | `PolicyEngine`(1200), `HookExecutor`(400), `TransitionDrainer` | P2 |
| `supervisor/mod.rs` | 867 | 런타임 감독, 고아 추적, KV, 정책 엔진 업그레이드, 의사결정 | `OrphanDetector`(300), `SupervisorDecisionEngine` | P2 |
| `pipeline.rs` | 2,125 | YAML 파싱 + 계층 병합 + 게이트 검증 + 정책 오버라이드 + 직렬화 | `PipelineResolver`(600), `GateValidator`, `OverrideApplier` | P2 |

**핵심 부채:** `dispatch_create_internal` 170줄 거대 함수, `_sqlite_test`/`_pg` 이중 경로, supervisor `String` 에러 vs `Result` 혼재, dispatch cancel 부모 체인 전파 없음.

### 4.4 Voice / Recovery

**SRP 위반 Top 5**

| 파일 | 줄수 | 책임 | 분리 제안 | 우선순위 |
|---|---|---|---|---|
| `voice/receiver.rs` | 881 | PCM 수신 + WAV 기록 + 세션 관리 + Discord 이벤트 | `EventHandler` / `WavWriter` / `SessionManager` | P1 |
| `voice/tts/mod.rs` | 486 | 진행 캐시 + 동시성 + 파일 작업 | `ProgressCacheLayer` + `ConcurrencyController` | P2 |
| `voice/stt.rs` | 719 | 명령 실행 + Whisper + 저음량 감지 + 재시도 | `CommandRunner` / `VolumeAnalyzer` / `RetryPolicy` | P2 |
| `voice/commands.rs` | 834 | 정규식 + 라우팅 + Wake word | `VoiceParser` / `CommandRouter` / `WakeWordEngine` | P3 |
| `reconcile.rs` | 1,867 | 상태 동기화 + 고아 감지 + 타임아웃 + DB 쿼리 | `ReconciliationStateMachine` / `OrphanDetector` / `QueryBuilder` | P2 |

**핵심 부채:** 하드코딩 8건 (sleep 10/50/130ms, STT timeout, 저음량 임계값), F3/F16 주석에 표시된 spawn_blocking 다중 진입 / synth_task abort cleanup 누락, expand_tilde 3중복.

### 4.5 Services / DB / Config / Credential

**SRP 위반 Top 5**

| 파일 | 줄수 | 책임 | 분리 제안 | 우선순위 |
|---|---|---|---|---|
| `services/onboarding.rs` | 5,287 | HTTP 핸들러 + Draft 영속성 + Provider 검증 + Discord 채널 매핑 + 토큰 검증 | `onboarding/{draft, provider_check, channel_mapping, validation}` | P0 |
| `services/observability/mod.rs` | 3,946 | Metric 정의 + Event 수집 + Sink + PG 쿼리 + Quality ranking | `events/`, `metrics/`, `sinks/`, `queries/` | P0 |
| `cli/doctor/orchestrator.rs` | 4,603 | 진단 오케스트레이션 + 체크 + Fix + JSON + 검증 | `doctor/{checks, fixes, reporters}` | P1 |
| `config.rs` | 2,946 | 파일 로드 + YAML + env 합성 + 스키마 + 픽스처 | validation layer + 스키마 분리 | P1 |
| `services/observability/mod.rs` (cont.) | — | retention sweep + counter snapshot + 불변식 + 경고 | `policies/`, `retention/` | P1 |

**핵심 부채:** Credential `unwrap()` 9회 (path traversal 검증 후 panic 가능), kanban_cards `unwrap_or(None)` 28회, Migration 정책 문서화 부재(52개 마이그레이션), `ADK_OBSERVABILITY_*_RETENTION_DAYS` 환경변수 정의되었으나 기본값만 사용.

### 4.6 Scripts / Dashboard / Policies

**SRP 위반 Top 5**

| 파일 | 줄수 | 책임 | 분리 제안 | 우선순위 |
|---|---|---|---|---|
| `policies/auto-queue.js` | 1,582 | Phase gate + dispatch + lifecycle + error recovery | phase-gate-verdict / dispatch-activation / lifecycle / error-recovery | P0 |
| `policies/merge-automation.js` | 2,501 | PR merge check + conflict + GitHub API + Discord notify | github-pr-adapter / merge-conflict-resolver / notification-dispatcher | P0 |
| `dashboard/components/SettingsView.tsx` | 3,832 | UI + API + validation + state + persistence | `SettingsForm` / `SettingsValidator` / `useSettingsPersistence` | P1 |
| `dashboard/components/agent-manager/KanbanTab.tsx` | 3,882 | Kanban + card + dispatch + timeline + auto-queue panel | `KanbanBoardController` / `CardEditor` / `DispatchTracker` / `TimelineRenderer` | P1 |
| `scripts/deploy-release.sh` | 1,048 | Build + sign + copy + health-check + cluster + manifest + cleanup | `binary-signer` / `artifact-promoter` / `health-verifier` / `cluster-deployer` | P1 |

**핵심 부채:** 정책 파일 26개에서 `agentdesk.db.query()` 126회 직접 호출 (DB facade 부재), `deploy.sh`/`deploy-release.sh` 중복 로직, 하드코딩 (`com.itismyfield.agentdesk`, `$HOME/ObsidianVault/...`), e2e 테스트가 정책/대시보드 배포 검증 안 함.

---

## 5. 안전성 / 보안 핫스팟 (별도 트랙)

| 위치 | 위험 | 권고 |
|---|---|---|
| `src/credential.rs` (9건) | path traversal 검증 후에도 `unwrap()` panic 가능 | `Result<_>` 변환, S |
| `src/services/observability/events.rs:216` | `HOME` env 미존재 시 fallback만, 명시 설정 없음 | config 명시화, S |
| `src/voice` provider 매핑 가드 (#2054 v6) | ephemeral lock — 다중 dcserver race 가능 | 분산 lock(Redis/DDB), M |

---

## 6. 메트릭 / 관측성 갭

- `voice/metrics.rs`는 기초만: synthesis_ms / cache_hit_ratio / codec_fail_count 미노출
- `observability/mod.rs` retention 환경변수 미사용 (테스트 외)
- Dispatch 취소 전파 추적 메트릭 없음 (고아 감지 의존)

---

## 7. 부록: 미해결 이슈 참조

`#747` POLICY_TICK_TIMEOUT_COUNT, `#1074`/`#1446`/`#2011` Discord 운영, `#1078` Policies giant file 면제, `#2054` Voice auto-join 시리즈 (v5/v6/v7).

---

**감사자:** 6개 병렬 Explore 에이전트
**감사 완료:** 2026-05-12
**다음 권장:** 30일 로드맵의 P0 4건 우선 진행
