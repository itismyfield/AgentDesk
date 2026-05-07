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

echo "=== Generated docs drift (warn) ==="
if python3 scripts/generate_inventory_docs.py --check; then
  echo "Inventory docs are up to date."
else
  echo "::warning::Inventory docs drift detected. The weekly Regen inventory docs workflow opens a maintenance PR; this check is intentionally warning-only."
fi

echo "=== Agent maintenance freshness gate (warn, #1432) ==="
python3 scripts/check_agent_maintenance_docs.py --warning-only

echo "=== Agent maintenance freshness tests ==="
python3 -m unittest tests.test_agent_maintenance_docs

echo "=== Maintainability audit ==="
mkdir -p target
python3 scripts/audit_maintainability.py --format yaml > target/maintainability-audit.yaml
python3 scripts/audit_maintainability.py --check
echo "Wrote target/maintainability-audit.yaml"
