#!/usr/bin/env bash
# Check committed PR changes in the caller's repository; see docs/pr-cap-check.md.
set -euo pipefail

fail() {
  printf 'CAP: ERROR (%s)\n' "$1" >&2
  exit 1
}

[[ $# -le 1 ]] || fail 'usage: scripts/pr_cap_check.sh [commit-ish]'
branch="${1-HEAD}"
[[ -n "$branch" && "$branch" != -* ]] || fail 'invalid target ref'

# Explicitly update origin/main even when remote.origin.fetch omits that ref.
# Do not cd to the script's directory: an absolute invocation measures the CWD.
git fetch --quiet --no-tags origin +refs/heads/main:refs/remotes/origin/main \
  || fail 'cannot fetch origin main; no local-main fallback'
target="$(git -c core.warnAmbiguousRefs=true rev-parse --verify --end-of-options \
  "${branch}^{commit}" 2>&1)" \
  || fail 'target must resolve to one commit'
[[ "$target" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] \
  || fail 'target ref must resolve without ambiguity or warnings'
upstream="$(git rev-parse --verify 'refs/remotes/origin/main^{commit}')" \
  || fail 'cannot resolve fetched origin/main'
# Same merge-base primitive as ratchet_admission._merge_base, without that
# ratchet's configurable candidates or local-main fallback.
base="$(git merge-base "$upstream" "$target")" \
  || fail 'target and origin/main have no usable merge-base'

stats="$(mktemp "${TMPDIR:-/tmp}/adk-pr-cap.XXXXXX")" \
  || fail 'cannot create numstat temporary file'
trap 'rm -f -- "$stats"' EXIT
# Finish Git successfully before printing a verdict. Process substitution would
# hide a failing diff producer from the counting loop.
git diff --numstat -z --find-renames=50% -l0 --no-ext-diff --no-textconv \
  "$base" "$target" -- > "$stats" || fail 'cannot compute numstat'

files=0
additions=0
deletions=0
binaries=0
while IFS= read -r -d '' record; do
  [[ "$record" == *$'\t'*$'\t'* ]] || fail 'invalid numstat record'
  added="${record%%$'\t'*}"
  rest="${record#*$'\t'}"
  deleted="${rest%%$'\t'*}"
  file_path="${rest#*$'\t'}"
  if [[ -z "$file_path" ]]; then
    # With -z, a detected rename has an empty path followed by two NUL paths.
    IFS= read -r -d '' old_path && IFS= read -r -d '' new_path \
      || fail 'incomplete numstat rename'
    [[ -n "$old_path" && -n "$new_path" ]] || fail 'empty numstat rename path'
  fi
  files=$((files + 1))
  if [[ "$added" == - && "$deleted" == - ]]; then
    binaries=$((binaries + 1))
  else
    [[ "$added" =~ ^[0-9]+$ && "$deleted" =~ ^[0-9]+$ ]] \
      || fail 'invalid numstat line counts'
    additions=$((additions + 10#$added))
    deletions=$((deletions + 10#$deleted))
  fi
done < "$stats"

printf 'base=%s target=%s\n' "$base" "$target"
printf '%d files +%d/-%d (binary files: %d; line totals are text-only)\n' \
  "$files" "$additions" "$deletions" "$binaries"
if [[ "$binaries" -gt 0 ]]; then
  printf 'CAP: FAIL (binary line counts unavailable; additions cannot be verified)\n'
  exit 1
fi
if [[ "$files" -gt 20 || "$additions" -gt 800 ]]; then
  printf 'CAP: FAIL (limit 20 files/+800; deletion credit 0)\n'
  exit 1
fi
printf 'CAP: PASS (remaining %d files/+%d; deletion credit 0)\n' \
  "$((20 - files))" "$((800 - additions))"
