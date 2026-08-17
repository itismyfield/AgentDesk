#!/usr/bin/env bash
# Discriminating tests for #5185's selection-set and known-failure gate.
#
# The defect this gate closes is a green check that proves nothing:
# `cargo test --lib <filter>` exits 0 when the filter matches zero tests, and
# this repository has recorded five false greens of exactly that shape.
#
# The first round of #5185 answered that with an execution-count floor, and the
# floor was measured failing to detect what it advertised. Two experiments
# against `--min-executed 6500`:
#
#   * narrowing one module by 402 tests  -> executed=6539, gate silent
#   * disabling a 213-test module        -> executed=6708, GATE_RC=0, false green
#
# So the assertions below are about *set identity*, not counts: the ids the
# lane is declared to run, compared against the ids the run reported, with both
# sides of the difference named. Test 1 pins the 213-test module-disable
# experiment as a transcript so that specific evasion stays detected.
#
# Every assertion drives `scripts/run_test_lane.py` with a libtest transcript
# and checks the exit status, not the log text alone. Most transcripts are
# synthetic because the property under test is the gate's own logic. The
# interleaved-stdout ones in §4c/§4d are copied verbatim -- ids included -- out
# of real sweeps, because the corruption they pin is not worth guessing at;
# §4f says so explicitly where it composes a shape instead.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RUNNER="$REPO_ROOT/scripts/run_test_lane.py"
REAL_LEDGER="$REPO_ROOT/scripts/known_test_failures.txt"
REAL_MANIFEST="$REPO_ROOT/scripts/lib_test_inventory_manifest.txt"
WORKFLOW="$REPO_ROOT/.github/workflows/ci-pr.yml"
PYTHON="${PYTHON:-python3}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-test-lane-5185.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

failures=0
passed=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}
# Counted on the else-branch of every assertion. Without it the closing line
# below prints "all assertions passed" for a file with no assertions left in
# it, which is the same sentence a green run prints.
pass_test() {
    passed=$((passed + 1))
}

LEDGER="$TMP_ROOT/ledger.txt"
MANIFEST="$TMP_ROOT/manifest.txt"
PRELUDE="$TMP_ROOT/prelude.txt"
OUT="$TMP_ROOT/out.log"
rc=0

# Builds a synthetic inventory manifest from the ids given on stdin.
#
# `run_test_lane.py` derives its expectation through the same reviewed
# static-only / cargo-only constants `--verify-lib-inventory` uses (#5144), and
# those constants are deliberately unconditional: there is no test-only escape
# that would let a real lane skip the adjustment. So the manifest written here
# also carries the known cargo-only ids, and `$PRELUDE` carries the matching
# `... ok` lines, which keeps the synthetic expectation equal to the ids under
# test.
write_manifest() {
    "$PYTHON" - "$REPO_ROOT/scripts" "$MANIFEST" "$PRELUDE" "$@" <<'PY'
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from check_test_target_integrity import (
    LIB_INVENTORY_KNOWN_CARGO_ONLY, expected_lib_static_only,
    render_lib_inventory_manifest,
)
manifest_path, prelude_path = Path(sys.argv[2]), Path(sys.argv[3])
ids = set(sys.argv[4:]) | set(LIB_INVENTORY_KNOWN_CARGO_ONLY)
static_only = expected_lib_static_only(sys.platform) or frozenset()
manifest_path.write_text(render_lib_inventory_manifest(ids | static_only), encoding="utf-8")
prelude_path.write_text(
    "".join(f"test {name} ... ok\n" for name in sorted(LIB_INVENTORY_KNOWN_CARGO_ONLY)),
    encoding="utf-8",
)
print(len(ids))
PY
}

# Drives the gate with a synthetic libtest transcript instead of cargo, so the
# assertions below are about the gate's own logic and run in milliseconds.
run_gate() {
    local emit="$1" exit_code="$2"
    shift 2
    rc=0
    # Command substitution strips the trailing newline, so re-add it or the
    # last prelude line and the first transcript line become one line.
    local body
    body="$(cat "$PRELUDE")"$'\n'"$emit"
    "$PYTHON" "$RUNNER" --ledger "$LEDGER" --inventory-manifest "$MANIFEST" "$@" -- \
        bash -c "printf '%s\n' \"\$0\"; exit $exit_code" "$body" \
        >"$OUT" 2>&1 || rc=$?
}

# Number of cargo-only ids the prelude reports, so summary lines can be built
# with a correct total.
EXTRA=$(grep -c '' "$PRELUDE" 2>/dev/null || echo 0)

summary_line() {
    # $1 passed (excluding prelude), $2 failed, $3 ignored
    printf 'test result: %s. %s passed; %s failed; %s ignored; 0 measured; 0 filtered out; finished in 0.01s\n' \
        "$([ "$2" -eq 0 ] && echo ok || echo FAILED)" \
        "$(( $1 + EXTRA ))" "$2" "$3"
}

# libtest's own list of failing ids, emitted before the summary whenever the
# run has failures. It is written by the console thread after every test thread
# has finished, which is why it survives the interleaving that corrupts the
# per-result lines -- and why the gate adjudicates failure attribution against
# it as a SET rather than against the summary's failure COUNT. A transcript
# that reports failures without one is itself an error, so every failing
# transcript below carries the block libtest would really have printed.
failures_block() {
    local name
    printf 'failures:'
    for name in "$@"; do printf '\n    %s' "$name"; done
}

cat >"$LEDGER" <<'EOF'
# no entries
EOF
write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")

PASSING_RUN="test alpha::beta ... ok
test alpha::gamma ... ok
$(summary_line 2 0 0)"

# --------------------------------------------------------------------------
# 0. The honest baseline: the full declared selection, nothing missing.
# --------------------------------------------------------------------------
run_gate "$PASSING_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "a run that reported its whole declared selection must pass (rc=$rc)"
else pass_test
fi

# --------------------------------------------------------------------------
# 1. THE REGRESSION THIS ROUND EXISTS FOR.
#    A module stops being compiled in. Its tests vanish from the run while
#    everything else still passes and cargo still reports a large number of
#    executed tests. Measured against the old floor this was GATE_RC=0.
#    The set comparison must fail AND name every missing id.
# --------------------------------------------------------------------------
"$PYTHON" - "$TMP_ROOT" <<'PY'
import sys
from pathlib import Path
root = Path(sys.argv[1])
kept = [f"kept::mod::t{index:04d}" for index in range(300)]
dropped = [f"disabled::mod::tests::t{index:04d}" for index in range(213)]
# Trailing newline matters: `while read` drops a final unterminated line.
(root / "kept.txt").write_text("".join(f"{name}\n" for name in kept), encoding="utf-8")
(root / "dropped.txt").write_text("".join(f"{name}\n" for name in dropped), encoding="utf-8")
PY
# shellcheck disable=SC2046
write_manifest $(cat "$TMP_ROOT/kept.txt") $(cat "$TMP_ROOT/dropped.txt") >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
DISABLED_MODULE_RUN="$( { while read -r name; do
    printf 'test %s ... ok\n' "$name"
done <"$TMP_ROOT/kept.txt"; } )
$(summary_line 300 0 0)"

