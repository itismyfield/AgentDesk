#!/usr/bin/env python3
"""Token Manager Daily Report v2 — quota-aware, baseline-relative, evidence-backed.

Redesign vs v1:
  1. 3 Claude accounts (cswap) via /api/claude-accounts — per-account 5h/7d/Fable.
  2. Rate-limit RESET detection from snapshot history (metrics/token-history/),
     classifying 5h (routine ~every 5h) vs 7d/weekly (scheduled vs manual/tibo).
     Codex-session bootstrap gives a best-effort count until snapshots accumulate.
  3. CONSUMPTION measured as "new tokens" = input+output+cache_create (Claude) /
     (input-cached)+output (Codex), EXCLUDING cache-read. This removes the massive
     cache-read re-count that made every heavy-Codex agent look like an anomaly.
  4. Baseline-relative anomaly detection: each agent (family-normalized) vs its own
     last-7-day norm. Ephemeral per-issue workspaces (adk-impl-1234) collapse to a
     family so they don't perpetually trip "new agent".
  5. Qualitative drill-down (Claude + Codex) on top consumers: what they DID.

Everything the LLM narrates is pre-computed here with evidence; the LLM is told
NOT to invent anomalies. That is the fix for "flagged abnormal, re-checked = normal".

Modes:
  --raw-json   full structured payload (consumed by .sh -> Sonnet narration)
  --dry-run    Python fallback report to stdout (no LLM, no send)
  (default)    render fallback report and send to Discord
"""
from __future__ import annotations

import glob
import json
import os
import re
import sqlite3
import sys
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

KST = timezone(timedelta(hours=9))
HOME = Path.home()
ROOT = Path(os.environ.get("AGENTDESK_ROOT_DIR", str(HOME / ".adk/release")))
API_PORT = os.environ.get("ADK_API_PORT", "8791")
API = f"http://127.0.0.1:{API_PORT}/api"

PCD_PROD_DB = HOME / ".local/state/pixel-claw-dashboard/prod/pixel-claw-dashboard.sqlite"
CLAUDE_PROJECTS = HOME / ".claude/projects"
CODEX_SESSIONS = HOME / ".codex" / "sessions"
HIST_DIR = ROOT / "metrics" / "token-history"
TOKEN_MANAGER_CHANNEL = "1481478222439907560"

# Claude API-equivalent pricing (USD/MTok). Accounts are subscription/quota, so USD
# is an ESTIMATE for scale intuition only — quota % is the real constraint.
PRICE = {"input": 15.0, "output": 75.0, "cache_create": 18.75, "cache_read": 1.50}

BASELINE_DAYS = 7
ANOM_Z = 2.5
ANOM_RATIO = 2.0
ANOM_FLOOR_NEW = 3_000_000        # new-token floor to ignore trivial agents
NEW_AGENT_FLOOR = 10_000_000      # family with no history flagged only if this big
RESET_DROP_PT = 15.0


# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #
def _agent_from_path(path: str) -> str:
    for sep in ("-workspaces-", "/workspaces/"):
        if sep in path:
            return path.split(sep)[-1].split("/")[0]
    for sep in ("-worktrees-", "/worktrees/"):
        if sep in path:
            name = path.split(sep)[-1].split("/")[0]
            name = re.sub(r"^(codex|claude)-", "", name)
            name = re.sub(r"-t?\d{6,}.*$", "", name)
            name = re.sub(r"-\d{8}-\d{6}$", "", name)
            return name
    last = path.rstrip("/").split("/")[-1].lstrip("-")
    if last.startswith("Users-") or last.startswith("private-") or last.startswith("tmp"):
        return "(personal/tmp)"
    return last or "unknown"


def family(agent: str) -> str:
    """Collapse ephemeral per-issue/per-run names to a family for baselining.
    adk-impl-4305 -> adk-impl-*, adk-review-4308-0fed -> adk-review-*, agent-ab12 -> agent-*."""
    m = re.match(r"^([a-zA-Z][a-zA-Z-]*?)-(?:\d{2,}|[0-9a-f]{6,})(?:[-.].*)?$", agent)
    if m:
        return m.group(1) + "-*"
    return agent


