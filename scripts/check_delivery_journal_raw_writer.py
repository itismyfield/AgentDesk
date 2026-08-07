#!/usr/bin/env python3
from __future__ import annotations

import re
import subprocess
import sys
from collections import Counter
from pathlib import Path
SYMBOL = "append_delivery_journal_batch"
# `CALL`/`call_sites()` use a cheap lexical text match, not Rust parsing: each
# line has only the suffix after `//` removed. Therefore symbols in block
# comments (`/* */`, `/** */`) and string literals are counted as calls and can
# make harmless text produce a false-red; line comments are excluded. A failure
# is intentionally loud and self-explanatory (it prints the filename and count)
# so the cause is immediately visible. This is a monotonic guard against new
# raw writers outside `journal.rs`, not exact call analysis.
CALL = re.compile(rf"\b{SYMBOL}\s*\(")
ALLOWLIST = Counter({"src/services/discord/session_relay_sink/journal.rs": 1})
BASELINE = 1
FAMILY_REGISTRY = (
    ("fresh sink vertical slice", "src/services/discord/session_relay_sink/task_notification_context.rs", "deliver_new_message_with_task_authority"),
    ("sink direct family (referenced / edit / split / long-chunk receipt)", "src/services/discord/session_relay_sink.rs", "deliver_response"),
    ("watcher terminal family (무전송 5곳 포함)", "src/services/discord/tmux_watcher.rs", "tmux_output_watcher_with_restore"),
    ("turn_bridge / controller family", "src/services/discord/turn_bridge/terminal_controller_cutover.rs", "deliver_short_replace_via_controller"),
    ("recovery / fresh-send / orphan family", "src/services/discord/recovery_engine/terminal_text_idempotency.rs", "record_successful_fresh_send"),
)
# Cheap lexical text match, not Rust parsing: each complete anchor file, including
# tests, is scanned with only the suffix after // on that line removed. The scan
# deliberately stays file-wide: lexical brace balancing cannot honestly bound a
# Rust fn body when strings and macros may contain braces.
# Strings, block comments (/* */ and /** */), raw strings, macros, and test-area
# text count; line comments (including /// and //! doc comments) do not. The
# result is a monotonic baseline signal, not proof of instrumentation.
#
# #5071 T1 S2 extension of that declaration. `uninstrumented families: 4/6`
# means "four anchor files contain no facade token". It does NOT mean:
#   - that the two matched families instrument any delivery at runtime — one
#     token anywhere in the file, including inside `#[cfg(test)] mod tests` or
#     a string literal, flips a family to instrumented;
#   - that the call sits on a reachable branch, is reached once, or is reached
#     at all;
#   - that a finish/settle exists for every begin. Deleting one of the sink's
#     three `journal::settle(..)` call sites still leaves THIS gate green, and
#     no RUNTIME test dies either: `begin_fresh` returns None without PG +
#     Shadow, so there is nothing to observe. CI does still catch that one
#     edit, but only through a source-contract text count -- see
#     `test_source_contract_sink_direct_success_arms_settle_each_terminal_arm`
#     in tests/test_delivery_journal_raw_writer.py, run by
#     scripts/ci-script-checks.sh. A settle that is genuinely lost (moved out
#     of the anchor file, or dropped in a way the count still accepts)
#     self-reports later, as an `Unknown` classification in shadow data.
# What the gate does buy is monotonicity: a family cannot silently regress to
# uninstrumented. Whether the instrumentation is CORRECT is proven only by the
# Rust runtime tests T1-T8 and their mutations M1-M7 (see the SOURCE-CONTRACT
# block in tests/test_delivery_journal_raw_writer.py for the index).
#
# #5071 T1 S3a extension. The watcher terminal family cannot spell the sink's
# facade: `tmux_output_watcher_with_restore` is a free function with no `self`,
# so it reaches the journal through the `journal::watcher` facade instead. The
# pattern below is therefore an alternation of two EXACT call shapes, not a
# loosened one — each alternative names its own module path and function, and
# neither matches a bare `journal`, a bare `watcher`, or an arbitrary method.
# Everything the S2 block says about what a match does NOT prove applies
# unchanged to the new alternative: one token anywhere in the anchor file,
# including a string literal or a test module, flips the family.
#
# #5071 T1 S4 adds the third alternative on the same terms. The turn_bridge
# cutover family reaches the journal through
# `turn_bridge::terminal_controller_cutover::unix_journal`, which re-exports
# `session_relay_sink::journal::controller`, so the alternative names
# that module path and its two exact functions — a bare `unix_journal`, a bare
# `controller`, or any other method on either does NOT match. The baseline moves
# 3 -> 2 because that anchor file now contains the token, and for no other
# reason: the two remaining uninstrumented families (recovery/fresh-send/orphan,
# pipe stream epoch) are unchanged by S4. That the drop is caused by the
# instrumentation rather than by the widened pattern is proven in
# tests/test_delivery_journal_raw_writer.py by the reverse mutation
# `test_controller_family_regresses_to_uninstrumented`.
#
# PLATFORM BLINDNESS (#5071 T1 S4, the windows regression). This whole file is a
# TEXT SCAN. It does not parse Rust, does not evaluate `cfg`, and has no notion
# of a compilation target, so a family it calls instrumented is instrumented on
# SOME target, never provably on all of them. That is not hypothetical here:
# `session_relay_sink` — and with it the entire journal — is `#[cfg(unix)]`
# (src/services/discord/mod.rs), while `turn_bridge` compiles on every target
# and its three durable delivered-frontier writes are NOT gated. So on
# windows/non-unix the cutover family advances the frontier UNINSTRUMENTED, and
# this gate reports it as instrumented anyway, byte for byte the same as on
# unix. Read `uninstrumented families: N/6` as "N families carry no facade token
# on unix". The one thing that actually holds the boundary the regression broke
# is `test_source_contract_turn_bridge_reaches_the_journal_through_one_cfg_gated_door`
# in tests/test_delivery_journal_raw_writer.py, which owns the door path as the
# literal `door` it scans for; see also
# src/services/discord/turn_bridge/terminal_controller_cutover/unix_journal.rs.
#
# #5071 T1 S5a. THE RECOVERY FAMILY'S ANCHOR WAS POINTING AT A FILE THAT WRITES NO
# DELIVERY, and this slice moves it. That is a correction to the family map, not
# instrumentation: nothing here becomes instrumented, and
# `UNINSTRUMENTED_FAMILY_BASELINE` stays 2.
#
# Measured on de8f3ab51, `src/services/discord/tmux_reaper.rs` contains, in
# production and in its inline tests alike: zero `delivery_record::` /
# `completed_turn_ledger::` calls, zero occurrences of the strings `frontier`,
# `journal` and `delivery_record`, and no Discord transport. It kills orphaned
# tmux sessions and finalizes stale-busy turns. It matched this family by NAME —
# `reap_fresh_routine_orphan` carries both "fresh" and "orphan" — and by nothing
# else, so for as long as it was the anchor this gate was counting the wrong file
# for this family, whatever the baseline happened to be.
#
# The family's durable writers all sit in ONE funnel,
# `RecoveryDeliveryContext::record_durable_frontier` in
# `recovery_engine/terminal_text_idempotency.rs`: `write_delivered_frontier` (the
# reuse-recorded-anchor arm), `write_proven_gone_equal_range_frontier` (the
# re-anchor taken only after Discord proved the recorded anchor GONE — the actual
# "orphan" of the family name) and the completed-turn ledger append above them.
# The anchor now names that file and the funnel's confirmed-delivery entry point.
#
# Three tests hold the move, in tests/test_delivery_journal_raw_writer.py:
# `test_source_contract_reaper_anchor_named_a_file_that_writes_no_delivery` keeps
# the measurement true, `test_source_contract_recovery_anchor_holds_the_family_durable_writers`
# keeps the new anchor holding the writers, and
# `test_every_family_anchor_sits_on_its_family_delivery_path` turns the whole
# thing into a RULE rather than a snapshot — every anchor must show delivery work
# in its own file. That rule is what would have caught this in the first place,
# and it is what fails if the anchor is ever moved back.
#
# The audit behind the move covered all six anchors, and it found a SECOND one
# with the same shape: "pipe stream epoch"
# (`tmux_watcher/turn_stream_collector.rs`) shows no durable write, no transport
# and no facade token either. It owns the epoch state the family is named for, so
# whether it is the wrong anchor or merely a family whose writer lives elsewhere
# is a real question — and it is not this slice's to answer. It was carried as the
# single named exemption in that rule test, pinned to stay empty so the exemption
# could not be used to admit a different bad anchor. S6 below answers the question
# and deletes the family and the exemption together. Of the four remaining anchors,
# three show durable writes or transport in their own file; `tmux_watcher.rs`
# shows neither and passes on its facade calls alone, its durable writes being
# delegated to child modules under `tmux_watcher/` through the `dr` alias it
# imports for them.
#
# WHAT THIS MOVE DOES NOT FIX. The gate reads ONE file per family. A durable write
# added to any other file of this family — `recovery_paths/controller_cutover.rs`,
# say — is caught by nothing here and by no source contract. That hole is the same
# one the S2 block declares, it is unchanged by S5a, and it is measured: adding an
# uninstrumented `write_delivered_frontier` to a non-anchor family file leaves
# this gate green and every test in the suite green.
#
# #5071 T1 S5b adds the fourth alternative on the same terms, now that S5a has
# pointed this family's anchor at the file that actually writes its delivery. The
# recovery family reaches the journal through `recovery_engine::unix_journal`,
# which re-exports `session_relay_sink::journal::recovery`, so the alternative
# names that module path and its two exact functions. `unix_journal::` now
# prefixes TWO different doors — turn_bridge's from S4 and recovery_engine's from
# S5b — and they stay separated by function name alone; a bare `unix_journal`, a
# bare `recovery`, or any other method on either does NOT match.
#
# The baseline moves 2 -> 1 because the anchor file now contains the token, and
# for no other reason. The one remaining uninstrumented family (pipe stream
# epoch) is untouched by S5b, and its anchor is the open question S5a raised and
# did not answer. That the drop is caused by the instrumentation rather than by
# the widened pattern is proven by the reverse mutation
# `test_recovery_family_regresses_to_uninstrumented`.
#
# PLATFORM BLINDNESS applies here for the same reason it applies to the cutover
# family: `mod recovery_engine`, `mod recovery_paths` and `mod outbound` are all
# ungated while the journal is `#[cfg(unix)]`, so on non-unix this family's
# frontier advances UNINSTRUMENTED and this text scan reports it as instrumented
# anyway. Read `uninstrumented families: 1/6` as "one family carries no facade
# token ON UNIX". The door is
# src/services/discord/recovery_engine/unix_journal.rs, and what holds the
# boundary is
# `test_source_contract_recovery_reaches_the_journal_through_one_cfg_gated_door`.
#
# What S5b does NOT observe, stated here because a green gate says nothing about
# it. This family's obligation opens AFTER transport (two of its three entry
# points only learn they advance the frontier from the edit transport's own
# answer), so a recovery delivery lost mid-POST leaves no trace at all. It emits
# no `T` either: no receipt is observable on this path, and synthesising one from
# the anchor message id would make `requested == returned` structurally true. The
# journal is shadow-only and nothing reads it back, and the recovery path's
# bypass of the `shadow_mirror_delivered_frontier` funnel is deliberately still
# there — S5b measures that bypass, #5071 T1 S7 closes it.
#
# #5071 T1 S6 DELETES THE SIXTH FAMILY. "pipe stream epoch" is not an
# uninstrumented family; it is the name of a coordinate system that does not
# exist yet. The registry goes 6 -> 5 and the baseline 1 -> 0, in ONE commit,
# because the ratchet is bidirectional and a removal without a re-pin is rc=1.
#
# What was measured, and what each measurement rules out:
#   - `grep -rn stream_epoch src/ scripts/ migrations/` returns nothing. The
#     name exists only in the design draft.
#   - the shipped schema `migrations/postgres/0103_delivery_journal.sql` has no
#     `source_kind`, no `pipe_stream_epoch` and no `pipe_sequence` column, so
#     there is no durable row a pipe obligation could be keyed by. The journal
#     facade has exactly one coordinate key, `TuiObligationKey` in journal.rs.
#   - the anchor `tmux_watcher/turn_stream_collector.rs` performs zero durable
#     writes, holds zero facade calls and runs no transport. The one thing it
#     does mutate it mutates through `commit_persistent_state!`, which writes
#     back into the CALLER's stack frame through `&'a mut` bindings. That is not
#     durable state, so an anchor move in the S5a style is impossible: there is
#     no file that could satisfy the two-way contract's "the new anchor holds
#     this family's writers" half, because the family has no writers.
#   - pipe (= legacy tmux wrapper) and TUI merge onto the SAME watcher in
#     `turn_bridge/runtime_handoff_loop.rs`, and `tmux_watcher.rs:480` is the
#     only call site of `collect_turn_stream_until_terminal` in the tree. Pipe
#     is a runtime-mode overlay on a shared delivery path, not a module family
#     with delivery code of its own.
#
# HOW TO REVIVE IT. Restore the entry when, and only when, a durable write is
# reached on a branch that is gated on `RuntimeHandoffKind::LegacyTmuxWrapper`
# or `::ProcessBackend` and cannot be reached by `ClaudeTui` — concretely, when
# either the journal grows a second obligation key beside `TuiObligationKey` or
# the delivery-journal schema grows the pipe columns above. Two tests in
# tests/test_delivery_journal_raw_writer.py exist to make that moment loud
# rather than silent, because the SCRIPT CANNOT SEE IT: restoring the family and
# its old anchor leaves this gate at rc=0 (measured in S5a's N1 — the gate does
# not detect anchor restoration; only the contract tests do).
#   `test_source_contract_stream_collector_is_no_longer_a_family_anchor` pins
#   both halves of the deletion: the registry has five entries and none of them
#   names the collector, and the collector still writes no delivery.
#   `test_source_contract_no_pipe_gated_production_file_holds_delivery_work`
#   pins that no production file gating on those two variants also contains a
#   delivery-work call, with the two measured co-existences named and explained.
#   That test's failure message IS the revival trigger.
#
# READ `uninstrumented families: 0/5` AS "ZERO INSIDE THIS GATE'S FIVE ANCHORS",
# NOT AS "NOTHING IS UNINSTRUMENTED". This is the sentence most likely to be
# misread now that the number is 0, so the summary line carries the caveat in
# its own output. The gate reads ONE file per family; every hole the S2 and S5a
# blocks declare is still open, and five uninstrumented production writers are
# known to sit outside the anchors right now:
#   src/services/discord/turn_bridge/terminal_delivery.rs
#   src/services/discord/turn_bridge/terminal_outcome_delivery.rs
#   src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs
#     — three turn_bridge durable frontier writes outside that family's anchor
#   src/services/discord/tui_prompt_relay/claude_idle_runtime.rs
#     — a frontier re-anchor that belongs to NO family in this registry. Not an
#       outside-the-anchor problem: a hole in the map itself.
#   src/services/discord/outbound/turn_output_controller/fresh_send.rs
#     — dormant, pinned by
#       test_source_contract_dormant_fresh_send_writer_is_pinned_uninstrumented
# SUCCESSION. All five are covered per file and per exact count by
# scripts/check_durable_frontier_writer_call_sites.py (#5071 T1 S7'), which has
# no anchors and walks every .rs under src/. Once this gate reads 0, the only
# discriminating power it has left is "does the anchor symbol still exist and
# still carry a facade token"; the writer-location question has moved to S7'.
# #5071 T1 S7 FALSIFIED TWO SENTENCES ABOVE. Both are named here rather than
# edited in place, because the blocks that carry them are anchored to their own
# slices and are history.
#
#   (1) The S5a block says: "The family's durable writers all sit in ONE funnel,
#       `RecoveryDeliveryContext::record_durable_frontier` in
#       `recovery_engine/terminal_text_idempotency.rs`:
#       `write_delivered_frontier` ..., `write_proven_gone_equal_range_frontier`
#       ... and the completed-turn ledger append above them." As of S7 that
#       funnel calls NONE of those three. It makes one call to
#       `delivery_record::record_recovery_terminal_delivery`, which reaches
#       `shadow_mirror_delivered_frontier_inner`. The anchor and the family map
#       are unchanged; only which symbols the anchor file spells has changed,
#       and `test_source_contract_recovery_anchor_holds_the_family_durable_writers`
#       was rewritten in the same commit to assert those three ABSENT.
#
#   (2) The S5b block says: "the recovery path's bypass of the
#       `shadow_mirror_delivered_frontier` funnel is deliberately still there —
#       S5b measures that bypass, #5071 T1 S7 closes it." S7 is this slice, and
#       it closed the bypass for the three writes named in (1). It did NOT close
#       it for `outbound/turn_output_controller/fresh_send.rs`, which still calls
#       `write_delivered_frontier` and `record_fresh_send_content_fingerprint`
#       raw. That file is measured dormant on this tree -- the only
#       `OutputPlan::SendFresh` CONSTRUCTOR expression in `src/` is in
#       `fresh_send_tests.rs`; every other mention is a pattern, a match arm or
#       the variant declaration -- and it is pinned by
#       `test_source_contract_dormant_fresh_send_writer_is_pinned_uninstrumented`
#       and per-file by S7'. "The recovery path" in (2) means the three writes,
#       not every durable write in the tree.
#
# THIS GATE DID NOT MOVE FOR S7, AND THAT IS THE POINT WORTH RECORDING. The
# baseline stays 0/5 and rc=0 across the whole change, because the anchor file
# still holds `record_successful_fresh_send` and still carries the
# `unix_journal::` facade tokens. A boolean-per-family gate cannot see a durable
# write change WHICH WRITER IT CALLS. What saw it was the per-file exact map in
# scripts/check_durable_frontier_writer_call_sites.py (five counts moved) and
# the source contract test above (which went red and had to be rewritten).
JOURNAL_FACADE_CALL = re.compile(
    r"\bself\.journal\.(?:begin_fresh|finish_fresh)\s*\("
    r"|\bjournal_watcher::(?:begin_watcher_terminal|settle_watcher_terminal|settle_without_transport)\s*\("
    r"|\bunix_journal::(?:begin_controller_terminal|settle_controller_terminal)\s*\("
    r"|\bunix_journal::(?:begin_recovery_terminal|settle_recovery_terminal)\s*\("
)
UNINSTRUMENTED_FAMILY_BASELINE = 0
# Printed on every run, pass or fail. It exists because `0/5` reads like "done"
# and is not: see the S6 block above for the five named production writers this
# gate structurally cannot see and for the succession to S7'.
ANCHOR_SCOPE_CAVEAT = (
    "ANCHOR-SCOPED: N counts anchor files missing the token, one file per family; "
    "0 does NOT mean no uninstrumented durable write exists — five known production "
    "writers sit outside these anchors (turn_bridge terminal_delivery.rs, "
    "terminal_outcome_delivery.rs, terminal_outcome_delivery/cancel_prompt_replace.rs; "
    "tui_prompt_relay/claude_idle_runtime.rs, which belongs to no family here; and the "
    "dormant outbound/turn_output_controller/fresh_send.rs), all five pinned per file "
    "and per count by scripts/check_durable_frontier_writer_call_sites.py"
)


