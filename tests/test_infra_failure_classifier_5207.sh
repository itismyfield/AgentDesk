#!/usr/bin/env bash
# #5207: discriminating tests for the PR infrastructure-failure classifier.
#
# The 2026-08-07 GitHub Actions major outage emitted
# `Failed to resolve action download info. Error: Service Unavailable` ten
# times in a single run and matched none of the classifier's patterns.
#
# What that cost, stated exactly: the run's summary labelled a pure
# infrastructure outage as an ordinary unexplained failure, and humans read it
# as a code regression. It did NOT cost a retry. Measured on run 31116949449,
# attempts 1-3: the PostgreSQL job never appears among the failed jobs, and
# `decide_retry` only ever reruns a single failed PostgreSQL job, so the
# decision was `no-op:no-pg-failure` before this change and is
# `no-op:no-pg-failure` after it — identical, bit for bit. This work changes
# the classification LABEL, not the retry behaviour. Widening the retry scope
# beyond the PostgreSQL job is a separate issue.
#
# These tests pin BOTH directions. Widening the infra patterns so a real
# regression is auto-rerun is strictly worse than the bug being fixed, so every
# positive case here is paired with a negative control.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/ci/infra-failure-rerun.sh"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/infra-classifier-5207.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT

FAILED=0

fail() {
  echo "not ok - $1" >&2
  FAILED=1
}

pass() {
  echo "ok - $1"
}

# Drives the shipped classification chain through the script's own entry point,
# so these assertions cover the same code path `run_classifier` executes.
assert_class() {
  local name="$1"
  local expected="$2"
  local job_name="$3"
  local log_path="$4"
  local executed_steps="$5"
  local actual

  actual="$("$SCRIPT" --classify-job "$job_name" "$log_path" "$executed_steps")"
  if [[ "$actual" == "$expected" ]]; then
    pass "$name"
  else
    fail "$name (expected '$expected', got '$actual')"
  fi
}

write_log() {
  local path="$WORK_DIR/$1"
  shift
  printf '%s\n' "$@" >"$path"
  printf '%s' "$path"
}

PG_JOB="PostgreSQL tests (ubuntu-postgres)"

# --- R1: the outage fingerprint is infrastructure -------------------------
OUTAGE_LOG="$(write_log outage.log \
  'Download action repository actions/checkout@v4' \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'Failed to resolve action download info. Error: Service Unavailable')"
assert_class "action download outage is classified as infrastructure" \
  "infra-unrelated" "$PG_JOB" "$OUTAGE_LOG" 1

# --- R4 / acceptance: regression outranks infrastructure ------------------
# The most important direction. A log carrying regression markers must not be
# rerun no matter how many infrastructure fingerprints sit beside it.
REGRESSION_LOG="$(write_log regression-with-outage.log \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'The runner has received a shutdown signal.' \
  'test result: FAILED. 163 passed; 1 failed')"
assert_class "regression outranks the action-download pattern" \
  "regression" "$PG_JOB" "$REGRESSION_LOG" 1
assert_class "regression outranks the zero-step structural signal" \
  "regression" "$PG_JOB" "$REGRESSION_LOG" 0

PANIC_LOG="$(write_log panic-with-outage.log \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'thread panicked at src/services/relay.rs:42:9')"
assert_class "panic outranks the action-download pattern" \
  "regression" "$PG_JOB" "$PANIC_LOG" 0

# --- R1 narrowness: negative controls -------------------------------------
BARE_ERROR_LOG="$(write_log bare-service-unavailable.log \
  'test tunnel::returns_503 ... ok' \
  'assert_eq!(status_text, "Service Unavailable");' \
  'Error: Service Unavailable' \
  'HTTP/1.1 503 Service Unavailable')"
assert_class "bare 'Service Unavailable' is not infrastructure" \
  "unrelated-failure" "$PG_JOB" "$BARE_ERROR_LOG" 1

