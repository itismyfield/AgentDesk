#!/usr/bin/env bash
set -euo pipefail

PYTHON="${PYTHON:-python3}"

python_probe_error() {
  local invocation="$1"
  local expected="$2"
  local actual="$3"

  printf "ERROR: '%s' failed the Python interpreter identity check for the %s invocation.\n" \
    "$PYTHON" "$invocation" >&2
  printf "Expected exact stdout marker '%s'; received %q.\n" "$expected" "$actual" >&2
  echo "AgentDesk script checks require a real Python 3.11+ interpreter for stdin, file, and -m invocations." >&2
  echo "Set PYTHON=/path/to/python3.11+ or put python3.11+ first on PATH." >&2
}

require_python_probe_marker() {
  local invocation="$1"
  local expected="$2"
  local actual="$3"

  if [ "$actual" != "$expected" ]; then
    python_probe_error "$invocation" "$expected" "$actual"
    return 1
  fi
}

if ! command -v "$PYTHON" >/dev/null 2>&1; then
  echo "ERROR: AgentDesk script checks require Python 3.11+, but '$PYTHON' was not found." >&2
  echo "Set PYTHON=/path/to/python3.11+ or put python3.11+ first on PATH." >&2
  exit 1
fi

stdin_probe_marker="agentdesk-python-probe:stdin"
if ! stdin_probe_output="$("$PYTHON" - <<'PY'
import platform
import sys

if sys.version_info < (3, 11):
    print(
        "ERROR: AgentDesk script checks require Python 3.11+; "
        f"{sys.executable} is Python {platform.python_version()}.",
        file=sys.stderr,
    )
    print(
        "Set PYTHON=/path/to/python3.11+ or put python3.11+ first on PATH.",
        file=sys.stderr,
    )
    raise SystemExit(1)
print("agentdesk-python-probe:stdin")
PY
)"; then
  python_probe_error "stdin (-)" "$stdin_probe_marker" "<process exited non-zero>"
  exit 1
fi
require_python_probe_marker "stdin (-)" "$stdin_probe_marker" "$stdin_probe_output"

# The aggregate uses both file and `-m` Python entry points. Verify each shape
# emits a marker from executed Python code. This catches rc-only no-op stubs and
# wrappers that do not preserve every invocation shape; the protected workflow
# execution contract separately prevents injecting a more elaborate wrapper.
python_probe_dir=""
cleanup_python_probe() {
  if [ -n "$python_probe_dir" ]; then
    rm -rf -- "$python_probe_dir"
    python_probe_dir=""
  fi
}
trap cleanup_python_probe EXIT
python_probe_dir="$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-python-probe.XXXXXX")"
cat > "$python_probe_dir/agentdesk_python_probe.py" <<'PY'
import sys

if sys.version_info < (3, 11):
    raise SystemExit(1)
if len(sys.argv) != 2 or sys.argv[1] not in {"file", "module"}:
    raise SystemExit(1)
print(f"agentdesk-python-probe:{sys.argv[1]}")
PY

file_probe_marker="agentdesk-python-probe:file"
if ! file_probe_output="$("$PYTHON" "$python_probe_dir/agentdesk_python_probe.py" file)"; then
  python_probe_error "file" "$file_probe_marker" "<process exited non-zero>"
  exit 1
fi
require_python_probe_marker "file" "$file_probe_marker" "$file_probe_output"

module_probe_marker="agentdesk-python-probe:module"
if ! module_probe_output="$(
  cd "$python_probe_dir"
  "$PYTHON" -m agentdesk_python_probe module
)"; then
  python_probe_error "-m module" "$module_probe_marker" "<process exited non-zero>"
  exit 1
fi
require_python_probe_marker "-m module" "$module_probe_marker" "$module_probe_output"
cleanup_python_probe
trap - EXIT

if command -v shellcheck >/dev/null 2>&1; then
  echo "=== shellcheck scripts ==="
  FAILED=0
  while IFS= read -r f; do
    shellcheck -S warning "$f" || FAILED=1
  done < <(find . -name '*.sh' -not -path './target/*' -not -path './.git/*')
  if [ "$FAILED" -ne 0 ]; then
    exit "$FAILED"
  fi