def call_sites(root: Path) -> tuple[Counter[str], int]:
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--", "src/"], cwd=root,
        check=True, capture_output=True, text=True,
    ).stdout.split("\0")
    listed = [rel for rel in listed if rel.endswith(".rs")]
    found: Counter[str] = Counter()
    for rel in listed:
        for line in (root / rel).read_text(encoding="utf-8").splitlines():
            code = line.split("//", 1)[0]
            if "fn append_delivery_journal_batch" not in code and CALL.search(code):
                found[rel] += 1
    return found, len(listed)


def family_status(root: Path) -> tuple[list[tuple[str, bool]] | None, str]:
    status = []
    for name, rel, symbol in FAMILY_REGISTRY:
        path = root / rel
        if not path.is_file():
            return None, f"family anchor missing: {name} ({rel}:{symbol})"
        text = "\n".join(line.split("//", 1)[0] for line in path.read_text(encoding="utf-8").splitlines())
        if not re.search(rf"\b(?:async\s+)?fn\s+{re.escape(symbol)}\b", text):
            return None, f"family anchor symbol missing: {name} ({rel}:{symbol})"
        instrumented = any(JOURNAL_FACADE_CALL.search(line) for line in text.splitlines())
        status.append((name, instrumented))
    return status, ""


def check(root: Path) -> tuple[bool, str]:
    families, error = family_status(root)
    if families is None:
        return False, f"FAIL CLOSED: {error}"
    found, scanned_files = call_sites(root)
    total = sum(found.values())
    if total > BASELINE:
        return False, f"raw writer call count {total} exceeds monotonic baseline {BASELINE}: {dict(found)} (scanned Rust files: {scanned_files})"
    if found != ALLOWLIST:
        return False, f"raw writer allowlist mismatch: expected={dict(ALLOWLIST)} actual={dict(found)} (scanned Rust files: {scanned_files})"
    uninstrumented = [name for name, instrumented in families if not instrumented]
    summary = (
        f"uninstrumented families: {len(uninstrumented)}/{len(families)} "
        f"(lexical baseline signal; whole anchor file including tests; only // suffix "
        f"excluded; not proof; {', '.join(uninstrumented) or 'none'}; {ANCHOR_SCOPE_CAVEAT})"
    )
    if len(uninstrumented) > UNINSTRUMENTED_FAMILY_BASELINE:
        return False, f"{summary}; exceeds baseline {UNINSTRUMENTED_FAMILY_BASELINE}: {', '.join(uninstrumented)}"
    if len(uninstrumented) < UNINSTRUMENTED_FAMILY_BASELINE:
        command = ("python3 -c \"from pathlib import Path; p=Path('scripts/check_delivery_journal_raw_writer.py'); "
                   f"s=p.read_text(); p.write_text(s.replace('UNINSTRUMENTED_FAMILY_BASELINE = {UNINSTRUMENTED_FAMILY_BASELINE}', "
                   f"'UNINSTRUMENTED_FAMILY_BASELINE = {len(uninstrumented)}'))\"")
        return False, f"{summary}; below baseline {UNINSTRUMENTED_FAMILY_BASELINE}; re-pin with: {command}"
    return True, f"OK: DeliveryJournal raw writer call sites exact ({total}/{BASELINE}); {summary}; scanned Rust files: {scanned_files}"
if __name__ == "__main__":
    ok, message = check(Path(__file__).resolve().parent.parent)
    print(message, file=sys.stdout if ok else sys.stderr)
    raise SystemExit(0 if ok else 1)
