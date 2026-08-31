#!/usr/bin/env bash
set -uo pipefail

root="${AGENTDESK_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
protocol="$root/src/services/writer_protocol.rs"
namespace="$root/src/services/writer_protocol/namespace.rs"
lexical="$root/src/services/writer_protocol/namespace/lexical.rs"
manifest="$root/scripts/lib_test_inventory_manifest.txt"
ids=(
  services::writer_protocol::namespace::lexical::tests::sealed_portable_roots_normalize_exactly
  services::writer_protocol::namespace::lexical::tests::unsupported_prefixes_and_escape_components_fail_closed
  services::writer_protocol::namespace::lexical::tests::normalized_candidates_preserve_case_separators_and_root_boundaries
)
count_literal() {
  local file="$1" literal="$2"
  [ -f "$file" ] || { echo 0; return; }
  awk -v needle="$literal" '{ line=$0; while ((at=index(line, needle)) != 0) { total++; line=substr(line, at + length(needle)) } } END { print total + 0 }' "$file"
}
activation="$(count_literal "$protocol" "mod namespace;")"
if [ "$activation" -eq 0 ]; then
  partial=0
  [ ! -e "$namespace" ] || partial=1
  [ ! -e "$lexical" ] || partial=1
  for id in "${ids[@]}"; do
    [ "$(count_literal "$manifest" "$id")" -eq 0 ] || partial=1
  done
  if [ "$partial" -ne 0 ]; then
    echo "ERROR: writer namespace is inactive but owner files or manifest IDs exist" >&2
    exit 1
  fi
  echo "WRITER_NAMESPACE_WINDOWS_TARGETS NOT_APPLICABLE"
  exit 0
fi
[ "$activation" -eq 1 ] || { echo "ERROR: mod namespace; count=$activation, expected=1" >&2; exit 1; }
[ "$(count_literal "$namespace" "mod lexical;")" -eq 1 ] || { echo "ERROR: mod lexical; must occur exactly once" >&2; exit 1; }
[ -f "$lexical" ] || { echo "ERROR: missing lexical owner $lexical" >&2; exit 1; }
for id in "${ids[@]}"; do
  name="${id##*::}"
  [ "$(count_literal "$lexical" "fn $name(")" -eq 1 ] || { echo "ERROR: test function $name must occur exactly once" >&2; exit 1; }
  [ "$(count_literal "$manifest" "$id")" -eq 1 ] || { echo "ERROR: manifest ID $id must occur exactly once" >&2; exit 1; }
done

cd "$root" || exit 1
for id in "${ids[@]}"; do
  output="$(mktemp)" || exit 1
  cargo test --lib "$id" -- --exact --test-threads=1 >"$output" 2>&1
  rc=$?
  tr -d '\r' <"$output" >"$output.normalized"
  cat "$output.normalized"
  headers="$(awk '$0 ~ /^running [0-9]+ tests?$/ { count++ } END { print count + 0 }' "$output.normalized")"
  running="$(awk '$0 == "running 1 test" { count++ } END { print count + 0 }' "$output.normalized")"
  results="$(awk '/^test result:/ { count++ } END { print count + 0 }' "$output.normalized")"
  passed="$(awk '$0 ~ /^test result: ok[.] 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out; finished in [0-9]+([.][0-9]+)?s$/ { count++ } END { print count + 0 }' "$output.normalized")"
  failures="$(awk '/^failures:$/ || / FAILED$/ { count++ } END { print count + 0 }' "$output.normalized")"
  rm -f "$output" "$output.normalized"
  if [ "$rc" -ne 0 ] || [ "$headers" -ne 1 ] || [ "$running" -ne 1 ] || [ "$results" -ne 1 ] || [ "$passed" -ne 1 ] || [ "$failures" -ne 0 ]; then
    echo "ERROR: $id rc=$rc headers=$headers running1=$running results=$results passed=$passed failures=$failures" >&2
    exit 1
  fi
  echo "WRITER_NAMESPACE_WINDOWS_TARGET PASS id=$id selected=1 passed=1"
done
