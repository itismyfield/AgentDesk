# AgentDesk — AI 에이전트 온보딩

이 파일은 **포인터 문서**다. 실제 내용은 아래 원본 문서에 있으니 여기에 복제하지 말 것(드리프트 방지).

## 코드 수정 전 필독

- **어디를 건드릴지 결정표**: [`docs/agent-maintenance/change-surfaces.md`](docs/agent-maintenance/change-surfaces.md) — 변경 표면별 필수 동반 수정·검증을 정의. 프로덕션 라인수는 [`docs/generated/module-inventory.md`](docs/generated/module-inventory.md)가 진실값.
- **코드 주석 언어 정책**: [`docs/agent-maintenance/comment-language-policy.md`](docs/agent-maintenance/comment-language-policy.md) — 신규·수정 코드 주석 및 문서 주석의 작성 언어와 적용 범위.
- **agent-maintenance 인덱스**: [`docs/agent-maintenance/index.md`](docs/agent-maintenance/index.md)
- **아키텍처 개요**: [`ARCHITECTURE.md`](ARCHITECTURE.md)
- **릴레이 불변식(디스코드 릴레이 상태 계약)**: [`docs/relay-state-contract.md`](docs/relay-state-contract.md)

## 로컬 사전 점검 (머지 전 최소 순서)

CI가 강제하는 것을 로컬에서 미리 통과시키는 순서. **권위 있는 CI 정의는
[`.github/workflows/ci-pr.yml`](.github/workflows/ci-pr.yml)와
[`scripts/ci-script-checks.sh`](scripts/ci-script-checks.sh)** (실제 CI는 `cargo check --workspace --all-targets`,
`cargo fmt --all --check`, docs 게이트 `generate_inventory_docs.py --check` +
`check_agent_maintenance_docs.py --warning-only --line-count-gate`로 더 넓다).

1. `cargo fmt` → `cargo fmt --check` 클린
2. `cargo check --lib` 클린 + 관련 `cargo test --lib <module>` 통과 (CI는 워크스페이스 전체)
3. `python3 scripts/generate_inventory_docs.py` 실행 후 `python3 scripts/check_agent_maintenance_docs.py`가
   `agent-maintenance freshness check passed`를 낼 때까지 반복
   - production line count 불일치 → `change-surfaces.md`를 module-inventory 진실값에 맞춤
   - `multinode-transition.md must be touched` → `docs/agent-maintenance/multinode-transition.md`의 `### Audited touches`에 노트 추가
   - DB migration 추가 시 `migrations/postgres/immutable-checksums.json` checksum 등록

## 핫파일 / 대형 파일 규칙

- **동시 작업 금지 핫파일 (한 번에 하나만 — #3016 동시성 규칙)**: `turn_bridge/mod.rs`,
  `tmux_watcher.rs`, `session_relay_sink.rs`, `turn_finalizer.rs`. 이 파일들을 건드리는 작업은
  병렬 금지·순차 단독. (파일별 도입/추적 이슈는 상이 — 예: `session_relay_sink.rs`는 #3036/#3405 계열.
  여기서 #3016은 "동시 편집 금지" 운영 규칙을 가리킨다.)
- **대형 파일(giant) 레지스트리**: [`scripts/giant_file_registry.toml`](scripts/giant_file_registry.toml) — 1000줄(giant threshold) 초과 파일은 등록·데드라인 관리 대상. 신규 모듈은 <1000 prod줄 설계가 기본.