else
  echo "::warning::shellcheck not found; skipping shell script lint"
fi

echo "=== PG audit guard ==="
./scripts/pg-audit.sh

echo "=== Postgres migration checksum guard ==="
"$PYTHON" scripts/check_postgres_migration_checksums.py

echo "=== message_outbox validated-insert guard (#4424) ==="
"$PYTHON" scripts/check_message_outbox_inserts.py
"$PYTHON" -m unittest tests.test_message_outbox_inserts

echo "=== Alert dedupe/authority/routing wiring contract (#4448/#4449) ==="
"$PYTHON" -m unittest tests.test_alert_dedupe_4448 tests.test_auto_queue_monitor tests.test_actionable_ops_alert_routing

echo "=== State/lint hardening guard ==="
"$PYTHON" scripts/audit_state_lint_hardening.py

echo "=== Policy DB capability manifest guard (#3734) ==="
"$PYTHON" scripts/check_policy_db_capabilities.py --no-silent-growth \
  --require-manifest policies/timeouts/active-monitor.cap.yaml \
  --require-manifest policies/review-automation.cap.yaml \
  --require-manifest policies/merge-automation.cap.yaml
"$PYTHON" -m unittest tests.test_policy_db_capabilities

echo "=== SQL execution surface inventory baseline (#5358) ==="
"$PYTHON" scripts/check_sql_execution_surface_inventory.py --check
git diff --exit-code HEAD -- scripts/sql_execution_surface_inventory.json
"$PYTHON" -m unittest tests.test_sql_execution_surface_inventory

echo "=== Destructive call-site per-file ratchet (#5071 T3-A4) ==="
"$PYTHON" scripts/check_destructive_call_site_ratchet.py --check
"$PYTHON" -m unittest tests.test_destructive_call_site_ratchet

echo "=== Reachability row-independence + change-surface gate (#5071 T4-B1) ==="
# 4987 §-1.5 withdrew the claim that I14 ("obligation production is independent
# of the inflight row") is compiler-enforced: InflightTurnState is
# pub(in crate::services::discord), so the compiler accepts an import from
# health/reachability/** without complaint. This is the source gate that
# replaces it, and it is a LINT, NOT A TYPE PROOF -- real enforcement needs a
# crate boundary, which is out of this series' scope. The second half enforces
# the relay_reachability change surface of 4987 §9.4, so a tree with no owner
# entry cannot grow a file nobody reviews as part of this surface. The unittest
# below is the gate's own mutation proof: it reproduces each violation shape,
# and each false-positive shape, against a synthetic repo root.
"$PYTHON" scripts/check_reachability_row_independence.py
"$PYTHON" -m unittest tests.test_reachability_row_independence

echo "=== Reachability canonical Rust<->Python equivalence gate (#5071 T4-B2a) ==="
# 4987 blocker B1': the obligation rule has two implementations, and two
# definitions of "assistant text block" are two oracles of which one is always
# wrong. Both are compared byte for byte against the golden corpus in
# tests/fixtures/relay_obligation/ -- this half checks Python, and the Rust half
# is the obligation lane's corpus test in `just test-non-pg`. The corpus being a
# THIRD PARTY to both is what stops "we drifted together" from passing.
#
# The invocation below also runs the PYTHON half of the mutation runner in
# process (each declared mutation edits one implementation and must turn its
# side red) and lints the T4-B2a inactivity invariant: no consumer outside the
# tree beyond its module declaration, no warn/fail bound inside it. The RUST
# half of the mutation runner needs a compiler and runs under `--with-rust`
# (see `just reachability-mutation-runner`); what CI holds instead is the lane,
# plus the unittest below, which fails if a declared Rust mutation stops
# anchoring on real source and would therefore be silently skipped.
"$PYTHON" scripts/check_reachability_canonical_equivalence.py
"$PYTHON" -m unittest tests.test_reachability_canonical_equivalence

echo "=== Merge automation policy tests (#4250) ==="
node --test policies/__tests__/merge-automation.test.js

