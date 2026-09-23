#!/usr/bin/env bash
set -uo pipefail

if [ -n "${AGENTDESK_REPO_ROOT+x}" ]; then echo "ERROR: AGENTDESK_REPO_ROOT is not honored; the runner validates only the checkout that contains it" >&2; exit 1; fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly engine="$root/scripts/exact_rust_test_proof.py"
readonly manifest="scripts/lib_test_inventory_manifest.txt"
readonly protocol="src/services/writer_protocol.rs"
readonly namespace="src/services/writer_protocol/namespace.rs"
readonly lexical="src/services/writer_protocol/namespace/lexical.rs"
readonly catalog="src/services/writer_protocol/namespace/catalog.rs"
readonly lexical_family="services::writer_protocol::namespace::lexical::tests"
readonly catalog_family="services::writer_protocol::namespace::catalog::tests"
readonly -a lexical_ids=(
  services::writer_protocol::namespace::lexical::tests::sealed_portable_roots_normalize_exactly
  services::writer_protocol::namespace::lexical::tests::unsupported_prefixes_and_escape_components_fail_closed
  services::writer_protocol::namespace::lexical::tests::normalized_candidates_preserve_case_separators_and_root_boundaries
)
readonly -a catalog_ids=(
  services::writer_protocol::namespace::catalog::tests::canonical_and_legacy_session_aliases_share_exact_authority_key
  services::writer_protocol::namespace::catalog::tests::sealed_roots_issue_only_exact_reviewed_artifact_bindings
  services::writer_protocol::namespace::catalog::tests::duplicate_and_overlapping_catalog_bindings_are_rejected_atomically
  services::writer_protocol::namespace::catalog::tests::catalog_bindings_are_deterministic_and_injective
  services::writer_protocol::namespace::catalog::tests::unknown_roots_and_artifacts_never_receive_fallback_identity
)
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
argv=(
  "$interpreter" "$engine" run
  --repo-root "$root"
  --manifest "$manifest"
  --pass-prefix WRITER_NAMESPACE_WINDOWS_TARGET
  --gate writer_namespace "$protocol" namespace "$namespace" optional
  --owner lexical writer_namespace "$namespace" lexical "$lexical" "$lexical_family" required
  --owner catalog writer_namespace "$namespace" catalog "$catalog" "$catalog_family" optional
)
for id in "${lexical_ids[@]}"; do
  argv+=(--owner-id lexical "$id")
done
for id in "${catalog_ids[@]}"; do
  argv+=(--owner-id catalog "$id")
done
"${argv[@]}" || exit
# Windows `fsync_parent_dir` contract: one gate per engine run, so one run per owner.
readonly worker_recovery="src/server/worker_recovery.rs"
readonly runtime_store="src/services/discord/runtime_store.rs"
"$interpreter" "$engine" run --repo-root "$root" --manifest "$manifest" \
  --pass-prefix DIR_FSYNC_WINDOWS_TARGET \
  --gate server src/lib.rs server src/server/mod.rs optional \
  --owner worker_recovery server src/server/mod.rs worker_recovery "$worker_recovery" \
    server::worker_recovery::windows_contract::tests required \
  --owner-id worker_recovery \
    server::worker_recovery::windows_contract::tests::first_fatal_exit_persists_ledger_and_exits \
  || exit
exec "$interpreter" "$engine" run --repo-root "$root" --manifest "$manifest" \
  --pass-prefix DIR_FSYNC_WINDOWS_TARGET \
  --gate discord src/services/mod.rs discord src/services/discord/mod.rs optional \
  --owner runtime_store discord src/services/discord/mod.rs runtime_store "$runtime_store" \
    services::discord::runtime_store::windows_contract::tests required \
  --owner-id runtime_store \
    services::discord::runtime_store::windows_contract::tests::windows_parent_dir_sync_succeeds_without_flushing
