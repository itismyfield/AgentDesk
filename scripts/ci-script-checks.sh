#!/usr/bin/env bash
set -euo pipefail

PYTHON="${PYTHON:-python3}"

if ! command -v "$PYTHON" >/dev/null 2>&1; then
  echo "ERROR: AgentDesk script checks require Python 3.11+, but '$PYTHON' was not found." >&2
  echo "Set PYTHON=/path/to/python3.11+ or put python3.11+ first on PATH." >&2
  exit 1
fi

if ! "$PYTHON" - <<'PY'
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
PY
then
  exit 1
fi

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

echo "=== State/lint hardening guard ==="
"$PYTHON" scripts/audit_state_lint_hardening.py

echo "=== Policy DB capability manifest guard (#3734) ==="
"$PYTHON" scripts/check_policy_db_capabilities.py --no-silent-growth \
  --require-manifest policies/timeouts/active-monitor.cap.yaml \
  --require-manifest policies/review-automation.cap.yaml \
  --require-manifest policies/merge-automation.cap.yaml
"$PYTHON" -m unittest tests.test_policy_db_capabilities

echo "=== await_holding_lock ratchet guard ==="
"$PYTHON" scripts/check_await_holding_lock_ratchet.py
"$PYTHON" -m unittest tests.test_await_holding_lock_ratchet

echo "=== Hotfile LOC ratchet guard (#3565) ==="
"$PYTHON" scripts/check_hotfile_ratchet.py

echo "=== Discord log field-key drift guard (#4218) ==="
"$PYTHON" scripts/check_log_key_drift.py
"$PYTHON" -m unittest tests.test_log_key_drift

echo "=== Inflight blind-save ratchet guard (#4259) ==="
"$PYTHON" scripts/check_inflight_blind_save_ratchet.py
"$PYTHON" -m unittest tests.test_inflight_blind_save_ratchet

echo "=== CI runner hardening guard ==="
./scripts/check-ci-runner-hardening.sh

echo "=== PR infrastructure failure rerun classifier (#4392) ==="
./scripts/ci/infra-failure-rerun.sh --self-test

echo "=== CI timeout wrapper tests (#4413) ==="
"$PYTHON" -m unittest tests.test_ci_timeout

echo "=== Relay recovery targeted-lane wiring contract (#4423) ==="
"$PYTHON" -m unittest tests.test_relay_recovery_ci_wiring

echo "=== Scratch file guard ==="
FAIL=0
for scratch_file in plan.md scratch.md scratch.txt scratch.sh scratchpad.md scratchpad.txt scratchpad.sh sql_test.rs test_scratch.rs plan.txt pr-body.md test.sh test.sql; do
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
# Generic committed markdown freshness drift is warning-only for ordinary PRs
# and is refreshed by the weekly regen-docs workflow. This CI invocation writes
# the current generated view into the workspace so the checks below compare
# against current source facts.
# The generator hard-fails (exit 2) on giant-file registry drift: unregistered
# new giants, ghost registrations left after decomposition, or deadline-less
# [[entry]] tables in scripts/giant_file_registry.toml. Generated-docs drift
# (exit 1) is a hard fail in PRs to prevent drift merging and spawning
# duplicate downstream inventory refresh PRs.
"$PYTHON" scripts/generate_inventory_docs.py --check

echo "=== Inventory prod/test split regression tests (#4394) ==="
"$PYTHON" -m unittest tests.test_inventory_giant_split

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

echo "=== Agent maintenance freshness gate (warn, #1432; LoC hard-gate, #3036) ==="
# --warning-only keeps the #1432 freshness/touch rollout non-fatal, while
# --line-count-gate hard-fails on change-surfaces.md production-LoC drift, ghost
# freeze entries, and decomposition regressions.
"$PYTHON" scripts/check_agent_maintenance_docs.py --warning-only --line-count-gate

echo "=== Shell test suites (tests/*.sh) ==="
# #4255: these suites existed but NOTHING executed them — `tests/**` appears in
# ci-pr.yml only as a path filter that triggers the Rust jobs. Their assertions
# had therefore never run on CI, so a shell guard could regress (or ship broken)
# while every required check stayed green. Run them here, in the job that already
# owns script-level gates.
SHELL_TESTS_FAILED=0
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