# A workflow that names an action which does not exist is a repo-config
# regression, not an outage. It shares the action-download prefix, so this is
# the tightest negative control on the R1 anchor.
#
# The step count here is 0, and that is the whole point. Action references are
# resolved inside `Set up job`, so this failure mode CANNOT produce a nonzero
# count; asserting it at 1 (as this control first did) pinned a state that
# never occurs, and the zero-step signal quietly re-admitted the very log the
# anchor above was written to exclude.
#
# #5207 (r3): the fixture is a VERBATIM real log line (actions/runner#1006; see
# also setup-gcloud#270, ratt-ru/tricolour#72, SpraxDev/Action-SpigotMC#21). The
# previous fixture said `Error: Not Found`, which no GitHub component emits — it
# was invented in the issue text and then used as its own oracle. Provenance for
# every alternation lives at REPO_CONFIG_FAULT_REGEX.
NOT_FOUND_LOG="$(write_log action-not-found.log \
  'Failed to resolve action download info. Error: Unable to resolve action `actions/chcekout@v2`, repository not found')"
assert_class "unresolvable action (real 'Unable to resolve action' wording) is not infrastructure" \
  "unrelated-failure" "$PG_JOB" "$NOT_FOUND_LOG" 0

BAD_GATEWAY_LOG="$(write_log bad-gateway.log \
  'Failed to resolve action download info. Error: Bad gateway')"
assert_class "action download bad gateway is infrastructure" \
  "infra-unrelated" "$PG_JOB" "$BAD_GATEWAY_LOG" 1

# #5207 (r4, P3-1): the text in this slot is `response.ReasonPhrase` verbatim
# (LaunchHttpClient.cs:51,77 -> ActionManager.cs:1086), and servers ship both
# RFC 2616's `Gateway Time-out` and RFC 7231's `Gateway Timeout`. The
# `time-?out` tolerance is deliberate, so both spellings are pinned.
assert_class "action download gateway timeout (RFC 7231 spelling) is infrastructure" \
  "infra-unrelated" "$PG_JOB" \
  "$(write_log gateway-timeout.log 'Failed to resolve action download info. Error: Gateway Timeout')" 1
assert_class "action download gateway time-out (RFC 2616 spelling) is infrastructure" \
  "infra-unrelated" "$PG_JOB" \
  "$(write_log gateway-time-out.log 'Failed to resolve action download info. Error: Gateway Time-out')" 1

# The two alternations REMOVED in r4, pinned as removed so neither can return
# without provenance. `too many requests` is unreachable by construction:
# LaunchHttpClient.cs:68-73 throws NonRetryableActionDownloadInfoException for a
# 429, and ActionManager.cs:1084-1086 prints this prefix only for exceptions
# that are neither that nor UnresolvableActionDownloadInfoException — so the
# line below is a log GitHub does not emit. `temporarily unavailable` is not an
# HTTP reason phrase and was found in this slot zero times.
assert_class "r4: a 429 in the action-download slot is not infrastructure (unreachable phrasing, removed)" \
  "unrelated-failure" "$PG_JOB" \
  "$(write_log too-many-requests.log 'Failed to resolve action download info. Error: Too Many Requests')" 1
assert_class "r4: 'temporarily unavailable' is not infrastructure (no provenance, removed)" \
  "unrelated-failure" "$PG_JOB" \
  "$(write_log temporarily-unavailable.log 'Failed to resolve action download info. Error: Temporarily Unavailable')" 1

# --- R3: zero executed repo steps is a structural infrastructure signal ----
# The failing jobs in the outage ran `Set up job` and nothing else. That count
# is a necessary condition, not a sufficient one (see F1 below), but once the
# repo-fault guard clears the log it carries the outage regardless of wording.
QUIET_LOG="$(write_log quiet.log \
  'Error: The operation could not be completed.')"
assert_class "a job that executed no repo steps is infrastructure" \
  "infra-no-steps" "$PG_JOB" "$QUIET_LOG" 0
assert_class "a job that executed repo steps does not take the zero-step path" \
  "unrelated-failure" "$PG_JOB" "$QUIET_LOG" 13
assert_class "one executed repo step does not take the zero-step path" \
  "unrelated-failure" "$PG_JOB" "$QUIET_LOG" 1
# Absent step data must fail closed. Reading a missing `steps` array as zero
# would classify every failed job as infrastructure.
assert_class "absent step data fails closed rather than reading as zero" \
  "unrelated-failure" "$PG_JOB" "$QUIET_LOG" unknown

