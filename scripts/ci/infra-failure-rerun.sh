#!/usr/bin/env bash
set -euo pipefail

SELF_NAME="$(basename "$0")"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TRIAGE_SCRIPT="$REPO_ROOT/scripts/main-ci-triage.sh"
REAL_FAILURE_PREDICATE="$SCRIPT_DIR/real-failure-predicate.sh"
# shellcheck source=/dev/null
source "$REAL_FAILURE_PREDICATE"
TMP_DIR="$(mktemp -d)"
SUMMARY_ROWS="$TMP_DIR/summary-rows.md"
PG_JOB_NAME="PostgreSQL tests (ubuntu-postgres)"
WINDOWS_ADVISORY_JOB_NAME="Fast check + non-PG tests (windows-latest)"
trap 'rm -rf "$TMP_DIR"' EXIT

: >"$SUMMARY_ROWS"

usage() {
  cat <<EOF
Usage: $SELF_NAME [--self-test]
       $SELF_NAME --classify-job <job-name> <log-path> <executed-steps>
       $SELF_NAME --failed-job-rows <jobs-json-path>

  --classify-job  Print the classification a failed job would receive.
                  <executed-steps> is the number of steps the job ran outside
                  the runner's own \`Set up job\` / \`Complete job\`, or the
                  literal \`unknown\` when the jobs payload carried no steps.

  --failed-job-rows
                  Print the \`<job-id>\\t<executed-steps>\\t<job-name>\` rows
                  the live loop consumes, from a jobs API payload, so the
                  step-count expression itself is testable (#5207 F3).

Diagnostic note (#5207): a job's \`conclusion\` is set by run-level aborts, so a
job that completed every one of its steps successfully can still report
\`cancelled\`. Read per-job STEP results, not \`conclusion\`, when judging whether
a gate actually ran.

Environment:
  GITHUB_REPOSITORY  owner/repository (required for a workflow run)
  RUN_ID             CI PR workflow run id
  RUN_ATTEMPT        attempt to classify (must be less than 3)
  RERUN_DRY_RUN      set to 1 to classify historical attempts without rerunning
EOF
}

require_cmd() {
  local name="$1"
  if ! command -v "$name" >/dev/null 2>&1; then
    echo "missing required command: $name" >&2
    exit 1
  fi
}

is_positive_integer() {
  [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

# #5207 (F3): the sole producer of the step count R3 consumes. Inline in
# `run_classifier` it was untestable — every test handed `classify_job` an
# integer — so it is a named constant that `--failed-job-rows` can drive against
# fixtures. `.name` is emitted LAST so a job name containing a tab cannot shift
# the count the caller's `IFS=$'\t' read` picks up.
#
# #5207 (F4): the else-branch yields `unknown`, never `0`, for `steps` absent,
# null, AND `[]`. An empty array is data-absence wearing an array's type — a job
# the API calls `failure` entered `Set up job`, so a truthful payload carries at
# least that step. Reading `[]` as 0 would auto-rerun every failed job whose
# steps we cannot see; `unknown` makes `job_ran_no_repo_steps` decline instead.
# The jq program is intentionally literal shell text.
# shellcheck disable=SC2016
FAILED_JOB_ROWS_JQ='
    .jobs[]
    | select(.conclusion == "failure")
    | [
        (.id | tostring),
        (if (.steps | type) == "array" and ((.steps | length) > 0)
         then ([.steps[] | select((.name // "" | ascii_downcase) as $n | $n != "set up job" and $n != "complete job")] | length | tostring)
         else "unknown"
         end),
        .name
      ]
    | @tsv'

failed_job_rows() {
  local jobs_payload="$1"
  jq -r "$FAILED_JOB_ROWS_JQ" "$jobs_payload"
}

# #5207 (R2): this literal must stay byte-identical to the validated
# termination regex in `log_has_infra_termination` (scripts/main-ci-triage.sh).
# That alignment used to be a comment a human had to honour; it is now enforced
# by `assert_termination_regex_synced`, which is exercised on every
# `--self-test` run (wired from scripts/ci-script-checks.sh). Editing either
# side alone makes the self-test fail.
INFRA_TERMINATION_REGEX='signal[: ]+(9|15)([^0-9]|$)|sig(term|kill)|terminated on line [0-9]+ by signal|(exit(ed)?|code|status)[^0-9]*143([^0-9]|$)|the operation was cancell?ed|runner has received a shutdown signal'

# Deliberately narrow: GitHub's step-level action timeout, not arbitrary
# timeout text from tests, dependencies, or cache statistics.
INFRA_ACTION_TIMEOUT_REGEX="the action '.+' has timed out after [0-9]+ minutes"

# #5207 (R1): the 2026-08-07 Actions outage emitted
# `Failed to resolve action download info. Error: Service Unavailable` ten
# times in a single run and matched none of the termination alternations, so a
# pure infrastructure outage was left unclassified and read as a code problem.
#
# The anchor is the runner-emitted action-download phrase, NOT the bare server
# error: `Service Unavailable` / `503` alone also appear in test output and
# dependency chatter, and widening to those would let a genuine regression be
# auto-rerun. Requiring both halves on the SAME line also keeps out the
# repo-config mode, which reuses this very prefix but reports
# `… Error: Unable to resolve action \`owner/repo@ref\`, repository not found`
# (RUNNER: LaunchHttpClientL0.cs:100; LOG: actions/runner#1006) — a real
# regression in this repo, which must stay unmatched here.
#
# #5207 (r3): `internal server error` is not speculative — it was observed
# beside `service unavailable` in the same log (LOG: ROCm/TheRock#7167).
#
# ★ PROVENANCE (#5207 r4, P3-1). This constant is used POSITIVELY — a match
# turns automatic retry ON — so it needs STRICTER evidence than the withholding
# regex below, not looser. r3 cited sources only on the withholding side, which
# is backwards: a wrong negative alternation costs a lost retry, a wrong
# positive one auto-retries something broken.
#   service unavailable    LOG: the 2026-08-07 outage above (ten occurrences in
#                          one run); also ROCm/TheRock#7167.
#   internal server error  LOG: ROCm/TheRock#7167, beside `service unavailable`
#                          in the same log.
#   bad gateway            RUNNER-DERIVED ON THE V2 RESOLVER PATH — no
#   gateway time-?out      per-phrase log needed there, and that derivation IS
#                          the citation. For any non-422/429 response
#                          LaunchHttpClient.cs:51,77 sets the exception message
#                          to `response.ReasonPhrase` VERBATIM and throws a
#                          retryable `Exception`, which ActionManager.cs:1086
#                          prints after `Error: `. On that path the text in
#                          this slot is therefore exactly the HTTP reason
#                          phrase, and 502/504 are the same edge-error family
#                          as the 500/503 observed above. `time-?out` covers
#                          both shipped spellings: RFC 2616 wrote `Gateway
#                          Time-out`, RFC 7231 writes `Gateway Timeout`.
#                          #5207 (r5, D2) — THE QUALIFIER MATTERS: :51,77 live
#                          in `GetResolveActionsDownloadInfoAsyncV2`
#                          (LaunchHttpClient.cs:40-79), and LaunchServer.cs:
#                          47-54 reaches it only when
#                          `actions_display_helpful_actions_download_errors` is
#                          on (read at ActionManager.cs:1067, default `?? false`
#                          ). The V1 entry point, `GetResolveActionsDownload
#                          InfoAsync` (LaunchHttpClient.cs:33-38), contains none
#                          of that mapping, so on V1 this slot is NOT known to
#                          hold a reason phrase and the derivation says nothing.
#                          It still carries the observed logs: the outage line
#                          `Failed to resolve action download info. Error:
#                          Service Unavailable` is `Error: ` followed by a bare
#                          reason phrase, i.e. the V2 shape. Claim scope: TRUE
#                          ON V2, UNPROVEN ON V1 — not "true of every runner".
#
# REMOVED in r4 for want of provenance — both were r3 inventions of the same
# kind the rule at REPO_CONFIG_FAULT_REGEX was written to stop:
#   temporarily unavailable  NOT an HTTP reason phrase, so the mechanical
#     derivation above does not reach it, and a GitHub-wide search found it in
#     this slot zero times.
#   too many requests / 429  UNREACHABLE ON THIS LINE ON THE V2 RESOLVER PATH,
#     proven from source: LaunchHttpClient.cs:68-73 — inside
#     `GetResolveActionsDownloadInfoAsyncV2` — throws
#     `NonRetryableActionDownloadInfoException` for 429, and
#     ActionManager.cs:1084-1086 prints the `Failed to resolve action download
#     info. Error: …` line ONLY when the exception is neither that type nor
#     `UnresolvableActionDownloadInfoException`. On V2 the prefix this regex
#     requires is therefore never emitted for a 429.
#     #5207 (r5, D2) — THAT PROOF IS PATH-SCOPED. LaunchServer.cs:47-54 routes
#     to V2 only when `actions_display_helpful_actions_download_errors` is on
#     (ActionManager.cs:1067, default `?? false`); the V1 entry point
#     (LaunchHttpClient.cs:33-38) runs none of that code, so on V1 the 429
#     handling is not established here. The removal still holds for the logs
#     this regex is aimed at, because the observed outage line is the V2 shape
#     (`Error: ` + a bare reason phrase).
#     #5207 (r5, D2) — NO COVERAGE IS LOST BY THE REMOVAL, EITHER WAY. A 429
#     aborts action resolution inside `Set up job`, so the job dies before any
#     step this repo defines runs: `executed_steps` is 0, `job_ran_no_repo_steps`
#     accepts it, and `job_is_infra_failure` still returns true via the
#     zero-step path. The retry decision is bit-identical; only the summary
#     label moves, `infra-shutdown` -> `infra-no-steps`. Do NOT re-add this
#     alternation on the belief that 429 would otherwise go unclassified.
#     The r3 `F5` comment argued the
#     alternation was "kept, deliberately" and reasoned about rate-limit
#     containment; that premise was false — the state it protected cannot occur.
#     Containment itself is unaffected and never depended on it: at most two
#     automatic attempts (`ci-pr-infra-retry.yml` gates on `run_attempt < 3`,
#     `run_classifier` returns `no-op:attempt-cap` at `RUN_ATTEMPT >= 3`), one
#     job per rerun.
INFRA_ACTION_DOWNLOAD_REGEX='failed to resolve action download info.*(service unavailable|bad gateway|gateway time-?out|internal server error)'

# #5207 (F1): repo-configuration faults that kill a job before any repo step.
# Needed because the original R3 premise ("a job that ran no repo step cannot
# have failed because of this repo's code") is false, and its falseness quietly
# re-admitted what the R1 anchor was written to exclude.
#
# ★ PROVENANCE RULE (#5207 r3) — READ BEFORE EDITING. Two alternations shipped
# here in r2, `no runner matching the labels` and `requesting permissions that
# are not allowed`, are phrases NO GitHub component emits: invented in prose,
# then reused verbatim as the test fixtures, so the suite proved only that the
# regex matched its author's imagination. Each alternation now cites where its
# literal was OBSERVED. Add none without such a citation. `RUNNER` = actions/
# runner source; `LOG` = a real pasted job log (GitHub-wide issue-body search).
#
# [1] failed to resolve action download info\. error: bad request
#     RUNNER ActionManager.cs:1086 prints "Failed to resolve action download
#     info. Error: {ex.Message}"; LaunchHttpClient.cs:77 makes {ex.Message} the
#     :51 `response.ReasonPhrase` for a non-422/429 response.
#     #5207 (r5, D3): the throw is on :77 — :76 is the opening brace of the
#     `else` arm — matching the :51,77 citation on the positive regex above.
#     #5207 (r5, D2): as there, this holds ON THE V2 RESOLVER PATH
#     (`GetResolveActionsDownloadInfoAsyncV2`), which LaunchServer.cs:47-54
#     selects only when `actions_display_helpful_actions_download_errors` is on
#     (ActionManager.cs:1067, default `?? false`). The log cited next carries
#     the V2 shape, `Error: ` + reason phrase, so the alternation is bound to an
#     observed literal regardless of how the derivation is scoped.
#     LOG actions/runner#1247:
#     "…Error: Bad request - freertos/ci-cd-github-actions@main is not allowed
#     to be used in aws/aws-iot-device-sdk-embedded-C." — an Actions policy
#     rejection, i.e. this repo's configuration, not an outage. `not found` /
#     `unauthorized` / `forbidden` were DROPPED: no real log carries them in
#     this slot, and the nonexistent-action case arrives as [2] inside this
#     same line.
# [2] unable to resolve action
#     RUNNER LaunchHttpClientL0.cs:100 mirrors the service's 422 body: "Unable
#     to resolve action 'owner1/invalid-action@0123456789', repository not
#     found". LOG actions/runner#1006, google-github-actions/setup-gcloud#270,
#     ratt-ru/tricolour#72, SpraxDev/Action-SpigotMC#21 — all wrapped as
#     "Failed to resolve action download info. Error: Unable to resolve action
#     `actions/chcekout@v2`, repository not found". Ref quoting varies
#     (backtick / quote / bare / "…action. Repository not found: X"), so the
#     anchor stops before the ref.
# [3] no runner matching the specified labels
#     Service-side; absent from actions/runner. LOG six unrelated repos, e.g.
#     ianbruene/ddgo#22 "Error: No runner matching the specified labels was
#     found: macos-15-intel" and "…was found: self-hosted, gpu". A search for
#     the r2 wording "no runner matching the labels" returned prose only, never
#     a log. CAVEAT: ddgo#22 names the failing step "Runner provisioning /
#     start job" — the job never starts, so this surfaces as the run/job
#     annotation and its presence in the DOWNLOADABLE job log is UNVERIFIED.
# [4] the workflow is not valid
#     RUNNER WorkflowStrings.resx:121,124 ("The workflow is not valid." /
#     "…{0}"), raised by WorkflowValidationException.cs:13,18. LOG "The workflow
#     is not valid. .github/workflows/ci.yml (Line: 49, Col: 3): …".
#     CAVEAT: same reachability limit as [3], and for a sharper reason. Both
#     literals are Sdk/WorkflowParser products, i.e. they are produced BEFORE
#     any job exists: the run ends as `startup_failure` under `Invalid workflow
#     file`, so there is no job and therefore no downloadable job log for this
#     script to read. Presence in a job log is UNVERIFIED.
# [5] is requesting '<x>', but is only allowed '<y>'
#     RUNNER PermissionsHelper.cs:47 (and :43, nested-job wording): "Error
#     calling workflow '{ref}'. The workflow is requesting '{requested}', but is
#     only allowed '{allowed}'." LOG "The nested job 'release' is requesting
#     'contents: write', but is only allowed 'contents: read'." The top-level
#     form is normally wrapped by [4]; the nested-job form is seen standalone,
#     so this alternation is not redundant.
#     CAVEAT: as [4] — Sdk/WorkflowParser/Conversion/PermissionsHelper.cs is
#     also pre-job, so job-log presence is UNVERIFIED.
#     [3]/[4]/[5] are all kept regardless: they are used only NEGATIVELY, and a
#     job with no log already fails closed at `no-op:unknown`.
# [6] manifest for .* not found
#     Docker daemon text, surfaced because the runner shells out to `docker
#     pull` (RUNNER DockerCommandManager.cs:100). LOG "Error response from
#     daemon: manifest for weirauchlab/reli:alpine not found: manifest unknown:
#     manifest unknown". The `.*` is over-broad, tolerated because this constant
#     is used only NEGATIVELY (to WITHHOLD the infrastructure label): a false
#     positive costs a lost auto-retry, never an auto-retry of a broken repo.
#
# ★ RESIDUAL RISK — a repo-configuration fault NOT enumerated above is still
# classified as infrastructure and automatically retried. That set is not small:
# environment-name errors, concurrency expressions, composite actions, OIDC/env
# misconfiguration, checkout auth, no-enabled-runners, the other `Invalid
# workflow file` families (missing called workflow, unsupplied required secret,
# unknown job dependency), missing hosted-runner images, Docker login and
# container init all reach `infra-no-steps` today. Bounded cost, measured: one
# wasted rerun plus a wrong label in the summary table. A DETERMINISTIC config
# fault fails again on the retry and then hits the `run_attempt >= 3` cap, so
# no deterministically broken repo is auto-retried into a pass.
#
# #5207 (r4): that conclusion is deliberately NOT stated as a universal. A
# NON-deterministic repo-configuration fault — one whose outcome depends on
# runner-pool availability, a flaky container registry, or an expiring token —
# can pass on the retry, and then a real defect has been papered over by an
# automatic rerun. The premise ("deterministic") bounds the conclusion; writing
# "nothing broken is auto-retried into a pass" would repeat exactly the
# over-general claim F1 was raised to remove from R3.
#
# Bare `Not Found` is deliberately not an alternation — too common on its own.
REPO_CONFIG_FAULT_REGEX="failed to resolve action download info\. error: bad request|unable to resolve action|no runner matching the specified labels|the workflow is not valid|is requesting '[^']*', but is only allowed '[^']*'|manifest for .* not found"

log_has_repo_config_fault() {
  local log_path="$1"
  [[ -s "$log_path" ]] || return 1
  grep -a -E -i -q -- "$REPO_CONFIG_FAULT_REGEX" "$log_path"
}

# #5207: the outage's SECOND fingerprint. In run 31120826793 (PR #5204) the
# gating `Changed paths` job waited ~16 minutes for a runner, was retired by
# GitHub as `abandoned`, and every downstream required-check mirror then failed
# closed, printing verbatim `CHANGED_PATHS_RESULT: abandoned` and
# `##[error]Changed paths result is unexpected: 'abandoned'`.
#
# Log string and not the API, because `abandoned` is not exposed there: on that
# run the `Changed paths` job reports `conclusion=cancelled` and the substring
# occurs zero times in the whole `.../attempts/1/jobs` payload. It lives only in
# the workflow expression context (`${{ needs.changes.result }}`), which
# `scripts/required-check-mirror.sh` echoes into the mirror's log.
#
# A SEPARATE predicate and label, not folded into `log_has_infra_failure`,
# because the mirror's `failure` is correct behaviour. Keeping it out of
# `job_is_infra_failure` means nothing here can move a retry decision by one bit
# — retrying a fail-closed gate is how you neuter it. All it changes is a label.
#
# Narrowness: bare `abandoned` is an ordinary English word, so each alternation
# binds it to the mirror's `<X>_RESULT:` echo or its quoted result message.
# #5207 (r3, P3-2): `_result:[[:space:]]+abandoned` also matches any other
# `<x>_result:  abandoned` echo, e.g. `session_result:  abandoned`. No producer
# of such a line exists in this repo today; if one appears, tighten the prefix.
# #5207 (r3, P3-1): on the PG job this label is unreachable by construction —
# `run_classifier` overwrites it with `unclassified-pg-failure` (steps >= 1) or
# `infra-no-steps` (steps == 0). That is intended: the label diagnoses the
# required-check MIRROR jobs, and the PG job's own label must stay tied to the
# retry decision. Nothing is lost — the mirror jobs still carry it.
UPSTREAM_ABANDONED_REGEX="result (is unexpected: |is |was )'abandoned'|_result:[[:space:]]+abandoned"

log_has_upstream_abandoned() {
  local log_path="$1"
  [[ -s "$log_path" ]] || return 1
  grep -a -E -i -q -- "$UPSTREAM_ABANDONED_REGEX" "$log_path"
}

log_has_infra_failure() {
  local log_path="$1"
  [[ -s "$log_path" ]] || return 1
  grep -a -E -i -q -- \
    "$INFRA_TERMINATION_REGEX|$INFRA_ACTION_TIMEOUT_REGEX|$INFRA_ACTION_DOWNLOAD_REGEX" \
    "$log_path"
}

# #5207 (R2 enforcement): pull the regex literal `log_has_infra_termination`
# hands to grep out of scripts/main-ci-triage.sh so the two siblings can be
# compared.
#
# #5207 (F6): safety comes from the FINAL COMPARISON in
# `assert_termination_regex_synced`, which compares against the non-empty
# constant, so an extractor that returned "" still fails. The `END { exit 1 }`
# below and the caller's `-n` guard are therefore redundant for correctness,
# kept only for the specific `<not found>` diagnostic — untested by design.
extract_termination_regex() {
  local path="$1"
  [[ -f "$path" ]] || return 1
  awk '
    BEGIN { q = sprintf("%c", 39); found = 0 }
    index($0, "log_has_infra_termination() {") == 1 { inside = 1; next }
    # #5207 (F7): accept an indented closing brace too. Anchoring on a
    # column-0 `}` alone would let the scan run past the end of the function
    # and pick up a later function s own literal, reporting SYNCED for a file
    # whose `log_has_infra_termination` no longer holds one.
    inside && $0 ~ /^[[:space:]]*}[[:space:]]*$/ { exit }
    inside {
      line = $0
      sub(/^[[:space:]]+/, "", line)
      if (substr(line, 1, 1) != q) next
      line = substr(line, 2)
      sub(/[[:space:]]*\\[[:space:]]*$/, "", line)
      if (substr(line, length(line), 1) == q) line = substr(line, 1, length(line) - 1)
      print line
      found = 1
      exit
    }
    END { if (found == 0) exit 1 }
  ' "$path"
}

