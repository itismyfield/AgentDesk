#!/usr/bin/env python3
"""Token usage snapshot sampler.

Appends a point-in-time usage snapshot (3 Claude accounts via cswap + Codex
rate-limits) to metrics/token-history/snapshots-YYYY-MM-DD.jsonl.

Designed to run frequently (e.g. every 30 min) with NO LLM cost. The daily
report reads these snapshots to:
  - detect rate-limit resets (scheduled vs manual, e.g. Codex resets by tibo),
  - anchor per-account usage over time.

Usage:
    token-usage-snapshot.py           # write one snapshot line
    token-usage-snapshot.py --print   # write + pretty-print the record
"""
from __future__ import annotations

import json
import os
import sys
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

KST = timezone(timedelta(hours=9))
ROOT = Path(os.environ.get("AGENTDESK_ROOT_DIR", str(Path.home() / ".adk/release")))
API_PORT = os.environ.get("ADK_API_PORT", "8791")
API = f"http://127.0.0.1:{API_PORT}/api"
HIST_DIR = ROOT / "metrics" / "token-history"


def _get(path: str) -> dict:
    try:
        req = urllib.request.Request(
            f"{API}{path}", headers={"Accept": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=8) as r:
            return json.loads(r.read())
    except Exception as e:  # noqa: BLE001
        return {"_error": str(e)}


def _compact_provider(rl: dict, provider: str) -> dict | None:
    for p in rl.get("providers", []) or []:
        if p.get("provider") == provider:
            return {
                "stale": p.get("stale"),
                "buckets": {
                    b.get("name"): {
                        "used": b.get("used"),
                        "reset": b.get("reset"),
                    }
                    for b in p.get("buckets", []) or []
                },
            }
    return None


def build_snapshot() -> dict:
    now = datetime.now(KST)
    accounts = _get("/claude-accounts")
    rl = _get("/rate-limits")

    accts = []
    for a in accounts.get("accounts", []) or []:
        u = a.get("usage", {}) or {}
        fh = u.get("fiveHour") or {}
        sd = u.get("sevenDay") or {}
        accts.append(
            {
                "n": a.get("number"),
                "email": a.get("email"),
                "active": a.get("active"),
                "org": a.get("organizationName"),
                "h5": fh.get("pct"),
                "h5_reset": fh.get("resetsAt"),
                "d7": sd.get("pct"),
                "d7_reset": sd.get("resetsAt"),
                "scoped": [
                    {"name": s.get("name"), "pct": s.get("pct"), "reset": s.get("resetsAt")}
                    for s in (u.get("scoped") or [])
                ],
                "age_s": a.get("usageAgeSeconds"),
                "status": a.get("usageStatus"),
            }
        )

    rec = {
        "ts": now.isoformat(),
        "epoch": round(now.timestamp(), 3),
        "host": accounts.get("hostname"),
        "accounts": accts,
        "codex": _compact_provider(rl, "codex"),
        "claude_rl": _compact_provider(rl, "claude"),
        "errors": {
            k: v.get("_error")
            for k, v in (("accounts", accounts), ("rate_limits", rl))
            if isinstance(v, dict) and v.get("_error")
        }
        or None,
    }
    return rec


def write_snapshot(rec: dict) -> Path:
    now = datetime.now(KST)
    HIST_DIR.mkdir(parents=True, exist_ok=True)
    outfile = HIST_DIR / f"snapshots-{now.strftime('%Y-%m-%d')}.jsonl"
    with open(outfile, "a", encoding="utf-8") as f:
        f.write(json.dumps(rec, ensure_ascii=False) + "\n")
    return outfile


def main() -> None:
    rec = build_snapshot()
    outfile = write_snapshot(rec)
    n = len(rec.get("accounts") or [])
    codex_ok = rec.get("codex") is not None
    if "--print" in sys.argv:
        print(json.dumps(rec, ensure_ascii=False, indent=2))
    print(
        f"snapshot -> {outfile.name}: {n} CC accounts, codex={'ok' if codex_ok else 'missing'}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