run_gate "$DISABLED_MODULE_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "disabling a 213-test module must fail the lane; the execution floor it replaced returned GATE_RC=0 for exactly this transcript"
else pass_test
fi
if ! grep -q 'lane-missing (expected but never reported): disabled::mod::tests::t0000' "$OUT"; then
    fail_test "the missing ids must be named individually, not summarised as a count"
else pass_test
fi
named=$(grep -c '^lane-missing' "$OUT" || true)
if [ "$named" -ne 213 ]; then
    fail_test "all 213 missing ids must be named, got $named"
else pass_test
fi
if ! grep -q 'test-lane summary: .*missing=213 ' "$OUT"; then
    fail_test "the summary must report the missing-id count: $(grep 'test-lane summary' "$OUT")"
else pass_test
fi

# The same run with the module restored must pass, so the gate is not simply
# always red on this shape.
FULL_RUN="$( { while read -r name; do printf 'test %s ... ok\n' "$name"; done <"$TMP_ROOT/kept.txt"
              while read -r name; do printf 'test %s ... ok\n' "$name"; done <"$TMP_ROOT/dropped.txt"; } )
$(summary_line 513 0 0)"
run_gate "$FULL_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "the restored selection must pass (rc=$rc): $(grep -E '^ERROR' "$OUT" | head -3)"
else pass_test
fi

# --------------------------------------------------------------------------
# 2. A zero-match run exits 0 from cargo. The gate must still fail: this is
#    the original false green.
# --------------------------------------------------------------------------
write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
ZERO_RUN="
running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 7538 filtered out; finished in 0.00s"
run_gate "$ZERO_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a run that executed 0 tests must fail even though cargo exited 0"
else pass_test
fi

# --------------------------------------------------------------------------
# 3. #[ignore] must not be a silent exit from the selection set. A test that
#    reports itself ignored without a ledger entry fails the lane; an entry
#    whose test is no longer ignored fails it too.
# --------------------------------------------------------------------------
IGNORED_RUN="test alpha::beta ... ok
test alpha::gamma ... ignored
$(summary_line 1 0 1)"
run_gate "$IGNORED_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "an undeclared #[ignore] must fail the lane; it removes a test from the run while keeping the selection intact"
else pass_test
fi
if ! grep -q 'undeclared-ignored: alpha::gamma' "$OUT"; then
    fail_test "the undeclared ignored id must be named"
else pass_test
fi

cat >"$LEDGER" <<'EOF'
ignored | alpha::gamma | lane=demo | #5185 | child-process entry point executed only by its parent test through a fresh binary
EOF
run_gate "$IGNORED_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "a declared #[ignore] must be tolerated (rc=$rc)"
else pass_test
fi

# libtest appends the `#[ignore = "..."]` reason after the verdict. Anchoring
# the parse at end-of-line drops every ignored result, which shows up as the
# whole declared ignore set going stale at once.
IGNORED_WITH_REASON_RUN="test alpha::beta ... ok
test alpha::gamma ... ignored, helper subprocess for the cross-process file-lock test
$(summary_line 1 0 1)"
run_gate "$IGNORED_WITH_REASON_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "an ignored verdict carrying its #[ignore] reason must still parse (rc=$rc): $(grep -E '^(stale-ignored|ERROR)' "$OUT" | head -2)"
else pass_test
fi
run_gate "$PASSING_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "an ignored-entry whose test is no longer ignored must fail as stale"
else pass_test
fi
if ! grep -q 'stale-ignored: alpha::gamma' "$OUT"; then
    fail_test "the stale ignored entry must be named so it can be deleted"
else pass_test
fi

# --------------------------------------------------------------------------
# 4. Nested libtest output. A test that re-executes the test binary prints a
#    second summary to the inherited stdout, and its `... ok` lines were being
#    counted as this run's -- measured as a +1 error in the real sweep. More
#    summaries than the lane declares is an error, and the primary summary is
#    cross-checked against the derived expectation.
# --------------------------------------------------------------------------
cat >"$LEDGER" <<'EOF'
# no entries
EOF
NESTED_RUN="test alpha::beta ... ok
test alpha::gamma ... ok

running 1 test
test nested::injected::t0 ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 60 filtered out; finished in 0.00s

$(summary_line 2 0 0)"
run_gate "$NESTED_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "an undeclared nested libtest summary must fail the lane"
else pass_test
fi
if ! grep -q 'libtest summary lines, but the lane declares at most 1' "$OUT"; then
    fail_test "the extra-summary failure must name the declared maximum"
else pass_test
fi
# Declaring the nesting is not enough on its own: the ids the nested run
# contributes are still not part of the expected selection.
run_gate "$NESTED_RUN" 0 --lane demo --max-summaries 2
if [ "$rc" -eq 0 ]; then
    fail_test "ids reported only by a nested run must still fail the set comparison"
else pass_test
fi
if ! grep -q 'lane-extra (reported but not expected): nested::injected::t0' "$OUT"; then
    fail_test "the nested-only id must be named as extra"
else pass_test
fi

# A transcript whose summary disagrees with the derived expectation fails even
# when every reported id is legitimate: this is the synthetic "1 real test plus
# 60 nested lines clears a floor of 50" shape.
SHORT_SUMMARY_RUN="test alpha::beta ... ok
test alpha::gamma ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
run_gate "$SHORT_SUMMARY_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a libtest summary that does not account for the whole expected selection must fail"
else pass_test
fi
if ! grep -q 'but the manifest derives' "$OUT"; then
    fail_test "the summary cross-check must state the derived expectation"
else pass_test
fi

# --------------------------------------------------------------------------
# 4b. Interleaved stdout. libtest writes `test <id> ... ` and the verdict as
#     two writes onto a stdout it shares with every test thread and with any
#     child process that inherited it. Both halves were observed corrupted in
#     a real sweep: a foreign write without a newline prefixed the line, and a
#     foreign newline-terminated write split the verdict onto the next line.
#     A `^`-anchored whole-line parse drops the result and reports the test as
#     missing -- a false red, which is still a wrong verdict.
# --------------------------------------------------------------------------
INTERLEAVED_RUN="test alpha::beta ... ok
/tmp/x/agentdesk.plist: OKtest alpha::gamma ...
ok
$(summary_line 2 0 0)"
run_gate "$INTERLEAVED_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "a result line split and prefixed by foreign stdout must still be attributed (rc=$rc): $(grep -E '^(lane-missing|ERROR)' "$OUT" | head -2)"
else pass_test
fi

# The same tolerance must not invent results: an id that is not in the
# inventory manifest fails as extra rather than being silently absorbed.
INVENTED_RUN="test alpha::beta ... ok
test alpha::gamma ... ok
test alpha::not_in_manifest ... ok
$(summary_line 3 0 0)"
run_gate "$INVENTED_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "an id absent from the inventory manifest must fail as extra"
else pass_test
fi
if ! grep -q 'lane-extra (reported but not expected): alpha::not_in_manifest' "$OUT"; then
    fail_test "the unexpected id must be named"
else pass_test
fi