assert_termination_regex_synced() {
  local path="$1"
  local extracted
  extracted="$(extract_termination_regex "$path")" || return 1
  [[ -n "$extracted" ]] || return 1
  [[ "$extracted" == "$INFRA_TERMINATION_REGEX" ]]
}

# #5207 (R3): purely structural test — did the job run any step this repo
# defines? `Set up job` / `Complete job` are runner-provided housekeeping, and
# action download happens inside `Set up job`, which is exactly where the
# 2026-08-07 outage killed the jobs (`steps_total` was 1 or 0, with that one
# step being `Set up job`).
#
# Fails closed: a non-numeric count (the jobs payload carried no usable `steps`
# array) is treated as "not infrastructure", never as zero.
job_ran_no_repo_steps() {
  local executed_steps="$1"
  [[ "$executed_steps" =~ ^[0-9]+$ ]] || return 1
  (( executed_steps == 0 ))
}

# #5207 (F1, correcting R3): zero executed repo steps is a NECESSARY condition
# for the structural signal, not a sufficient one. The claim R3 used to make —
# "a job that reached none of the repo's own steps cannot have failed because of
# this repo's code" — is false, refuted by the repo faults enumerated at
# REPO_CONFIG_FAULT_REGEX, which die before any repo step runs. A true
# enumeration replaces the false universal: what survives is "zero repo steps
# AND no known repo-configuration fault". The conjunction is no longer spelled
# here — as of r3 the repo-fault half is a single hoisted guard (P2-1 below),
# so it now covers the R1 path as well as this one.
#
# #5207 (r3, P2-1): the repo-fault guard is hoisted so it covers the R1 path
# too. r2 applied it only to the zero-step path, arguing that R1 demands a
# positive outage fingerprint. That argument does not cover ONE LOG CARRYING
# BOTH, which is exactly a configuration regression deployed during an outage:
#   Failed to resolve action download info. Error: Service Unavailable
#   Failed to resolve action download info. Error: Unable to resolve action …
# R1 matched line 1, the guard never ran, and the job was auto-rerun.
#
# The cost of the fix is real and accepted: during a genuine outage a log that
# also happens to contain an enumerated phrase loses its automatic rerun. That
# is fail-closed — a human reruns it — whereas the other direction auto-retries
# a broken repo. Both directions are pinned by tests.
job_is_infra_failure() {
  local log_path="$1"
  local executed_steps="$2"
  ! log_has_repo_config_fault "$log_path" || return 1
  log_has_infra_failure "$log_path" && return 0
  job_ran_no_repo_steps "$executed_steps"
}

