#!/usr/bin/env python3
from __future__ import annotations

import re
import subprocess
import sys
from collections import Counter
from pathlib import Path
SYMBOL = "append_delivery_journal_batch"
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
# A family is instrumented when its anchor file's non-test area calls self.journal's
# typed begin/finish facade; this is file-level, not limited to the anchor function body.
JOURNAL_FACADE_CALL = re.compile(r"\bself\.journal\.(?:begin_fresh|finish_fresh)\s*\(")
TOP_LEVEL_TEST_CFG = re.compile(r"^#\[cfg\((?:test|all\(\s*test\b[^\]]*\))\)\]\s*$")
TOP_LEVEL_MODULE = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?mod\b")
UNINSTRUMENTED_FAMILY_BASELINE = 5


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
        lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
        for index, line in enumerate(lines):
            candidate = line.split("//", 1)[0].rstrip()
            if not line.startswith("#[") or not TOP_LEVEL_TEST_CFG.fullmatch(candidate):
                continue
            following = next((item.strip() for item in lines[index + 1:]
                              if item.strip() and not item.strip().startswith(("//", "#["))), "")
            if TOP_LEVEL_MODULE.match(following) and not following.endswith(";"):
                lines = lines[:index]
                break
        text = "\n".join(line.split("//", 1)[0] for line in lines)
        if not re.search(rf"\b(?:async\s+)?fn\s+{re.escape(symbol)}\b", text):
            return None, f"family anchor symbol missing: {name} ({rel}:{symbol})"
        # Cheap quote parity is intentional; raw strings and escaped quotes are not handled.
        instrumented = any(text[:match.start()].count('"') % 2 == 0 for match in JOURNAL_FACADE_CALL.finditer(text))
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
    summary = f"uninstrumented families: {len(uninstrumented)}/{len(families)} (anchor-file non-test area; {', '.join(uninstrumented) or 'none'})"
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