# --------------------------------------------------------------------------
# 4c. THE PARSER REGRESSION THIS ROUND EXISTS FOR.
#     The three transcript lines below are copied byte-for-byte -- ids
#     included -- out of real sweeps of this lane, not composed here:
#
#       lane5185r2-f3.log:1216   the foreign write landed BEFORE the name and
#                                its newline split the verdict onto line 2
#       lane5185-sweep-3.log:1215  the foreign write landed BETWEEN the name
#                                  and the verdict, on one line
#
#     The writer is the launchd-plist helper that the `discord_thread_create`
#     tests spawn; it inherits the test binary's stdout, and libtest emits
#     `test <id> ... ` and the verdict as two unsynchronised writes.
#
#     The `$`-anchored whole-line parse this replaces handled the first shape
#     (the verdict is simply on the next line) and dropped the second one
#     ENTIRELY: no `ok|FAILED|ignored` follows `... `, and there is no second
#     `test ` token to re-anchor on. A test that PASSED was then reported as a
#     missing id. Measured: one false red in a five-run sweep -- which, for a
#     required context, is a fifth of all PRs turning red for no reason.
# --------------------------------------------------------------------------
REAL_SPLIT_ID=cli::discord_thread_create::tests::required_or_unavailable_tags_fail_after_conclusive_lookup_without_post
REAL_MERGED_ID=cli::discord_thread_create::tests::malformed_archived_has_more_never_posts
REAL_TMP=/var/folders/7h/1gr2yb5933b36t80rxpj60lr0000gn/T
write_manifest alpha::beta "$REAL_SPLIT_ID" "$REAL_MERGED_ID" >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")

# The observed name write ends in a trailing space; keep it rather than let an
# editor strip it, or this stops being the shape that was observed.
INTERLEAVED_REAL_RUN="test alpha::beta ... ok
$REAL_TMP/.tmp1glx3H/agentdesk.plist: OKtest $REAL_SPLIT_ID ...$(printf ' ')
ok
test $REAL_MERGED_ID ... $REAL_TMP/.tmpu2cjsm/agentdesk.plist: OK$REAL_TMP/.tmpC3uuqw/agentdesk.plist: OKok
$(summary_line 3 0 0)"
run_gate "$INTERLEAVED_REAL_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "both observed interleavings must be attributed (rc=$rc): $(grep -E '^(lane-missing|ERROR)' "$OUT" | head -3)"
else pass_test
fi

# --------------------------------------------------------------------------
# 4d. THE DIRECTION THAT MATTERS MORE. The same corruption rides the same
#     parser when the verdict is FAILED, and losing a FAILED is a false GREEN.
#     Identical shape to 4c's merged line, verdict substituted.
# --------------------------------------------------------------------------
FAILED_MERGED_RUN="test alpha::beta ... ok
test $REAL_SPLIT_ID ... ok
test $REAL_MERGED_ID ... $REAL_TMP/.tmpu2cjsm/agentdesk.plist: OK$REAL_TMP/.tmpC3uuqw/agentdesk.plist: OKFAILED

$(failures_block "$REAL_MERGED_ID")

$(summary_line 2 1 0)"
run_gate "$FAILED_MERGED_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a FAILED verdict merged with foreign stdout must still fail the lane"
else pass_test
fi
if ! grep -q "unlisted test failure: $REAL_MERGED_ID" "$OUT"; then
    fail_test "the merged FAILED result must be attributed to its own id, not dropped: $(grep -E '^ERROR' "$OUT" | head -2)"
else pass_test
fi

# The split shape, with the verdict on the following line, must not lose a
# FAILED either -- and foreign text containing `ok` (here inside the tempdir
# name `.tmpok9Z`) must not be read as a verdict on the way past.
FAILED_SPLIT_RUN="test alpha::beta ... ok
test $REAL_SPLIT_ID ... ok
test $REAL_MERGED_ID ... $REAL_TMP/.tmpok9Z/agentdesk.plist: OK
FAILED

$(failures_block "$REAL_MERGED_ID")

$(summary_line 2 1 0)"
run_gate "$FAILED_SPLIT_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a FAILED verdict split onto the next line by foreign stdout must still fail the lane"
else pass_test
fi
if ! grep -q "unlisted test failure: $REAL_MERGED_ID" "$OUT"; then
    fail_test "an 'ok' inside foreign text must not be consumed as the verdict: $(grep -E '^ERROR' "$OUT" | head -2)"
else pass_test
fi

# --------------------------------------------------------------------------
# 4e. The backstop that does NOT depend on the parser being right.
#     Foreign text can end in exactly `ok`, and then no line-shape rule can
#     tell it from a verdict. That residual ambiguity must not be able to turn
#     a failing run green: the parsed sets are cross-checked against libtest's
#     own arithmetic AND against the id set libtest's `failures:` block names,
#     so a failure the parser read as a pass is a disagreement the lane fails
#     on, and the failing id is named.
# --------------------------------------------------------------------------
STOLEN_VERDICT_RUN="test alpha::beta ... ok
test $REAL_SPLIT_ID ... ok
test $REAL_MERGED_ID ... $REAL_TMP/.tmpC3uuqw/agentdesk.plist: ok

$(failures_block "$REAL_MERGED_ID")

$(summary_line 2 1 0)"
run_gate "$STOLEN_VERDICT_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a failure the parser mis-read as a pass must still fail the lane"
else pass_test
fi
if ! grep -q 'parsed 0 failing ids but libtest reported 1 failed' "$OUT"; then
    fail_test "the parse must be cross-checked against libtest's own failure count: $(grep -E '^ERROR' "$OUT" | head -2)"
else pass_test
fi
if ! grep -q "^failure-unattributed .*: $REAL_MERGED_ID" "$OUT"; then
    fail_test "the failure libtest named must be reported by id, not only as a count: $(grep -E '^ERROR' "$OUT" | head -2)"
else pass_test
fi

# --------------------------------------------------------------------------
# 4e-1. ATTACK 1 -- TWO MIS-READS THAT CANCEL.
#     This is the false green that was actually produced against the previous
#     round of this file, and it is why the three "cannot produce a false
#     green" sentences in this repository were rewritten.
#
#     The ledger below carries `always | alpha::gamma`. The truth is the
#     reverse of what the transcript parses to: alpha::beta FAILED (unlisted,
#     must be red) and alpha::gamma passed (a stale `always` entry, also red).
#     Foreign text ending in `ok` steals beta's verdict, foreign text ending in
#     `FAILED` steals gamma's, and the real verdicts land alone on the next
#     line where nothing is pending. `executed`, `failed` and `selected` are
#     all COUNTS, so the two mis-reads cancel and every one of them agrees:
#     measured rc=0, unexpected=0, stale=0.
#
#     Only a SET comparison distinguishes the two readings, which is what
#     libtest's own `failures:` block supplies.
# --------------------------------------------------------------------------
cat >"$LEDGER" <<'EOF'
always | alpha::gamma | lane=demo | #5185 | isolated while the owning slice lands its fix
EOF
write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
CANCELLING_MISREAD_RUN="test alpha::beta ... $REAL_TMP/.tmpQQyyzz/agentdesk.plist: ok
FAILED
test alpha::gamma ... $REAL_TMP/.tmpZZxxww/relay.lock: FAILED
ok

