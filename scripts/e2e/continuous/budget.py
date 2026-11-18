"""Per-provider daily budget reserved under the channel lock before any provider turn.

An unset limit or an unmeasured reservation refuses the run: unset is not unlimited.
"""

import time

from .store import read_json, write_json

FIELDS = ("turns", "time_s")


def day(now):
    return time.strftime("%Y-%m-%d", time.gmtime(now))


def _path(state_dir, when):
    return state_dir / "budget" / f"{when}.json"


def reserve(state_dir, limits, amounts, run_id, now):
    """Record `amounts` for run_id and return None, or return why the day's budget cannot hold them."""
    ledger = read_json(_path(state_dir, day(now)), {})
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
    write_json(path, ledger)