def _iso2epoch(s) -> float | None:
    if not s:
        return None
    try:
        return datetime.fromisoformat(str(s).replace("Z", "+00:00")).timestamp()
    except Exception:
        return None


def _get(path: str) -> dict:
    try:
        req = urllib.request.Request(f"{API}{path}", headers={"Accept": "application/json"})
        with urllib.request.urlopen(req, timeout=8) as r:
            return json.loads(r.read())
    except Exception as e:  # noqa: BLE001
        return {"_error": str(e)}


def fmt_tok(n) -> str:
    n = int(n or 0)
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}K"
    return str(n)


def usd_equiv(inp, out, cc, cr) -> float:
    return (inp / 1e6 * PRICE["input"] + out / 1e6 * PRICE["output"]
            + cc / 1e6 * PRICE["cache_create"] + cr / 1e6 * PRICE["cache_read"])


# --------------------------------------------------------------------------- #
# account quota + codex rate limit
# --------------------------------------------------------------------------- #
def get_accounts() -> list[dict]:
    data = _get("/claude-accounts")
    out = []
    for a in data.get("accounts", []) or []:
        u = a.get("usage", {}) or {}
        fh, sd = u.get("fiveHour") or {}, u.get("sevenDay") or {}
        out.append({
            "n": a.get("number"), "email": a.get("email"), "org": a.get("organizationName"),
            "active": a.get("active"),
            "h5": fh.get("pct"), "h5_countdown": fh.get("countdown"),
            "d7": sd.get("pct"), "d7_countdown": sd.get("countdown"),
            "scoped": [{"name": s.get("name"), "pct": s.get("pct"), "countdown": s.get("countdown")}
                       for s in (u.get("scoped") or [])],
            "status": a.get("usageStatus"), "age_s": a.get("usageAgeSeconds"),
        })
    return out


def get_codex_rate() -> dict | None:
    data = _get("/rate-limits")
    for p in data.get("providers", []) or []:
        if p.get("provider") == "codex":
            now = datetime.now(KST)
            buckets = {}
            for b in p.get("buckets", []) or []:
                reset = b.get("reset")
                cd = None
                if reset:
                    hrs = (datetime.fromtimestamp(reset, KST) - now).total_seconds() / 3600
                    cd = f"{int(hrs // 24)}d {hrs % 24:.0f}h" if hrs > 24 else f"{hrs:.1f}h"
                buckets[b.get("name")] = {"used": b.get("used"), "countdown": cd}
            return {"stale": p.get("stale"), "buckets": buckets}
    return None


# --------------------------------------------------------------------------- #
# reset detection
# --------------------------------------------------------------------------- #
def load_snapshots(days: int = 2) -> list[dict]:
    snaps, now = [], datetime.now(KST)
    for dd in range(days + 1):
        f = HIST_DIR / f"snapshots-{(now - timedelta(days=dd)).strftime('%Y-%m-%d')}.jsonl"
        if f.exists():
            with open(f, encoding="utf-8") as fh:
                for line in fh:
                    try:
                        snaps.append(json.loads(line))
                    except Exception:
                        pass
    snaps.sort(key=lambda s: s.get("epoch", 0))
    return snaps


def detect_resets_from_snapshots(snaps: list[dict]) -> list[dict]:
    resets, series = [], {}
    for s in snaps:
        ep = s.get("epoch")
        for a in s.get("accounts") or []:
            for bname, field in (("5h", "h5"), ("7d", "d7")):
                pct = a.get(field)
                if pct is not None:
                    series.setdefault((a.get("email"), bname), []).append((ep, pct))
        for bname, bd in ((s.get("codex") or {}).get("buckets") or {}).items():
            if bd.get("used") is not None:
                series.setdefault(("codex", bname), []).append((ep, bd["used"]))
    for (who, bname), rows in series.items():
        rows.sort()
        for i in range(1, len(rows)):
            if rows[i - 1][1] - rows[i][1] >= RESET_DROP_PT:
                resets.append({
                    "who": who, "bucket": bname,
                    "from_pct": rows[i - 1][1], "to_pct": rows[i][1],
                    "at": datetime.fromtimestamp(rows[i][0], KST).strftime("%m-%d %H:%M"),
                    "kind": "weekly" if bname in ("7d",) else "routine5h",
                    "source": "snapshot",
                })
    return resets


