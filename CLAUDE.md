# AgentDesk — AI 에이전트 온보딩

이 파일은 **포인터 문서**다. 실제 내용은 아래 원본 문서에 있으니 여기에 복제하지 말 것(드리프트 방지).

## 코드 수정 전 필독

- **어디를 건드릴지 결정표**: [`docs/agent-maintenance/change-surfaces.md`](docs/agent-maintenance/change-surfaces.md) — 변경 표면별 필수 동반 수정·검증을 정의. 프로덕션 라인수는 [`docs/generated/module-inventory.md`](docs/generated/module-inventory.md)가 진실값.
- **agent-maintenance 인덱스**: [`docs/agent-maintenance/index.md`](docs/agent-maintenance/index.md)
- **아키텍처 개요**: [`ARCHITECTURE.md`](ARCHITECTURE.md)
- **릴레이 불변식(디스코드 릴레이 상태 계약)**: [`docs/relay-state-contract.md`](docs/relay-state-contract.md)

## CI 게이트 (로컬에서 순서대로 통과시킬 것)

1. `cargo fmt` → `cargo fmt --check` 클린
2. `cargo check --lib` 클린 + 관련 `cargo test --lib <module>` 통과
3. `python3 scripts/generate_inventory_docs.py` 실행 후 `python3 scripts/check_agent_maintenance_docs.py`가
   `agent-maintenance freshness check passed`를 낼 때까지 반복
   - production line count 불일치 → `change-surfaces.md`를 module-inventory 진실값에 맞춤
   - `multinode-transition.md must be touched` → `docs/agent-maintenance/multinode-transition.md`의 `### Audited touches`에 노트 추가
   - DB migration 추가 시 `migrations/postgres/immutable-checksums.json` checksum 등록

## 핫파일 / 대형 파일 규칙

- **#3016 코어 핫파일은 동시 작업 금지, 한 번에 하나만**: `turn_bridge/mod.rs`, `tmux_watcher.rs`, `session_relay_sink.rs`, `turn_finalizer.rs`.
- **대형 파일(giant) 레지스트리**: [`scripts/giant_file_registry.toml`](scripts/giant_file_registry.toml) — 1000줄(giant threshold) 초과 파일은 등록·데드라인 관리 대상. 신규 모듈은 <1000 prod줄 설계가 기본.