# --- F1: zero steps is NECESSARY, not SUFFICIENT --------------------------
# Six repo-configuration faults, each this repo's own defect, each killing the
# job before any repo step runs and so arriving with exactly zero executed repo
# steps. Unguarded, all six classify as `infra-no-steps` and get auto-rerun —
# precisely the exclusion the R1 anchor above was written to make.
#
# ★ #5207 (r3) — EVERY FIXTURE LINE BELOW IS A VERBATIM OBSERVED STRING, not a
# paraphrase. r2 shipped two invented ones (`no runner matching the labels`,
# `The workflow is requesting permissions that are not allowed`); the regex
# matched them and nothing else, so the suite proved only self-consistency.
# Per-alternation sources: the provenance block at REPO_CONFIG_FAULT_REGEX in
# scripts/ci/infra-failure-rerun.sh, cases [1]..[6] in the order below.
while IFS='|' read -r FAULT_DESC FAULT_LINE; do
  [ -n "$FAULT_DESC" ] || continue
  assert_class "F1: $FAULT_DESC is not infrastructure at 0 steps" \
    "unrelated-failure" "$PG_JOB" "$(write_log repo-fault.log "$FAULT_LINE")" 0
done <<'CASES'
nonexistent action ref|Failed to resolve action download info. Error: Unable to resolve action `actions/chcekout@v2`, repository not found
action blocked by Actions policy|Failed to resolve action download info. Error: Bad request - freertos/ci-cd-github-actions@main is not allowed to be used in aws/aws-iot-device-sdk-embedded-C.
bad runs-on labels|Error: No runner matching the specified labels was found: macos-15-intel
bad container image|Error response from daemon: manifest for weirauchlab/reli:alpine not found: manifest unknown: manifest unknown
invalid workflow YAML|The workflow is not valid. .github/workflows/ci.yml (Line: 49, Col: 3): Unexpected value
over-broad nested-job permissions|The nested job 'release' is requesting 'contents: write', but is only allowed 'contents: read'.
CASES

# The opposite direction, which is the reason this change exists at all. If the
# guard ever widens far enough to swallow these, #5207 is un-fixed.
assert_class "F1 reverse: the real outage fingerprint at 0 steps is still infrastructure" \
  "infra-unrelated" "$PG_JOB" "$OUTAGE_LOG" 0
# ★ LIMIT, not a feature. This pins the RESIDUAL RISK enumerated at
# REPO_CONFIG_FAULT_REGEX: a repo-configuration fault whose wording is NOT in
# that list is still labelled infrastructure and still auto-retried. The fixture
# is an unrecognised string standing for ~15 measured real cases. The cost, and
# the limit of the `run_attempt >= 3` containment argument (it bounds only
# DETERMINISTIC faults), are stated once — at that constant.
NOVEL_OUTAGE_LOG="$(write_log novel-outage.log \
  'Error: a transient failure wording that no pattern here knows yet')"
assert_class "LIMIT: an unenumerated repo-config fault at 0 steps is still auto-retried as infrastructure" \
  "infra-no-steps" "$PG_JOB" "$NOVEL_OUTAGE_LOG" 0

# --- P2-1: a repo-config fault beside an outage fingerprint ---------------
# Both fingerprints in ONE log, on different lines — a configuration regression
# merged while GitHub was degraded. r2 let R1 win on line 1 and never consulted
# the repo-fault guard, so the job was labelled `infra-unrelated` and rerun.
MIXED_FAULT_LOG="$(write_log outage-plus-repo-fault.log \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'Failed to resolve action download info. Error: Unable to resolve action `acme/gone@v1`, repository not found')"
assert_class "P2-1: a repo-config fault beside the outage fingerprint is not infrastructure" \
  "unrelated-failure" "$PG_JOB" "$MIXED_FAULT_LOG" 1
assert_class "P2-1: the same log at 0 steps is withheld from the structural path too" \
  "unrelated-failure" "$PG_JOB" "$MIXED_FAULT_LOG" 0