# Real-failure markers are checked across every failed job except the explicitly
# advisory Windows lane. This remains separate from the infrastructure
# predicate so mixed logs from required-context upstream jobs fail safe.
job_log_has_blocking_regression() {
  local job_name="$1"
  local log_path="$2"

  [[ "$job_name" != "$WINDOWS_ADVISORY_JOB_NAME" ]] || return 1
  log_has_real_failure "$log_path"
}

# #5207 (R4): the single classification chain used by both the live loop and
# `--classify-job`, so the tested path and the executed path cannot drift.
#
# Ordering is the contract: BOTH regression predicates are consulted before any
# infrastructure predicate, so a log carrying regression markers can never be
# relabelled as infrastructure no matter how many infra fingerprints it also
# carries. R1 and R3 are strictly appended below that guard.
classify_job() {
  local job_name="$1"
  local log_path="$2"
  local executed_steps="$3"

  if job_log_has_blocking_regression "$job_name" "$log_path"; then
    printf 'regression'
  elif log_has_real_failure "$log_path"; then
    printf 'advisory-regression'
  elif log_has_repo_config_fault "$log_path"; then
    # #5207 (r3, P2-1): an enumerated repo-configuration fault outranks the R1
    # outage fingerprint, so a log carrying both cannot be labelled — or rerun —
    # as pure infrastructure. See job_is_infra_failure.
    printf 'unrelated-failure'
  elif log_has_infra_failure "$log_path"; then
    printf 'infra-unrelated'
  elif log_has_upstream_abandoned "$log_path"; then
    # Diagnosis only — see UPSTREAM_ABANDONED_REGEX. This label is intentionally
    # NOT reachable by `job_is_infra_failure`, so it cannot cause a rerun.
    printf 'infra-upstream-abandoned'
  elif job_ran_no_repo_steps "$executed_steps"; then
    printf 'infra-no-steps'
  else
    printf 'unrelated-failure'
  fi
}