def _codex_rl_points(path: str) -> list[tuple]:
    pts = []
    try:
        with open(path, encoding="utf-8", errors="ignore") as f:
            for i, line in enumerate(f):
                if i > 120:
                    break
                if '"rate_limits"' in line:
                    o = json.loads(line); rl = o.get("payload", {}).get("rate_limits") or {}
                    up = (rl.get("primary") or {}).get("used_percent"); ep = _iso2epoch(o.get("timestamp"))
                    if up is not None and ep:
                        pts.append((ep, up)); break
        sz = os.path.getsize(path)
        with open(path, "rb") as f:
            f.seek(max(0, sz - 65536)); data = f.read().decode("utf-8", "ignore")
        for line in reversed(data.splitlines()):
            if '"rate_limits"' in line:
                try:
                    o = json.loads(line); rl = o.get("payload", {}).get("rate_limits") or {}
                    up = (rl.get("primary") or {}).get("used_percent"); ep = _iso2epoch(o.get("timestamp"))
                    if up is not None and ep:
                        pts.append((ep, up)); break
                except Exception:
                    pass
    except Exception:
        pass
    return pts


def codex_reset_count_bootstrap(window_hours: int = 30) -> dict | None:
    """Count Codex 5h resets from session logs (bootstrap until snapshots exist).
    5h resets are ROUTINE (~every 5h). Returns a summary, not per-event noise."""
    now = datetime.now(KST); cutoff = now.timestamp() - window_hours * 3600
    pts = []
    for dd in range(0, 2):
        day = now - timedelta(days=dd)
        ddir = CODEX_SESSIONS / f"{day.year:04d}" / f"{day.month:02d}" / f"{day.day:02d}"
        if not ddir.exists():
            continue
        for jf in ddir.glob("*.jsonl"):
            try:
                if jf.stat().st_mtime < cutoff:
                    continue
            except Exception:
                continue
            pts.extend(_codex_rl_points(str(jf)))
    pts = [(e, p) for e, p in pts if e >= cutoff]
    if len(pts) < 4:
        return None
    buckets = {}
    for ep, pct in pts:
        b = int(ep // 900)
        buckets[b] = max(buckets.get(b, 0), pct)
    series = sorted(buckets.items())
    count, last_at, peak = 0, None, 0
    for i in range(1, len(series)):
        peak = max(peak, series[i - 1][1])
        if series[i - 1][1] - series[i][1] >= 30:
            count += 1
            last_at = datetime.fromtimestamp(series[i][0] * 900, KST).strftime("%m-%d %H:%M")
    return {"count": count, "last_at": last_at, "peak": peak, "window_h": window_hours} if count else None


# --------------------------------------------------------------------------- #
# usage scan — "new tokens" excludes cache-read
# --------------------------------------------------------------------------- #
def _blank():
    return {"new": 0, "cache_read": 0, "input": 0, "output": 0, "cc": 0, "sessions": 0}


def scan_claude(dates: set, yday: str):
    history = {d: {} for d in dates}
    yday_sessions = {}
    for jf in glob.glob(str(CLAUDE_PROJECTS / "**" / "*.jsonl"), recursive=True):
        try:
            mdate = datetime.fromtimestamp(os.path.getmtime(jf)).strftime("%Y-%m-%d")
        except Exception:
            continue
        if mdate not in dates:
            continue
        parts = jf.replace(str(CLAUDE_PROJECTS) + "/", "").split("/")
        agent = _agent_from_path(parts[0] if parts else "")
        si = so = scc = scr = 0
        try:
            with open(jf, encoding="utf-8") as f:
                for line in f:
                    if '"usage"' not in line:
                        continue
                    try:
                        u = json.loads(line).get("message", {}).get("usage", {})
                    except Exception:
                        continue
                    if not u:
                        continue
                    si += u.get("input_tokens", 0); so += u.get("output_tokens", 0)
                    scc += u.get("cache_creation_input_tokens", 0); scr += u.get("cache_read_input_tokens", 0)
        except Exception:
            continue
        new = si + so + scc
        if new + scr <= 0:
            continue
        a = history[mdate].setdefault(agent, _blank())
        a["new"] += new; a["cache_read"] += scr
        a["input"] += si; a["output"] += so; a["cc"] += scc; a["sessions"] += 1
        if mdate == yday:
            yday_sessions.setdefault(agent, []).append((jf, new))
    return history, yday_sessions


def _codex_summary(path: str):
    cwd = None
    with open(path, encoding="utf-8", errors="ignore") as f:
        for i, line in enumerate(f):
            if i > 40:
                break
            if '"session_meta"' in line:
                try:
                    o = json.loads(line)
                    if o.get("type") == "session_meta":
                        cwd = o.get("payload", {}).get("cwd"); break
                except Exception:
                    pass
    total = None
    try:
        sz = os.path.getsize(path)
        with open(path, "rb") as f:
            f.seek(max(0, sz - 65536)); data = f.read().decode("utf-8", "ignore")
        for line in reversed(data.splitlines()):
            if '"total_token_usage"' in line:
                try:
                    o = json.loads(line); p = o.get("payload", {})
                    if p.get("type") == "token_count":
                        total = p.get("info", {}).get("total_token_usage"); break
                except Exception:
                    pass
    except Exception:
        pass
    return cwd, total


def scan_codex(dates: set, yday: str):
    history = {d: {} for d in dates}
    yday_sessions = {}
    for d in dates:
        yy, mm, dd = d.split("-")
        ddir = CODEX_SESSIONS / yy / mm / dd
        if not ddir.exists():
            continue
        for jf in ddir.glob("*.jsonl"):
            cwd, total = _codex_summary(str(jf))
            if not total:
                continue
            agent = _agent_from_path(cwd or "")
            si = total.get("input_tokens", 0); so = total.get("output_tokens", 0)
            cached = total.get("cached_input_tokens", 0)
            new = max(0, si - cached) + so
            if new + cached <= 0:
                continue
            a = history[d].setdefault(agent, _blank())
            a["new"] += new; a["cache_read"] += cached
            a["input"] += si; a["output"] += so; a["sessions"] += 1
            if d == yday:
                yday_sessions.setdefault(agent, []).append((str(jf), new, cached, cwd))
    return history, yday_sessions


def merge_history(hc, hx, dates):
    out = {}
    for d in dates:
        day = {}
        for src, prov in ((hc, "claude"), (hx, "codex")):
            for agent, agg in (src.get(d) or {}).items():
                e = day.setdefault(agent, {"new": 0, "cache_read": 0, "claude": 0, "codex": 0,
                                           "input": 0, "output": 0, "cc": 0, "sessions": 0})
                e["new"] += agg["new"]; e["cache_read"] += agg["cache_read"]
                e[prov] += agg["new"]
                e["input"] += agg["input"]; e["output"] += agg["output"]
                e["cc"] += agg.get("cc", 0); e["sessions"] += agg["sessions"]
        out[d] = day
    return out


# --------------------------------------------------------------------------- #
# baseline + anomaly (family-normalized, on new tokens)
# --------------------------------------------------------------------------- #
def compute_family_baselines(history, yday, baseline_dates):
    fams = {}
    for d in baseline_dates:
        for ag, agg in (history.get(d) or {}).items():
            fams.setdefault(family(ag), {})
            fams[family(ag)].setdefault(d, 0)
            fams[family(ag)][d] += agg["new"]
    out = {}
    for fam, byday in fams.items():
        vals = [byday.get(d, 0) for d in baseline_dates]
        nz = [v for v in vals if v > 0]
        n = len(nz); mean = sum(nz) / n if n else 0
        std = (sum((v - mean) ** 2 for v in nz) / n) ** 0.5 if n else 0
        out[fam] = {"mean": mean, "std": std, "n_days": n}
    return out


def detect_anomalies(history, fam_base, yday, drill):
    out = []
    for ag, agg in (history.get(yday) or {}).items():
        y = agg["new"]; base = fam_base.get(family(ag), {})
        mean, std, n = base.get("mean", 0), base.get("std", 0), base.get("n_days", 0)
        if n == 0:
            if y >= NEW_AGENT_FLOOR:
                out.append({"kind": "new_family_spike", "agent": ag, "family": family(ag),
                            "yesterday_new": y, "confidence": "low",
                            "why": f"패밀리 baseline 없음, 어제 새토큰 {fmt_tok(y)}",
                            "evidence": drill.get(ag, {}).get("evidence", [])})
            continue
        if y < ANOM_FLOOR_NEW:
            continue
        z = (y - mean) / std if std > 0 else (999 if y > mean * ANOM_RATIO else 0)
        ratio = y / mean if mean > 0 else 999
        if z >= ANOM_Z and ratio >= ANOM_RATIO:
            out.append({"kind": "agent_spike", "agent": ag, "family": family(ag),
                        "yesterday_new": y, "baseline_mean": mean, "baseline_days": n,
                        "z": round(z, 1), "ratio": round(ratio, 1),
                        "confidence": "high" if z >= 3.5 and n >= 4 else "medium",
                        "why": f"어제 새토큰 {fmt_tok(y)} = 패밀리 평소({fmt_tok(mean)})의 {ratio:.1f}배 (z={z:.1f}, {n}일)",
                        "evidence": drill.get(ag, {}).get("evidence", [])})
    out.sort(key=lambda a: a.get("z", 0), reverse=True)
    return out


def account_pressure(accounts, codex):
    out = []
    for a in accounts:
        for bname, field, cdf in (("5h", "h5", "h5_countdown"), ("7d", "d7", "d7_countdown")):
            pct = a.get(field)
            if pct is not None and pct >= 80:
                out.append({"who": a["email"], "bucket": bname, "pct": pct,
                            "countdown": a.get(cdf), "note": "활성" if a.get("active") else "비활성"})
    if codex:
        for bname, bd in (codex.get("buckets") or {}).items():
            if bd.get("used") is not None and bd["used"] >= 80:
                out.append({"who": "codex", "bucket": bname, "pct": bd["used"], "countdown": bd.get("countdown"), "note": ""})
    return out


# --------------------------------------------------------------------------- #
# drill-down
# --------------------------------------------------------------------------- #
def drilldown_claude(session_files, max_files=12):
    files = sorted(session_files, key=lambda x: x[1], reverse=True)[:max_files]
    tool_calls, sizes, turns, cc, cr = {}, [], 0, 0, 0
    biggest = files[0][1] if files else 0
    for jf, _n in files:
        try:
            with open(jf, encoding="utf-8") as f:
                for line in f:
                    if not line.strip():
                        continue
                    try:
                        d = json.loads(line)
                    except Exception:
                        continue
                    t, msg = d.get("type"), d.get("message", {})
                    if t == "assistant":
                        turns += 1; u = msg.get("usage", {})
                        cc += u.get("cache_creation_input_tokens", 0); cr += u.get("cache_read_input_tokens", 0)
                        for it in msg.get("content", []) or []:
                            if isinstance(it, dict) and it.get("type") == "tool_use":
                                tool_calls[it.get("name", "?")] = tool_calls.get(it.get("name", "?"), 0) + 1
                    elif t == "user":
                        c = msg.get("content")
                        if isinstance(c, list):
                            for it in c:
                                if isinstance(it, dict) and it.get("type") == "tool_result":
                                    sizes.append(len(str(it.get("content", ""))))
        except Exception:
            continue
    sizes.sort(reverse=True)
    top_tools = sorted(tool_calls.items(), key=lambda x: -x[1])[:5]
    big = [s for s in sizes if s > 10_000]
    ev = [f"{len(files)}세션, {turns}턴, 최대 세션 새토큰 {fmt_tok(biggest)}"]
    if top_tools:
        ev.append("주요 도구: " + ", ".join(f"{n}×{c}" for n, c in top_tools))
    if sizes:
        ev.append(f"최대 도구출력: {', '.join(f'{s//1000}K자' for s in sizes[:3])} (총 {sum(sizes)//1000}K자)")
    if big:
        ev.append(f"10K자↑ 원본 도구출력 {len(big)}건")
    # cache_read(재사용)는 정상·저비용($1.50/MTok)이므로 문제로 표기하지 않는다.
    # 비용을 주도하는 것은 cache_create($18.75/MTok)이므로 그쪽을 사실로만 병기.
    ev.append(f"cache_create {fmt_tok(cc)}(비용주도) · cache_read {fmt_tok(cr)}(재사용, 저비용·정상)")
    # 실질 비효율 신호: 대량 '원본' 도구출력(캐시가 아니라 새 입력/cache_create를 키움).
    flags = []
    if len(big) >= 8:
        flags.append(f"원본 도구출력 {len(big)}건(10K자↑) — 결과 크기 상한 시 cache_create 절감 여지")
    return {"evidence": ev, "flags": flags, "turns": turns, "top_tools": top_tools,
            "cache_read": cr, "cache_create": cc, "big_dumps": len(big)}


def drilldown_codex(session_files, max_files=80):
    files = session_files[:max_files]
    n_sess = len(files)
    new_tot = sum(x[1] for x in files)
    cached_tot = sum(x[2] for x in files)
    cwds = {}
    for _p, _n, _c, cwd in files:
        key = _agent_from_path(cwd or "")
        cwds[key] = cwds.get(key, 0) + 1
    top_cwds = sorted(cwds.items(), key=lambda x: -x[1])[:4]
    biggest = max((x[1] for x in files), default=0)
    # 캐시재사용(cached)은 장기 세션에서 정상이며 저비용이므로 문제로 표기하지 않는다.
    ev = [f"{n_sess}개 codex 세션, 새토큰 합 {fmt_tok(new_tot)} (캐시재사용 {fmt_tok(cached_tot)}, 저비용·정상)",
          f"최대 단일세션 새토큰 {fmt_tok(biggest)}",
          "작업 경로: " + ", ".join(f"{k}×{c}" for k, c in top_cwds)]
    # 세션 수는 fan-out 규모를 보는 중립 지표 — '과다'로 단정하지 않고 확인만 권한다.
    flags = []
    if n_sess >= 40:
        flags.append(f"동시 codex 세션 {n_sess}개(fan-out) — 의도된 병렬작업인지 확인")
    return {"evidence": ev, "flags": flags, "sessions": n_sess, "new": new_tot, "cached": cached_tot}


# --------------------------------------------------------------------------- #
def query_pcd(yday):
    if not PCD_PROD_DB.exists():
        return []
    try:
        conn = sqlite3.connect(str(PCD_PROD_DB)); conn.row_factory = sqlite3.Row
        rows = [dict(r) for r in conn.execute(
            "SELECT name, stats_tokens FROM agents WHERE stats_tokens > 0 ORDER BY stats_tokens DESC LIMIT 10").fetchall()]
        conn.close(); return rows
    except Exception:
        return []


def collect(yday):
    now = datetime.now(KST)
    baseline_dates = [(now - timedelta(days=i)).strftime("%Y-%m-%d") for i in range(2, 2 + BASELINE_DAYS)]
    dates = set(baseline_dates) | {yday}

    hc, yc = scan_claude(dates, yday)
    hx, yx = scan_codex(dates, yday)
    history = merge_history(hc, hx, dates)

    yagents = history.get(yday) or {}
    ranked = sorted(yagents.items(), key=lambda kv: kv[1]["new"], reverse=True)

    drill = {}
    for ag, _agg in ranked[:3]:
        d = {}
        if ag in yc:
            dc = drilldown_claude(yc[ag])
            d = dc
        if ag in yx:
            dx = drilldown_codex(yx[ag])
            if d:
                d = {"evidence": d["evidence"] + dx["evidence"], "flags": d["flags"] + dx["flags"]}
            else:
                d = dx
        if d:
            drill[ag] = d

    fam_base = compute_family_baselines(history, yday, baseline_dates)
    accounts = get_accounts()
    codex = get_codex_rate()

    resets = detect_resets_from_snapshots(load_snapshots(days=2))
    codex_boot = None if any(r["who"] == "codex" for r in resets) else codex_reset_count_bootstrap()

    anomalies = detect_anomalies(history, fam_base, yday, drill)
    pressure = account_pressure(accounts, codex)

    rows = []
    for ag, agg in ranked[:8]:
        fb = fam_base.get(family(ag), {})
        rows.append({"agent": ag, "new": agg["new"], "cache_read": agg["cache_read"],
                     "claude": agg["claude"], "codex": agg["codex"],
                     "input": agg["input"], "output": agg["output"], "cc": agg["cc"],
                     "sessions": agg["sessions"],
                     "usd_equiv": round(usd_equiv(agg["input"], agg["output"], agg["cc"], agg["cache_read"]), 2),
                     "family_mean": round(fb.get("mean", 0))})

    return {
        "report_date": yday, "generated_at": now.isoformat(),
        "accounts": accounts, "codex_rate": codex,
        "resets": resets, "codex_reset_bootstrap": codex_boot,
        "snapshot_history_available": (HIST_DIR / f"snapshots-{yday}.jsonl").exists(),
        "account_pressure": pressure,
        "yesterday": {
            "day_new": sum(a["new"] for a in yagents.values()),
            "day_cache_read": sum(a["cache_read"] for a in yagents.values()),
            "agent_count": len(yagents),
            "by_provider_new": {"claude": sum(a["claude"] for a in yagents.values()),
                                "codex": sum(a["codex"] for a in yagents.values())},
            "top_agents": rows,
        },
        "anomalies": anomalies, "drilldown": drill,
        "baseline_window_days": BASELINE_DAYS, "pcd_cumulative": query_pcd(yday),
    }


# --------------------------------------------------------------------------- #
def render(data):
    L = [f"**[Token Manager] 일일 리포트 — {data['report_date']}**", ""]
    L.append("**Claude 계정 (cswap 3개)**")
    for a in data["accounts"]:
        star = "🟢활성" if a["active"] else "⚪"
        sc = ""
        if a.get("scoped"):
            sc = " / " + " ".join(f"{s['name']} {s['pct']:.0f}%" for s in a["scoped"] if s.get("pct") is not None)
        L.append(f"- {star} #{a['n']} {a['email']}: 5h **{a['h5']:.0f}%**({a['h5_countdown']}) · 7d **{a['d7']:.0f}%**({a['d7_countdown']}){sc}")
    cx = data.get("codex_rate")
    if cx:
        parts = [f"{bn} **{bd['used']:.0f}%**" + (f"({bd['countdown']})" if bd.get("countdown") else "")
                 for bn, bd in (cx.get("buckets") or {}).items() if bd.get("used") is not None]
        L.append("- 🤖 Codex: " + " · ".join(parts) + (" ⚠️stale" if cx.get("stale") else ""))
    L.append("")

    weekly = [r for r in data["resets"] if r.get("kind") == "weekly"]
    boot = data.get("codex_reset_bootstrap")
    if weekly or boot:
        L.append("**리셋 감지**")
        for r in weekly:
            who = "Codex" if r["who"] == "codex" else r["who"]
            L.append(f"- 🔴 {who} 주간(7d): {r['from_pct']:.0f}%→{r['to_pct']:.0f}% @{r['at']} — 수동/조기 리셋 추정")
        if boot:
            la = f", 최근 {boot['last_at']}" if boot.get("last_at") else ""
            L.append(f"- Codex 5h: 최근 {boot['window_h']}h간 {boot['count']}회 리셋(5h 주기, 정상{la})")
        L.append("")
    elif not data.get("snapshot_history_available"):
        L.append("_주간(7d)·계정별 리셋 감지는 스냅샷 축적 후 활성화됩니다._"); L.append("")

    y = data["yesterday"]; bp = y["by_provider_new"]
    L.append(f"**어제 새토큰 소비 — {y['agent_count']}개 에이전트, 총 {fmt_tok(y['day_new'])}** _(캐시재사용 {fmt_tok(y['day_cache_read'])} 별도)_")
    L.append(f"- 프로바이더: claude {fmt_tok(bp['claude'])} / codex {fmt_tok(bp['codex'])}")
    L.append("- Top 소비 에이전트(새토큰):")
    for i, r in enumerate(y["top_agents"][:6], 1):
        base = f", 평소 {fmt_tok(r['family_mean'])}" if r["family_mean"] else ""
        L.append(f"  {i}. {r['agent']} — {fmt_tok(r['new'])} ({r['sessions']}세션{base})")
    L.append("")

    if data["drilldown"]:
        L.append("**Top 소비자 심층분석**")
        for ag, d in data["drilldown"].items():
            L.append(f"- {ag}:")
            for e in d["evidence"]:
                L.append(f"  · {e}")
            if d.get("flags"):
                L.append(f"  ⚠️ {', '.join(d['flags'])}")
        L.append("")

    issues = []
    for p in data["account_pressure"]:
        issues.append(f"⚠️ {p['who']} {p['bucket']} {p['pct']:.0f}% — quota 임박({p.get('note','')}, {p.get('countdown','')})")
    for a in data["anomalies"]:
        issues.append(f"⚠️ [{a.get('confidence')}] {a['agent']}: {a['why']}")
        for e in a.get("evidence", [])[:3]:
            issues.append(f"    · {e}")
    if issues:
        L.append("**특이사항 (근거 기반)**")
        L += [x if x.startswith("    ") else f"- {x}" for x in issues]
    else:
        L.append("**특이사항**: 없음 — baseline 대비 정상, quota 여유")
    return "\n".join(L)


def send_to_discord(message):
    chunks, cur = [], ""
    for para in message.split("\n\n"):
        cand = (cur + "\n\n" + para).strip() if cur else para
        if len(cand) <= 1900:
            cur = cand
        else:
            if cur:
                chunks.append(cur)
            cur = para[:1900]
    if cur:
        chunks.append(cur)
    ok = True
    for chunk in chunks:
        payload = json.dumps({"target": f"channel:{TOKEN_MANAGER_CHANNEL}", "content": chunk,
                              "source": "token-manager", "bot": "notify"}).encode()
        try:
            req = urllib.request.Request(f"{API}/discord/send", data=payload,
                                         headers={"Content-Type": "application/json"})
            r = json.loads(urllib.request.urlopen(req, timeout=10).read())
            if not r.get("ok"):
                ok = False
        except Exception as e:  # noqa: BLE001
            print(f"send failed: {e}", file=sys.stderr); ok = False
    return ok


def main():
    now = datetime.now(KST)
    yday = (now - timedelta(days=1)).strftime("%Y-%m-%d")
    data = collect(yday)
    if "--raw-json" in sys.argv:
        print(json.dumps(data, ensure_ascii=False, indent=2)); return
    report = render(data)
    if "--emit" in sys.argv:
        # single collect -> both the deterministic body and the raw JSON, for the
        # .sh to hand the JSON to a constrained LLM recommendations pass.
        print("<<<REPORT>>>"); print(report)
        print("<<<RAWJSON>>>"); print(json.dumps(data, ensure_ascii=False))
        return
    if "--dry-run" in sys.argv:
        print(report); return
    print(f"sent {yday}" if send_to_discord(report) else "send failed")


if __name__ == "__main__":
    main()