# The accepted COST of that guard, pinned so it cannot be paid by accident: a
# genuine outage log that happens to carry an enumerated phrase loses its
# automatic rerun. Fail-closed — a human reruns it.
COINCIDENCE_LOG="$(write_log outage-with-incidental-manifest.log \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'docker: Error response from daemon: manifest for ghcr.io/acme/cache:v3 not found: manifest unknown')"
assert_class "P2-1 cost: an outage log carrying an incidental enumerated phrase is withheld too" \
  "unrelated-failure" "$PG_JOB" "$COINCIDENCE_LOG" 1
# The direction that must NOT change: a clean outage log still reruns.
assert_class "P2-1 reverse: a clean outage log is unaffected by the hoisted guard" \
  "infra-unrelated" "$PG_JOB" "$OUTAGE_LOG" 1

# --- r4: the NARROWNESS of two withhold alternations, pinned ---------------
# Both of these were previously asserted only by a comment. Because
# REPO_CONFIG_FAULT_REGEX is consulted NEGATIVELY, over-broadening it is
# fail-closed and so passes every other test in this file: the cost is silent —
# a genuine outage stops being auto-retried. These two controls make that cost
# visible. Each fixture carries the outage fingerprint (so the expected answer
# is `infra-unrelated`) plus one line of ordinary log noise that a widened
# alternation would seize on.
#
# [1]: the `failed to resolve action download info\. error: ` prefix is what
# separates an Actions POLICY rejection from any other 400 in the log. Drop it
# and a bare `bad request` anywhere in the run withholds the retry.
BARE_BAD_REQUEST_LOG="$(write_log outage-with-bare-bad-request.log \
  'Failed to resolve action download info. Error: Service Unavailable' \
  '2026-08-07T00:00:01Z WARN relay: upstream answered 400 Bad Request, retrying')"
assert_class "r4 [1] narrowness: a bare 'bad request' elsewhere in the log does not withhold the retry" \
  "infra-unrelated" "$PG_JOB" "$BARE_BAD_REQUEST_LOG" 1

# [5]: the quoted `'<x>' … '<y>'` shape is what makes this a permissions
# rejection. Relaxed to `requesting.*allowed`, ordinary prose containing both
# words on one line withholds the retry instead.
LOOSE_PERMISSIONS_LOG="$(write_log outage-with-requesting-allowed-prose.log \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'DEBUG scheduler: worker is requesting a slot; queue growth is allowed')"
assert_class "r4 [5] narrowness: unquoted 'requesting … allowed' prose does not withhold the retry" \
  "infra-unrelated" "$PG_JOB" "$LOOSE_PERMISSIONS_LOG" 1

# --- second outage fingerprint: an abandoned upstream ---------------------
# Run 31120826793 (PR #5204): the gating `Changed paths` job waited ~16 minutes
# for a runner, was retired as `abandoned`, and every downstream mirror failed
# closed. Those mirror failures are correct behaviour, not regressions, and
# they must be labelled as upstream infrastructure WITHOUT ever becoming
# rerunnable — retrying a fail-closed gate is how you neuter it.
MIRROR_JOB='Fast check cross OS required context (ubuntu-latest)'
ABANDONED_LOG="$(write_log upstream-abandoned.log \
  '  CHANGED_PATHS_RESULT: abandoned' \
  '  UPSTREAM_RESULT:      skipped' \
  "##[error]Changed paths result is unexpected: 'abandoned'")"
assert_class "abandoned upstream is reported as upstream infrastructure" \
  "infra-upstream-abandoned" "$MIRROR_JOB" "$ABANDONED_LOG" 5

# Narrowness: `abandoned` is an ordinary English word. The anchor must require
# the mirror's own `<X>_RESULT:` echo or its quoted result message.
ABANDONED_WORD_LOG="$(write_log abandoned-word.log \
  'test store::abandoned_session_is_reaped ... ok' \
  'INFO dropping the abandoned queue entry' \
  'the request was abandoned by the client')"
assert_class "the bare word 'abandoned' is not an infrastructure fingerprint" \
  "unrelated-failure" "$PG_JOB" "$ABANDONED_WORD_LOG" 5

# A mirror failing closed for a different upstream result keeps the plain
# label — only `abandoned` means "GitHub never gave the job a runner".
MIRROR_CANCELLED_LOG="$(write_log mirror-cancelled.log \
  "Changed paths result is 'cancelled'; failing closed instead of treating Fast check result 'skipped' as pass")"
