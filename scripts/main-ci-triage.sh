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
      identifier="job::$job_name"
      record_failed_identifier "$prefix" "$identifier" "$job_name" "$job_id" "$job_url" "$log_path"
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
  done < <(jq -c '.jobs[] | select(.conclusion == "success")' <<<"$jobs_json")
}

render_log_snippet() {
  local identifier="$1"
  local log_path="$2"

  if [[ "$identifier" == job::* ]]; then
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
  local snippet repro

  snippet="$(render_log_snippet "$identifier" "$log_path")"
  if [[ "$identifier" == job::* ]]; then
    repro="_job-level failure; see failing workflow job_"
  else
    repro="cargo test -p agentdesk $identifier -- --exact --nocapture"
  fi

  cat <<EOF
## 배경

main 의 \`CI Main\` 에서 동일 실패가 2회 연속 관측되어 자동 생성된 ci-red 이슈입니다.

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

run_self_test() {
  require_cmd jq
  scenario_two_run_failure_creates_issue
  scenario_new_issue_triggers_immediate_sync
  scenario_existing_issue_gets_comment_only
  scenario_existing_issue_triggers_immediate_sync
  scenario_two_run_green_closes_issue
  scenario_cancelled_run_does_not_close_issue
  scenario_skipped_lane_does_not_close_issue
  scenario_single_failure_stays_pending
  scenario_three_gate_failures_produce_distinct_identifiers
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