echo "=== Timeout shadow aggregation gate tests (#3950) ==="
node --test scripts/__tests__/timeout-shadow-gate.test.mjs

echo "=== Daily log-digest routine tests (#4263) ==="
node --test policies/__tests__/daily-log-digest.test.js
"$PYTHON" -m unittest tests.test_daily_log_digest

echo "=== Weekly regression-churn audit tests (#4265) ==="
"$PYTHON" -m unittest tests.test_weekly_churn_audit

echo "=== External toolchain draft/approval/smoke tests (#4555) ==="
"$PYTHON" -m unittest tests.test_toolchain_update

echo "=== await_holding_lock ratchet guard ==="
"$PYTHON" scripts/check_await_holding_lock_ratchet.py
"$PYTHON" -m unittest tests.test_await_holding_lock_ratchet

echo "=== DeliveryJournal raw-writer allowlist ==="
"$PYTHON" scripts/check_delivery_journal_raw_writer.py
"$PYTHON" -m unittest tests.test_delivery_journal_raw_writer

echo "=== Durable frontier writer per-file call-site allowlist (#5071) ==="
# Built for #5071 T1 S7, which changes the recovery path's durable behaviour.
# Nothing in the repo pinned WHERE these writer symbols are called from, so this
# fixes an exact per-file count for each of them over all of src/ before that
# behaviour moves. It changes no production behaviour itself. Lexical scan: it
# does not see `use .. as` aliases, renamed re-exports, or name-constructing
# macros -- the script's docstring declares those holes and the unittest module
# measures each one.
"$PYTHON" scripts/check_durable_frontier_writer_call_sites.py
"$PYTHON" -m unittest tests.test_durable_frontier_writer_call_sites

echo "=== Intake-outbox done writer per-file call-site allowlist (#5071 T2) ==="
# Pins the pre-T2 `mark_done` owner by exact per-file textual count over src/;
# the script docstring declares the lexical forms and semantic facts it cannot see.
"$PYTHON" scripts/check_intake_outbox_done_writer_call_sites.py
"$PYTHON" -m unittest tests.test_intake_outbox_done_writer_call_sites
"$PYTHON" -m unittest tests.test_rust_lex

echo "=== Hotfile LOC ratchet guard (#3565) ==="
"$PYTHON" scripts/check_hotfile_ratchet.py
"$PYTHON" -m unittest scripts.test_ratchet_admission
"$PYTHON" -m unittest scripts.test_intervention_log

echo "=== Discord log field-key drift guard (#4218) ==="
"$PYTHON" scripts/check_log_key_drift.py
"$PYTHON" -m unittest tests.test_log_key_drift

echo "=== Inflight blind-save ratchet guard (#4259) ==="
"$PYTHON" scripts/check_inflight_blind_save_ratchet.py
"$PYTHON" -m unittest tests.test_inflight_blind_save_ratchet

# #4511 post-deploy smoke WARN post-restart scoping
bash tests/test_deploy_smoke_warn_scope_4511.sh

echo "=== Cluster deploy peer verdict + terminal marker contract (#5189) ==="
bash tests/test_cluster_deploy_peer_verdict_5189.sh

echo "=== CI runner hardening guard ==="
./scripts/check-ci-runner-hardening.sh
"$PYTHON" -m unittest tests.test_discord_thread_create_ci_wiring

echo "=== PR infrastructure failure rerun classifier (#4392/#5207) ==="
# These self-tests also enforce the #5207 sibling-regex sync contract: the
# classifier's INFRA_TERMINATION_REGEX must stay byte-identical to
# log_has_infra_termination in scripts/main-ci-triage.sh, and drift fails here.
# tests/test_infra_failure_classifier_5207.sh (tests/*.sh loop below) carries
# the discriminating matrix and proves drift detection actually discriminates.
./scripts/ci/infra-failure-rerun.sh --self-test
bash scripts/main-ci-triage.sh --self-test

echo "=== CI timeout wrapper tests (#4413) ==="
"$PYTHON" -m unittest tests.test_ci_timeout

echo "=== Relay-authority fixed mutation gate (#5071) ==="
"$PYTHON" -m unittest tests.test_relay_authority_mutations