assert_class "a fail-closed mirror with a non-abandoned upstream keeps the plain label" \
  "unrelated-failure" "$PG_JOB" "$MIRROR_CANCELLED_LOG" 5

# Regression precedence still outranks the new fingerprint.
assert_class "regression outranks the abandoned-upstream fingerprint" \
  "regression" "$PG_JOB" "$(write_log abandoned-with-regression.log \
    "##[error]Changed paths result is unexpected: 'abandoned'" \
    'test result: FAILED. 1 passed; 1 failed')" 5

# --- F3: the step-count expression itself ---------------------------------
# Everything above hands the classifier an integer. The jq expression that
# PRODUCES that integer in the live loop had no test at all, so mutating away
# the housekeeping exclusion list (permanently disabling the zero-step signal)
# or turning its fail-closed `unknown` into `0` (making every failed job look
# like infrastructure) both went unnoticed.
JOBS_PAYLOAD="$WORK_DIR/jobs-payload.json"
cat >"$JOBS_PAYLOAD" <<'JSON'
{"total_count": 7, "jobs": [
 {"id":101,"name":"setup-only","conclusion":"failure","steps":[{"name":"Set up job"}]},
 {"id":102,"name":"housekeeping","conclusion":"failure","steps":[{"name":"Set up job"},{"name":"Complete job"}]},
 {"id":103,"name":"mixed-case","conclusion":"failure","steps":[{"name":"SET UP JOB"},{"name":"complete job"}]},
 {"id":104,"name":"one-repo-step","conclusion":"failure","steps":[{"name":"Set up job"},{"name":"Run tests"},{"name":"Complete job"}]},
 {"id":105,"name":"no-steps-key","conclusion":"failure"},
 {"id":106,"name":"empty-steps","conclusion":"failure","steps":[]},
 {"id":107,"name":"green","conclusion":"success","steps":[{"name":"Set up job"},{"name":"Run tests"}]}
]}
JSON

step_count_for() {
  "$SCRIPT" --failed-job-rows "$JOBS_PAYLOAD" | awk -F'\t' -v id="$1" '$1 == id { print $2 }'
}

assert_step_count() {
  local name="$1"
  local job_id="$2"
  local expected="$3"
  local actual
  actual="$(step_count_for "$job_id")"
  if [[ "$actual" == "$expected" ]]; then
    pass "$name"
  else
    fail "$name (expected '$expected', got '$actual')"
  fi
}

# Kills "drop the housekeeping exclusion": without it these become 1, 2, 2, 3.
assert_step_count "F3: a job that ran only 'Set up job' counts 0 repo steps" 101 0
assert_step_count "F3: 'Set up job' + 'Complete job' counts 0 repo steps" 102 0
assert_step_count "F3: housekeeping names are matched case-insensitively" 103 0
assert_step_count "F3: one real step among the housekeeping counts 1" 104 1
# Kills "fail open to 0": without the `unknown` branch these become 0, which
# would classify every failed job with unreadable step data as infrastructure.
assert_step_count "F3: an absent steps array yields 'unknown', never 0" 105 unknown
# F4: an empty array is data-absence wearing an array's type. A job the API
# calls `failure` necessarily entered `Set up job`, so a truthful payload for it
# always carries at least that step; `[]` therefore means the steps were not
# reported, not that the job ran none. It must fail closed like an absent key.
assert_step_count "F4: an empty steps array yields 'unknown', never 0" 106 unknown

ROW_COUNT="$("$SCRIPT" --failed-job-rows "$JOBS_PAYLOAD" | wc -l | tr -d ' ')"
if [[ "$ROW_COUNT" == "6" ]]; then
  pass "F3: only failed jobs produce rows"
else
  fail "F3: only failed jobs produce rows (expected 6, got $ROW_COUNT)"
fi

# End-to-end: the count the expression produced must drive the classification.
assert_class "F3 end-to-end: the produced 'unknown' fails closed in the classifier" \
  "unrelated-failure" "$PG_JOB" "$QUIET_LOG" "$(step_count_for 106)"
