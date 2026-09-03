#!/usr/bin/env bash
set -uo pipefail

if [ -n "${AGENTDESK_REPO_ROOT+x}" ]; then echo "ERROR: AGENTDESK_REPO_ROOT is not honored; the runner validates only the checkout that contains it" >&2; exit 1; fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly engine="$root/scripts/exact_rust_test_proof.py"
readonly manifest="scripts/lib_test_inventory_manifest.txt"
readonly protocol="src/services/writer_protocol.rs"
readonly namespace="src/services/writer_protocol/namespace.rs"
readonly lexical="src/services/writer_protocol/namespace/lexical.rs"
readonly lexical_family="services::writer_protocol::namespace::lexical::tests"
readonly -a lexical_ids=(
  services::writer_protocol::namespace::lexical::tests::sealed_portable_roots_normalize_exactly
  services::writer_protocol::namespace::lexical::tests::unsupported_prefixes_and_escape_components_fail_closed
  services::writer_protocol::namespace::lexical::tests::normalized_candidates_preserve_case_separators_and_root_boundaries
)
argv=(
  python3 "$engine" run
  --repo-root "$root"
  --manifest "$manifest"
  --pass-prefix WRITER_NAMESPACE_WINDOWS_TARGET
  --gate writer_namespace "$protocol" namespace "$namespace" optional
  --owner lexical writer_namespace "$namespace" lexical "$lexical" "$lexical_family" required
)
for id in "${lexical_ids[@]}"; do
  argv+=(--owner-id lexical "$id")
done
exec "${argv[@]}"