echo "=== Relay recovery targeted-lane wiring contract (#4423) ==="
"$PYTHON" -m unittest tests.test_relay_recovery_ci_wiring

echo "=== TUI relay assertion unit tests (#5065) ==="
"$PYTHON" -m unittest scripts.e2e.tui_relay.test_assertions

echo "=== Relay-authority named-target floor contract (#5071) ==="
"$PYTHON" scripts/check_relay_authority_contract.py --check-manifest
"$PYTHON" -m unittest tests.test_check_relay_authority_contract

# The axis-A promotion report is the only reader of the observation log, so its
# false-green modes are promotion decisions. These pin the completeness floors:
# an entry-only window and a re-entered fingerprint must fail, not pass; the
# `line_integrity` denominator counts only the target segment's own records, so
# the previous segment cohabiting the target's first file cannot dilute its
# losses and the verdict does not move with how much cohabits there; and a JSON
# line that is a bare scalar is counted as unusable instead of aborting the run
# before any criterion is evaluated.
echo "=== Relay-authority axis-A promotion report (#5464 T5 S2) ==="
"$PYTHON" -m unittest tests.test_relay_authority_rollout_report

echo "=== Fast compile check PR/main/nightly split contract (#4747) ==="
"$PYTHON" -m unittest tests.test_fast_check_ci_wiring

echo "=== Rust test-lane coverage ratchet (#4846/#4910) ==="
if [[ -z "${TEST_LANE_BASELINE_REF:-}" ]]; then
  echo "ERROR: TEST_LANE_BASELINE_REF must name an immutable comparison snapshot" >&2
  exit 1
fi
"$PYTHON" scripts/check_test_lane_coverage.py --baseline-ref "$TEST_LANE_BASELINE_REF"
"$PYTHON" -m unittest tests.test_test_lane_coverage

echo "=== Test-target integrity gate (#5003/#5008) ==="
# cargo exits 0 on zero filter matches, so a curated lane with the wrong
# --lib/--bin/--test flag can run 0 tests while its required check stays
# green. The gate consumes workflow and justfile command sites and is enforced
# here. The unittest run below is the gate's own mutation proof.
"$PYTHON" scripts/check_test_target_integrity.py --enforce
"$PYTHON" -m unittest tests.test_check_test_target_integrity
AGENTDESK_CI_TIMEOUT_REPORT=1 "$PYTHON" scripts/ci-timeout.py 900 "$PYTHON" scripts/check_test_target_integrity.py --verify-lib-inventory

echo "=== PostgreSQL test-lane membership gate (#4979, enforced) ==="
"$PYTHON" scripts/check_pg_test_lane_membership.py --baseline-ref "$TEST_LANE_BASELINE_REF"
"$PYTHON" -m unittest tests.test_check_pg_test_lane_membership

echo "=== Process-global Mutex<()> poison-recovery gate (#5185) ==="
# The rule this enforces was documented in src/config.rs and recurred anyway:
# one real failure reported itself as 11, was repaired at one mutex, and then a
# different process-global Mutex<()> turned one real failure into 68 (67 of 73
# panics were PoisonError). A rule that only exists as prose is #5003.
"$PYTHON" scripts/check_test_mutex_poison_recovery.py
"$PYTHON" -m unittest tests.test_check_test_mutex_poison_recovery

echo "=== Scheduled-message PG path-filter wiring contract ==="
"$PYTHON" -m unittest tests.test_scheduled_messages_ci_wiring

echo "=== Scratch file guard ==="
FAIL=0
for scratch_file in plan.md scratch.md scratch.txt scratch.sh scratchpad.md scratchpad.txt scratchpad.sh sql_test.rs test_scratch.rs plan.txt pr-body.md test.sh test.sql verify.sh; do
  if [ -f "$scratch_file" ]; then
    echo "ERROR: Scratch file detected in repository root: $scratch_file"
    FAIL=1
  fi
