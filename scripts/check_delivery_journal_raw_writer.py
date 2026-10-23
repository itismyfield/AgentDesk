#!/usr/bin/env python3
"""Keep strict episode metadata behind its registered journal writer."""
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path
SYMBOL = "append_delivery_journal_batch"
CALL = re.compile(r"\bappend_delivery_journal_batch\s*\(")
BASELINE = 1
ALLOWLIST = Counter({"src/services/discord/session_relay_sink/journal.rs": 1})
EXACT_METADATA_FACADES = {"src/services/tui_o/exact_pg.rs": 1}
def exact_facade_error(root: Path) -> str:
    registry = EXACT_METADATA_FACADES
    expected = {"src/services/tui_o/exact_pg.rs": 1}
    if registry != expected:
        return "strict metadata facade registry drift"
    actual = {}
    for path in root.glob("src/**/*.rs"):
        if path.name.endswith("_tests.rs"):
            continue
        text = path.read_text(encoding="utf-8")
        if path.as_posix().endswith("journal/pg_store.rs"):
            text = text.split("#[cfg(test)]",1)[0]
        count = len(re.findall(r"discord::append_exact_metadata\s*\(", text))
        if count:
            actual[path.relative_to(root).as_posix()] = count
    return "" if actual == registry else f"strict metadata facade count drift: {actual}"


def call_sites(root: Path) -> tuple[Counter[str], int]:
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--", "src/"], cwd=root,
        check=True, capture_output=True, text=True,
    ).stdout.split("\0")
    listed = [rel for rel in listed if rel.endswith(".rs")]
    found: Counter[str] = Counter()
    for rel in listed:
        if not (root / rel).is_file():
            continue
        for line in (root / rel).read_text(encoding="utf-8").splitlines():
            code = line.split("//", 1)[0]
            if "fn append_delivery_journal_batch" not in code and CALL.search(code):
                found[rel] += 1
    return found, len(listed)


def check(root: Path) -> tuple[bool, str]:
    found, scanned_files = call_sites(root)
    total = sum(found.values())
    if total > BASELINE:
        return False, f"raw writer call count {total} exceeds monotonic baseline {BASELINE}: {dict(found)} (scanned Rust files: {scanned_files})"
    facade_error = exact_facade_error(root)
    if facade_error:
        return False, facade_error
    if found != ALLOWLIST:
        return False, f"raw writer allowlist mismatch: expected={dict(ALLOWLIST)} actual={dict(found)} (scanned Rust files: {scanned_files})"
    return True, f"OK: DeliveryJournal raw writer call sites exact ({total}/{BASELINE}); scanned Rust files: {scanned_files}"
if __name__ == "__main__":
    ok, message = check(Path(__file__).resolve().parent.parent)
    print(message, file=sys.stdout if ok else sys.stderr)
    raise SystemExit(0 if ok else 1)
