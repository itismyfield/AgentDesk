#!/usr/bin/env bash
set -euo pipefail

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
python3 scripts/check_postgres_migration_checksums.py

echo "=== State/lint hardening guard ==="
python3 scripts/audit_state_lint_hardening.py

echo "=== await_holding_lock ratchet guard ==="
python3 scripts/check_await_holding_lock_ratchet.py

echo "=== Hotfile LOC ratchet guard (#3565) ==="
python3 scripts/check_hotfile_ratchet.py

echo "=== CI runner hardening guard ==="
./scripts/check-ci-runner-hardening.sh

echo "=== Scratch file guard ==="
FAIL=0
for scratch_file in plan.md scratch.md scratch.txt scratchpad.md scratchpad.txt test_scratch.rs plan.txt pr-body.md test.sh test.sql; do
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
  --exclude-dir=target --exclude-dir=.git --exclude-dir=node_modules \
  | grep -v 'Cargo.lock' \
  | grep -v '// port' \
  | grep -v '# port' || true

echo ""
echo "=== Checking hardcoded home paths (informational; see #100) ==="
if grep -rn 'env!("HOME")' --include='*.rs' \
  --exclude-dir=target --exclude-dir=.git 2>/dev/null; then
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
python3 scripts/check-portable-paths.py
python3 -m unittest \
  tests.test_portable_path_lint \
  tests.test_install_bootstrap_portable \
  tests.test_analyze_prs

echo "=== Generate inventory docs (also gates giant-file registry, #3036) ==="
# The generator hard-fails (exit 2) on giant-file registry drift: unregistered
# new giants, ghost registrations left after decomposition, or deadline-less
# [[entry]] tables in scripts/giant_file_registry.toml.
python3 scripts/generate_inventory_docs.py

echo "=== Agent maintenance freshness gate (warn, #1432; LoC hard-gate, #3036) ==="
# --warning-only keeps the #1432 freshness/touch rollout non-fatal, while
# --line-count-gate hard-fails on change-surfaces.md production-LoC drift, ghost
# freeze entries, and decomposition regressions.
python3 scripts/check_agent_maintenance_docs.py --warning-only --line-count-gate

echo "=== Agent maintenance freshness tests ==="
python3 -m unittest tests.test_agent_maintenance_docs

echo "=== Maintainability audit ==="
mkdir -p target
python3 scripts/audit_maintainability.py --format yaml > target/maintainability-audit.yaml
python3 scripts/audit_maintainability.py --check
echo "Wrote target/maintainability-audit.yaml"