done
for scratch_file in scratch.sql scratchpad.sql scratch[._-]*.sql scratchpad[._-]*.sql test_scratch[._-]*.sql; do
  if [ -f "$scratch_file" ]; then
    echo "ERROR: Scratch SQL file detected in repository root: $scratch_file"
    FAIL=1
  fi
done
for scratch_file in scratch[._-]*.sh scratchpad[._-]*.sh test_scratch[._-]*.sh; do
  if [ -f "$scratch_file" ]; then
    echo "ERROR: Scratch shell file detected in repository root: $scratch_file"
    FAIL=1
  fi
done
for scratch_file in scratch[._-]*.md scratchpad[._-]*.md test_scratch[._-]*.md scratch[._-]*.txt scratchpad[._-]*.txt test_scratch[._-]*.txt scratch[._-]*.rs scratchpad[._-]*.rs test_scratch[._-]*.rs test_*.rs; do
  if [ -f "$scratch_file" ]; then
    echo "ERROR: Scratch file detected in repository root: $scratch_file"
    FAIL=1
  fi
done
if [ "$FAIL" -ne 0 ]; then
  exit "$FAIL"
fi

echo "=== Check hardcoded port/path drift ==="
grep -rn '8791\|8799' --include='*.rs' --include='*.js' --include='*.yaml' --include='*.json' \
  --exclude-dir=target --exclude-dir=.git --exclude-dir=node_modules --exclude-dir=.claude \
  | grep -v 'Cargo.lock' \
  | grep -v '// port' \
  | grep -v '# port' || true

echo ""
echo "=== Checking hardcoded home paths (informational; see #100) ==="
if grep -rn 'env!("HOME")' --include='*.rs' \
  --exclude-dir=target --exclude-dir=.git --exclude-dir=.claude 2>/dev/null; then
  echo "NOTE: env!(\"HOME\") found; tracked in #100"
else
  echo "OK: No env!(\"HOME\") found"
fi

echo "=== Path integrity check ==="
FAIL=0
if grep -n '/Users/\|/home/' Cargo.toml 2>/dev/null; then
  echo "ERROR: Absolute paths found in Cargo.toml"
  FAIL=1
fi

for f in policies/default-pipeline.yaml policies/kanban-rules.js policies/timeouts.js policies/auto-queue.js policies/review-automation.js; do
  if [ ! -f "$f" ]; then
    echo "ERROR: Required policy file missing: $f"
    FAIL=1
  fi
done
if [ "$FAIL" -ne 0 ]; then
  exit "$FAIL"
fi

echo "=== Portable deployable path lint ==="
"$PYTHON" scripts/check-portable-paths.py
"$PYTHON" -m unittest \
  tests.test_portable_path_lint \
  tests.test_install_bootstrap_portable \
  tests.test_script_python_policy \
  tests.test_analyze_prs

echo "=== Relay watchdog + PG tunnel supervisor tests (#4381/#4378) ==="
# The out-of-band relay watchdog is a deployable Python script; it is not
# covered by shellcheck (only *.sh) nor by cargo, so this unittest run is its
# ONLY CI gate. It also pins the deploy/plist wiring so the watchdog cannot
# silently fall out of the deploy again (the 06-29 relay-gap-watch failure).
"$PYTHON" -m unittest tests.test_relay_watchdog tests.test_pg_tunnel

echo "=== Generate inventory docs (refresh workspace; gate source-of-truth invariants, #3036) ==="
# Inventory snapshots are untracked, so generate them in the CI workspace
# before checks consume their source-of-truth data. The generator hard-fails
# (exit 2) on giant-file registry drift: unregistered new giants, ghost
# registrations left after decomposition, or deadline-less [[entry]] tables in
# scripts/giant_file_registry.toml. The following git diff is the PR-time
# drift gate: generation updates snapshots, then CI rejects changes to tracked
# source-of-truth docs instead of comparing the generated workspace to itself.
# The #5234 gate consumes open/closed truth and the two count baselines in
# scripts/giant_file_issue_metadata.json. It fails if either the closed-deadline
# count or transition-list count grows. A lower measured count must be recorded
# by the writer, which automatically lowers (and never raises) both baselines:
# `python3 scripts/refresh_giant_file_issue_metadata.py && git add scripts/giant_file_issue_metadata.json`
# The same writer re-queries live GitHub for each issue's state and stamps a new
# refreshed_at; it resets the freshness clock rather than checking it, and the
# 30-day staleness gate lives in the generator above. Entries already in the
# transition list warn but pass; a new dead pointer is fatal.
"$PYTHON" scripts/generate_inventory_docs.py
git diff --exit-code -- ARCHITECTURE.md docs/generated/route-inventory.md docs/generated/worker-inventory.md