assert_class "F3 end-to-end: the produced 0 reaches the structural signal" \
  "infra-no-steps" "$PG_JOB" "$QUIET_LOG" "$(step_count_for 101)"

# --- r4 P2: `run_classifier`, the production entry point -------------------
# ★ Everything above drives `--classify-job`, a DIAGNOSTIC entry point nothing
# in production calls. The real one is `run_classifier` — the no-argument form
# `.github/workflows/ci-pr-infra-retry.yml:36` invokes — and it had zero
# coverage. That gap is not cosmetic: `run_classifier` decides the retry with a
# SECOND, independent call to `job_is_infra_failure`, and swapping that call
# back to `log_has_infra_failure` (i.e. undoing the P2-1 guard completely, in
# the only place where undoing it causes an actual rerun) passed every
# assertion in this file and every assertion in the self-test.
#
# So the classification chain is now driven end to end: real script, real jq
# expression, real summary writer, with only `gh` replaced. `RERUN_DRY_RUN=1`
# stops the flow before `gh run rerun`, AND the stub refuses — and records —
# any call it is not given a fixture for, so a case that reached the rerun would
# fail here rather than touch a repository.
#
# ★ #5207 (r5, D4) — WHAT THIS HARNESS DOES NOT REACH. The classifier consumes
# FOUR read-only endpoints, not three. The stub serves three of them:
#   1. `repos/{repo}/actions/runs/{id}/attempts/{n}`        (script :527)
#   2. `repos/{repo}/actions/runs/{id}/attempts/{n}/jobs`   (script :541)
#   3. `repos/{repo}/actions/jobs/{job-id}/logs`            (script :571)
# The fourth, `repos/{repo}/actions/runs/{id}` — the freshness re-check in
# `latest_attempt_is_still_failed` (script :494) — is DELIBERATELY NOT SERVED.
# It sits past the `RERUN_DRY_RUN=1` early return (script :618-622), so no case
# here reaches it, and leaving it unserved means an accidental live-path escape
# lands in `refused.log` instead of hitting the API.
# The price is stated rather than hidden: everything downstream of that early
# return is UNVERIFIED by this suite. Three `run_classifier` branches have ZERO
# coverage — `no-op:stale-attempt`, `no-op:rerun-request-failed`, and
# `rerun-requested:infra` (script :624-639). Do not read the E2E cases below as
# evidence about them.
E2E_BIN="$WORK_DIR/e2e-bin"
mkdir -p "$E2E_BIN"
cat >"$E2E_BIN/gh" <<'GH_STUB'
#!/usr/bin/env bash
# #5207 (r4) `gh` stand-in. Serves fixtures for three of the FOUR read-only
# endpoints the classifier consumes; the fourth -- `repos/.../actions/runs/<id>`,
# the freshness re-check that lives past the dry-run early return -- is refused
# on purpose, along with every other invocation, `gh run rerun` above all.
# See the r5/D4 note above for the branches that consequently go unverified.
set -u
if [ "${1-}" = "api" ]; then
  case "${2-}" in
    */attempts/*/jobs*) src="$GH_FIXTURE_DIR/jobs.json" ;;
    */attempts/*) src="$GH_FIXTURE_DIR/attempt.json" ;;
    */actions/jobs/*/logs)
      job="${2##*/actions/jobs/}"
      src="$GH_FIXTURE_DIR/job-${job%/logs}.log"
      ;;
    *)
      printf 'gh %s\n' "$*" >>"$GH_FIXTURE_DIR/refused.log"
      exit 1
      ;;
  esac
  [ -f "$src" ] || exit 1
  cat "$src"
  exit 0
fi
printf 'gh %s\n' "$*" >>"$GH_FIXTURE_DIR/refused.log"
exit 1
GH_STUB
chmod +x "$E2E_BIN/gh"

E2E_N=0
E2E_CASE=""

new_e2e_case() {
  E2E_N=$((E2E_N + 1))
  E2E_CASE="$WORK_DIR/e2e-$E2E_N"
  mkdir -p "$E2E_CASE"
  printf '%s\n' \
    '{"name":"CI PR","event":"pull_request","status":"completed","conclusion":"failure","run_attempt":1}' \
    >"$E2E_CASE/attempt.json"
  : >"$E2E_CASE/refused.log"
  : >"$E2E_CASE/summary.md"
}

