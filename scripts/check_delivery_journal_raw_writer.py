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


def call_sites(root: Path) -> Counter[str]:
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--", "src/**/*.rs"], cwd=root,
        check=True, capture_output=True, text=True,
    ).stdout.split("\0")
    found: Counter[str] = Counter()
    for rel in filter(None, listed):
        for line in (root / rel).read_text(encoding="utf-8").splitlines():
            code = line.split("//", 1)[0]
            if "fn append_delivery_journal_batch" not in code and CALL.search(code):
                found[rel] += 1
    return found


def check(root: Path) -> tuple[bool, str]:
    found = call_sites(root)
    total = sum(found.values())
    if total > BASELINE:
        return False, f"raw writer call count {total} exceeds monotonic baseline {BASELINE}: {dict(found)}"
    if found != ALLOWLIST:
        return False, f"raw writer allowlist mismatch: expected={dict(ALLOWLIST)} actual={dict(found)}"
    return True, f"OK: DeliveryJournal raw writer call sites exact ({total}/{BASELINE})"


if __name__ == "__main__":
    ok, message = check(Path(__file__).resolve().parent.parent)
    print(message, file=sys.stdout if ok else sys.stderr)
    raise SystemExit(0 if ok else 1)
