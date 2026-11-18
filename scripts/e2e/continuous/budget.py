"""Per-provider daily budget reserved under the channel lock before any provider turn.

An unset limit or an unmeasured reservation refuses the run: unset is not unlimited.
"""

import math
import time

from .store import read_json, write_json

FIELDS = ("turns", "tokens", "time_s")


def day(now):
    return time.strftime("%Y-%m-%d", time.gmtime(now))


def _path(state_dir, when):
    return state_dir / "budget" / f"{when}.json"


def reserved(state_dir, when, run_id):
    return run_id in read_json(_path(state_dir, when), {})


def configured(limits, amounts):
    for provider, need in sorted(amounts.items()):
        cap = limits.get(provider, {})
        for field in FIELDS:
            wanted = need.get(field) if isinstance(need, dict) else None
            limit = cap.get(f"daily_{field}") if isinstance(cap, dict) else None
            if type(wanted) not in (int, float) or not math.isfinite(wanted) or wanted <= 0:
                return f"{provider} {field} reservation is unmeasured"
            if type(limit) not in (int, float) or not math.isfinite(limit) or limit < 0:
                return f"{provider} daily {field} limit is unset"
    return None


def reserve(state_dir, limits, amounts, run_id, now):
    """Record `amounts` for run_id and return None, or return why the day's budget cannot hold them."""
    ledger = read_json(_path(state_dir, day(now)), {})
    if refused := configured(limits, amounts):
        return refused
    for provider, need in sorted(amounts.items()):
        cap = limits.get(provider) if isinstance(limits, dict) else None
        for field in FIELDS:
            wanted = need.get(field) if isinstance(need, dict) else None
            if type(wanted) not in (int, float) or wanted <= 0:
                return f"{provider} {field} reservation is unmeasured"
            limit = cap.get(f"daily_{field}") if isinstance(cap, dict) else None
            if type(limit) not in (int, float):
                return f"{provider} daily {field} limit is unset"
            used = sum(row.get(provider, {}).get(field, 0) for row in ledger.values())
            if used + wanted > limit:
                return f"{provider} daily {field} budget exhausted ({used}+{wanted}>{limit})"
    ledger[run_id] = amounts
    write_json(_path(state_dir, day(now)), ledger)
    return None


def settle(state_dir, when, run_id, actual):
    """Replace reserved amounts with measured ones; an unknown (None) amount keeps the reservation."""
    path = _path(state_dir, when)
    ledger = read_json(path, {})
    row = ledger.get(run_id)
    if row is None:
        return
    for provider, used in actual.items():
        for field in FIELDS:
            if used.get(field) is not None and provider in row:
                row[provider][field] = used[field]
        if any(used.get(field) is None for field in FIELDS):
            paused = read_json(state_dir / "budget" / "paused.json", {})
            paused[provider] = run_id
            write_json(state_dir / "budget" / "paused.json", paused)
    write_json(path, ledger)


class TurnGuard:
    """Driver contract: reserve a bounded token request immediately before each native turn."""
    def __init__(self, remaining):
        self.remaining = dict(remaining)
        self.deadline = remaining.get("deadline", time.monotonic() + remaining["time_s"])
        self.started = 0
        self.pending = None

    def start(self, max_tokens):
        if (self.pending is not None or type(max_tokens) is not int or max_tokens <= 0
                or self.remaining["turns"] < 1 or self.remaining["tokens"] < max_tokens
                or time.monotonic() >= self.deadline):
            return False
        self.remaining["turns"] -= 1
        self.remaining["tokens"] -= max_tokens
        self.pending = max_tokens
        self.started += 1
        return True

    def finish(self, tokens):
        if self.pending is None:
            raise ValueError("no native turn reserved")
        if type(tokens) is int and 0 <= tokens <= self.pending:
            self.remaining["tokens"] += self.pending - tokens
            self.pending = None
