#!/usr/bin/env bash
# Measure committed production code in the caller's repository.
set -euo pipefail
fail() { printf 'CAP: ERROR (%s)\n' "$1" >&2; exit 1; }
[[ $# -le 1 ]] || fail 'usage: scripts/pr_cap_check.sh [commit-ish]'
branch="${1-HEAD}"
base_ref=refs/remotes/origin/main
reason=''
mode="${PR_CAP_MODE:-enforce}"
case "$mode" in enforce|report-only|off) ;; *) fail 'invalid PR_CAP_MODE';; esac
if [[ "$mode" == off ]]; then
  printf 'CAP: DISABLED (PR_CAP_MODE=off)\n'
  exit 0
fi
if [[ "${PR_CAP_CI:-0}" == 1 ]]; then
  event="$(python3 - "$GITHUB_EVENT_PATH" <<'EVENT'
import json,re,sys
p=json.load(open(sys.argv[1]))['pull_request']
for sha in (p['head']['sha'],p['base']['sha']):
    if not re.fullmatch('[0-9a-f]{40}',sha): raise ValueError('invalid PR SHA')
    print(sha)
reasons=re.findall(r'^PR-CAP-EXEMPT:[ \t]*([^ \t\r\n][^\r\n]*)$',p.get('body') or '',re.M)
if len(reasons)>1: raise ValueError('multiple exemption reasons')
print(reasons[0] if reasons else '')
EVENT
)" || fail 'cannot read PR head/base/exemption'
  branch="$(printf '%s\n' "$event" | sed -n '1p')"
  base_ref="$(printf '%s\n' "$event" | sed -n '2p')"
  reason="$(printf '%s\n' "$event" | sed -n '3p')"
fi
[[ -n "$branch" && "$branch" != -* ]] || fail 'invalid target ref'
git fetch --quiet --no-tags origin +refs/heads/main:refs/remotes/origin/main \
  || fail 'cannot fetch origin main; no local-main fallback'
target="$(git -c core.warnAmbiguousRefs=true rev-parse --verify --end-of-options "${branch}^{commit}" 2>&1)" \
  || fail 'target must resolve to one commit'
[[ "$target" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || fail 'target ref must resolve without ambiguity or warnings'
upstream="$(git rev-parse --verify "${base_ref}^{commit}")" || fail 'cannot resolve base'
base="$(git merge-base "$upstream" "$target")" || fail 'target and base have no usable merge-base'
printf 'base=%s target=%s mode=%s\n' "$base" "$target" "$mode"
helper="$(dirname "${BASH_SOURCE[0]}")/pr_cap_prod.py"
rc=0
output="$(python3 "$helper" "$base" "$target" --repo "$(git rev-parse --show-toplevel)" 2>&1)" || rc=$?
printf '%s\n' "$output"
[[ "$rc" == 0 ]] && exit 0
# Only a measured cap violation may be advisory or exempt; producer errors fail.
[[ "$rc" == 1 && "$output" == *'CAP: FAIL ('* ]] || fail 'production measurement failed'
if [[ -n "$reason" ]]; then
  printf 'CAP: EXEMPT (%s)\n' "$reason"
  exit 0
fi
if [[ "$mode" == report-only ]]; then
  printf 'CAP: REPORT-ONLY (measured violation; enforcement disabled)\n'
  exit 0
fi
exit 1