$(failures_block alpha::beta)

$(summary_line 1 1 0)"
run_gate "$CANCELLING_MISREAD_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "two mis-reads that cancel in every count must not produce a green lane; only the failure-id set separates them"
else pass_test
fi
if ! grep -q '^failure-unattributed .*: alpha::beta' "$OUT"; then
    fail_test "the failure libtest named but the parse missed must be named: $(grep -E '^ERROR' "$OUT" | head -3)"
else pass_test
fi
if ! grep -q '^failure-misattributed .*: alpha::gamma' "$OUT"; then
    fail_test "the id the parse invented a failure for must be named: $(grep -E '^ERROR' "$OUT" | head -3)"
else pass_test
fi

# The same shape with the mis-read removed must still pass, so the check is not
# simply always red on a transcript that carries foreign text.
HONEST_MISREAD_CONTROL="test alpha::beta ... ok
test alpha::gamma ... $REAL_TMP/.tmpZZxxww/relay.lock: FAILED

$(failures_block alpha::gamma)

$(summary_line 1 1 0)"
run_gate "$HONEST_MISREAD_CONTROL" 101 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "a ledgered failure whose id set agrees with libtest must still pass (rc=$rc): $(grep -E '^ERROR' "$OUT" | head -3)"
else pass_test
fi

# A transcript that reports failures but carries no `failures:` block at all
# cannot be adjudicated: the ledger is applied per id, and there is no
# independent list of ids to apply it to.
NO_FAILURES_BLOCK_RUN="test alpha::beta ... ok
test alpha::gamma ... FAILED
$(summary_line 1 1 0)"
run_gate "$NO_FAILURES_BLOCK_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a summary reporting failures with no failures: block must fail the lane"
else pass_test
fi
if ! grep -q 'no .failures:. block naming them could be recovered' "$OUT"; then
    fail_test "the missing failures: block must be reported: $(grep -E '^ERROR' "$OUT" | head -2)"
else pass_test
fi

write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")

# --------------------------------------------------------------------------
# 4e-2. THE OTHER HALF OF THE CROSS-VALIDATION.
#     `evaluate` cross-checks TWO quantities against libtest's summary: the
#     failing-id count and the executed-id count. The failing half was pinned
#     above; without this assertion the executed half was unpinned, and
#     deleting it left the suite fully green (measured: mutation (e),
#     SURVIVED 59/59).
#
#     The transcript below reports both ids as passes while libtest's summary
#     counts one of them as ignored. Selection identity is satisfied (both ids
#     reported), the ignore-identity checks are satisfied (the parse saw no
#     ignored result and the ledger declares none), the failing-id set is empty
#     on both sides and `selected` matches. Only `parsed N executed ids but
#     libtest reported M` separates this transcript from an honest one.
# --------------------------------------------------------------------------
cat >"$LEDGER" <<'EOF'
# no entries
EOF
EXECUTED_COUNT_MISMATCH_RUN="test alpha::beta ... ok
test alpha::gamma ... ok
$(summary_line 1 0 1)"
run_gate "$EXECUTED_COUNT_MISMATCH_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a transcript that reports more executed ids than libtest counted must fail the lane"
else pass_test
fi
if ! grep -q 'executed ids but libtest reported' "$OUT"; then
    fail_test "the executed half of the cross-validation must name both counts: $(grep -E '^ERROR' "$OUT" | head -3)"
else pass_test
fi

# --------------------------------------------------------------------------
# 4f. Two libtest results merged onto one line.
#     Unlike §4c/§4d these two transcripts are composed rather than copied:
#     the writer that interleaves in this repository's sweeps is the
#     launchd-plist helper, and a thread-against-thread merge has not been
#     observed here. It is the same two-write mechanism though -- libtest
#     emits `test <id> ... ` and the verdict as separate writes onto one
#     shared descriptor -- so a parse that matches ONE result per line drops
#     the first id whenever it happens. That is why the parser iterates
#     fragments instead of matching once, and this is where that is pinned.
# --------------------------------------------------------------------------
MERGED_RESULTS_RUN="test alpha::beta ... test alpha::gamma ... okok
$(summary_line 2 0 0)"
run_gate "$MERGED_RESULTS_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "two results merged onto one line must both be attributed (rc=$rc): $(grep -E '^(lane-missing|ERROR)' "$OUT" | head -2)"
else pass_test
fi

# ...and the verdicts must go to the right ids. libtest writes a name and its
# verdict back to back from one thread, so the verdict that follows the LAST
# name on the line belongs to that name, not to the first one. Getting this
# backwards attributes the FAILED to alpha::beta -- a green lane for the test
# that actually failed if alpha::beta happens to be in the ledger.
MERGED_FAILURE_RUN="test alpha::beta ... test alpha::gamma ... FAILED
ok

$(failures_block alpha::gamma)

$(summary_line 1 1 0)"
run_gate "$MERGED_FAILURE_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a FAILED merged onto a shared line must fail the lane"
else pass_test
fi
if ! grep -q 'unlisted test failure: alpha::gamma' "$OUT"; then
    fail_test "the merged FAILED belongs to the last name on the line: $(grep -E '^ERROR' "$OUT" | head -2)"
else pass_test
fi

# ATTACK 2 -- the LIFO choice made wrong, in bytes identical to the transcript
# above. `test A ... test B ... <verdict>` cannot come from ONE libtest process
# (one console thread writes a name and its verdict back to back, so a process
# never leaves two names pending); it needs the parent and its nested child
# both writing to the inherited descriptor, and then W1-name/W2-name/
# W2-verdict/W1-verdict and W1-name/W2-name/W1-verdict/W2-verdict emit exactly
# the same bytes. LIFO fixes one reading as the answer; here the other one is
# true -- alpha::beta FAILED (unlisted) and alpha::gamma passed (a stale
# `always` entry). Both are red, and the previous round returned rc=0.
cat >"$LEDGER" <<'EOF'
always | alpha::gamma | lane=demo | #5185 | isolated while the owning slice lands its fix
EOF
MERGED_FAILURE_SWAPPED_RUN="test alpha::beta ... test alpha::gamma ... FAILED
ok

$(failures_block alpha::beta)

$(summary_line 1 1 0)"
run_gate "$MERGED_FAILURE_SWAPPED_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a wrong LIFO attribution on an ambiguous merged line must fail the lane, not hand the failure to a ledgered id"
else pass_test
fi
if ! grep -q '^failure-unattributed .*: alpha::beta' "$OUT"; then
    fail_test "the id libtest actually named as failing must be reported: $(grep -E '^ERROR' "$OUT" | head -3)"
else pass_test
fi
cat >"$LEDGER" <<'EOF'
# no entries
EOF

# ATTACK 2b -- the `failures:` block parsed as a SET, not as its first element.
# Every other assertion in this file names exactly ONE id in the block, so a
# parser that read only the first entry and discarded the rest satisfied all of
# them: measured, that mutation SURVIVED the whole suite. The block is the only
# evidence the gate has for WHICH ids failed, so a truncating read silently
# turns every failure after the first into `failure-misattributed` -- or, with
# the counts still matching, into no finding at all.
cat >"$LEDGER" <<'EOF'
always | alpha::beta | lane=demo | #5185 | isolated while the owning slice lands its fix
always | alpha::gamma | lane=demo | #5185 | isolated while the owning slice lands its fix
EOF
TWO_FAILURES_RUN="test alpha::beta ... FAILED
test alpha::gamma ... FAILED

