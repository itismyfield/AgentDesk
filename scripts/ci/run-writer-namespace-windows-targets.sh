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
# Windows `fsync_parent_dir` contract: one gate per engine run, so one run per owner. Both owners are
# required; a failed proof is kept so the other owner and the Windows targets below still run.
status=0
readonly worker_recovery="src/server/worker_recovery.rs"
readonly runtime_store="src/services/discord/runtime_store.rs"
"$interpreter" "$engine" run --repo-root "$root" --platform windows \
  --pass-prefix DIR_FSYNC_WINDOWS_TARGET --package agentdesk src/lib.rs \
  --gate server src/lib.rs server src/server/mod.rs required \
  --owner worker_recovery server src/server/mod.rs worker_recovery "$worker_recovery" \
    server::worker_recovery::windows_contract::tests required \
  --owner-id worker_recovery \
    server::worker_recovery::windows_contract::tests::first_fatal_exit_persists_ledger_and_exits \
  || status=$?
# adk-runtime declares `services::discord` inline, so its gate is the `mod runtime_store;` line itself.
"$interpreter" "$engine" run --repo-root "$root" --platform windows \
  --pass-prefix DIR_FSYNC_WINDOWS_TARGET --package adk-runtime src/adk_runtime_lib.rs \
  --gate discord src/adk_runtime_lib.rs runtime_store "$runtime_store" required \
  --owner runtime_store discord src/adk_runtime_lib.rs runtime_store "$runtime_store" \
    services::discord::runtime_store::windows_contract::tests required \
  --owner-id runtime_store \
    services::discord::runtime_store::windows_contract::tests::windows_parent_dir_sync_succeeds_without_flushing \
  || { rc=$?; [ "$status" -ne 0 ] || status=$rc; }
# Keep the Windows-only transcript refusal and keyless first registration executable in the PR job.
# The CLI parse test pins the Windows 1 MiB default main-thread stack contract on Windows debug.
cargo nextest run --workspace --lib --profile ci --no-tests fail \
  -E 'test(=services::discord::health::turn_deliver::inject::queued_reconcile::mismatched_file_identity_refuses_transcript_proof) | test(=services::tui_input::human_frame::history::v2::keyless::platform_tests::keyless_first_register_succeeds_unless_a_directory_fsync_fails) | test(=cli::args::tests::cli_parse_fits_in_the_windows_default_main_thread_stack) | test(=services::tui_input::human_frame::history::notice_owner::census::tests::native_census_without_unix_change_witness_scans_each_time) | test(=runtime_layout::skill_sync::provider_sync_tests::retired_gemini_removes_deleted_directory_symlinks_windows) | test(=runtime_layout::skill_sync::provider_sync_tests::windows_link_removal_preserves_live_targets_and_removes_dangling_links) | test(=runtime_layout::skill_sync::provider_sync_tests::retired_gemini_removes_dangling_links_alone_or_with_skill_links)' \
  || { rc=$?; [ "$status" -ne 0 ] || status=$rc; }
exit "$status"
