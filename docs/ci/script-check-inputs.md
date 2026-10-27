# Script checks 입력과 자체 시험 선택

`scripts/ci-script-checks.sh`의 현재 소스 검사(a)는 모든 PR에서 실행한다. 검사기·CI 배선·스크립트 자체 시험(b)은 PR 변경 목록과 입력이 겹칠 때 실행한다. 입력을 확정하지 못한 혼합 묶음과 실제 Rust 소스에 대한 독립 oracle는 a로 유지한다. 현재 73묶음(a 56, b 17)이며 과거 76묶음에서 퇴역한 시험을 복원하지 않는다.

선택 입력의 정본은 `scripts/ci/script_check_inputs.py:SELF_TEST_INPUTS`다. 표의 b 경로에는 다음 공통 입력을 항상 더한다. 넓은 scripts/tests glob은 import·source·fixture 복사의 전이 의존성을 보수적으로 포함한다.

`scripts/ci-script-checks.sh`, `scripts/ci/script_check_inputs.py`, `tests/test_script_check_inputs.py`, `.github/workflows/**`

PR의 changes job이 기존 단일 `dorny/paths-filter@v3` step의 JSON 파일 목록을 전달한다. comment-only gate의 정확한 필터 집합에도 자체 시험 입력 필터를 포함한다. 루트 `*`와 하위 `*/**`를 OR해 새 디렉터리·숨김 경로도 포함한다. 파일의 존재 여부로 거르지 않으므로 삭제도 선택한다. action은 rename의 이전/이후 이름을 모두 제공한다([v3 구현](https://github.com/dorny/paths-filter/blob/v3/src/main.ts#L183-L195)).

PR 이외의 push·schedule·workflow_dispatch, changes 실패/skip, 누락/빈/잘못된 JSON 목록에서는 b 전부를 실행한다. helper 실행 실패도 PR에서 전부 실행으로 되돌린다. b를 건너뛰면 묶음 banner 아래 `skipped: 입력 변경 없음 (<입력 경로들>)` 한 줄이 남는다. shard job은 계속 실행되며 기존 CI required 집계는 세 job의 성공을 요구한다.

main은 기존 세 shard에서 a+b 전체를 실행한다. nightly는 기존 정적 검사를 유지하고 `SCRIPT_CHECK_SELF_TESTS_ONLY=1`로 b 전체를 실행한다. schedule을 허용하지 않는 giant inventory evaluator까지 새로 호출하지 않기 위한 모드다. 이 모드는 자식 시험에 상속하지 않는다. nightly의 자체 시험 목록을 읽을 수 없거나 목록이 비면 실패한다. `SCRIPT_CHECK_LIST=1`의 전체 shard/TAB/title 출력은 유지한다.

## 측정과 예상 절감

`gh run view <id> --json jobs`의 Run script checks step 시각과 원시 로그의 연속 묶음 banner 시각을 사용했다. 최신 성공 실행 40277510358(head `9f2436a141c90a391654eff71451969d6a83b7a4`, 2026-10-26)의 guards 2051초/contracts 1227초/cargo 346초가 기준이다. 마지막 묶음은 API 시각 정밀도로 약 ±1초 오차가 있다. 이슈 기준 실행 40145107435는 guards 1576초/contracts 1770초/cargo 628초였다.

| shard | b 입력이 모두 무관할 때 예상 절감 | 근거 |
|---|---:|---|
| guards | 183.5초 | 기존 자체 시험 9개 합 |
| contracts | 약 406.3초 | 기존 자체 시험 7개 합; shell 정적 시험 약 1.2초는 유지 |
| cargo | 0초 | 실제 source/registry 의존을 분리하지 못한 묶음 유지 |

위 값은 동일 실행의 묶음 시간을 더한 예상치이며 실제 PR 감소 실측은 머지 뒤 두 실행에서 확인한다. 404.179초인 Fast compile 계약은 실제 Rust inventory/module walk를 읽어 a로 유지했다. 퇴역한 Retired PR lane tooling의 과거 259.025초는 절감에 포함하지 않는다. shell 묶음은 과거 실행 raw 기준 396.136초(이슈 본문의 234초와 다름), 최신 374.504초였다. 아래 표는 이를 source shell 3개 약 1.192초와 나머지 373.312초로 나눴다.

## 묶음 표

실측 초는 최신 성공 실행 기준이다. 신규 선택 계약은 로컬 Python 3.14/Bash 3.2 실행 3.9초이며 CI 시간은 미측정이다. a 입력 열의 tree는 현재 checkout을 뜻하며 경로 gate에 사용하지 않는다. 근거는 runner 호출부와 기존 의존 감사에서 확인한 시험의 실제 파일 읽기다.

| shard·묶음 | 분류 | 입력 경로 | 실측 초 | 근거 |
|---|---|---|---:|---|
| guards: shellcheck scripts | a | `**/*.sh` | 22.507 | `scripts/ci-script-checks.sh:251`; 현재 *.sh 전체 lint |
| guards: PG audit guard | a | `src/**`; `docs/generated/pg-audit-checklist.md`; `scripts/**` | 0.023 | `scripts/ci-script-checks.sh:265`; 현재 PG audit checklist 및 Rust PG 경로 |
| guards: Writer namespace Windows exact-target runner fixtures (#5670) | b | `scripts/**`; `tests/**` | 30.282 | `scripts/ci-script-checks.sh:269`; 합성 Rust fixture |
| guards: Postgres migration checksum guard | a | `migrations/postgres/**`; `scripts/**`; `tests/**` | 1.860 | `scripts/ci-script-checks.sh:273`; 현재 migrations/postgres/** checksum |
| guards: message_outbox validated-insert guard (#4424) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 1.098 | `scripts/ci-script-checks.sh:278`; 현재 Rust insert scanner |
| guards: Directory fsync single-path guard | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 17.004 | `scripts/ci-script-checks.sh:283`; 현재 Rust fsync scanner |
| guards: TUI output shadow write-zero census | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 0.078 | `scripts/ci-script-checks.sh:288`; 현재 Rust shadow-write census |
| guards: hrot transition sender surface | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 63.317 | `scripts/ci-script-checks.sh:292`; 현재 Rust hrot sender surface |
| guards: Alert dedupe/authority/routing wiring contract (#4448/#4449) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 0.086 | `scripts/ci-script-checks.sh:297`; 실제 Rust dedupe/routing 계약 |
| guards: State/lint hardening guard | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 0.642 | `scripts/ci-script-checks.sh:301`; 현재 Rust state/lint scanner |
| guards: Policy DB capability manifest guard (#3734) | a | `policies/**`; `scripts/**`; `tests/**` | 0.274 | `scripts/ci-script-checks.sh:306`; 현재 policies/** 및 capability manifest |
| guards: Destructive call-site per-file ratchet (#5071 T3-A4) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 285.385 | `scripts/ci-script-checks.sh:313`; src/**·baseline·manual oracle |
| guards: Reachability row-independence + change-surface gate (#5071 T4-B1) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 7.553 | `scripts/ci-script-checks.sh:319`; 실제 src/**·contract 문서; unittest도 실제 root 검사 |
| guards: Reachability judgment-authority source lint | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 32.575 | `scripts/ci-script-checks.sh:334`; 실제 reachability source fixture; test_reachability_canonical_equivalence.py:31,553 |
| guards: Idle-kill owner and timeout policy regressions (#5714) | a | `policies/**`; `scripts/**`; `tests/**` | 2.734 | `scripts/ci-script-checks.sh:340`; 실제 policies/timeouts.js 회귀 |
| guards: Operator routine scripts must stay out of git (docs/source-of-truth.md) | a | `routines/**` | 0.004 | `scripts/ci-script-checks.sh:344`; 현재 tracked routines/** |
| guards: External toolchain draft/approval/smoke tests (#4555) | b | `scripts/**`; `tests/**` | 0.197 | `scripts/ci-script-checks.sh:352`; test_toolchain_update.py:21,215 |
| guards: await_holding_lock ratchet guard | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 2.373 | `scripts/ci-script-checks.sh:356`; 현재 tracked Rust ratchet |
| guards: DeliveryJournal raw-writer allowlist | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 1.308 | `scripts/ci-script-checks.sh:361`; 현재 Rust raw-writer scanner |
| guards: Durable frontier writer per-file call-site allowlist (#5071) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 189.969 | `scripts/ci-script-checks.sh:366`; 현재 Rust manual classification oracle; test_durable_frontier_writer_call_sites.py:65,133 |
| guards: Input thread mapping census | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 48.198 | `scripts/ci-script-checks.sh:379`; 현재 src/** mapping census |
| guards: TUI O writer census: Legacy send sites and cutover gates | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 281.903 | `scripts/ci-script-checks.sh:383`; 현재 source census·실제 setup source oracle |
| guards: Dormant boot retirement census | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 342.035 | `scripts/ci-script-checks.sh:392`; 현재 source census·registry·skip pins |
| guards: Intake-outbox done writer per-file call-site allowlist (#5071 T2) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 168.680 | `scripts/ci-script-checks.sh:398`; 현재 src/** worker/owner/bootstrap oracle |
| guards: Run-owner raw owner-write allowlist | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 162.637 | `scripts/ci-script-checks.sh:406`; 현재 source 및 token acquisition ordering oracle |
| guards: Comment-only change checker self-tests | b | `scripts/**`; `tests/**`; `src/services/discord/tui_prompt_relay/tests/scenario_census_e2e.rs` | 10.613 | `scripts/ci-script-checks.sh:413`; test_comment_only_ci_gate.py:335,387 |
| guards: Hotfile LOC ratchet guard (#3565) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 10.327 | `scripts/ci-script-checks.sh:422`; 현재 hotfile scan·Cargo.toml·docs/ci/rust-quality-gates.md |
| guards: Dead-code suppression ratchet | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 46.184 | `scripts/ci-script-checks.sh:430`; 현재 전체 Rust source collection |
| guards: Discord log field-key drift guard (#4218) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 13.527 | `scripts/ci-script-checks.sh:435`; 현재 Rust log-key scan |
| guards: Inflight blind-save ratchet guard (#4259) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 17.377 | `scripts/ci-script-checks.sh:440`; 현재 Rust blind-save scan + 운영 script 자체 시험 혼합 |
| guards: CI runner hardening guard | a | `.github/**`; `scripts/**`; `tests/**` | 45.924 | `scripts/ci-script-checks.sh:448`; 현재 workflow hardening 검사 + 자체 시험 혼합 |
| guards: PR infrastructure failure rerun classifier (#4392/#5207) | b | `scripts/**`; `tests/**`; `.config/nextest.toml` | 40.438 | `scripts/ci-script-checks.sh:456`; test_known_flaky_retry.py:128,139 |
| guards: Lib test build and clippy swap + memory probe contract | b | `scripts/**`; `tests/**`; `.github/actions/ensure-swap/**` | 9.259 | `scripts/ci-script-checks.sh:472`; test_ensure_swap_ci_wiring.py:22,120,148 |
| guards: CI timeout wrapper tests (#4413) | b | `scripts/ci-timeout.py`; `tests/test_ci_timeout.py` | 0.648 | `scripts/ci-script-checks.sh:476`; test_ci_timeout.py:17,166 |
| guards: Relay-authority fixed mutation gate (#5071) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 41.447 | `scripts/ci-script-checks.sh:480`; 현재 checkout/committed source hash·manifest·mutation source fixture |
| guards: Relay-authority mutation wiring digest (#5997) | b | `scripts/**`; `tests/**` | 2.985 | `scripts/ci-script-checks.sh:486`; test_relay_mutation_wiring_digest.py:15 |
| guards: Relay recovery targeted-lane wiring contract (#4423) | b | `tests/test_relay_recovery_ci_wiring.py` | 0.048 | `scripts/ci-script-checks.sh:490`; test_relay_recovery_ci_wiring.py:30 |
| guards: TUI relay e2e harness unit tests (#5065/#5997) | b | `scripts/**`; `tests/**`; `src/services/discord/tui_prompt_relay/injected_prompt_policy.rs`; `src/services/discord/commands/**` | 89.074 | `scripts/ci-script-checks.sh:494`; test_assertions.py:615,628; test_herdr_observe.py:581-589; test_e2e_scenario_lane_wiring.py:18 |
| guards: Relay-authority named-target floor contract (#5071) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 59.978 | `scripts/ci-script-checks.sh:520`; 현재 manifest·실제 lib test ID inventory |
| contracts: Fast compile check PR/main/nightly split contract (#4747) | a | `src/**`; `.github/**`; `scripts/**`; `tests/**`; `justfile` | 404.179 | `scripts/ci-script-checks.sh:525`; src/** 실제 inventory·cfg 소비자·workflow; test_fast_check_ci_wiring.py:475,569,722 |
| contracts: Script checks shard ownership + result mirror contract | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 3.803 | `scripts/ci-script-checks.sh:529`; test_ci_script_check_shards.py:127-141가 실제 Giant reporter 시험도 호출; 전이 registry 미분리 |
| contracts: Script checks input selection contract | b | `scripts/ci/script_check_inputs.py`; `tests/test_script_check_inputs.py`; `tests/test_high_risk_recovery_path_filter.py` | 신규: 로컬 3.9 | `scripts/ci-script-checks.sh:533`; 새 CLI/runner/workflow 선택 계약 |
| contracts: Giant deadline warning reporter contract | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 1.802 | `scripts/ci-script-checks.sh:537`; scripts/**; tests/test_report_giant_deadlines.py; .github/workflows/**; registry/metadata/ledger 입력 확정 필요; test_report_giant_deadlines.py:249,349,466,492 |
| contracts: Nested libtest declarations | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 17.683 | `scripts/ci-script-checks.sh:542`; 현재 src/** 및 nested declarations; test_nested_libtest.py:138 |
| contracts: Unfiltered cargo test in main, nightly and Windows warm | b | `scripts/**`; `tests/**` | 0.275 | `scripts/ci-script-checks.sh:546`; test_cargo_test_filters.py:18 |
| cargo: Shared lib test artifact and PG15 lane gate | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 1.059 | `scripts/ci-script-checks.sh:551`; scripts/**; tests/test_lib_test_artifact.py; tests/test_run_test_lane.py; 전이 helper 확정 필요; 각 시험의 import 선언 |
| contracts: Rust test source wiring and PG classification | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 97.256 | `scripts/ci-script-checks.sh:555`; 현재 Rust source inventory 및 PG 분류 |
| contracts: D4-B acceptance manifest (#5340) | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 92.393 | `scripts/ci-script-checks.sh:562`; manifest 및 실제 Rust inventory |
| contracts: New production file comment-ratio gate | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 2.707 | `scripts/ci-script-checks.sh:567`; 현재 diff comment ratio |
| contracts: Process-global Mutex<()> poison-recovery gate (#5185) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 63.194 | `scripts/ci-script-checks.sh:573`; 현재 src/**/*.rs poison recovery |
| contracts: Test ROOT file-reference policy | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 5.165 | `scripts/ci-script-checks.sh:582`; 현재 source ROOT lock reference |
| contracts: Scheduled-message PG path-filter wiring contract | b | `tests/test_scheduled_messages_ci_wiring.py` | 0.035 | `scripts/ci-script-checks.sh:588`; test_scheduled_messages_ci_wiring.py:9,24 |
| contracts: High-risk recovery path-filter selection contract (#5232) | b | `tests/test_high_risk_recovery_path_filter.py`; `docs/ci/release-gates.md` | 0.670 | `scripts/ci-script-checks.sh:592`; test_high_risk_recovery_path_filter.py:20,39,126 |
| contracts: Scratch file guard | a | root scratch 파일; 전체 tracked 경로 | 0.295 | `scripts/ci-script-checks.sh:596`; 현재 tracked 파일 및 root scratch 파일 |
| contracts: Path integrity check | a | `Cargo.toml`; `policies/**` | 0.002 | `scripts/ci-script-checks.sh:640`; Cargo.toml 및 필수 policy 파일 |
| contracts: Portable deployable path lint | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 6.767 | `scripts/ci-script-checks.sh:658`; 현재 저장소 path lint·docs·dashboard manifest |
| contracts: Legacy restart replay activation validator (#5925) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 0.091 | `scripts/ci-script-checks.sh:670`; fixture 위주이나 전이 입력 확정 전 a 유지 |
| contracts: PG tunnel supervisor and retired relay cleanup tests | b | `scripts/**`; `tests/**` | 9.179 | `scripts/ci-script-checks.sh:674`; shell 시험:52 |
| contracts: Session anchor CLI tests | b | `scripts/**`; `tests/**` | 13.806 | `scripts/ci-script-checks.sh:679`; test_session_anchor.py:16,75,279,287 |
| contracts: Build token serialization tests (#5663) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 37.748 | `scripts/ci-script-checks.sh:683`; 현재 release wiring 및 모든 tracked .sh/Makefile census |
| contracts: Build token fairness tests (#5968) | b | `scripts/**`; `tests/**` | 9.018 | `scripts/ci-script-checks.sh:691`; test_build_token_fairness_5968.py:34,270 |
| cargo: Generate inventory docs (refresh workspace; gate source-of-truth invariants, #3036) | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 113.219 | `scripts/ci-script-checks.sh:698`; 현재 source·registry로 문서 생성/검사 |
| cargo: Generate env + CLI reference docs (README source-of-truth tables) | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 24.401 | `scripts/ci-script-checks.sh:704`; 현재 source 기반 env/CLI 생성·src/cli/args.rs fixture |
| cargo: Inventory prod/test split regression tests (#4394) | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 5.619 | `scripts/ci-script-checks.sh:713`; scripts/**; tests/**; .gitignore; .githooks/**; test_giant_deadline_entrypoints.py:35; 합성 source fixture |
| contracts: Structural Clippy allow occurrence ratchet (#4519) | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 6.538 | `scripts/ci-script-checks.sh:717`; 현재 Rust clippy allow occurrence |
| contracts: API docs coverage gate (#3719) | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 79.599 | `scripts/ci-script-checks.sh:722`; 현재 source/docs API coverage |
| cargo: Contract symbol-ref doc<->code sync gate (#4268) | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 0.575 | `scripts/ci-script-checks.sh:727`; 현재 doc/source symbol sync |
| cargo: Agent maintenance freshness gate (warn, #1432; targeted hard gates) | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 40.782 | `scripts/ci-script-checks.sh:733`; 현재 문서 freshness |
| contracts: Repository source shell contracts | a | `docs/**`; `src/**`; `scripts/**`; `tests/**` | 1.192 | `scripts/ci-script-checks.sh:739`; 실제 docs 참조·Rust T5·prompt API inventory |
| contracts: Shell test suites (tests/*.sh) | b | `scripts/**`; `tests/**`; `.github/**`; `.config/**`; `skills/**`; `src/cli/build_info.rs`; `Cargo.toml`; `defaults.json`; `Makefile`; `.gitignore`; `.githooks/**`; `policies/**` | 373.312 | `scripts/ci-script-checks.sh:745`; 실제 source shell 3개를 별도 a로 분리 완료 |
| cargo: Agent maintenance freshness tests | a | 현재 tree (`src/**`, `scripts/**`, `tests/**`, `docs/**`, `.github/**`, registry/metadata) | 0.092 | `scripts/ci-script-checks.sh:784`; fixture 위주이나 전이 입력 확정 전 a 유지 |
| cargo: Maintainability audit tests | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 0.269 | `scripts/ci-script-checks.sh:788`; 실제 auto-queue-monitor.sh·Rust source fixture; test_audit_maintainability.py:867 |
| cargo: Maintainability audit | a | `src/**`; `scripts/**`; `tests/**`와 현재 tree의 scanner/manifest 입력 | 159.726 | `scripts/ci-script-checks.sh:792`; 현재 전체 source maintainability audit |

## 실제 저장소 fixture 경계

- Fast compile: `tests/test_fast_check_ci_wiring.py:475-489,569-659,722-729`가 실제 Rust inventory/cfg/module walk를 검사하므로 항상 실행한다.
- Comment-only: `tests/test_rust_runtime_reads.py:65-72`, `tests/test_comment_only_ci_gate.py:335,387`이 scenario_census_e2e.rs를 읽는다. 해당 source를 b 입력으로 명시했다.
- E2E harness: `scripts/e2e/tui_relay/test_assertions.py:615,628`이 injected_prompt_policy.rs를 읽는다. source 경로를 b 입력으로 명시했다. `test_herdr_observe.py:581-589`가 읽는 commands source도 `src/services/discord/commands/**`로 포함했다.
- Shell: `test_doc_path_refs.sh`, `test_t5_post_terminal_user_boundary_5464.sh`, `test_prompt_api_routes.sh`는 a로 분리했다. 나머지도 `_defaults.sh`가 읽는 defaults.json, restart skill, build_info.rs, nextest config, workflow를 입력에 포함한다.
- Reachability·destructive/durable-frontier/intake-outbox/run-owner oracle·registry 생성기 등 분리하지 못한 실제 source/fixture 혼합 묶음은 a로 남긴다.
- shard ownership suite가 Giant reporter suite를 중첩 실행하는 전이 의존도 분리하지 못했으므로 a로 남긴다.
