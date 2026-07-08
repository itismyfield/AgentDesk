#!/usr/bin/env bash
set -euo pipefail

if [[ "${TRACE-}" == "1" ]]; then
  set -x
fi

SELF_NAME="$(basename "$0")"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

CURRENT_IDS_FILE="$TMP_DIR/current-ids.txt"
PREVIOUS_IDS_FILE="$TMP_DIR/previous-ids.txt"
CURRENT_PASSED_IDS_FILE="$TMP_DIR/current-passed-ids.txt"
PREVIOUS_PASSED_IDS_FILE="$TMP_DIR/previous-passed-ids.txt"
CURRENT_META_FILE="$TMP_DIR/current-meta.tsv"
OPEN_ISSUES_FILE="$TMP_DIR/open-issues.tsv"

: >"$CURRENT_IDS_FILE"
: >"$PREVIOUS_IDS_FILE"
: >"$CURRENT_PASSED_IDS_FILE"
: >"$PREVIOUS_PASSED_IDS_FILE"
: >"$CURRENT_META_FILE"
: >"$OPEN_ISSUES_FILE"

# #4245: A one-off infrastructure-level termination (SIGTERM / signal 15 / exit
# 143 / runner shutdown) is flaky runner pressure and is skipped (#3991). But the
# SAME job terminating this way across N *consecutive* main runs is no longer
# flake — it is a deterministic infra regression (typically OOM) that today stays
# permanently silent because the 2-consecutive real-failure promotion condition
# is never met (infra terminations record no identifier). This is the consecutive
# streak threshold at which a persistent infra termination is escalated to a
# ci-red issue. 1–2 occurrences remain skipped (anti-flake intent preserved).
SIGTERM_ESCALATION_STREAK="${SIGTERM_ESCALATION_STREAK:-3}"

usage() {
  cat <<EOF
Usage: $SELF_NAME [--self-test]
EOF
}