decide_retry() {
  local pg_failed_count="$1"
  local pg_classified_count="$2"
  local regression_count="$3"
  local unknown_count="$4"

  if (( unknown_count > 0 )); then
    printf 'no-op:unknown'
  elif (( regression_count > 0 )); then
    printf 'no-op:regression'
  elif (( pg_failed_count == 0 )); then
    printf 'no-op:no-pg-failure'
  elif (( pg_failed_count != 1 )); then
    printf 'no-op:ambiguous-pg-jobs'
  elif (( pg_classified_count != pg_failed_count )); then
    printf 'no-op:unclassified-pg-failure'
  else
    printf 'would-rerun:infra'
  fi
}

append_summary_row() {
  local job_id="$1"
  local job_class="$2"
  printf '%s\n' "| \`$job_id\` | \`$job_class\` |" >>"$SUMMARY_ROWS"
}

write_summary() {
  local run_id="$1"
  local run_attempt="$2"
  local decision="$3"
  local destination="${GITHUB_STEP_SUMMARY-}"

  [[ -n "$destination" ]] || return 0
  {
    printf '### CI PR infrastructure retry\n\n'
    printf '%s\n' "- Run: \`$run_id\`, attempt: \`$run_attempt\`"
    printf '%s\n\n' "- Decision: \`$decision\`"
    printf '| Failed job id | Classification |\n'
    printf '| --- | --- |\n'
    cat "$SUMMARY_ROWS"
  } >>"$destination"
}

