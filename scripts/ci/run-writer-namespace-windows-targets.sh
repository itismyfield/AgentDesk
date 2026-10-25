#!/usr/bin/env bash
set -uo pipefail

if [ -n "${AGENTDESK_REPO_ROOT+x}" ]; then echo "ERROR: AGENTDESK_REPO_ROOT is not honored; the runner validates only the checkout that contains it" >&2; exit 1; fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The first cargo below starts the job's sccache server. Its idle timer resets only on new
# requests, so the long Windows lib-test rustc must not run against the 600s default.
export SCCACHE_IDLE_TIMEOUT="${SCCACHE_IDLE_TIMEOUT:-0}"
readonly engine="$root/scripts/exact_rust_test_proof.py"
if command -v python3 >/dev/null 2>&1; then
  interpreter=python3
elif command -v python >/dev/null 2>&1; then
  interpreter=python
else
  echo "ERROR: exact Rust proof requires python3 or python >= 3.11" >&2; exit 86
fi
if ! "$interpreter" -c 'import sys; raise SystemExit(sys.version_info < (3, 11))'; then
  echo "ERROR: exact Rust proof interpreter must be >= 3.11" >&2; exit 87
fi
readonly interpreter
# Windows `fsync_parent_dir` contract: one gate per engine run, so one run per owner.
readonly worker_recovery="src/server/worker_recovery.rs"
readonly runtime_store="src/services/discord/runtime_store.rs"
"$interpreter" "$engine" run --repo-root "$root" --platform windows \
  --pass-prefix DIR_FSYNC_WINDOWS_TARGET \
  --gate server src/lib.rs server src/server/mod.rs optional \
  --owner worker_recovery server src/server/mod.rs worker_recovery "$worker_recovery" \
    server::worker_recovery::windows_contract::tests required \
  --owner-id worker_recovery \
    server::worker_recovery::windows_contract::tests::first_fatal_exit_persists_ledger_and_exits \
  || exit
exec "$interpreter" "$engine" run --repo-root "$root" --platform windows \
  --pass-prefix DIR_FSYNC_WINDOWS_TARGET \
  --gate discord src/services/mod.rs discord src/services/discord/mod.rs optional \
  --owner runtime_store discord src/services/discord/mod.rs runtime_store "$runtime_store" \
    services::discord::runtime_store::windows_contract::tests required \
  --owner-id runtime_store \
    services::discord::runtime_store::windows_contract::tests::windows_parent_dir_sync_succeeds_without_flushing
