#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
DASHBOARD_DIR="$REPO/dashboard"

bash "$SCRIPT_DIR/check-dashboard-toolchain.sh" "$REPO"

bash "$SCRIPT_DIR/install-dashboard-dependencies.sh" "$DASHBOARD_DIR"

cd "$DASHBOARD_DIR"

echo "==> Dashboard security audit (high+)"
# High/critical dashboard advisories fail CI by default. To waive a specific
# advisory that has no available fix, set DASHBOARD_AUDIT_WAIVER to a short
# documented reason (it is echoed into the CI log for an audit trail). The
# waiver downgrades the failure to a warning; it does not silence the report.
audit_status=0
npm audit --audit-level=high || audit_status=$?
if [ "$audit_status" -ne 0 ]; then
  if [ -n "${DASHBOARD_AUDIT_WAIVER:-}" ]; then
    echo "::warning::Dashboard high/critical npm audit findings WAIVED — reason: ${DASHBOARD_AUDIT_WAIVER}" >&2
  else
    echo "Error: dashboard npm audit found high/critical advisories." >&2
    echo "       Upgrade the affected dependency, or waive with a documented reason:" >&2
    echo "       DASHBOARD_AUDIT_WAIVER='<reason>' ./scripts/verify-dashboard.sh" >&2
    exit "$audit_status"
  fi
fi

echo "==> Dashboard build"
npm run build

echo "==> Dashboard test"
npm test