$(failures_block alpha::beta alpha::gamma)

$(summary_line 0 2 0)"
run_gate "$TWO_FAILURES_RUN" 101 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "two ledgered failures named in one failures: block must leave the lane green (rc=$rc): $(grep -E '^(failure-|ERROR)' "$OUT" | head -3)"
else pass_test
fi
if ! grep -q 'declared-failed[^0-9]*2' "$OUT"; then
    fail_test "both ids in the block must be declared, not just the first: $(grep -i 'declared-failed' "$OUT" | head -2)"
else pass_test
fi
# The same block with a THIRD id libtest named but the transcript never
# reported: the missing side of the set difference must be caught too, which a
# first-element read cannot see once the first element matches.
write_manifest alpha::beta alpha::gamma alpha::delta >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
cat >"$LEDGER" <<'EOF'
always | alpha::beta | lane=demo | #5185 | isolated while the owning slice lands its fix
always | alpha::gamma | lane=demo | #5185 | isolated while the owning slice lands its fix
EOF
LOST_VERDICT_RUN="test alpha::beta ... FAILED
test alpha::gamma ... FAILED
test alpha::delta ... ok

$(failures_block alpha::beta alpha::gamma alpha::delta)

$(summary_line 0 3 0)"
run_gate "$LOST_VERDICT_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "an id libtest named as failing but the transcript parsed as passing must fail the lane"
else pass_test
fi
if ! grep -q '^failure-unattributed .*: alpha::delta' "$OUT"; then
    fail_test "the third id in the block must be adjudicated, not dropped after the first: $(grep -E '^(failure-|ERROR)' "$OUT" | head -3)"
else pass_test
fi
write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
cat >"$LEDGER" <<'EOF'
# no entries
EOF

# --------------------------------------------------------------------------
# 5. A failure that is not in the ledger must fail the lane.
# --------------------------------------------------------------------------
FAILING_RUN="test alpha::beta ... ok
test alpha::gamma ... FAILED

$(failures_block alpha::gamma)

$(summary_line 1 1 0)"
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a failure absent from the ledger must fail the lane"
else pass_test
fi
if ! grep -q 'unlisted test failure: alpha::gamma' "$OUT"; then
    fail_test "the unlisted failure must be named"
else pass_test
fi

cat >"$LEDGER" <<'EOF'
always | alpha::gamma | lane=demo | #5185 | isolated while the owning slice lands its fix
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "a ledger-listed failure must not fail the lane (rc=$rc)"
else pass_test
fi
run_gate "$FAILING_RUN" 101 --lane other
if [ "$rc" -eq 0 ]; then
    fail_test "a ledger entry must not apply to a lane it does not name"
else pass_test
fi

# --------------------------------------------------------------------------
# 6. Listed-but-passing must be reported, so isolation cannot quietly rot.
# --------------------------------------------------------------------------
run_gate "$PASSING_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "an always-entry whose tests all pass must fail as stale"
else pass_test
fi
if ! grep -q 'stale ledger entry: alpha::gamma' "$OUT"; then
    fail_test "the stale entry must be named so it can be deleted"
else pass_test
fi

# --------------------------------------------------------------------------
# 7. `flaky` isolation is per-id and expires.
#    A module-prefix flaky entry was measured swallowing an injected,
#    unrelated regression in a sibling test of the isolated module (rc=0).
# --------------------------------------------------------------------------
cat >"$LEDGER" <<'EOF'
flaky | alpha | lane=demo | #5185 | shares a process-global recorder so the failing member rotates expires=2999-01-01
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a module-prefix flaky entry must not cover a failing member; module-wide isolation swallows unrelated regressions"
else pass_test
fi

cat >"$LEDGER" <<'EOF'
flaky | alpha::gamma | lane=demo | #5185 | shares a process-global recorder so the failing member rotates expires=2999-01-01
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "an exact-id flaky entry must cover its own test (rc=$rc)"
else pass_test
fi
run_gate "$PASSING_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "a passing flaky entry must warn rather than fail (rc=$rc)"
else pass_test
fi
if ! grep -q '::warning::known-flaky entry passed: alpha::gamma' "$OUT"; then
    fail_test "a passing flaky entry must still be announced"
else pass_test
fi

# Expiry pressure: nothing else can ever make a flaky entry expire, because it
# passes silently by construction.
cat >"$LEDGER" <<'EOF'
flaky | alpha::gamma | lane=demo | #5185 | shares a process-global recorder so the failing member rotates
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a flaky entry without expires= must be rejected"
else pass_test
fi
cat >"$LEDGER" <<'EOF'
flaky | alpha::gamma | lane=demo | #5185 | shares a process-global recorder so the failing member rotates expires=2000-01-01
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "an expired flaky entry must fail the lane"
else pass_test
fi
if ! grep -q 'expired on 2000-01-01' "$OUT"; then
    fail_test "the expired entry must name its expiry date"
else pass_test
fi

# --------------------------------------------------------------------------
# 8. Debt without a tracking issue or a real reason is not admissible.
#    A bare length check passes `aaaaaaaaaaaa`, which is not a reason.
# --------------------------------------------------------------------------
cat >"$LEDGER" <<'EOF'
always | alpha::gamma | lane=demo | none | isolated while the owning slice lands its fix
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a ledger entry without a tracking issue must be rejected"
else pass_test
fi

# It is the REASON that fails this entry, not the `#1`. The issue field is a
# format check with no way to verify that the number names a real issue, so
# `#1` with a substantive reason is admissible; the pressure on an entry comes
# from the prose, the `always` stale check and the `flaky` expiry instead.
cat >"$LEDGER" <<'EOF'
always | alpha::gamma | lane=demo | #1 | aaaaaaaaaaaa
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "'#1 | aaaaaaaaaaaa' must be rejected: length alone is not a reason"
else pass_test
fi

cat >"$LEDGER" <<'EOF'
always | alpha::gamma | lane=demo | #5185 | short
EOF
run_gate "$FAILING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a ledger entry without a real reason must be rejected"
else pass_test
fi

# --------------------------------------------------------------------------
# 9. A command that dies without reporting a test failure is not a green lane.
# --------------------------------------------------------------------------
cat >"$LEDGER" <<'EOF'
# no entries
EOF
run_gate "$PASSING_RUN" 101 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "a nonzero exit with no reported test failure must fail the lane"
else pass_test
fi

# --------------------------------------------------------------------------
# 10. The adjudicator must be told the same selection the command executes.
#     Narrowing the lane by adding a --skip the adjudicator does not know
#     about would otherwise shrink the expectation to fit whatever ran.
# --------------------------------------------------------------------------
rc=0
"$PYTHON" "$RUNNER" --ledger "$LEDGER" --inventory-manifest "$MANIFEST" \
    --lane demo --skip _pg -- \
    bash -c 'printf "%s\n" "$0"' "$PASSING_RUN" --skip turn_bridge >"$OUT" 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "an undeclared --skip in the command must fail before the lane runs"
