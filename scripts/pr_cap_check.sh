#!/usr/bin/env bash
# Measure committed production code in the caller's repository.
set -euo pipefail
fail() { printf 'CAP: ERROR (%s)\n' "$1" >&2; exit 1; }
notice() {
  local message="$1" escaped="$1"
  printf '%s\n' "$message"
  [[ "${GITHUB_ACTIONS:-false}" == true ]] || return 0
  escaped="${escaped//%/%25}"
  escaped="${escaped//$'\r'/%0D}"
  escaped="${escaped//$'\n'/%0A}"
  printf '::warning title=PR cap::%s\n' "$escaped"
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    printf '%s\n' "$message" >> "$GITHUB_STEP_SUMMARY"
  fi
}
[[ $# -le 1 ]] || fail 'usage: scripts/pr_cap_check.sh [commit-ish]'
branch="${1-HEAD}"
base_ref=refs/remotes/origin/main
reason=''
reason_count=0
mode="${PR_CAP_MODE:-enforce}"
case "$mode" in enforce|report-only|off) ;; *) fail 'invalid PR_CAP_MODE';; esac
if [[ "$mode" == off ]]; then
  notice 'CAP: DISABLED (PR_CAP_MODE=off)'
  exit 0
fi
if [[ "${PR_CAP_CI:-0}" == 1 ]]; then
  event="$(python3 - "$GITHUB_EVENT_PATH" <<'EVENT'
import json,re,sys
p=json.load(open(sys.argv[1]))['pull_request']
for sha in (p['head']['sha'],p['base']['sha']):
    if not re.fullmatch('[0-9a-f]{40}',sha): raise ValueError('invalid PR SHA')
    print(sha)
body=(p.get('body') or '').replace('\r\n','\n').replace('\r','\n')
reasons=[]
fence=None
in_comment=False
quoted=False
# Examples and hidden template text do not authorize an exception.
for line in body.split('\n'):
    if fence:
        if re.fullmatch(r' {0,3}'+re.escape(fence[0])+r'{'+str(fence[1])+r',}[ \t]*',line):
            fence=None
        continue
    if line.lstrip().startswith('>'):
        quoted=True
        continue
    if quoted:
        if line.strip(): continue
        quoted=False
    if in_comment or '<!--' in line:
        for token in re.split(r'(<!--|-->)',line):
            if token=='<!--': in_comment=True
            elif token=='-->': in_comment=False
        continue
    opening=re.match(r' {0,3}(`{3,}|~{3,})(.*)$',line)
    if opening and (opening[1][0]!='`' or '`' not in opening[2]):
        fence=(opening[1][0],len(opening[1]))
        continue
    match=re.fullmatch(r'PR-CAP-EXEMPT:[ \t]*([^ \t\r\n][^\r\n]*)',line)
    if match: reasons.append(match[1])
print(len(reasons))
print(reasons[0] if reasons else '')
EVENT
)" || fail 'cannot read PR head/base/exemption'
  branch="$(printf '%s\n' "$event" | sed -n '1p')"
  base_ref="$(printf '%s\n' "$event" | sed -n '2p')"
  reason_count="$(printf '%s\n' "$event" | sed -n '3p')"
  reason="$(printf '%s\n' "$event" | sed -n '4p')"
fi
[[ -n "$branch" && "$branch" != -* ]] || fail 'invalid target ref'
if [[ "${PR_CAP_CI:-0}" != 1 ]]; then
  git fetch --quiet --no-tags origin +refs/heads/main:refs/remotes/origin/main \
    || fail 'cannot fetch origin main; no local-main fallback'
fi
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
if [[ "$rc" == 0 ]]; then
  if [[ "$mode" == report-only ]]; then
    notice 'CAP: REPORT-ONLY (within limits; enforcement disabled)'
  fi
  exit 0
fi
# Only a measured cap violation may be advisory or exempt; producer errors fail.
[[ "$rc" == 1 && "$output" == *'CAP: FAIL ('* ]] || fail 'production measurement failed'
[[ "$reason_count" -le 1 ]] || fail 'multiple exemption reasons'
if [[ -n "$reason" ]]; then
  notice "CAP: EXEMPT ($reason)"
  exit 0
fi
if [[ "$mode" == report-only ]]; then
  notice 'CAP: REPORT-ONLY (measured violation; enforcement disabled)'
  exit 0
fi
exit 1