validate_attempt_payload() {
  local payload="$1"
  local expected_attempt="$2"
  jq -e \
    --argjson attempt "$expected_attempt" \
    '.name == "CI PR" and .event == "pull_request" and .status == "completed" and .conclusion == "failure" and .run_attempt == $attempt' \
    "$payload" >/dev/null
}

latest_attempt_is_still_failed() {
  local repo="$1"
  local run_id="$2"
  local expected_attempt="$3"
  local payload="$TMP_DIR/latest-run.json"

  gh api "repos/$repo/actions/runs/$run_id" >"$payload" 2>/dev/null || return 1
  jq -e \
    --argjson attempt "$expected_attempt" \
    '.status == "completed" and .conclusion == "failure" and .run_attempt == $attempt' \
    "$payload" >/dev/null
}

run_classifier() {
  require_cmd gh
  require_cmd jq

  local repo="${GITHUB_REPOSITORY-}"
  local run_id="${RUN_ID-}"
  local run_attempt="${RUN_ATTEMPT-}"
  local dry_run="${RERUN_DRY_RUN:-0}"
  local attempt_payload="$TMP_DIR/attempt.json"
  local jobs_payload="$TMP_DIR/jobs.json"
  local decision="no-op:invalid-input"

  if [[ -z "$repo" ]] || ! is_positive_integer "$run_id" || ! is_positive_integer "$run_attempt"; then
    echo "invalid GITHUB_REPOSITORY, RUN_ID, or RUN_ATTEMPT" >&2
    write_summary "${run_id:-unknown}" "${run_attempt:-unknown}" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if (( run_attempt >= 3 )); then
    decision="no-op:attempt-cap"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! gh api "repos/$repo/actions/runs/$run_id/attempts/$run_attempt" >"$attempt_payload" 2>/dev/null; then
    decision="no-op:attempt-api-failure"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! validate_attempt_payload "$attempt_payload" "$run_attempt"; then
    decision="no-op:invalid-attempt"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! gh api "repos/$repo/actions/runs/$run_id/attempts/$run_attempt/jobs?per_page=100" >"$jobs_payload" 2>/dev/null; then
    decision="no-op:jobs-api-failure"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  local total_count
  local returned_count
  total_count="$(jq -r '.total_count // -1' "$jobs_payload")"
  returned_count="$(jq -r '.jobs | length' "$jobs_payload")"
  if ! is_positive_integer "$total_count" || [[ "$total_count" != "$returned_count" ]]; then
    decision="no-op:incomplete-jobs"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  local pg_failed_count=0
  local pg_classified_count=0
  local regression_count=0
  local unknown_count=0
  local pg_job_id=""
  local job_id job_name log_path job_class executed_steps

  while IFS=$'\t' read -r job_id executed_steps job_name; do
    [[ -n "$job_id" ]] || continue
    log_path="$TMP_DIR/job-$job_id.log"
    job_class="unrelated-failure"

    if ! gh api "repos/$repo/actions/jobs/$job_id/logs" >"$log_path" 2>/dev/null || [[ ! -s "$log_path" ]]; then
      unknown_count=$((unknown_count + 1))
      job_class="unknown"
      append_summary_row "$job_id" "$job_class"
      continue
    fi

    job_class="$(classify_job "$job_name" "$log_path" "$executed_steps")"
    if [[ "$job_class" == "regression" ]]; then
      regression_count=$((regression_count + 1))
    fi

    if [[ "$job_name" == "$PG_JOB_NAME" ]]; then
      pg_failed_count=$((pg_failed_count + 1))
      pg_job_id="$job_id"
      if job_is_infra_failure "$log_path" "$executed_steps"; then
        pg_classified_count=$((pg_classified_count + 1))
        if [[ "$job_class" != "regression" ]]; then
          if grep -a -E -i -q -- "$INFRA_ACTION_TIMEOUT_REGEX" "$log_path"; then
            job_class="infra-timeout"
          elif log_has_infra_failure "$log_path"; then
            job_class="infra-shutdown"
          else
            job_class="infra-no-steps"
          fi
        fi
      elif [[ "$job_class" == "regression" ]]; then
        # A regression is a complete classification, but the separate global
        # regression-count guard must block it before the retry decision. This
        # conjunctive shape makes that guard independently load-bearing while
        # still requiring every non-regression PG log to match infra.
        pg_classified_count=$((pg_classified_count + 1))
      else
        job_class="unclassified-pg-failure"
      fi
    fi

    append_summary_row "$job_id" "$job_class"
  done < <(failed_job_rows "$jobs_payload")

  decision="$(decide_retry "$pg_failed_count" "$pg_classified_count" "$regression_count" "$unknown_count")"
  if [[ "$decision" != "would-rerun:infra" ]]; then
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if [[ "$dry_run" == "1" ]]; then
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision pg_job_id=$pg_job_id"
    return 0
  fi

  if ! latest_attempt_is_still_failed "$repo" "$run_id" "$run_attempt"; then
    decision="no-op:stale-attempt"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! gh run rerun "$run_id" --repo "$repo" --job "$pg_job_id"; then
    decision="no-op:rerun-request-failed"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision pg_job_id=$pg_job_id" >&2
    return 1
  fi
  decision="rerun-requested:infra"
  write_summary "$run_id" "$run_attempt" "$decision"
  echo "decision=$decision pg_job_id=$pg_job_id"
}

assert_equal() {
  local expected="$1"
  local actual="$2"
  local label="$3"
  if [[ "$actual" != "$expected" ]]; then
    echo "assertion failed ($label): expected '$expected', got '$actual'" >&2
    exit 1
  fi
}