else pass_test
fi
if ! grep -q 'does not match the command' "$OUT"; then
    fail_test "the skip mismatch must name both sides"
else pass_test
fi

# --------------------------------------------------------------------------
# 11. The shipped ledger and manifest must parse, and the shipped lane must be
#     wired to the gate with the exact skip set the adjudicator is told about.
#     Pinning the skip set here is what makes "declare the narrowing on both
#     sides" a test-breaking change rather than a quiet one.
# --------------------------------------------------------------------------
rc=0
"$PYTHON" - "$REPO_ROOT/scripts" "$REAL_LEDGER" "$REAL_MANIFEST" >"$OUT" 2>&1 <<'PY' || rc=$?
import datetime, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from run_test_lane import parse_ledger, derive_expected_selection
entries, errors = parse_ledger(Path(sys.argv[2]), "non-pg-sweep", datetime.date.today())
for error in errors:
    print(f"ledger error: {error}")
expected, manifest_errors = derive_expected_selection(
    Path(sys.argv[3]), sys.platform, ["_pg", "pg_", "postgres"])
for error in manifest_errors:
    print(f"manifest error: {error}")
if errors or manifest_errors:
    sys.exit(1)
print(f"shipped ledger entries={len(entries)} derived-selection={len(expected)}")
PY
if [ "$rc" -ne 0 ]; then
    fail_test "the shipped ledger and inventory manifest must parse cleanly (rc=$rc): $(cat "$OUT")"
else pass_test
fi

# --------------------------------------------------------------------------
# 11b. The three Linux-only failures that the `library_sweep` job exposed must
#      be absorbed by the shipped ledger, and absorbed as `flaky`. Both
#      directions matter and both are pinned here. If an id stops matching an
#      entry, the job goes red with `unexpected` and the lane cannot land. If
#      one were recorded as `always` instead, the stale check would turn a run
#      where they PASS into a hard lane failure -- and determinism has not been
#      established for any of the three, so that run is expected to happen.
# --------------------------------------------------------------------------
EXPOSED_IDS='services::discord::jsonl_watcher::tests::notify_fires_when_file_modified
services::discord::jsonl_watcher::tests::jsonl_watcher_notifies_on_dead_marker_create
services::platform::tmux::live_pane_tests::dead_marker_hook_writes_marker_on_pane_exit'

rc=0
"$PYTHON" - "$REPO_ROOT/scripts" "$REAL_LEDGER" "$EXPOSED_IDS" >"$OUT" 2>&1 <<'PY' || rc=$?
import datetime, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from run_test_lane import parse_ledger

today = datetime.date.today()
entries, errors = parse_ledger(Path(sys.argv[2]), "non-pg-sweep", today)
for error in errors:
    print(f"ledger error: {error}")
if errors:
    sys.exit(1)
failures = []
for test_id in sys.argv[3].splitlines():
    matched = [e for e in entries if e.mode != "ignored" and e.matches(test_id)]
    if len(matched) != 1:
        failures.append(f"{test_id}: expected exactly one entry, got {len(matched)}")
        continue
    entry = matched[0]
    if entry.mode != "flaky":
        failures.append(f"{test_id}: expected mode flaky, got {entry.mode}")
    if entry.pattern != test_id:
        failures.append(f"{test_id}: entry must name the exact id, got {entry.pattern}")
    if entry.expires is None or entry.expires <= today:
        failures.append(f"{test_id}: flaky entry needs a future expiry, got {entry.expires}")
for failure in failures:
    print(failure)
if failures:
    sys.exit(1)
print("library_sweep linux exposures absorbed as flaky=3")
PY
if [ "$rc" -ne 0 ]; then
    fail_test "the ledger must absorb the three Linux-only library_sweep exposures as flaky (rc=$rc): $(cat "$OUT")"
else pass_test
fi

# --------------------------------------------------------------------------
# 11c. Those three entries must keep saying WHY they are admissible. They are
#      the first entries this repository admits on the strength of "the lane
#      had never run them", and that basis is only checkable if the entry
#      names the evidence. An entry rewritten to claim they were base-red
#      would be the overclaim this whole change exists to close: there is no
#      prior Linux verdict for them to pre-exist, and the ledger would then be
#      resting on a false one.
# --------------------------------------------------------------------------
rc=0
"$PYTHON" - "$REPO_ROOT/scripts" "$REAL_LEDGER" "$EXPOSED_IDS" >"$OUT" 2>&1 <<'PY' || rc=$?
import datetime, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from run_test_lane import parse_ledger

entries, errors = parse_ledger(Path(sys.argv[2]), "non-pg-sweep", datetime.date.today())
if errors:
    for error in errors:
        print(f"ledger error: {error}")
    sys.exit(1)
by_pattern = {entry.pattern: entry for entry in entries}
failures = []
for test_id in sys.argv[3].splitlines():
    entry = by_pattern.get(test_id)
    if entry is None:
        failures.append(f"{test_id}: no entry naming this exact id")
        continue
    if "newly exposed" not in entry.reason:
        failures.append(f"{test_id}: reason must state that it is newly exposed")
    if "31465b184" not in entry.reason:
        failures.append(f"{test_id}: reason must cite the main commit that was checked")
    if entry.issue != "#5217":
        failures.append(f"{test_id}: expected tracking issue #5217, got {entry.issue}")
for failure in failures:
    print(failure)
if failures:
    sys.exit(1)
print("library_sweep linux exposures carry their provenance=3")
PY
if [ "$rc" -ne 0 ]; then
    fail_test "the three exposure entries must cite why they are admissible (rc=$rc): $(cat "$OUT")"
else pass_test
fi

# A substring match is not a pin. `--skip postgres --` is a prefix of
# `--skip postgres --skip turn_bridge`, so the `grep -q` that used to stand
# here accepted a lane narrowed by an extra skip -- the exact evasion the
# assertion claimed to block. Compare the whole command line instead, as a
# fixed string, with the YAML block indentation normalised away.
LANE_COMMAND='python3 scripts/run_test_lane.py --lane non-pg-sweep --max-summaries 2 "${NON_PG_SKIP_ARGS[@]}" -- env -u AGENTDESK_ROOT_DIR cargo test --lib -- "${NON_PG_SKIP_ARGS[@]}"'
# `grep -q` exits the moment it matches, which closes the pipe while `sed`
# still has the rest of the file to write. GNU sed reports that as
# "couldn't flush stdout: Broken pipe" and exits non-zero; under the
# `set -o pipefail` above, that non-zero becomes the pipeline's status and
# this assertion fails *because the pin was found*. BSD sed on the
# development machines finished writing first, so the suite was green
# locally and red on ubuntu-latest. Dropping `-q` makes grep read to EOF,
# so sed always completes and the status reflects the match alone.
if ! sed 's/^[[:space:]]*//' "$WORKFLOW" | grep -xF "$LANE_COMMAND" >/dev/null; then
    fail_test "ci-pr.yml must run the non-pg-sweep lane through the gate with exactly the pinned command"
else pass_test
fi