api_base_url() {
  local raw="${AGENTDESK_API_URL-}"
  raw="${raw%"${raw##*[![:space:]]}"}"
  raw="${raw#"${raw%%[![:space:]]*}"}"
  printf '%s' "${raw%/}"
}

require_cmd() {
  local name="$1"
  if ! command -v "$name" >/dev/null 2>&1; then
    echo "missing required command: $name" >&2
    exit 1
  fi
}

sanitize_filename() {
  local raw="$1"
  raw="${raw//[^A-Za-z0-9_.-]/_}"
  printf '%s' "$raw"
}

identifier_title() {
  local identifier="$1"
  printf '[ci-red] %s 실패 (main)' "$identifier"
}

extract_identifier_from_title() {
  local title="$1"
  if [[ "$title" =~ ^\[ci-red\]\ (.+)\ 실패\ \(main\)$ ]]; then
    printf '%s' "${BASH_REMATCH[1]}"
  fi
}

ensure_label() {
  local repo="$1"
  local label="$2"
  local color="$3"
  local description="$4"
  gh label create "$label" --repo "$repo" --color "$color" --description "$description" >/dev/null 2>&1 || true
}

file_has_exact_line() {
  local path="$1"
  local needle="$2"
  grep -F -x -q -- "$needle" "$path"
}

append_unique_line() {
  local path="$1"
  local line="$2"
  if ! file_has_exact_line "$path" "$line"; then
    printf '%s\n' "$line" >>"$path"
  fi
}

lookup_open_issue_by_title() {
  local title="$1"
  awk -F '\t' -v title="$title" '$3 == title { print $2; exit }' "$OPEN_ISSUES_FILE"
}

lookup_open_issue_by_identifier() {
  local identifier="$1"
  awk -F '\t' -v identifier="$identifier" '$1 == identifier { print $2; exit }' "$OPEN_ISSUES_FILE"
}

lookup_current_meta_field() {
  local identifier="$1"
  local field_index="$2"
  awk -F '\t' -v identifier="$identifier" -v field="$field_index" '
    $1 == identifier {
      print $field
      exit
    }
  ' "$CURRENT_META_FILE"
}

load_open_issues() {
  local repo="$1"
  local line number title identifier

  : >"$OPEN_ISSUES_FILE"
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    number="$(jq -r '.number' <<<"$line")"
    title="$(jq -r '.title' <<<"$line")"
    identifier="$(extract_identifier_from_title "$title" || true)"
    if [[ -n "$identifier" ]]; then
      printf '%s\t%s\t%s\n' "$identifier" "$number" "$title" >>"$OPEN_ISSUES_FILE"
    fi
  done < <(gh issue list --repo "$repo" --label ci-red --state open --limit 200 --json number,title | jq -c '.[]')
}

fetch_job_log() {
  local repo="$1"
  local run_id="$2"
  local job_id="$3"
  local path="$4"

  if gh run view "$run_id" --repo "$repo" --job "$job_id" --log-failed >"$path" 2>"$path.stderr"; then
    return 0
  fi

  if gh run view "$run_id" --repo "$repo" --job "$job_id" --log >"$path" 2>"$path.stderr"; then
    return 0
  fi

  cat "$path.stderr" >"$path"
}

parse_failed_identifiers_from_log() {
  local log_path="$1"
  sed -nE 's/^test ([A-Za-z0-9_:]+) \.\.\. FAILED$/\1/p' "$log_path" | sort -u
}

parse_passed_identifiers_from_log() {
  local log_path="$1"
  sed -nE 's/^test ([A-Za-z0-9_:]+) \.\.\. ok$/\1/p' "$log_path" | sort -u
}

# #3991: Infrastructure-level termination is flaky runner pressure (OOM /
# SIGTERM / exit 143 / runner cancellation), not a test assertion. This is only
# consulted on the job-level fallback path — a failed job whose log has NO
# `test … FAILED` line. A real test failure produces a `test … FAILED` line and
# never reaches this check, so genuine red is never suppressed.
log_has_infra_termination() {
  local log_path="$1"
  [[ -f "$log_path" ]] || return 1
  grep -E -i -q -- \
    'signal[: ]+(9|15)([^0-9]|$)|sig(term|kill)|terminated on line [0-9]+ by signal|(exit(ed)?|code|status)[^0-9]*143([^0-9]|$)|the operation was cancell?ed|runner has received a shutdown signal' \
    "$log_path"
}

# #3996: Real-failure guard for the flaky skip filter. `log_has_infra_termination`
# is too coarse on its own — a genuine job-level regression (e.g. a compile error
# that never emits a `test … FAILED` line, so it hits the job-level fallback) can
# ALSO carry SIGTERM / exit-143 noise (the runner tears down remaining steps after
# a hard failure). Skipping that as flaky would be a false negative: real red
# silently dropped, never promoted to ci-red. This helper detects *deterministic*
# failure signals that unambiguously mean "the code is broken", so infra-only
# skips can be gated on their absence. Kept deliberately narrow — only signals
# that appear on a genuine compile/test failure, not benign `error:`/`failed`
# chatter — to avoid re-widening into a false-positive filter.
log_has_real_failure() {
  local log_path="$1"
  [[ -f "$log_path" ]] || return 1
  grep -E -i -q -- \
    'error\[E[0-9]|error: could not compile|test result: FAILED|panicked at|assertion .*failed' \
    "$log_path"
}

record_failed_identifier() {
  local prefix="$1"
  local identifier="$2"
  local job_name="$3"
  local job_id="$4"
  local job_url="$5"
  local log_path="$6"

  if [[ "$prefix" == "current" ]]; then
    append_unique_line "$CURRENT_IDS_FILE" "$identifier"
    if ! awk -F '\t' -v identifier="$identifier" '$1 == identifier { found = 1 } END { exit found ? 0 : 1 }' "$CURRENT_META_FILE"; then
      printf '%s\t%s\t%s\t%s\t%s\n' "$identifier" "$job_name" "$job_id" "$job_url" "$log_path" >>"$CURRENT_META_FILE"
    fi
  else
    append_unique_line "$PREVIOUS_IDS_FILE" "$identifier"
  fi
}

record_passed_identifier() {
  local prefix="$1"
  local identifier="$2"

  if [[ "$prefix" == "current" ]]; then
    append_unique_line "$CURRENT_PASSED_IDS_FILE" "$identifier"
  else
    append_unique_line "$PREVIOUS_PASSED_IDS_FILE" "$identifier"
  fi
}

# #4245: Persistent-infra-termination track. Infra-only skips (see
# collect_failed_identifiers) are kept out of the real-failure identifier files so
# a single/double SIGTERM never trips the 2-consecutive promotion. Instead each
# infra termination is accumulated on a per-run track keyed by run id; the
# escalation loop in run_triage promotes an `infra::job::…` identifier only when
# it recurs across SIGTERM_ESCALATION_STREAK consecutive runs. `record_meta=1`
# additionally records current-run job metadata (job url / log path) so the ci-red
# issue body can be rendered, mirroring record_failed_identifier's current branch.
record_infra_identifier() {
  local run_id="$1"
  local identifier="$2"
  local job_name="$3"
  local job_id="$4"
  local job_url="$5"
  local log_path="$6"
  local record_meta="$7"
  local ids_file="$TMP_DIR/infra-ids-${run_id}.txt"

  # Pre-create the per-run track file: append_unique_line greps it first, and the
  # infra files are created lazily (unlike the eagerly `: >`-initialised id files),
  # so the very first write would otherwise emit a benign `grep: No such file`.
  [[ -f "$ids_file" ]] || : >"$ids_file"
  append_unique_line "$ids_file" "$identifier"
  if [[ "$record_meta" == "1" ]]; then
    if ! awk -F '\t' -v identifier="$identifier" '$1 == identifier { found = 1 } END { exit found ? 0 : 1 }' "$CURRENT_META_FILE"; then
      printf '%s\t%s\t%s\t%s\t%s\n' "$identifier" "$job_name" "$job_id" "$job_url" "$log_path" >>"$CURRENT_META_FILE"
    fi
  fi
}

collect_failed_identifiers() {
  local prefix="$1"
  local repo="$2"
  local run_id="$3"
  local jobs_json line job_id job_name job_url log_path matched identifier

  jobs_json="$(gh api "/repos/$repo/actions/runs/$run_id/jobs?per_page=100")"
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    job_id="$(jq -r '.id' <<<"$line")"
    job_name="$(jq -r '.name' <<<"$line")"
    job_url="$(jq -r '.html_url // empty' <<<"$line")"
    log_path="$TMP_DIR/${prefix}-${job_id}-$(sanitize_filename "$job_name").log"
    fetch_job_log "$repo" "$run_id" "$job_id" "$log_path"

    matched=0
    while IFS= read -r identifier; do
      [[ -n "$identifier" ]] || continue
      matched=1
      record_failed_identifier "$prefix" "$identifier" "$job_name" "$job_id" "$job_url" "$log_path"
    done < <(parse_failed_identifiers_from_log "$log_path")

    if [[ "$matched" == "0" ]]; then
      # #3991/#3996: No `test … FAILED` assertion in a failed job's log means this
      # is a job-level fallback. Skip it as flaky ONLY when an infrastructure-level
      # termination (SIGTERM / signal 15 / exit 143 / runner cancellation) is the
      # *sole* failure signal. If the log ALSO carries a deterministic real-failure
      # signal (compile error `error[E…]` / `could not compile`, `test result:
      # FAILED`, `panicked at`, failed assertion), the infra noise is incidental —
      # promote it normally so genuine red is never silently dropped (false
      # negative). Only pure infra terminations are suppressed.
      if log_has_infra_termination "$log_path" && ! log_has_real_failure "$log_path"; then
        # #4245: don't drop the signal entirely — record it on the separate
        # `infra::job::…` track so a *persistent* streak (SIGTERM_ESCALATION_STREAK
        # consecutive runs) can still be escalated. A single/double occurrence
        # accumulates here but is never promoted, preserving the anti-flake intent.
        if [[ "$prefix" == "current" ]]; then
          record_infra_identifier "$run_id" "infra::job::$job_name" "$job_name" "$job_id" "$job_url" "$log_path" 1
        else
          record_infra_identifier "$run_id" "infra::job::$job_name" "$job_name" "$job_id" "$job_url" "$log_path" 0
        fi
        continue
      fi
      identifier="job::$job_name"
      record_failed_identifier "$prefix" "$identifier" "$job_name" "$job_id" "$job_url" "$log_path"
    fi
  done < <(jq -c '.jobs[] | select(.conclusion == "failure")' <<<"$jobs_json")
}

# #4245: Lightweight infra-only pass over an older prior run (the runs before
# `previous`) used purely to measure the persistent-SIGTERM streak. It deliberately
# does NOT touch the real-failure identifier files — the 2-consecutive real-failure
# promotion still compares only current vs previous. A run only counts toward the
# streak when the job is a *pure* infra termination (no `test … FAILED` assertion
# and no real-failure signal), the identical gate the current/previous collection
# applies, so a real failure or a green run in the middle breaks the streak.
collect_infra_identifiers_only() {
  local repo="$1"
  local run_id="$2"
  local jobs_json line job_id job_name job_url log_path

  jobs_json="$(gh api "/repos/$repo/actions/runs/$run_id/jobs?per_page=100")"
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    job_id="$(jq -r '.id' <<<"$line")"
    job_name="$(jq -r '.name' <<<"$line")"
    job_url="$(jq -r '.html_url // empty' <<<"$line")"
    log_path="$TMP_DIR/infra-${run_id}-${job_id}-$(sanitize_filename "$job_name").log"
    fetch_job_log "$repo" "$run_id" "$job_id" "$log_path"

    # A `test … FAILED` assertion means this was a real test failure, not a pure
    # infra termination — it does not extend the infra streak.
    if parse_failed_identifiers_from_log "$log_path" | grep -q .; then
      continue
    fi
    if log_has_infra_termination "$log_path" && ! log_has_real_failure "$log_path"; then
      record_infra_identifier "$run_id" "infra::job::$job_name" "$job_name" "$job_id" "$job_url" "$log_path" 0
    fi
  done < <(jq -c '.jobs[] | select(.conclusion == "failure")' <<<"$jobs_json")
}

collect_passed_identifiers() {
  local prefix="$1"
  local repo="$2"
  local run_id="$3"
  local jobs_json line job_id job_name log_path matched identifier

  jobs_json="$(gh api "/repos/$repo/actions/runs/$run_id/jobs?per_page=100")"
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    job_id="$(jq -r '.id' <<<"$line")"
    job_name="$(jq -r '.name' <<<"$line")"
    log_path="$TMP_DIR/${prefix}-pass-${job_id}-$(sanitize_filename "$job_name").log"
    fetch_job_log "$repo" "$run_id" "$job_id" "$log_path"

    matched=0
    while IFS= read -r identifier; do
      [[ -n "$identifier" ]] || continue
      matched=1
      record_passed_identifier "$prefix" "$identifier"
    done < <(parse_passed_identifiers_from_log "$log_path")

    if [[ "$matched" == "0" ]]; then
      record_passed_identifier "$prefix" "job::$job_name"
    fi

    # #4245: A successful job clears any persistent infra-termination streak for
    # that job. Record the infra identifier on the passed track (unconditionally —
    # a recovered infra job may pass with per-test `... ok` lines, so `matched`
    # cannot gate this) so an escalated `infra::job::…` ci-red issue auto-closes
    # after two consecutive green runs, symmetric to the escalation.
    record_passed_identifier "$prefix" "infra::job::$job_name"
  done < <(jq -c '.jobs[] | select(.conclusion == "success")' <<<"$jobs_json")
}

render_log_snippet() {
  local identifier="$1"
  local log_path="$2"

  if [[ "$identifier" == job::* || "$identifier" == infra::job::* ]]; then
    head -n 40 "$log_path"
    return 0
  fi

  if grep -F -q -- "$identifier" "$log_path"; then
    awk -v needle="$identifier" '
      index($0, needle) {
        start = NR - 3
        if (start < 1) {
          start = 1
        }
        end = NR + 6
      }
      { lines[NR] = $0 }
      END {
        if (start == 0) {
          exit 1
        }
        for (i = start; i <= end && i <= NR; i++) {
          print lines[i]
        }
      }
    ' "$log_path" | head -n 20
    return 0
  fi

  if grep -E 'FAILED|error:' "$log_path" >/dev/null 2>&1; then
    grep -E 'FAILED|error:' "$log_path" | head -n 20
  else
    head -n 20 "$log_path"
  fi
}

build_issue_body() {
  local identifier="$1"
  local run_url="$2"
  local head_sha="$3"
  local job_url="$4"
  local log_path="$5"
  local snippet repro background

  snippet="$(render_log_snippet "$identifier" "$log_path")"
  if [[ "$identifier" == infra::job::* ]]; then
    # #4245: persistent infra termination — not a test assertion, so there is no
    # cargo repro; point the investigation at runner resources instead.
    repro="_persistent infra-level termination (SIGTERM / signal 15 / exit 143 / OOM / runner shutdown) across ${SIGTERM_ESCALATION_STREAK} consecutive main runs; not a test assertion — investigate runner memory / timeouts_"
    background="main 의 \`CI Main\` 에서 동일 job 의 인프라 레벨 종료(SIGTERM / exit 143 / OOM / runner shutdown)가 ${SIGTERM_ESCALATION_STREAK}회 연속 관측되어 자동 생성된 ci-red 이슈입니다. 일회성 flake 가 아니라 지속성 인프라 회귀입니다."
  elif [[ "$identifier" == job::* ]]; then
    repro="_job-level failure; see failing workflow job_"
    background="main 의 \`CI Main\` 에서 동일 실패가 2회 연속 관측되어 자동 생성된 ci-red 이슈입니다."
  else
    repro="cargo test -p agentdesk $identifier -- --exact --nocapture"
    background="main 의 \`CI Main\` 에서 동일 실패가 2회 연속 관측되어 자동 생성된 ci-red 이슈입니다."
  fi

  cat <<EOF
## 배경

$background

## 내용

- 식별자: \`$identifier\`
- 최근 red run: $run_url
- 실패 job: ${job_url:-_job url unavailable_}
- 최근 red 후보 커밋: \`$head_sha\`
- 재현 커맨드: \`$repro\`

## 실패 로그 head

\`\`\`text
$snippet
\`\`\`
EOF
}

comment_for_repeat_failure() {
  local run_url="$1"
  local head_sha="$2"
  cat <<EOF
동일 ci-red 가 main 에서 다시 관측되었습니다.

- run: $run_url
- head: \`$head_sha\`
EOF
}

comment_for_recovery() {
  local run_url="$1"
  cat <<EOF
자동 회복 확인: 해당 ci-red 식별자가 main 에서 2회 연속 green 이었습니다.

- recovery run: $run_url
EOF
}

sync_issue_card_now() {
  local repo="$1"
  local api_base curl_config status

  api_base="$(api_base_url)"
  [[ -n "$api_base" ]] || return 0

  require_cmd curl

  curl_config=(
    --silent
    --show-error
    --output /dev/null
    --write-out '%{http_code}'
    --request POST
  )
  if [[ -n "${AGENTDESK_API_TOKEN-}" ]]; then
    curl_config+=(--header "Authorization: Bearer ${AGENTDESK_API_TOKEN}")
  fi

  status="$(
    curl \
      "${curl_config[@]}" \
      "${api_base}/api/github/repos/${repo}/sync"
  )" || {
    echo "warn: failed to invoke immediate AgentDesk GitHub sync for ${repo}" >&2
    return 0
  }

  if [[ "$status" -lt 200 || "$status" -ge 300 ]]; then
    echo "warn: immediate AgentDesk GitHub sync for ${repo} returned HTTP ${status}" >&2
  fi
}

create_or_comment_issue() {
  local repo="$1"
  local identifier="$2"
  local run_url="$3"
  local head_sha="$4"
  local title number job_url log_path body

  title="$(identifier_title "$identifier")"
  number="$(lookup_open_issue_by_title "$title")"
  if [[ -n "$number" ]]; then
    gh issue comment "$number" --repo "$repo" --body "$(comment_for_repeat_failure "$run_url" "$head_sha")" >/dev/null
    sync_issue_card_now "$repo"
    return 0
  fi

  ensure_label "$repo" "ci-red" "B60205" "Main branch CI red triage issue"
  ensure_label "$repo" "agent:project-agentdesk" "1D76DB" "Assigned to project-agentdesk"

  job_url="$(lookup_current_meta_field "$identifier" 4)"
  log_path="$(lookup_current_meta_field "$identifier" 5)"
  body="$(build_issue_body "$identifier" "$run_url" "$head_sha" "$job_url" "$log_path")"

  gh issue create \
    --repo "$repo" \
    --title "$title" \
    --body "$body" \
    --label "ci-red" \
    --label "agent:project-agentdesk" >/dev/null
  sync_issue_card_now "$repo"
}

close_recovered_issue() {
  local repo="$1"
  local identifier="$2"
  local run_url="$3"
  local number

  number="$(lookup_open_issue_by_identifier "$identifier")"
  [[ -n "$number" ]] || return 0

  gh issue comment "$number" --repo "$repo" --body "$(comment_for_recovery "$run_url")" >/dev/null
  gh issue close "$number" --repo "$repo" >/dev/null
}

run_triage() {
  require_cmd gh
  require_cmd jq

  if [[ -n "${GITHUB_TOKEN-}" && -z "${GH_TOKEN-}" ]]; then
    export GH_TOKEN="$GITHUB_TOKEN"
  fi

  local repo event_path workflow_name workflow_id current_run_id head_branch current_run_url head_sha current_run_conclusion previous_runs_json previous_run_id previous_run_conclusion identifier
  local streak_idx streak_ok _rid
  local -a prior_run_ids=()
  repo="${GITHUB_REPOSITORY:-}"
  event_path="${GITHUB_EVENT_PATH:-}"

  if [[ -z "$repo" || -z "$event_path" ]]; then
    echo "GITHUB_REPOSITORY and GITHUB_EVENT_PATH are required" >&2
    exit 1
  fi

  workflow_name="$(jq -r '.workflow_run.name // empty' "$event_path")"
  workflow_id="$(jq -r '.workflow_run.workflow_id // empty' "$event_path")"
  current_run_id="$(jq -r '.workflow_run.id // empty' "$event_path")"
  head_branch="$(jq -r '.workflow_run.head_branch // empty' "$event_path")"
  current_run_url="$(jq -r '.workflow_run.html_url // empty' "$event_path")"
  head_sha="$(jq -r '.workflow_run.head_sha // empty' "$event_path")"
  current_run_conclusion="$(jq -r '.workflow_run.conclusion // empty' "$event_path")"

  if [[ "$workflow_name" != "CI Main" || "$head_branch" != "main" ]]; then
    echo "skip: workflow_run is not CI Main on main" >&2
    exit 0
  fi

  previous_runs_json="$(gh api "/repos/$repo/actions/workflows/$workflow_id/runs?branch=main&status=completed&per_page=5")"
  previous_run_id="$(
    jq -r --arg current_run_id "$current_run_id" '
      .workflow_runs
      | map(select((.id | tostring) != $current_run_id))
      | map(select(.conclusion != "cancelled"))
      | .[0].id // empty
    ' <<<"$previous_runs_json"
  )"
  previous_run_conclusion="$(
    jq -r --arg current_run_id "$current_run_id" '
      .workflow_runs
      | map(select((.id | tostring) != $current_run_id))
      | map(select(.conclusion != "cancelled"))
      | .[0].conclusion // empty
    ' <<<"$previous_runs_json"
  )"

  # #4245: ordered list of prior (non-current, non-cancelled) run ids, newest
  # first — prior_run_ids[0] is `previous`. Used by the persistent-infra streak
  # check to look back over SIGTERM_ESCALATION_STREAK-1 runs. Built with a read
  # loop rather than `mapfile` for bash 3.2 (macOS) portability.
  while IFS= read -r _rid; do
    [[ -n "$_rid" ]] || continue
    prior_run_ids+=("$_rid")
  done < <(
    jq -r --arg current_run_id "$current_run_id" '
      .workflow_runs
      | map(select((.id | tostring) != $current_run_id))
      | map(select(.conclusion != "cancelled"))
      | .[].id // empty
    ' <<<"$previous_runs_json"
  )

  if [[ -z "$previous_run_id" ]]; then
    echo "skip: no previous CI Main run on main to compare against" >&2
    exit 0
  fi

  load_open_issues "$repo"

  if [[ "$current_run_conclusion" == "failure" ]]; then
    collect_failed_identifiers "current" "$repo" "$current_run_id"
    collect_failed_identifiers "previous" "$repo" "$previous_run_id"

    while IFS= read -r identifier; do
      [[ -n "$identifier" ]] || continue
      if file_has_exact_line "$PREVIOUS_IDS_FILE" "$identifier"; then
        create_or_comment_issue "$repo" "$identifier" "$current_run_url" "$head_sha"
      fi
    done <"$CURRENT_IDS_FILE"

    # #4245: persistent-SIGTERM escalation. current + previous infra sets are
    # already populated (collect_failed_identifiers above records onto the
    # `infra::job::…` track when it skips a pure infra termination). We only have a
    # streak to evaluate when at least SIGTERM_ESCALATION_STREAK-1 prior runs are
    # known; otherwise history is too short to distinguish flake from regression
    # and the anti-flake skip stands. Gather the remaining (STREAK-2) older prior
    # runs' infra sets on demand, then promote an infra identifier only when it is
    # present in the current run AND every one of the STREAK-1 prior runs.
    if (( SIGTERM_ESCALATION_STREAK >= 2 )) && (( ${#prior_run_ids[@]} >= SIGTERM_ESCALATION_STREAK - 1 )); then
      for (( streak_idx = 1; streak_idx <= SIGTERM_ESCALATION_STREAK - 2; streak_idx++ )); do
        collect_infra_identifiers_only "$repo" "${prior_run_ids[streak_idx]}"
      done

      # Guarantee every track file exists so grep/read never trip under `set -e`
      # when a run contributed zero infra terminations.
      touch "$TMP_DIR/infra-ids-${current_run_id}.txt"
      for (( streak_idx = 0; streak_idx <= SIGTERM_ESCALATION_STREAK - 2; streak_idx++ )); do
        touch "$TMP_DIR/infra-ids-${prior_run_ids[streak_idx]}.txt"
      done

      while IFS= read -r identifier; do
        [[ -n "$identifier" ]] || continue
        streak_ok=1
        for (( streak_idx = 0; streak_idx <= SIGTERM_ESCALATION_STREAK - 2; streak_idx++ )); do
          if ! file_has_exact_line "$TMP_DIR/infra-ids-${prior_run_ids[streak_idx]}.txt" "$identifier"; then
            streak_ok=0
            break
          fi
        done
        if [[ "$streak_ok" == "1" ]]; then
          create_or_comment_issue "$repo" "$identifier" "$current_run_url" "$head_sha"
        fi
      done <"$TMP_DIR/infra-ids-${current_run_id}.txt"
    fi
  fi

  if [[ "$current_run_conclusion" == "success" && "$previous_run_conclusion" == "success" ]]; then
    collect_passed_identifiers "current" "$repo" "$current_run_id"
    collect_passed_identifiers "previous" "$repo" "$previous_run_id"

    while IFS= read -r identifier; do
      [[ -n "$identifier" ]] || continue
      if file_has_exact_line "$CURRENT_PASSED_IDS_FILE" "$identifier" && file_has_exact_line "$PREVIOUS_PASSED_IDS_FILE" "$identifier"; then
        close_recovered_issue "$repo" "$identifier" "$current_run_url"
      fi
    done < <(cut -f1 "$OPEN_ISSUES_FILE")
  fi
}

assert_contains() {
  local needle="$1"
  local path="$2"
  if ! grep -F -q -- "$needle" "$path"; then
    echo "assertion failed: missing '$needle' in $path" >&2
    exit 1
  fi
}

install_mock_gh() {
  local scenario_dir="$1"
  cat >"$scenario_dir/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

scenario_dir="$(cd "$(dirname "$0")" && pwd)"
log_path="$scenario_dir/gh.log"
printf '%s\n' "$*" >>"$log_path"

cmd="${1-}"
sub="${2-}"

if [[ "$cmd" == "label" && "$sub" == "create" ]]; then
  exit 0
fi

if [[ "$cmd" == "api" ]]; then
  endpoint="${2-}"
  if [[ "$endpoint" == "/repos/test/repo/actions/workflows/1/runs?branch=main&status=completed&per_page=5" ]]; then
    cat "$scenario_dir/workflow-runs.json"
    exit 0
  fi
  if [[ "$endpoint" == "/repos/test/repo/actions/runs/200/jobs?per_page=100" ]]; then
    cat "$scenario_dir/current-jobs.json"
    exit 0
  fi
  if [[ "$endpoint" == "/repos/test/repo/actions/runs/199/jobs?per_page=100" ]]; then
    cat "$scenario_dir/previous-jobs.json"
    exit 0
  fi
  if [[ "$endpoint" == "/repos/test/repo/actions/runs/198/jobs?per_page=100" ]]; then
    cat "$scenario_dir/previous2-jobs.json"
    exit 0
  fi
fi

if [[ "$cmd" == "run" && "$sub" == "view" ]]; then
  run_id="${3-}"
  job_id=""
  prev=""
  for arg in "$@"; do
    if [[ "$prev" == "--job" ]]; then
      job_id="$arg"
      break
    fi
    prev="$arg"
  done
  cat "$scenario_dir/log-${run_id}-${job_id}.txt"
  exit 0
fi

if [[ "$cmd" == "issue" && "$sub" == "list" ]]; then
  cat "$scenario_dir/open-issues.json"
  exit 0
fi

if [[ "$cmd" == "issue" && "$sub" == "create" ]]; then
  printf '%s\n' "$*" >"$scenario_dir/issue-create.txt"
  exit 0
fi

if [[ "$cmd" == "issue" && "$sub" == "comment" ]]; then
  printf '%s\n' "$*" >>"$scenario_dir/issue-comment.txt"
  exit 0
fi

if [[ "$cmd" == "issue" && "$sub" == "close" ]]; then
  printf '%s\n' "$*" >>"$scenario_dir/issue-close.txt"
  exit 0
fi

echo "unexpected gh args: $*" >&2
exit 1
EOF
  chmod +x "$scenario_dir/gh"
}

install_mock_curl() {
  local scenario_dir="$1"
  cat >"$scenario_dir/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

scenario_dir="$(cd "$(dirname "$0")" && pwd)"
printf '%s\n' "$*" >>"$scenario_dir/curl.log"

url="${*: -1}"
status="${MOCK_CURL_STATUS:-200}"
if [[ -n "${MOCK_CURL_FAIL-}" ]]; then
  exit 1
fi

if [[ "$url" == "https://agentdesk.example/api/github/repos/test/repo/sync" ]]; then
  printf '%s' "$status"
  exit 0
fi

echo "unexpected curl args: $*" >&2
exit 1
EOF
  chmod +x "$scenario_dir/curl"
}

write_event_payload() {
  local path="$1"
  cat >"$path" <<'EOF'
{
  "workflow_run": {
    "name": "CI Main",
    "workflow_id": 1,
    "id": 200,
    "head_branch": "main",
    "conclusion": "failure",
    "html_url": "https://example.com/runs/200",
    "head_sha": "deadbeef200"
  }
}
EOF
}

write_success_event_payload() {
  local path="$1"
  cat >"$path" <<'EOF'
{
  "workflow_run": {
    "name": "CI Main",
    "workflow_id": 1,
    "id": 200,
    "head_branch": "main",
    "conclusion": "success",
    "html_url": "https://example.com/runs/200",
    "head_sha": "deadbeef200"
  }
}
EOF
}

write_cancelled_event_payload() {
  local path="$1"
  cat >"$path" <<'EOF'
{
  "workflow_run": {
    "name": "CI Main",
    "workflow_id": 1,
    "id": 200,
    "head_branch": "main",
    "conclusion": "cancelled",
    "html_url": "https://example.com/runs/200",
    "head_sha": "deadbeef200"
  }
}
EOF
}

scenario_two_run_failure_creates_issue() {
  local scenario_dir="$TMP_DIR/selftest-create"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":301,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/301"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":302,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/302"}]}
EOF
  cat >"$scenario_dir/log-200-301.txt" <<'EOF'
test server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state ... FAILED
EOF
  cat >"$scenario_dir/log-199-302.txt" <<'EOF'
test server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state ... FAILED
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  assert_contains "issue create --repo test/repo --title [ci-red] server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state 실패 (main)" "$scenario_dir/issue-create.txt"
  assert_contains "--label ci-red" "$scenario_dir/issue-create.txt"
  assert_contains "--label agent:project-agentdesk" "$scenario_dir/issue-create.txt"
}

scenario_new_issue_triggers_immediate_sync() {
  local scenario_dir="$TMP_DIR/selftest-sync-create"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  install_mock_curl "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":311,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/311"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":312,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/312"}]}
EOF
  cat >"$scenario_dir/log-200-311.txt" <<'EOF'
test server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state ... FAILED
EOF
  cat >"$scenario_dir/log-199-312.txt" <<'EOF'
test server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state ... FAILED
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    AGENTDESK_API_URL="https://agentdesk.example" \
    AGENTDESK_API_TOKEN="sync-token" \
    bash "$0"

  assert_contains "--request POST --header Authorization: Bearer sync-token https://agentdesk.example/api/github/repos/test/repo/sync" "$scenario_dir/curl.log"
}

scenario_existing_issue_gets_comment_only() {
  local scenario_dir="$TMP_DIR/selftest-comment"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":401,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/401"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":402,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/402"}]}
EOF
  cat >"$scenario_dir/log-200-401.txt" <<'EOF'
test server::routes::routes_tests::transition_to_done_records_true_negative_in_postgres_review_tuning ... FAILED
EOF
  cat >"$scenario_dir/log-199-402.txt" <<'EOF'
test server::routes::routes_tests::transition_to_done_records_true_negative_in_postgres_review_tuning ... FAILED
EOF
  cat >"$scenario_dir/open-issues.json" <<'EOF'
[{"number":9461,"title":"[ci-red] server::routes::routes_tests::transition_to_done_records_true_negative_in_postgres_review_tuning 실패 (main)"}]
EOF

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  assert_contains "issue comment 9461 --repo test/repo" "$scenario_dir/issue-comment.txt"
  if [[ -f "$scenario_dir/issue-create.txt" ]]; then
    echo "assertion failed: issue create must not run for existing ci-red issue" >&2
    exit 1
  fi
}

scenario_existing_issue_triggers_immediate_sync() {
  local scenario_dir="$TMP_DIR/selftest-sync-comment"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  install_mock_curl "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":411,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/411"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":412,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/412"}]}
EOF
  cat >"$scenario_dir/log-200-411.txt" <<'EOF'
test server::routes::routes_tests::transition_to_done_records_true_negative_in_postgres_review_tuning ... FAILED
EOF
  cat >"$scenario_dir/log-199-412.txt" <<'EOF'
test server::routes::routes_tests::transition_to_done_records_true_negative_in_postgres_review_tuning ... FAILED
EOF
  cat >"$scenario_dir/open-issues.json" <<'EOF'
[{"number":9461,"title":"[ci-red] server::routes::routes_tests::transition_to_done_records_true_negative_in_postgres_review_tuning 실패 (main)"}]
EOF

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    AGENTDESK_API_URL="https://agentdesk.example" \
    bash "$0"

  assert_contains "--request POST https://agentdesk.example/api/github/repos/test/repo/sync" "$scenario_dir/curl.log"
}

scenario_two_run_green_closes_issue() {
  local scenario_dir="$TMP_DIR/selftest-close"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_success_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"success"},{"id":199,"conclusion":"success"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":601,"name":"PostgreSQL tests","conclusion":"success","html_url":"https://example.com/jobs/601"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":602,"name":"PostgreSQL tests","conclusion":"success","html_url":"https://example.com/jobs/602"}]}
EOF
  cat >"$scenario_dir/log-200-601.txt" <<'EOF'
test server::routes::routes_tests::force_transition_to_ready_cancels_live_dispatches_and_skips_auto_queue_entries ... ok
EOF
  cat >"$scenario_dir/log-199-602.txt" <<'EOF'
test server::routes::routes_tests::force_transition_to_ready_cancels_live_dispatches_and_skips_auto_queue_entries ... ok
EOF
  cat >"$scenario_dir/open-issues.json" <<'EOF'
[{"number":9462,"title":"[ci-red] server::routes::routes_tests::force_transition_to_ready_cancels_live_dispatches_and_skips_auto_queue_entries 실패 (main)"}]
EOF

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  assert_contains "issue comment 9462 --repo test/repo" "$scenario_dir/issue-comment.txt"
  assert_contains "issue close 9462 --repo test/repo" "$scenario_dir/issue-close.txt"
}

scenario_recovered_infra_job_closes_issue() {
  # #4245: symmetric recovery for the escalation. Once an escalated
  # `infra::job::…` ci-red issue exists, two consecutive green runs of that job
  # (it stopped terminating) must auto-close it — mirroring the normal-identifier
  # recovery. The recovered job here succeeds with no per-test `... ok` line, so
  # the infra recovery record must not depend on a per-test match.
  local scenario_dir="$TMP_DIR/selftest-infra-recover"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_success_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"success"},{"id":199,"conclusion":"success"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":651,"name":"Full tests (ubuntu-latest)","conclusion":"success","html_url":"https://example.com/jobs/651"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":652,"name":"Full tests (ubuntu-latest)","conclusion":"success","html_url":"https://example.com/jobs/652"}]}
EOF
  cat >"$scenario_dir/log-200-651.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
    Finished test profile in 240s
EOF
  cat >"$scenario_dir/log-199-652.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
    Finished test profile in 236s
EOF
  cat >"$scenario_dir/open-issues.json" <<'EOF'
[{"number":9471,"title":"[ci-red] infra::job::Full tests (ubuntu-latest) 실패 (main)"}]
EOF

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  assert_contains "issue comment 9471 --repo test/repo" "$scenario_dir/issue-comment.txt"
  assert_contains "issue close 9471 --repo test/repo" "$scenario_dir/issue-close.txt"
}

scenario_cancelled_run_does_not_close_issue() {
  local scenario_dir="$TMP_DIR/selftest-cancelled"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_cancelled_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"cancelled"},{"id":199,"conclusion":"success"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":701,"name":"PostgreSQL tests","conclusion":"success","html_url":"https://example.com/jobs/701"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":702,"name":"PostgreSQL tests","conclusion":"success","html_url":"https://example.com/jobs/702"}]}
EOF
  cat >"$scenario_dir/log-200-701.txt" <<'EOF'
test server::routes::routes_tests::force_transition_to_ready_cancels_live_dispatches_and_skips_auto_queue_entries ... ok
EOF
  cat >"$scenario_dir/log-199-702.txt" <<'EOF'
test server::routes::routes_tests::force_transition_to_ready_cancels_live_dispatches_and_skips_auto_queue_entries ... ok
EOF
  cat >"$scenario_dir/open-issues.json" <<'EOF'
[{"number":9463,"title":"[ci-red] server::routes::routes_tests::force_transition_to_ready_cancels_live_dispatches_and_skips_auto_queue_entries 실패 (main)"}]
EOF

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  if [[ -f "$scenario_dir/issue-create.txt" || -f "$scenario_dir/issue-comment.txt" || -f "$scenario_dir/issue-close.txt" ]]; then
    echo "assertion failed: cancelled run must not mutate ci-red issues" >&2
    exit 1
  fi
}

scenario_skipped_lane_does_not_close_issue() {
  local scenario_dir="$TMP_DIR/selftest-skipped"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_success_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"success"},{"id":199,"conclusion":"success"}]}
EOF
  echo '{"jobs":[]}' >"$scenario_dir/current-jobs.json"
  echo '{"jobs":[]}' >"$scenario_dir/previous-jobs.json"
  cat >"$scenario_dir/open-issues.json" <<'EOF'
[{"number":9464,"title":"[ci-red] job::High-risk recovery 실패 (main)"}]
EOF

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  if [[ -f "$scenario_dir/issue-create.txt" || -f "$scenario_dir/issue-comment.txt" || -f "$scenario_dir/issue-close.txt" ]]; then
    echo "assertion failed: skipped lane must not count toward recovery" >&2
    exit 1
  fi
}

scenario_single_failure_stays_pending() {
  local scenario_dir="$TMP_DIR/selftest-pending"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"success"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":501,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/501"}]}
EOF
  echo '{"jobs":[]}' >"$scenario_dir/previous-jobs.json"
  cat >"$scenario_dir/log-200-501.txt" <<'EOF'
test server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state ... FAILED
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  if [[ -f "$scenario_dir/issue-create.txt" || -f "$scenario_dir/issue-comment.txt" || -f "$scenario_dir/issue-close.txt" ]]; then
    echo "assertion failed: single failure must stay pending without issue mutation" >&2
    exit 1
  fi
}

scenario_three_gate_failures_produce_distinct_identifiers() {
  # Release-gate triage contract (#1011):
  # Full tests / PostgreSQL tests / High-risk recovery 가 한 run 에 동시에 red
  # 이고 이전 run 에서도 동일하게 red 였다면, 세 gate 모두에 대해 서로 다른
  # identifier + 서로 다른 repro 커맨드를 가진 ci-red 이슈가 생성되어야 한다.
  local scenario_dir="$TMP_DIR/selftest-three-gate"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[
  {"id":801,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/801"},
  {"id":802,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/802"},
  {"id":803,"name":"High-risk recovery","conclusion":"failure","html_url":"https://example.com/jobs/803"}
]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[
  {"id":811,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/811"},
  {"id":812,"name":"PostgreSQL tests","conclusion":"failure","html_url":"https://example.com/jobs/812"},
  {"id":813,"name":"High-risk recovery","conclusion":"failure","html_url":"https://example.com/jobs/813"}
]}
EOF
  # Full tests lane: normal cargo test identifier
  cat >"$scenario_dir/log-200-801.txt" <<'EOF'
test pipeline::tests::full_suite_regression ... FAILED
EOF
  cat >"$scenario_dir/log-199-811.txt" <<'EOF'
test pipeline::tests::full_suite_regression ... FAILED
EOF
  # PG lane: a _pg_ test identifier
  cat >"$scenario_dir/log-200-802.txt" <<'EOF'
test server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state ... FAILED
EOF
  cat >"$scenario_dir/log-199-812.txt" <<'EOF'
test server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state ... FAILED
EOF
  # High-risk recovery lane: high_risk_recovery:: scenario
  cat >"$scenario_dir/log-200-803.txt" <<'EOF'
test integration_tests::tests::high_risk_recovery::outbox_boundary::scenario_160_1_outbox_batch_delivers_exactly_once ... FAILED
EOF
  cat >"$scenario_dir/log-199-813.txt" <<'EOF'
test integration_tests::tests::high_risk_recovery::outbox_boundary::scenario_160_1_outbox_batch_delivers_exactly_once ... FAILED
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  # Capture every `gh issue create` invocation (there must be 3 distinct ones).
  cat >"$scenario_dir/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
scenario_dir="$(cd "$(dirname "$0")" && pwd)"
printf '%s\n' "$*" >>"$scenario_dir/gh.log"
cmd="${1-}"; sub="${2-}"
if [[ "$cmd" == "label" && "$sub" == "create" ]]; then exit 0; fi
if [[ "$cmd" == "api" ]]; then
  endpoint="${2-}"
  if [[ "$endpoint" == "/repos/test/repo/actions/workflows/1/runs?branch=main&status=completed&per_page=5" ]]; then
    cat "$scenario_dir/workflow-runs.json"; exit 0
  fi
  if [[ "$endpoint" == "/repos/test/repo/actions/runs/200/jobs?per_page=100" ]]; then
    cat "$scenario_dir/current-jobs.json"; exit 0
  fi
  if [[ "$endpoint" == "/repos/test/repo/actions/runs/199/jobs?per_page=100" ]]; then
    cat "$scenario_dir/previous-jobs.json"; exit 0
  fi
fi
if [[ "$cmd" == "run" && "$sub" == "view" ]]; then
  run_id="${3-}"; job_id=""; prev=""
  for arg in "$@"; do
    if [[ "$prev" == "--job" ]]; then job_id="$arg"; break; fi
    prev="$arg"
  done
  cat "$scenario_dir/log-${run_id}-${job_id}.txt"; exit 0
fi
if [[ "$cmd" == "issue" && "$sub" == "list" ]]; then cat "$scenario_dir/open-issues.json"; exit 0; fi
if [[ "$cmd" == "issue" && "$sub" == "create" ]]; then
  # Capture title + body so downstream assertions can inspect repro.
  title_flag=0; body_flag=0; title=""; body=""
  for arg in "$@"; do
    if [[ "$title_flag" == "1" ]]; then title="$arg"; title_flag=0; continue; fi
    if [[ "$body_flag" == "1" ]]; then body="$arg"; body_flag=0; continue; fi
    if [[ "$arg" == "--title" ]]; then title_flag=1; fi
    if [[ "$arg" == "--body" ]]; then body_flag=1; fi
  done
  printf '=== CREATE ===\ntitle=%s\nbody<<END\n%s\nEND\n' "$title" "$body" >>"$scenario_dir/issue-create.log"
  exit 0
fi
if [[ "$cmd" == "issue" && "$sub" == "comment" ]]; then
  printf '%s\n' "$*" >>"$scenario_dir/issue-comment.txt"; exit 0
fi
if [[ "$cmd" == "issue" && "$sub" == "close" ]]; then
  printf '%s\n' "$*" >>"$scenario_dir/issue-close.txt"; exit 0
fi
echo "unexpected gh args: $*" >&2
exit 1
EOF
  chmod +x "$scenario_dir/gh"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  # 3 distinct issues created — one per gate identifier
  local create_count
  create_count="$(grep -c '^=== CREATE ===$' "$scenario_dir/issue-create.log" || true)"
  if [[ "$create_count" != "3" ]]; then
    echo "assertion failed: expected 3 ci-red issues for 3 gate failures, got $create_count" >&2
    cat "$scenario_dir/issue-create.log" >&2
    exit 1
  fi

  # Each gate has a distinct identifier in title + distinct repro in body
  assert_contains "pipeline::tests::full_suite_regression" "$scenario_dir/issue-create.log"
  assert_contains "postgres_force_transition_to_ready_cleans_up_live_state" "$scenario_dir/issue-create.log"
  assert_contains "scenario_160_1_outbox_batch_delivers_exactly_once" "$scenario_dir/issue-create.log"

  # Distinct repro lines (one per identifier) — verifies owner/repro/followup per gate.
  assert_contains "cargo test -p agentdesk pipeline::tests::full_suite_regression -- --exact --nocapture" "$scenario_dir/issue-create.log"
  assert_contains "cargo test -p agentdesk server::routes::routes_tests::postgres_force_transition_to_ready_cleans_up_live_state -- --exact --nocapture" "$scenario_dir/issue-create.log"
  assert_contains "cargo test -p agentdesk integration_tests::tests::high_risk_recovery::outbox_boundary::scenario_160_1_outbox_batch_delivers_exactly_once -- --exact --nocapture" "$scenario_dir/issue-create.log"

  # Follow-up label (agent:project-agentdesk) applied to every gate failure.
  local followup_count
  followup_count="$(grep -c -- "--label agent:project-agentdesk" "$scenario_dir/gh.log" || true)"
  if [[ "$followup_count" -lt "3" ]]; then
    echo "assertion failed: expected at least 3 agent:project-agentdesk label applications, got $followup_count" >&2
    exit 1
  fi
}

scenario_sigterm_job_failure_is_skipped_as_flaky() {
  # #3991 / #4245: A failed job whose log has no `test … FAILED` assertion but
  # shows an infrastructure-level termination (SIGTERM / signal 15 / exit 143 /
  # runner cancellation) is flaky runner pressure. A one-off or two-in-a-row
  # occurrence stays skipped and is NOT promoted to a ci-red issue. Here only two
  # runs of history exist (200 + 199) — below the default SIGTERM_ESCALATION_STREAK
  # of 3 — so the persistent-infra escalation cannot fire and the anti-flake skip
  # stands. The paired scenario_persistent_sigterm_escalates covers the 3-streak
  # promotion; scenario_sigterm_streak_broken_by_non_infra_run_stays_skipped covers
  # a broken streak within a sufficient window.
  local scenario_dir="$TMP_DIR/selftest-sigterm-flaky"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":901,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/901"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":902,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/902"}]}
EOF
  cat >"$scenario_dir/log-200-901.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
    Building [=======================>   ] 812/845: agentdesk(test)
/home/runner/work/_temp/abc.sh: line 3: 1234 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  cat >"$scenario_dir/log-199-902.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
    Building [=====================>     ] 790/845: agentdesk(test)
/home/runner/work/_temp/def.sh: line 3: 5678 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  if [[ -f "$scenario_dir/issue-create.txt" || -f "$scenario_dir/issue-comment.txt" || -f "$scenario_dir/issue-close.txt" ]]; then
    echo "assertion failed: SIGTERM/infra-termination job failure must be skipped as flaky, not promoted to ci-red" >&2
    exit 1
  fi
}

scenario_persistent_sigterm_escalates() {
  # #4245: The SAME job terminating at the infrastructure level (SIGTERM / exit
  # 143 / runner shutdown) across SIGTERM_ESCALATION_STREAK (default 3) consecutive
  # main runs is no longer flake — it is a deterministic infra regression (e.g.
  # OOM) and MUST be promoted to a ci-red issue on the `infra::job::…` track. Runs
  # 200, 199 and 198 all show the identical Full-tests infra termination.
  local scenario_dir="$TMP_DIR/selftest-sigterm-escalate"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"},{"id":198,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":931,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/931"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":932,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/932"}]}
EOF
  cat >"$scenario_dir/previous2-jobs.json" <<'EOF'
{"jobs":[{"id":933,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/933"}]}
EOF
  cat >"$scenario_dir/log-200-931.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
    Building [=======================>   ] 812/845: agentdesk(test)
/home/runner/work/_temp/abc.sh: line 3: 1234 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  cat >"$scenario_dir/log-199-932.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
    Building [=====================>     ] 790/845: agentdesk(test)
/home/runner/work/_temp/def.sh: line 3: 5678 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  cat >"$scenario_dir/log-198-933.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
    Building [====================>      ] 770/845: agentdesk(test)
/home/runner/work/_temp/ghi.sh: line 3: 9012 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  assert_contains "issue create --repo test/repo --title [ci-red] infra::job::Full tests (ubuntu-latest) 실패 (main)" "$scenario_dir/issue-create.txt"
  assert_contains "--label ci-red" "$scenario_dir/issue-create.txt"
  assert_contains "--label agent:project-agentdesk" "$scenario_dir/issue-create.txt"
}

scenario_sigterm_streak_broken_by_non_infra_run_stays_skipped() {
  # #4245 anti-flake boundary: even with a sufficient history window (3 runs), a
  # persistent-SIGTERM escalation must fire ONLY on an unbroken streak. Here runs
  # 200 and 199 are infra terminations, but the oldest run 198 is a real
  # `test … FAILED` (a different failure mode), so the infra streak is only 2
  # consecutive — below the default-3 threshold. No ci-red issue may be created.
  local scenario_dir="$TMP_DIR/selftest-sigterm-streak-broken"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"},{"id":198,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":941,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/941"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":942,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/942"}]}
EOF
  cat >"$scenario_dir/previous2-jobs.json" <<'EOF'
{"jobs":[{"id":943,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/943"}]}
EOF
  cat >"$scenario_dir/log-200-941.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
/home/runner/work/_temp/abc.sh: line 3: 1234 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  cat >"$scenario_dir/log-199-942.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
/home/runner/work/_temp/def.sh: line 3: 5678 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  # Oldest run in the window is a genuine test failure, not an infra termination —
  # this breaks the infra streak so no escalation may fire.
  cat >"$scenario_dir/log-198-943.txt" <<'EOF'
running 42 tests
test pipeline::tests::some_unrelated_regression ... FAILED
error: test failed, to rerun pass `-p agentdesk --lib`
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  if [[ -f "$scenario_dir/issue-create.txt" || -f "$scenario_dir/issue-comment.txt" || -f "$scenario_dir/issue-close.txt" ]]; then
    echo "assertion failed: a broken infra streak (2 of 3 runs) must stay skipped, not promoted to ci-red" >&2
    exit 1
  fi
}

scenario_sigterm_noise_with_real_test_failure_still_creates_issue() {
  # #3991 regression guard: a real `test … FAILED` assertion must still be
  # promoted to a ci-red issue even when the same job log ALSO contains SIGTERM /
  # exit 143 infrastructure noise. The flaky filter only applies to the
  # job-level fallback (zero test-FAILED matches), never to real failures.
  local scenario_dir="$TMP_DIR/selftest-sigterm-real-fail"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":911,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/911"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":912,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/912"}]}
EOF
  cat >"$scenario_dir/log-200-911.txt" <<'EOF'
running 42 tests
test pipeline::tests::sigterm_noise_regression ... FAILED
error: test failed, to rerun pass `-p agentdesk --lib`
Caused by:
  process didn't exit successfully: `.../agentdesk-abc123 --skip _pg` (signal: 15, SIGTERM: termination signal)
Error: The process '/usr/bin/just' failed with exit code 143
EOF
  cat >"$scenario_dir/log-199-912.txt" <<'EOF'
running 42 tests
test pipeline::tests::sigterm_noise_regression ... FAILED
error: test failed, to rerun pass `-p agentdesk --lib`
Caused by:
  process didn't exit successfully: `.../agentdesk-def456 --skip _pg` (signal: 15, SIGTERM: termination signal)
Error: The process '/usr/bin/just' failed with exit code 143
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  assert_contains "issue create --repo test/repo --title [ci-red] pipeline::tests::sigterm_noise_regression 실패 (main)" "$scenario_dir/issue-create.txt"
}

scenario_compile_error_with_sigterm_noise_still_creates_issue() {
  # #3996 regression guard: a genuine job-level compile regression (`error[E…]` /
  # `error: could not compile`) emits NO `test … FAILED` line, so it takes the
  # job-level fallback path. When SIGTERM / exit-143 infra noise is ALSO present
  # (runner tears down after the hard failure), the flaky filter must NOT skip it.
  # `log_has_real_failure` gates the infra skip: real red is still promoted to a
  # ci-red issue across two consecutive red runs. Before #3996 this was a false
  # negative (silently dropped).
  local scenario_dir="$TMP_DIR/selftest-compile-error-sigterm"
  mkdir -p "$scenario_dir"
  install_mock_gh "$scenario_dir"
  write_event_payload "$scenario_dir/event.json"
  cat >"$scenario_dir/workflow-runs.json" <<'EOF'
{"workflow_runs":[{"id":200,"conclusion":"failure"},{"id":199,"conclusion":"failure"}]}
EOF
  cat >"$scenario_dir/current-jobs.json" <<'EOF'
{"jobs":[{"id":921,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/921"}]}
EOF
  cat >"$scenario_dir/previous-jobs.json" <<'EOF'
{"jobs":[{"id":922,"name":"Full tests (ubuntu-latest)","conclusion":"failure","html_url":"https://example.com/jobs/922"}]}
EOF
  cat >"$scenario_dir/log-200-921.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
error[E0308]: mismatched types
  --> src/pipeline/mod.rs:42:9
   |
42 |         dispatch(count)
   |         ^^^^^^^^ expected `u64`, found `String`
error: could not compile `agentdesk` (lib) due to 1 previous error
/home/runner/work/_temp/abc.sh: line 3: 1234 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  cat >"$scenario_dir/log-199-922.txt" <<'EOF'
   Compiling agentdesk v0.1.0 (/home/runner/work/AgentDesk/AgentDesk)
error[E0308]: mismatched types
  --> src/pipeline/mod.rs:42:9
   |
42 |         dispatch(count)
   |         ^^^^^^^^ expected `u64`, found `String`
error: could not compile `agentdesk` (lib) due to 1 previous error
/home/runner/work/_temp/def.sh: line 3: 5678 Terminated (signal 15) just check
Error: The process '/usr/bin/just' failed with exit code 143
##[error]The operation was canceled.
EOF
  echo '[]' >"$scenario_dir/open-issues.json"

  PATH="$scenario_dir:$PATH" \
    GITHUB_REPOSITORY="test/repo" \
    GITHUB_EVENT_PATH="$scenario_dir/event.json" \
    GH_TOKEN="test-token" \
    bash "$0"

  assert_contains "issue create --repo test/repo --title [ci-red] job::Full tests (ubuntu-latest) 실패 (main)" "$scenario_dir/issue-create.txt"
}

run_self_test() {
  require_cmd jq
  scenario_two_run_failure_creates_issue
  scenario_new_issue_triggers_immediate_sync
  scenario_existing_issue_gets_comment_only
  scenario_existing_issue_triggers_immediate_sync
  scenario_two_run_green_closes_issue
  scenario_recovered_infra_job_closes_issue
  scenario_cancelled_run_does_not_close_issue
  scenario_skipped_lane_does_not_close_issue
  scenario_single_failure_stays_pending
  scenario_three_gate_failures_produce_distinct_identifiers
  scenario_sigterm_job_failure_is_skipped_as_flaky
  scenario_persistent_sigterm_escalates
  scenario_sigterm_streak_broken_by_non_infra_run_stays_skipped
  scenario_sigterm_noise_with_real_test_failure_still_creates_issue
  scenario_compile_error_with_sigterm_noise_still_creates_issue
  echo "self-test passed"
}

main() {
  case "${1-}" in
    --self-test)
      run_self_test
      ;;
    "")
      run_triage
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
}

main "${1-}"