run_self_test() {
  local infra_log="$TMP_DIR/selftest-infra.log"
  local timeout_log="$TMP_DIR/selftest-timeout.log"
  local regression_log="$TMP_DIR/selftest-regression.log"
  local mixed_log="$TMP_DIR/selftest-mixed.log"
  local run_29067936620_windows_log="$TMP_DIR/selftest-run-29067936620-windows.log"
  local run_29067936620_pg_log="$TMP_DIR/selftest-run-29067936620-postgres.log"

  printf '%s\n' 'The runner has received a shutdown signal.' 'Error: Process completed with exit code 143.' >"$infra_log"
  printf '%s\n' "The action 'just test-postgres' has timed out after 15 minutes." >"$timeout_log"
  printf '%s\n' 'thread panicked at src/example.rs:42' 'test result: FAILED. 1 passed; 1 failed' >"$regression_log"
  # #4392 mutation fixture: a real regression can also carry shutdown cleanup
  # noise. Both predicates intentionally match; the explicit regression-count
  # guard in decide_retry must win.
  cp "$regression_log" "$mixed_log"
  printf '%s\n' 'The runner has received a shutdown signal.' >>"$mixed_log"
  # Run 29067936620 attempt 1: the advisory Windows job panicked while the PG
  # job exited 143 during runner shutdown. The PG failure remains rerunnable.
  printf '%s\n' \
    'thread panicked at src\services\routines\store.rs:4244:9' \
    'test result: FAILED. 163 passed; 1 failed' \
    >"$run_29067936620_windows_log"
  printf '%s\n' \
    'Error: Process completed with exit code 143.' \
    'The runner has received a shutdown signal.' \
    >"$run_29067936620_pg_log"

  log_has_infra_failure "$infra_log" || { echo "assertion failed: shutdown must be infrastructure" >&2; exit 1; }
  log_has_infra_failure "$timeout_log" || { echo "assertion failed: action timeout must be infrastructure" >&2; exit 1; }
  log_has_real_failure "$regression_log" || { echo "assertion failed: regression markers must be detected" >&2; exit 1; }
  log_has_infra_failure "$mixed_log" || { echo "assertion failed: mixed log must contain infrastructure" >&2; exit 1; }
  log_has_real_failure "$mixed_log" || { echo "assertion failed: mixed log must contain regression" >&2; exit 1; }
  log_has_real_failure "$run_29067936620_windows_log" || { echo "assertion failed: run 29067936620 Windows fixture must contain regression" >&2; exit 1; }
  log_has_infra_failure "$run_29067936620_pg_log" || { echo "assertion failed: run 29067936620 PG fixture must contain infrastructure" >&2; exit 1; }

  assert_equal "would-rerun:infra" "$(decide_retry 1 1 0 0)" "infra-only rerun"
  assert_equal "no-op:regression" "$(decide_retry 1 1 1 0)" "mixed regression guard"
  assert_equal "no-op:unknown" "$(decide_retry 1 1 0 1)" "unknown log guard"
  assert_equal "no-op:no-pg-failure" "$(decide_retry 0 0 0 0)" "vacuous truth guard"
  assert_equal "no-op:ambiguous-pg-jobs" "$(decide_retry 2 2 0 0)" "ambiguous target guard"
  assert_equal "no-op:unclassified-pg-failure" "$(decide_retry 1 0 0 0)" "unclassified guard"

  local historical_regression_count=0
  if job_log_has_blocking_regression "$WINDOWS_ADVISORY_JOB_NAME" "$run_29067936620_windows_log"; then
    historical_regression_count=$((historical_regression_count + 1))
  fi
  assert_equal \
    "would-rerun:infra" \
    "$(decide_retry 1 1 "$historical_regression_count" 0)" \
    "run 29067936620 advisory regression reclassification"

  local pg_regression_count=0
  if job_log_has_blocking_regression "$PG_JOB_NAME" "$regression_log"; then
    pg_regression_count=$((pg_regression_count + 1))
  fi
  assert_equal \
    "no-op:regression" \
    "$(decide_retry 1 1 "$pg_regression_count" 0)" \
    "PG regression remains fail-closed"

  run_action_download_assertions
  run_zero_step_assertions
  run_upstream_abandoned_assertions
  run_regression_precedence_assertions
  run_real_failure_assertions
  run_real_failure_predicate_sharing_assertions
  run_termination_sync_assertions

  echo "self-test passed"
}

# #5210: every deterministic gate-failure form that the old three-marker
# rerun predicate missed is paired with runner-shutdown noise. Each must win
# before both the textual infrastructure fingerprint and the zero-step signal.
run_real_failure_assertions() {
  local case_name marker log_path nonmatch_log

  while IFS='|' read -r case_name marker; do
    [[ -n "$case_name" ]] || continue
    log_path="$TMP_DIR/selftest-real-failure-${case_name}.log"
    printf '%s\n' \
      'The runner has received a shutdown signal.' \
      "$marker" \
      >"$log_path"

    log_has_infra_failure "$log_path" || {
      echo "assertion failed (#5210 $case_name): fixture must carry infrastructure noise" >&2
      exit 1
    }
    log_has_real_failure "$log_path" || {
      echo "assertion failed (#5210 $case_name): real failure marker was missed" >&2
      exit 1
    }
    assert_equal "regression" \
      "$(classify_job "$PG_JOB_NAME" "$log_path" 0)" \
      "#5210 $case_name outranks infrastructure and zero steps"
  done <<'CASES'
clippy|error: could not compile `agentdesk` (lib) due to 1 previous error
rustfmt|Diff in /home/runner/work/AgentDesk/AgentDesk/src/main.rs:12:
shellcheck|              ^-- SC3014 (warning): In POSIX sh, == in place of = is undefined.
python-unittest|FAILED (failures=1)
yaml-pyyaml|yaml.scanner.ScannerError: mapping values are not allowed here
linker|ld: cannot find -lpq: No such file or directory
linker-prefixed|/usr/bin/ld: cannot find -lpq: No such file or directory
failed-assertion|assertion `left == right` failed
rerun-only-rustc-prefix|error[Efuture]: a future rustc diagnostic shape
CASES

  nonmatch_log="$TMP_DIR/selftest-real-failure-linker-nonmatches.log"
  printf '%s\n' \
    'Hello world: cannot find the config file' \
    'rebuild: cannot find cached artifact' \
    '/ruby/psych.rb:455: mapping values are not allowed (Psych::SyntaxError)' \
    >"$nonmatch_log"
  if log_has_real_failure "$nonmatch_log"; then
    echo "assertion failed (#5210 narrowness): unsupported/prose shape matched" >&2
    exit 1
  fi
}