# --------------------------------------------------------------------------
# 12. Prose in the workflow must not name flags that do not exist.
#     `--min-executed` was deleted from the wrapper in this round and a
#     sentence describing it survived twenty lines above another comment that
#     states the opposite -- two comments in one file contradicting each
#     other. The guard that used to stand here missed it because it only
#     matched lines that ALSO mention `run_test_lane.py`, and the stale
#     sentence did not. So check every `--flag` token in the whole
#     library_sweep region against the flags that actually exist.
# --------------------------------------------------------------------------
rc=0
"$PYTHON" - "$REPO_ROOT/scripts" "$WORKFLOW" >"$OUT" 2>&1 <<'PY' || rc=$?
import re
import sys
from pathlib import Path

scripts, workflow = Path(sys.argv[1]), Path(sys.argv[2])
text = workflow.read_text(encoding="utf-8")

# The region is the `library_sweep` job plus the comment block introducing it.
job = text.index("\n  library_sweep:\n") + 1
lines = text[:job].splitlines(keepends=True)
start = len(text[:job])
while lines and lines[-1].lstrip().startswith("#"):
    start -= len(lines.pop())
end = text.index("\n  library_sweep_required_context:\n")
region = text[start:end]

wrapper = (scripts / "run_test_lane.py").read_text(encoding="utf-8")
declared = set(re.findall(r'add_argument\(\s*"(--[a-z][a-z0-9-]*)"', wrapper))
if not declared:
    print("could not read the wrapper's own option list")
    raise SystemExit(1)
# Flags belonging to the tools the region invokes rather than to the wrapper.
#
# MEASURED LIMIT OF THIS GUARD: the allowlist is a literal in the same file as
# the assertion, so anyone who resurrects a deleted wrapper flag in the
# workflow prose can also add it here and the guard reports nothing. Adding
# `--min-executed` to this set was measured turning this check green again.
# That is an edit to the oracle rather than to the code under test, so it is
# not a mutation this suite can be expected to kill -- but it is the one line
# a reviewer must read before trusting the check.
external = {"--lib", "--skip", "--test-threads", "--exact", "--all-targets",
            "--all-features", "--manifest-path", "--list", "--show-stats"}
used = set(re.findall(r"--[a-z][a-z0-9-]*", region))
unknown = sorted(used - declared - external)
if unknown:
    print("the library_sweep region names flag(s) that do not exist: "
          + ", ".join(unknown))
    raise SystemExit(1)
print("library_sweep region flags: " + " ".join(sorted(used)))
PY
if [ "$rc" -ne 0 ]; then
    fail_test "$(cat "$OUT")"
else pass_test
fi

# --------------------------------------------------------------------------
# 13. `--platform` must reach the expectation derivation.
#     The static-only set is reviewed, platform-specific data (#5144): four
#     macOS-only tests are `#[cfg]`-absent on Linux and one Linux-only test is
#     absent on darwin, so the two platforms derive DIFFERENT id sets from the
#     same manifest. Nothing else in this file constrains the parameter, and
#     the lane runs on ubuntu-latest while every machine that develops it is
#     darwin: a refactor pinning the derivation to the host would stay green
#     everywhere it is run by hand and make the required context permanently
#     red (missing 4, extra 1) the first time it runs on a runner.
# --------------------------------------------------------------------------
rc=0
"$PYTHON" - "$REPO_ROOT/scripts" "$REAL_MANIFEST" >"$OUT" 2>&1 <<'PY' || rc=$?
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from run_test_lane import derive_expected_selection

manifest = Path(sys.argv[2])
skips = ["_pg", "pg_", "postgres"]
darwin, darwin_errors = derive_expected_selection(manifest, "darwin", skips)
linux, linux_errors = derive_expected_selection(manifest, "linux", skips)
if darwin_errors or linux_errors:
    print(f"derivation failed: {darwin_errors} {linux_errors}")
    raise SystemExit(1)
if not darwin or not linux:
    print("a platform derived an empty selection")
    raise SystemExit(1)
if darwin == linux:
    print("darwin and linux derived the same id set; --platform is not "
          "reaching the derivation")
    raise SystemExit(1)
print(f"darwin={len(darwin)} linux={len(linux)} "
      f"darwin-only={len(darwin - linux)} linux-only={len(linux - darwin)}")
PY
if [ "$rc" -ne 0 ]; then
    fail_test "the two supported platforms must derive different selections: $(cat "$OUT")"
else pass_test
fi

# End to end through the CLI: a transcript that is exactly right for Linux
# must pass under `--platform linux` and fail under `--platform darwin`.
PLATFORM_IDS="$TMP_ROOT/platform-ids.txt"
"$PYTHON" - "$REPO_ROOT/scripts" "$MANIFEST" "$PRELUDE" "$PLATFORM_IDS" >/dev/null <<'PY'
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from check_test_target_integrity import (
    LIB_INVENTORY_KNOWN_CARGO_ONLY, expected_lib_static_only,
    render_lib_inventory_manifest,
)
manifest_path, prelude_path, ids_path = (Path(arg) for arg in sys.argv[2:5])
darwin_only = expected_lib_static_only("darwin")
linux_only = expected_lib_static_only("linux")
cargo_only = set(LIB_INVENTORY_KNOWN_CARGO_ONLY)
manifest_ids = {"alpha::beta"} | cargo_only | darwin_only | linux_only
manifest_path.write_text(render_lib_inventory_manifest(manifest_ids),
                         encoding="utf-8")
prelude_path.write_text(
    "".join(f"test {name} ... ok\n" for name in sorted(cargo_only)),
    encoding="utf-8")
# What a Linux runner must report: the manifest minus what does not compile
# on Linux. The prelude already carries the cargo-only ids.
ids_path.write_text(
    "".join(f"{name}\n"
            for name in sorted((manifest_ids - linux_only) - cargo_only)),
    encoding="utf-8")
PY
EXTRA=$(grep -c '' "$PRELUDE")
LINUX_COUNT=$(grep -c '' "$PLATFORM_IDS")
LINUX_RUN="$( { while read -r name; do
    printf 'test %s ... ok\n' "$name"
done <"$PLATFORM_IDS"; } )
$(summary_line "$LINUX_COUNT" 0 0)"

run_gate "$LINUX_RUN" 0 --lane demo --platform linux
if [ "$rc" -ne 0 ]; then
    fail_test "the Linux selection must pass under --platform linux (rc=$rc): $(grep -E '^(lane-missing|lane-extra|ERROR)' "$OUT" | head -3)"
else pass_test
fi
run_gate "$LINUX_RUN" 0 --lane demo --platform darwin
if [ "$rc" -eq 0 ]; then
    fail_test "the Linux selection must NOT satisfy --platform darwin; the two platforms compile different test sets"
else pass_test
fi
if ! grep -q '^lane-missing' "$OUT" || ! grep -q '^lane-extra' "$OUT"; then
    fail_test "the platform mismatch must name both sides of the difference"
else pass_test
fi
run_gate "$LINUX_RUN" 0 --lane demo --platform freebsd
if [ "$rc" -eq 0 ]; then
    fail_test "an unsupported --platform must fail; the static-only set is reviewed data and cannot be guessed"