echo "=== Inventory prod/test split regression tests (#4394) ==="
"$PYTHON" -m unittest tests.test_inventory_giant_split

echo "=== Structural Clippy allow occurrence ratchet (#4519) ==="
"$PYTHON" scripts/check_clippy_allow_ratchet.py
"$PYTHON" -m unittest tests.test_clippy_allow_ratchet

echo "=== API docs coverage gate (#3719) ==="
"$PYTHON" scripts/check_api_docs_coverage.py
"$PYTHON" -m unittest tests.test_api_docs_coverage

echo "=== Contract symbol-ref doc<->code sync gate (#4268) ==="
# docs/relay-state-contract.md anchors code with `sym:` symbol paths. This check
# verifies the doc's `sym:` anchors exactly match the references PARSED FROM THE
# CODE in the relay_state_contract_refs blocks (use / field / assoc-fn forms,
# never comments) — it does NOT judge whether a symbol exists. Symbol EXISTENCE
# is proven by the compiler: those reference blocks fail
# `cargo check --workspace --all-targets` (a required gate) if a symbol is
# renamed/moved/removed. Splitting it this way is what killed the regex-bypass
# game (raw strings / macros / cfg can't fool a real compile), and deriving the
# anchor set from the compiled code (not `// sym:` comments) is what killed the
# r3 comment-decoupling bypass.
"$PYTHON" scripts/check_contract_symbol_refs.py
"$PYTHON" -m unittest tests.test_contract_symbol_refs

echo "=== Agent maintenance freshness gate (warn, #1432; targeted hard gates) ==="
# --warning-only keeps the #1432 freshness/touch rollout non-fatal. The LoC gate
# remains unconditional; the migration 0093 rollout gate activates only when the
# migration itself is in the changed-file set.
"$PYTHON" scripts/check_agent_maintenance_docs.py --warning-only --line-count-gate \
  --migration-0093-rollout-gate

echo "=== Shell test suites (tests/*.sh) ==="
# #4255: these suites existed but NOTHING executed them — `tests/**` appears in
# ci-pr.yml only as a path filter that triggers the Rust jobs. Their assertions
# had therefore never run on CI, so a shell guard could regress (or ship broken)
# while every required check stayed green. Run them here, in the job that already
# owns script-level gates.
SHELL_TESTS_FAILED=0
required_shell_suites=(
  tests/test_build_token_5663.sh
  tests/test_deploy_smoke_wedge_coverage_5244.sh
  tests/test_required_check_mirror.sh
)
for required_suite in "${required_shell_suites[@]}"; do
  [ -f "$required_suite" ] || { echo "✗ required shell suite missing: $required_suite" >&2; SHELL_TESTS_FAILED=1; }
done
for shell_test in tests/*.sh; do
  [ -f "$shell_test" ] || continue
  echo "--- $shell_test"
  bash "$shell_test" || SHELL_TESTS_FAILED=1
done
if [ "$SHELL_TESTS_FAILED" -ne 0 ]; then
  echo "one or more tests/*.sh suites failed" >&2
  exit 1
fi

echo "=== Agent maintenance freshness tests ==="
"$PYTHON" -m unittest tests.test_agent_maintenance_docs

echo "=== Maintainability audit tests ==="
"$PYTHON" -m unittest tests.test_audit_maintainability.FooterViewWritesCheck

echo "=== Maintainability audit ==="
mkdir -p target
"$PYTHON" scripts/audit_maintainability.py --format yaml > target/maintainability-audit.yaml
"$PYTHON" scripts/audit_maintainability.py --check
echo "Wrote target/maintainability-audit.yaml"