# Execute triage as a sourced consumer in a separate Bash process. This makes
# its effective function, including any later local override, prove the shared
# rustfmt and Python boundaries rather than trusting source-text wiring alone.
# The grep check remains only a structural diagnostic for an executable source
# statement; effective predicate behavior is the regression authority.
run_real_failure_predicate_sharing_assertions() {
  local rerun_script="$REPO_ROOT/scripts/ci/infra-failure-rerun.sh"
  local rustfmt_log="$TMP_DIR/selftest-triage-shared-rustfmt.log"
  local python_log="$TMP_DIR/selftest-triage-shared-python.log"
  # This is literal source text used to audit both consumers.
  # shellcheck disable=SC2016
  local source_statement='source "$REAL_FAILURE_PREDICATE"'

  printf '%s\n' 'Diff in /tmp/main.rs:12:' >"$rustfmt_log"
  printf '%s\n' 'FAILED (errors=1)' >"$python_log"
  TRIAGE_SCRIPT="$TRIAGE_SCRIPT" bash -s -- "$rustfmt_log" "$python_log" <<'BASH'
set -euo pipefail
source "$TRIAGE_SCRIPT"
if [[ "$(type -t log_has_real_failure)" != "function" ]]; then
  echo "assertion failed (#5210 DRY behavior): triage predicate is not a function" >&2
  exit 1
fi
log_has_real_failure "$1" || {
  echo "assertion failed (#5210 DRY behavior): triage missed shared rustfmt marker" >&2
  exit 1
}
log_has_real_failure "$2" || {
  echo "assertion failed (#5210 DRY behavior): triage missed shared Python unittest marker" >&2
  exit 1
}
BASH

  for consumer in "$rerun_script" "$TRIAGE_SCRIPT"; do
    grep -E -q -- "^[[:space:]]*${source_statement//\$/\\\$}[[:space:]]*(#.*)?$" "$consumer" || {
      echo "assertion failed (#5210 DRY): $consumer does not source the shared real-failure predicate" >&2
      exit 1
    }
  done
}

# #5207: smoke check for the `abandoned` upstream label; the full matrix lives
# in tests/test_infra_failure_classifier_5207.sh, which CI runs alongside this.
run_upstream_abandoned_assertions() {
  local mirror_log="$TMP_DIR/selftest-upstream-abandoned.log"
  local word_log="$TMP_DIR/selftest-abandoned-word.log"

  printf '%s\n' "##[error]Changed paths result is unexpected: 'abandoned'" >"$mirror_log"
  printf '%s\n' 'test store::abandoned_session_is_reaped ... ok' >"$word_log"

  assert_equal "infra-upstream-abandoned" \
    "$(classify_job 'Fast check cross OS required context (ubuntu-latest)' "$mirror_log" 5)" \
    "an abandoned upstream is reported as upstream infrastructure"
  assert_equal "unrelated-failure" \
    "$(classify_job "$PG_JOB_NAME" "$word_log" 5)" \
    "the bare word 'abandoned' is not an infrastructure fingerprint"

  # Containment: diagnosing the mirror must never make it rerunnable, or the
  # fail-closed gate would be neutralised by retry.
  if job_is_infra_failure "$mirror_log" 5; then
    echo "assertion failed: an abandoned upstream must NOT satisfy the rerun predicate" >&2
    exit 1
  fi
}

# #5207 R1: the 2026-08-07 outage fingerprint must classify as infrastructure,
# and the narrowness of the anchor must be proven in the same place — a pattern
# that also matched bare server errors or `Error: Not Found` would let genuine
# failures be auto-rerun.
run_action_download_assertions() {
  local outage_log="$TMP_DIR/selftest-action-download.log"
  local bare_server_error_log="$TMP_DIR/selftest-bare-service-unavailable.log"
  local missing_action_log="$TMP_DIR/selftest-action-not-found.log"

  printf '%s\n' \
    'Failed to resolve action download info. Error: Service Unavailable' \
    'Failed to resolve action download info. Error: Service Unavailable' \
    >"$outage_log"
  # A test that merely prints the server error string must not be rerunnable.
  printf '%s\n' \
    'assert_eq!(body, "Service Unavailable");' \
    'Error: Service Unavailable' \
    'HTTP 503 Service Unavailable' \
    >"$bare_server_error_log"
  # A workflow referencing an action that does not exist is a repo regression.
  # #5207 (r3): this is the REAL wording, copied from actions/runner#1006 — an
  # earlier revision used an invented `Error: Not Found`, which no GitHub
  # component emits. See the provenance block at REPO_CONFIG_FAULT_REGEX.
  # The backticks are literal runner output, not command substitutions.
  # shellcheck disable=SC2016
  printf '%s\n' \
    'Failed to resolve action download info. Error: Unable to resolve action `actions/chcekout@v2`, repository not found' \
    >"$missing_action_log"

  log_has_infra_failure "$outage_log" || {
    echo "assertion failed: action download outage must be infrastructure" >&2
    exit 1
  }
  assert_equal "infra-unrelated" \
    "$(classify_job "$PG_JOB_NAME" "$outage_log" 0)" \
    "action download outage classifies as infra"
  if log_has_infra_failure "$bare_server_error_log"; then
    echo "assertion failed: bare 'Service Unavailable' must NOT be infrastructure" >&2
    exit 1
  fi
  if log_has_infra_failure "$missing_action_log"; then
    echo "assertion failed: unresolvable action (Not Found) must NOT be infrastructure" >&2
    exit 1
  fi
  # #5207 (F1): the count this failure mode actually produces is ZERO — action
  # refs resolve inside `Set up job`, so no repo step starts. Asserting it at 1
  # pinned a state that cannot occur and let the zero-step path re-admit the
  # very log the anchor above excludes.
  assert_equal "unrelated-failure" \
    "$(classify_job "$PG_JOB_NAME" "$missing_action_log" 0)" \
    "unresolvable action stays non-infrastructure at its real step count of 0"

  # #5207 (r3, P2-1): the RETRY PREDICATE, not only the label — no `classify_job`
  # assertion reaches `job_is_infra_failure`, which is what gates
  # `would-rerun:infra`. Rationale and accepted cost: see that function.
  local mixed_fault_log="$TMP_DIR/selftest-outage-plus-repo-fault.log"
  # The backticks are literal runner output, not command substitutions.
  # shellcheck disable=SC2016
  printf '%s\n' \
    'Failed to resolve action download info. Error: Service Unavailable' \
    'Failed to resolve action download info. Error: Unable to resolve action `acme/gone@v1`, repository not found' \
    >"$mixed_fault_log"
  if job_is_infra_failure "$mixed_fault_log" 0; then
    echo "assertion failed (P2-1): a repo-config fault beside the outage fingerprint must NOT satisfy the rerun predicate" >&2
    exit 1
  fi
  job_is_infra_failure "$outage_log" 0 || {
    echo "assertion failed (P2-1 reverse): a clean outage log must still satisfy the rerun predicate" >&2
    exit 1
  }
}