# One failed PostgreSQL job, its `steps` array written so the real jq expression
# derives the executed-step count the case needs.
e2e_pg_job() {
  local job_id="$1"
  local steps_json="$2"
  printf '{"total_count":1,"jobs":[{"id":%s,"name":"%s","conclusion":"failure","steps":%s}]}\n' \
    "$job_id" "$PG_JOB" "$steps_json" >"$E2E_CASE/jobs.json"
}

assert_e2e() {
  local name="$1"
  local want_decision="$2"
  local want_label="$3"
  local out rc=0 got

  out="$(
    PATH="$E2E_BIN:$PATH" \
    GH_FIXTURE_DIR="$E2E_CASE" \
    GITHUB_REPOSITORY="itismyfield/AgentDesk" \
    RUN_ID=31116949449 \
    RUN_ATTEMPT=1 \
    RERUN_DRY_RUN=1 \
    GITHUB_STEP_SUMMARY="$E2E_CASE/summary.md" \
    bash "$SCRIPT" 2>&1
  )" || rc=$?

  if [ "$rc" -ne 0 ]; then
    fail "$name (run_classifier exited $rc: $(printf '%s' "$out" | tr '\n' ' '))"
    return
  fi
  got="$(printf '%s\n' "$out" | sed -n 's/^decision=\([^ ]*\).*$/\1/p' | tail -1)"
  if [ "$got" != "$want_decision" ]; then
    fail "$name (expected decision '$want_decision', got '$got')"
    return
  fi
  if ! grep -q "| \`$want_label\` |" "$E2E_CASE/summary.md"; then
    fail "$name (summary lacks the \`$want_label\` row: $(tr '\n' ' ' <"$E2E_CASE/summary.md"))"
    return
  fi
  if [ -s "$E2E_CASE/refused.log" ]; then
    fail "$name (reached a forbidden gh call: $(tr '\n' ' ' <"$E2E_CASE/refused.log"))"
    return
  fi
  pass "$name"
}

# E2E-1 — the wiring hole itself. A repo-config fault beside the outage
# fingerprint, on the PostgreSQL job, at its real step count of 0. The retry
# predicate must decline, so the run must NOT be rerun and the job must be
# labelled `unclassified-pg-failure`. Point `run_classifier` at
# `log_has_infra_failure` instead and both flip: `would-rerun:infra` /
# `infra-shutdown`.
new_e2e_case
e2e_pg_job 901 '[{"name":"Set up job"}]'
printf '%s\n' \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'Failed to resolve action download info. Error: Unable to resolve action `acme/gone@v1`, repository not found' \
  >"$E2E_CASE/job-901.log"
assert_e2e "E2E: run_classifier withholds the rerun when a repo-config fault sits beside the outage" \
  "no-op:unclassified-pg-failure" "unclassified-pg-failure"

# E2E-2 — the label chain must reach the decision. A PostgreSQL regression that
# also carries a shutdown fingerprint: `classify_job` calls it a regression,
# that count blocks the rerun. Stop `run_classifier` from consulting
# `classify_job` and the infra predicate alone reruns a red test suite.
new_e2e_case
e2e_pg_job 902 '[{"name":"Set up job"},{"name":"Run tests"},{"name":"Complete job"}]'
printf '%s\n' \
  'The runner has received a shutdown signal.' \
  'test result: FAILED. 163 passed; 1 failed' \
  >"$E2E_CASE/job-902.log"
assert_e2e "E2E: run_classifier lets the regression classification block the rerun" \
  "no-op:regression" "regression"