else pass_test
fi

# --------------------------------------------------------------------------
# 4g. THE BOUNDARY CASE CLOSED BY THIS ROUND (#5227).
#     The lookahead `(?=ok|FAILED|ignored|\W|$)` keeps the two merged-write
#     forms (`okok` and `OKok`) while refusing `okhttp:` and similar false
#     greens where `ok` is followed by a word character (part of an identifier).
#
#     The first two cases (`okok` and `OKok`) are already tested above in §4c
#     and §4f; they are listed here for completeness. New cases that were NOT
#     tested before and that the lookahead closes are added below.
# --------------------------------------------------------------------------

# Test 4g-1: The lookahead must reject ok followed by word characters.
#     `okhttp:` starts with ok but continues with word characters, so the
#     ok at the start is not a verdict.
write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
cat >"$LEDGER" <<'EOF'
# no entries
EOF

OKHTTP_RUN="test alpha::beta ... ok
test alpha::gamma ... okhttp://example.com
$(summary_line 1 0 0)"
run_gate "$OKHTTP_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "okhttp: prefix starting with ok must not be consumed as a verdict"
else pass_test
fi

# Test 4g-2: The lookahead must also reject FAILED and ignored with word chars.
write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
FAILEDBAR_RUN="test alpha::beta ... ok
test alpha::gamma ... FAILED_upload_error
$(summary_line 1 0 0)"
run_gate "$FAILEDBAR_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "FAILED_something must not be consumed as a verdict"
else pass_test
fi

# Test 4g-3: The lookahead ALLOWS ok/FAILED/ignored when followed by
#     non-word characters or another verdict word (the whole point).
write_manifest alpha::beta alpha::gamma >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
LOOKAHEAD_ALLOW_RUN="test alpha::beta ... test alpha::gamma ... okok
$(summary_line 2 0 0)"
run_gate "$LOOKAHEAD_ALLOW_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "lookahead must allow okok (two verdicts): $(grep -E '^(lane-missing|ERROR)' "$OUT" | head -1)"
else pass_test
fi

# --------------------------------------------------------------------------
# 4h. THE END ANCHOR MUST REMAIN FIXED (regression test for r1 review).
#     VERDICT_AT_END uses `$` anchor to refuse mid-segment matches. The `$`
#     is structural: without it, re.search() picks leftmost `ok` in the
#     segment, not the last one, and foreign text at segment start shadows
#     the real verdict at the end. All forms below must preserve this
#     property: verdicts at the end are matched, verdicts in the middle or
#     at the start of foreign text are not.
# --------------------------------------------------------------------------

# Test 4h-1: Foreign text with `ok` followed by FAILED must let FAILED win.
#     Regression: r1 leftmost `ok` in `/Users/ok-dev/cache:` stole FAILED.
write_manifest alpha::beta >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
cat >"$LEDGER" <<'EOF'
always | alpha::beta | lane=demo | #5227 | regression test: mid-segment ok must not shadow end FAILED
EOF

FOREIGN_OK_THEN_FAILED_RUN="test alpha::beta ... /Users/ok-dev/cache: FAILED
$(summary_line 0 1 0)

$(failures_block alpha::beta)
"
run_gate "$FOREIGN_OK_THEN_FAILED_RUN" 101 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "FAILED at end must override foreign ok in the middle (rc=$rc)"
else pass_test
fi

# Test 4h-2: Foreign text with `ok` in middle, no real verdict, must stay missing.
#     Regression: r1 END leftmost invented `ok` from `/tmp/ok-cache/x`.
write_manifest alpha::beta >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
cat >"$LEDGER" <<'EOF'
# no entries
EOF

FOREIGN_OK_NO_VERDICT_RUN="test alpha::beta ... /tmp/ok-cache/x
$(summary_line 0 0 0)"
run_gate "$FOREIGN_OK_NO_VERDICT_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "mid-segment ok in foreign text must not be invented as a verdict"
else pass_test
fi

# Test 4h-3: Summary line with pending id must not supply the verdict.
#     When a test name has no verdict on its line, pending remains until END
#     can supply it. The summary line has no NAME_FRAGMENT so drain_verdicts
#     sees it as a segment. Under the original broken r1 regex (without END
#     anchor), `ok.` at end-of-line would match. With the fixed END anchor,
#     it must not. Since alpha::beta has no verdict and summary is not one,
#     beta must be reported as lane-missing.
write_manifest alpha::beta >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
cat >"$LEDGER" <<'EOF'
# no entries
EOF

SUMMARY_NO_VERDICT_RUN="test alpha::beta ... /tmp/error-ok-cache
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
run_gate "$SUMMARY_NO_VERDICT_RUN" 0 --lane demo
if [ "$rc" -eq 0 ]; then
    fail_test "pending id with no END match should fail lane-missing; summary line is not a verdict source"
else pass_test
fi
if ! grep -q 'lane-missing' "$OUT"; then
    fail_test "beta should be lane-missing since neither line supplies a verdict"
else pass_test
fi

# Test 4h-4: Foreign text ending differently (message/warning), real verdict follows.
#     Verify that `error: FAILED-to-open` is NOT parsed as FAILED verdict on
#     the first line, but the second line `ok` is properly parsed.
write_manifest alpha::beta >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
cat >"$LEDGER" <<'EOF'
# no entries
EOF

FOREIGN_FAILED_THEN_OK_RUN="test alpha::beta ... error: FAILED-to-open /tmp/x, retrying
ok
$(summary_line 1 0 0)"
run_gate "$FOREIGN_FAILED_THEN_OK_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "foreign FAILED-to-open must not be parsed; next line ok must complete (rc=$rc): $(grep -E '^(lane-missing|ERROR)' "$OUT" | head -1)"
else pass_test
fi

# Test 4h-5: Foreign text ending with `ignored`, real verdict follows.
write_manifest alpha::beta >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
FOREIGN_IGNORED_THEN_OK_RUN="test alpha::beta ... [warn] flag ignored, using default
ok
$(summary_line 1 0 0)"
run_gate "$FOREIGN_IGNORED_THEN_OK_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "foreign ignored in message must not be parsed; next line ok must complete (rc=$rc): $(grep -E '^(lane-missing|ERROR)' "$OUT" | head -1)"
else pass_test
fi

# Test 4h-6: OKok (END anchor must still match at segment end even with AT_START lookahead).
#     This pins that the END anchor remains fixed and still matches final ok.
write_manifest alpha::beta >/dev/null
EXTRA=$(grep -c '' "$PRELUDE")
OKOBJECT_END_OKOK_RUN="test alpha::beta ... /var/folders/tmp/agentdesk.plist: OKok
$(summary_line 1 0 0)"
run_gate "$OKOBJECT_END_OKOK_RUN" 0 --lane demo
if [ "$rc" -ne 0 ]; then
    fail_test "OKok at end must parse the final ok (rc=$rc): $(grep -E '^(lane-missing|ERROR)' "$OUT" | head -1)"
else pass_test
fi

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_run_test_lane_5185: $failures assertion(s) failed, $passed passed" >&2
    exit 1
fi

printf '%s\n' "test_run_test_lane_5185: all assertions passed (passed=$passed)"