# #5207 R3: zero executed repo steps reaches the structural signal (once the
# F1 repo-fault guard has cleared the log), and a job that did run steps must
# never take that path.
run_zero_step_assertions() {
  local quiet_log="$TMP_DIR/selftest-zero-step.log"

  printf '%s\n' 'Error: The operation could not be completed.' >"$quiet_log"

  assert_equal "infra-no-steps" \
    "$(classify_job "$PG_JOB_NAME" "$quiet_log" 0)" \
    "zero executed repo steps classifies as infra"
  assert_equal "unrelated-failure" \
    "$(classify_job "$PG_JOB_NAME" "$quiet_log" 13)" \
    "a job that executed steps must not take the zero-step path"
  assert_equal "unrelated-failure" \
    "$(classify_job "$PG_JOB_NAME" "$quiet_log" unknown)" \
    "absent step data must fail closed, not read as zero"

  job_ran_no_repo_steps 0 || {
    echo "assertion failed: 0 executed steps must be a structural infra signal" >&2
    exit 1
  }
  if job_ran_no_repo_steps 1; then
    echo "assertion failed: 1 executed step must not be a structural infra signal" >&2
    exit 1
  fi
  if job_ran_no_repo_steps unknown; then
    echo "assertion failed: unknown step count must not be a structural infra signal" >&2
    exit 1
  fi
}

# #5207 R4: pin the precedence contract. A log carrying regression markers is a
# regression regardless of how many infrastructure fingerprints accompany it,
# and no infra path may reach the retry decision past that guard.
run_regression_precedence_assertions() {
  local regression_with_outage_log="$TMP_DIR/selftest-regression-with-outage.log"

  printf '%s\n' \
    'Failed to resolve action download info. Error: Service Unavailable' \
    'test result: FAILED. 1 passed; 1 failed' \
    >"$regression_with_outage_log"

  log_has_infra_failure "$regression_with_outage_log" || {
    echo "assertion failed: fixture must carry the infrastructure fingerprint" >&2
    exit 1
  }
  assert_equal "regression" \
    "$(classify_job "$PG_JOB_NAME" "$regression_with_outage_log" 0)" \
    "regression outranks both the R1 pattern and the R3 zero-step signal"

  local mixed_regression_count=0
  if job_log_has_blocking_regression "$PG_JOB_NAME" "$regression_with_outage_log"; then
    mixed_regression_count=$((mixed_regression_count + 1))
  fi
  assert_equal "no-op:regression" \
    "$(decide_retry 1 1 "$mixed_regression_count" 0)" \
    "regression blocks rerun even when the infra predicates match"
}

# #5207 R2: prove the sibling regexes are aligned AND that the comparison is
# capable of rejecting drift. The negative controls are what make neutering
# `assert_termination_regex_synced` fail here instead of passing silently.
run_termination_sync_assertions() {
  local synced_fixture="$TMP_DIR/selftest-triage-synced.sh"
  local drifted_fixture="$TMP_DIR/selftest-triage-drifted.sh"
  local absent_fixture="$TMP_DIR/selftest-triage-absent.sh"
  local indented_close_fixture="$TMP_DIR/selftest-triage-indented-close.sh"

  assert_termination_regex_synced "$TRIAGE_SCRIPT" || {
    echo "assertion failed: scripts/ci/infra-failure-rerun.sh INFRA_TERMINATION_REGEX has drifted from log_has_infra_termination in $TRIAGE_SCRIPT" >&2
    echo "  rerun script: $INFRA_TERMINATION_REGEX" >&2
    echo "  triage script: $(extract_termination_regex "$TRIAGE_SCRIPT" || echo '<not found>')" >&2
    exit 1
  }

  {
    # The `${1}` is load-bearing: it puts a `}` on a line INSIDE the function
    # and before the literal, so widening the F7 brace guard to a bare `/}/`
    # truncates the scan here and this fixture stops being accepted.
    #
    # #5207 (r4): state the limit plainly — that discriminator exists ONLY
    # here. The real scripts/main-ci-triage.sh writes `local log_path="$1"`,
    # with no braces and so no `}` before its literal, so widening the guard to
    # `/}/` changes nothing when the extractor is pointed at production alone.
    # This fixture is what makes the mutation detectable; deleting it, or
    # "simplifying" `${1}` to `$1`, silently retires that coverage.
    # These are literal source fixtures; their parameter syntax must not expand.
    # shellcheck disable=SC2016
    printf '%s\n' 'log_has_infra_termination() {' '  local log_path="${1}"'
    printf "  grep -E -i -q -- \\\\\n    '%s' \\\\\n" "$INFRA_TERMINATION_REGEX"
    # shellcheck disable=SC2016
    printf '%s\n' '    "$log_path"' '}'
  } >"$synced_fixture"
  {
    printf '%s\n' 'log_has_infra_termination() {'
    printf "  grep -E -i -q -- \\\\\n    '%s' \\\\\n" "sig(term|kill)"
    # shellcheck disable=SC2016
    printf '%s\n' '    "$log_path"' '}'
  } >"$drifted_fixture"
  printf '%s\n' 'log_has_something_else() {' '  :' '}' >"$absent_fixture"
  # #5207 (r3, P2-3): the discriminating fixture for F7. `log_has_infra_termination`
  # holds NO literal and closes with an INDENTED `  }`; the NEXT function holds a
  # correct one. Anchoring the scan on a column-0 `}` (the pre-F7 shape) walks
  # past the indented close, adopts the neighbour's literal, and reports SYNCED
  # for a file whose termination check no longer has a regex at all. F7 makes it
  # stop at the indented brace and report drift instead.
  {
    # shellcheck disable=SC2016
    printf '%s\n' 'log_has_infra_termination() {' '  local log_path="$1"' '  }'
    printf '%s\n' 'log_has_neighbour() {'
    printf "  grep -E -i -q -- \\\\\n    '%s' \\\\\n" "$INFRA_TERMINATION_REGEX"
    # shellcheck disable=SC2016
    printf '%s\n' '    "$log_path"' '}'
  } >"$indented_close_fixture"

  assert_termination_regex_synced "$synced_fixture" || {
    echo "assertion failed: an aligned sibling must be accepted" >&2
    exit 1
  }
  if assert_termination_regex_synced "$drifted_fixture"; then
    echo "assertion failed: a drifted sibling regex must be rejected" >&2
    exit 1
  fi
  if assert_termination_regex_synced "$absent_fixture"; then
    echo "assertion failed: a missing log_has_infra_termination must be rejected" >&2
    exit 1
  fi
  if assert_termination_regex_synced "$indented_close_fixture"; then
    echo "assertion failed (F7): an indented closing brace must end the scan, not let a neighbouring function's literal be read as SYNCED" >&2
    exit 1
  fi
}

main() {
  case "${1-}" in
    --self-test)
      run_self_test
      ;;
    --classify-job)
      if [[ $# -ne 4 ]]; then
        usage >&2
        exit 1
      fi
      classify_job "$2" "$3" "$4"
      printf '\n'
      ;;
    --failed-job-rows)
      if [[ $# -ne 2 ]]; then
        usage >&2
        exit 1
      fi
      require_cmd jq
      failed_job_rows "$2"
      ;;
    "")
      run_classifier
      ;;
    -h|--help)
      usage
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
}

main "$@"