# E2E-3 — anti-vacuity. Without this, E2E-1 and E2E-2 could both be passing
# because the harness never reaches a rerun at all. A clean outage on the
# PostgreSQL job at 0 steps must arrive at `would-rerun:infra` — and `gh run
# rerun` must still never be called, which the stub's refusal log checks.
#
# #5207 (r5, D5) — HOW STRONG THAT CHECK ACTUALLY IS. It is a redundant second
# guard, not a measured-independent detector. Measured: neutering the
# `refused.log` assertion alone leaves the suite GREEN (the mutant SURVIVES);
# neutering it together with `RERUN_DRY_RUN`, and dropping `RERUN_DRY_RUN`
# alone, fail with the SAME message. A stub refusal always perturbs the
# decision too, and the decision assertion above catches it first, so no
# mutation exists for which this assertion is the sole detector. Keep it —
# defence in depth against a future path that refuses without moving the
# decision — but do not claim it proves anything `RERUN_DRY_RUN` does not.
new_e2e_case
e2e_pg_job 903 '[{"name":"Set up job"}]'
printf '%s\n' \
  'Failed to resolve action download info. Error: Service Unavailable' \
  'Failed to resolve action download info. Error: Service Unavailable' \
  >"$E2E_CASE/job-903.log"
assert_e2e "E2E: a clean outage still reaches would-rerun:infra without calling gh run rerun" \
  "would-rerun:infra" "infra-shutdown"

# --- R2: sibling regex drift must break the build -------------------------
# End-to-end proof, not an inspection: mirror both scripts into a scratch repo,
# drift only the triage side, and require the classifier's self-test to fail.
SYNC_REPO="$WORK_DIR/sync-repo"
mkdir -p "$SYNC_REPO/scripts/ci"
cp "$ROOT_DIR/scripts/main-ci-triage.sh" "$SYNC_REPO/scripts/main-ci-triage.sh"
cp "$SCRIPT" "$SYNC_REPO/scripts/ci/infra-failure-rerun.sh"
cp "$ROOT_DIR/scripts/ci/real-failure-predicate.sh" "$SYNC_REPO/scripts/ci/real-failure-predicate.sh"

if bash "$SYNC_REPO/scripts/ci/infra-failure-rerun.sh" --self-test >/dev/null 2>&1; then
  pass "mirrored, in-sync scripts pass the self-test"
else
  fail "mirrored, in-sync scripts pass the self-test"
fi

DRIFT_MARKER="runner has received a shutdown signal"
if ! grep -q -- "$DRIFT_MARKER'" "$SYNC_REPO/scripts/main-ci-triage.sh"; then
  fail "drift anchor no longer present in main-ci-triage.sh"
fi
# The trailing quote pins this to the regex literal, not to prose or fixtures.
awk -v marker="$DRIFT_MARKER'" -v repl="$DRIFT_MARKER DRIFTED'" '
  {
    idx = index($0, marker)
    if (idx > 0) {
      $0 = substr($0, 1, idx - 1) repl substr($0, idx + length(marker))
    }
    print
  }
' "$SYNC_REPO/scripts/main-ci-triage.sh" >"$SYNC_REPO/scripts/main-ci-triage.drifted.sh"
mv "$SYNC_REPO/scripts/main-ci-triage.drifted.sh" "$SYNC_REPO/scripts/main-ci-triage.sh"

# F8: this is a HYGIENE check on the test itself, not a check on production
# code — it kills no production mutation. It exists so that if the awk rewrite
# above ever silently no-ops (marker renamed, quoting changed), the drift
# assertion below cannot pass vacuously by testing an unmodified file.
if grep -q -- "$DRIFT_MARKER DRIFTED'" "$SYNC_REPO/scripts/main-ci-triage.sh"; then
  pass "drift was actually applied to the mirrored triage script"
else
  fail "drift was actually applied to the mirrored triage script"
fi

if bash "$SYNC_REPO/scripts/ci/infra-failure-rerun.sh" --self-test >/dev/null 2>&1; then
  fail "drifted sibling regex must fail the classifier self-test"
else
  pass "drifted sibling regex fails the classifier self-test"
fi

# --- wiring: the self-test that carries these guards must run on CI --------
if grep -q 'scripts/ci/infra-failure-rerun.sh --self-test' "$ROOT_DIR/scripts/ci-script-checks.sh"; then
  pass "classifier self-test is wired into scripts/ci-script-checks.sh"
else
  fail "classifier self-test is wired into scripts/ci-script-checks.sh"
fi

if [ "$FAILED" -ne 0 ]; then
  echo "tests/test_infra_failure_classifier_5207.sh FAILED" >&2
  exit 1
fi

echo "tests/test_infra_failure_classifier_5207.sh passed"
