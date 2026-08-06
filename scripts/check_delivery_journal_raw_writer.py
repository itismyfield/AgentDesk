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
    ("recovery / fresh-send / orphan family", "src/services/discord/tmux_reaper.rs", "reap_fresh_routine_orphan"),
    ("pipe stream epoch", "src/services/discord/tmux_watcher/turn_stream_collector.rs", "collect_turn_stream_until_terminal"),
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
JOURNAL_FACADE_CALL = re.compile(
    r"\bself\.journal\.(?:begin_fresh|finish_fresh)\s*\("
    r"|\bjournal_watcher::(?:begin_watcher_terminal|settle_watcher_terminal|settle_without_transport)\s*\("
)
UNINSTRUMENTED_FAMILY_BASELINE = 3


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
    summary = f"uninstrumented families: {len(uninstrumented)}/{len(families)} (lexical baseline signal; whole anchor file including tests; only // suffix excluded; not proof; {', '.join(uninstrumented) or 'none'})"
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
