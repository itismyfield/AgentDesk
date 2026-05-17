#!/usr/bin/env python3
"""Token Manager Daily Report — 에이전트 세션 없이 DB 직접 조회로 리포트 생성."""
from __future__ import annotations

import json
import glob
import os
import re
import sqlite3
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional

KST = timezone(timedelta(hours=9))
HOME = Path.home()
PCD_PROD_DB = HOME / ".local/state/pixel-claw-dashboard/prod/pixel-claw-dashboard.sqlite"
ADK_DB = HOME / ".adk/release/data/agentdesk.sqlite"
CLAUDE_CACHE_DIR = HOME / ".cache/claude-dashboard"
CLAUDE_PROJECTS = HOME / ".claude/projects"
CODEX_SESSIONS = HOME / ".codex" / "sessions"
TOKEN_MANAGER_CHANNEL = "1481478222439907560"
ADK_API = "http://127.0.0.1:8791/api/discord/send"


def _agent_from_path(path: str) -> str:
    """Extract agent/workspace name from project dir or cwd path."""
    for sep in ("-workspaces-", "/workspaces/"):
        if sep in path:
            tail = path.split(sep)[-1]
            return tail.split("/")[0]
    for sep in ("-worktrees-", "/worktrees/"):
        if sep in path:
            tail = path.split(sep)[-1]
            name = tail.split("/")[0]
            name = re.sub(r"^(codex|claude)-", "", name)
            name = re.sub(r"-\d{8}-\d{6}$", "", name)
            return name
    last = path.rstrip("/").split("/")[-1].lstrip("-")
    if last.startswith("Users-"):
        return "(personal)"
    return last or "unknown"


def query_pcd_db(yesterday_str: str) -> dict:
    """PCD 프로덕션 DB에서 에이전트별 토큰 현황 조회."""
    if not PCD_PROD_DB.exists():
        return {"agents": [], "sessions": [], "daily": []}

    conn = sqlite3.connect(str(PCD_PROD_DB))
    conn.row_factory = sqlite3.Row

    agents = [
        dict(r) for r in conn.execute(
            "SELECT name, stats_tokens, stats_xp FROM agents WHERE stats_tokens > 0 ORDER BY stats_tokens DESC"
        ).fetchall()
    ]

    sessions = [
        dict(r) for r in conn.execute(
            """SELECT session_key, name, provider, model, tokens, stats_xp, linked_agent_id,
                      datetime(connected_at/1000, 'unixepoch', 'localtime') as connected,
                      datetime(last_seen_at/1000, 'unixepoch', 'localtime') as last_seen
               FROM dispatched_sessions
               WHERE date(last_seen_at/1000, 'unixepoch', 'localtime') = ?
               ORDER BY tokens DESC""",
            (yesterday_str,),
        ).fetchall()
    ]

    daily = [
        dict(r) for r in conn.execute(
            "SELECT agent_id, tasks_done, xp_earned, skill_calls FROM daily_activity WHERE date = ?",
            (yesterday_str,),
        ).fetchall()
    ]

    conn.close()
    return {"agents": agents, "sessions": sessions, "daily": daily}


def get_rate_limit() -> dict | None:
    """ADK API /api/rate-limits에서 실시간 rate limit 조회. Claude/Codex 분리.

    SQLite의 rate_limit_cache 테이블은 2026-04-19 PostgreSQL cutover 이후 미갱신.
    런타임은 PG에 쓰고 API가 PG를 읽으므로 API를 호출해야 fresh 데이터를 얻는다.
    """
    import urllib.request
    try:
        req = urllib.request.Request(
            "http://127.0.0.1:8791/api/rate-limits",
            headers={"Accept": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            payload = json.loads(resp.read())
    except Exception:
        return None

    result = {}
    for entry in payload.get("providers", []):
        provider = entry.get("provider")
        if provider not in ("claude", "codex"):
            continue
        for bucket in entry.get("buckets", []):
            name = bucket.get("name", "")
            key = f"{provider}_{name.replace(' ', '_').replace('-', '_').lower()}"
            result[key] = {
                "provider": provider,
                "label": f"{provider.capitalize()} {name}",
                "utilization": bucket.get("used", 0),
                "remaining": bucket.get("remaining", 0),
                "limit": bucket.get("limit", 100),
                "resets_at": datetime.fromtimestamp(bucket["reset"], tz=timezone.utc).isoformat() if bucket.get("reset") else "",
            }
    return result if result else None


def _parse_codex_session(filepath: str) -> dict | None:
    """Stream-parse a Codex JSONL, extracting only token_count + session_meta."""
    meta = None
    last_tc_info = None
    first_tc_rl = None
    last_tc_rl = None

    with open(filepath) as f:
        for line in f:
            if '"session_meta"' in line:
                try:
                    obj = json.loads(line)
                    if obj.get("type") == "session_meta":
                        meta = obj.get("payload", {})
                except json.JSONDecodeError:
                    pass
            elif '"token_count"' in line:
                try:
                    obj = json.loads(line)
                    if obj.get("type") != "event_msg":
                        continue
                    p = obj.get("payload", {})
                    if p.get("type") != "token_count":
                        continue
                    if p.get("info"):
                        last_tc_info = p["info"]
                    rl = p.get("rate_limits", {})
                    if rl:
                        if first_tc_rl is None:
                            first_tc_rl = rl
                        last_tc_rl = rl
                except json.JSONDecodeError:
                    pass

    if not meta:
        return None

    cwd = meta.get("cwd", "")
    usage = last_tc_info.get("total_token_usage", {}) if last_tc_info else {}
    si = usage.get("input_tokens", 0)
    so = usage.get("output_tokens", 0)
    if si == 0 and so == 0:
        return None

    weekly_delta = None
    if first_tc_rl and last_tc_rl:
        w_start = first_tc_rl.get("secondary", {}).get("used_percent", 0)
        w_end = last_tc_rl.get("secondary", {}).get("used_percent", 0)
        weekly_delta = w_end - w_start

    return {
        "provider": "codex",
        "project": _agent_from_path(cwd),
        "model": "codex",
        "input": si,
        "output": so,
        "cache_create": 0,
        "cache_read": usage.get("cached_input_tokens", 0),
        "total": usage.get("total_tokens", 0) or (si + so),
        "weekly_pct_delta": weekly_delta,
    }


def analyze_jsonl(yesterday_str: str) -> dict:
    """로컬 JSONL에서 어제 세션 토큰 분석 (Claude Code + Codex 통합)."""
    sessions = []

    # --- Claude Code sessions ---
    for jsonl_path in glob.glob(str(CLAUDE_PROJECTS / "**" / "*.jsonl"), recursive=True):
        mtime = os.path.getmtime(jsonl_path)
        mdate = datetime.fromtimestamp(mtime).strftime("%Y-%m-%d")
        if mdate != yesterday_str:
            continue

        si = so = scc = scr = 0
        smodel = ""
        parts = jsonl_path.replace(str(CLAUDE_PROJECTS) + "/", "").split("/")
        proj_dir = parts[0] if parts else ""
        sproj = _agent_from_path(proj_dir)

        try:
            with open(jsonl_path) as f:
                for line in f:
                    try:
                        d = json.loads(line.strip())
                        msg = d.get("message", {})
                        u = msg.get("usage", {})
                        if u:
                            si += u.get("input_tokens", 0)
                            so += u.get("output_tokens", 0)
                            scc += u.get("cache_creation_input_tokens", 0)
                            scr += u.get("cache_read_input_tokens", 0)
                            smodel = msg.get("model", smodel)
                    except Exception:
                        pass
        except Exception:
            pass

        if si > 0 or so > 0:
            sessions.append({
                "provider": "claude",
                "project": sproj, "model": smodel,
                "input": si, "output": so,
                "cache_create": scc, "cache_read": scr,
                "total": si + so,
            })

    # --- Codex sessions ---
    yparts = yesterday_str.split("-")
    if len(yparts) == 3:
        codex_day_dir = CODEX_SESSIONS / yparts[0] / yparts[1] / yparts[2]
        if codex_day_dir.exists():
            for jf in sorted(codex_day_dir.glob("*.jsonl")):
                try:
                    result = _parse_codex_session(str(jf))
                    if result:
                        sessions.append(result)
                except Exception:
                    pass

    # Aggregate by (provider, project)
    agg = {}
    for s in sessions:
        key = (s.get("provider", "unknown"), s["project"])
        if key not in agg:
            agg[key] = {
                "provider": s.get("provider", "unknown"),
                "project": s["project"],
                "model": s["model"],
                "input": 0, "output": 0,
                "cache_create": 0, "cache_read": 0,
                "total": 0, "session_count": 0,
            }
        a = agg[key]
        a["input"] += s["input"]
        a["output"] += s["output"]
        a["cache_create"] += s.get("cache_create", 0)
        a["cache_read"] += s.get("cache_read", 0)
        a["total"] += s["total"]
        a["session_count"] += 1

    aggregated = sorted(agg.values(), key=lambda x: x["total"], reverse=True)

    # Provider subtotals
    by_provider = {}
    for a in aggregated:
        p = a["provider"]
        by_provider[p] = by_provider.get(p, 0) + a["total"]

    return {
        "count": len(sessions),
        "agent_count": len(aggregated),
        "total_input": sum(s["input"] for s in sessions),
        "total_output": sum(s["output"] for s in sessions),
        "total": sum(s["total"] for s in sessions),
        "top5": aggregated[:5],
        "by_provider": by_provider,
    }


def format_tokens(n: int) -> str:
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}K"
    return str(n)


def build_report(yesterday_str: str) -> str:
    now_kst = datetime.now(KST)
    pcd = query_pcd_db(yesterday_str)
    rate = get_rate_limit()
    jsonl = analyze_jsonl(yesterday_str)

    lines = [f"**[Token Manager] 일일 리포트 — {yesterday_str}**"]
    lines.append("")

    # Rate Limit (Claude/Codex 분리)
    if rate:
        for provider in ["claude", "codex"]:
            provider_items = {k: v for k, v in rate.items() if v.get("provider") == provider}
            if not provider_items:
                continue
            lines.append(f"**{provider.capitalize()} Rate Limit**")
            for k, v in provider_items.items():
                util = v.get("utilization", "?")
                label = v.get("label", k)
                reset = v.get("resets_at", "")
                if reset:
                    try:
                        reset_dt = datetime.fromisoformat(reset.replace("Z", "+00:00"))
                        remain_h = (reset_dt - now_kst).total_seconds() / 3600
                        remain_d = int(remain_h // 24)
                        remain_hh = remain_h % 24
                        if remain_d > 0:
                            lines.append(f"- {label}: **{util}%** ({remain_d}d {remain_hh:.0f}h 후 리셋)")
                        else:
                            lines.append(f"- {label}: **{util}%** ({remain_hh:.1f}h 후 리셋)")
                    except Exception:
                        lines.append(f"- {label}: **{util}%**")
                else:
                    lines.append(f"- {label}: **{util}%**")
            lines.append("")

    # JSONL 기반 어제 사용량
    if jsonl["count"] > 0:
        agent_count = jsonl.get("agent_count", "?")
        lines.append(f"**어제 세션 토큰 ({jsonl['count']}개 세션, {agent_count}개 에이전트)**")
        lines.append(f"- 입력: {format_tokens(jsonl['total_input'])} / 출력: {format_tokens(jsonl['total_output'])} / 총합: **{format_tokens(jsonl['total'])}**")
        # Provider subtotals
        by_provider = jsonl.get("by_provider", {})
        if len(by_provider) > 1:
            parts_str = " / ".join(f"{p}: {format_tokens(t)}" for p, t in sorted(by_provider.items(), key=lambda x: -x[1]))
            lines.append(f"- 프로바이더별: {parts_str}")
        if jsonl["top5"]:
            lines.append("- Top 5:")
            for i, s in enumerate(jsonl["top5"]):
                provider = s.get("provider", "?")
                model = s["model"] or "?"
                # shorten model name
                if provider == "codex":
                    m = "codex"
                elif "opus" in model:
                    m = "opus"
                elif "sonnet" in model:
                    m = "sonnet"
                elif "haiku" in model:
                    m = "haiku"
                elif "synthetic" in model:
                    m = "synthetic"
                else:
                    m = model.split("/")[-1][:15]
                sc = s.get("session_count", 1)
                sc_str = f", {sc}세션" if sc > 1 else ""
                lines.append(f"  {i + 1}. {s['project']} ({m}{sc_str}) — {format_tokens(s['total'])}")
        lines.append("")
    else:
        lines.append("**어제 로컬 JSONL 세션**: 없음")
        lines.append("")

    # PCD 에이전트별 누적 토큰
    if pcd["agents"]:
        lines.append("**에이전트 누적 토큰 (리셋 이후)**")
        total_all = sum(a["stats_tokens"] for a in pcd["agents"])
        for a in pcd["agents"][:10]:
            pct = (a["stats_tokens"] / total_all * 100) if total_all > 0 else 0
            lines.append(f"- {a['name']}: {format_tokens(a['stats_tokens'])} ({pct:.0f}%)")
        lines.append(f"- **전체**: {format_tokens(total_all)}")
        lines.append("")

    # PCD 세션 (어제)
    if pcd["sessions"]:
        active = [s for s in pcd["sessions"] if s.get("tokens", 0) > 0]
        if active:
            lines.append(f"**어제 PCD 세션 (토큰 기록 {len(active)}건)**")
            for s in active[:5]:
                lines.append(f"- {s['name']} ({s['provider']}) — {format_tokens(s['tokens'])}")
            lines.append("")

    # 특이사항 / 효율화 제안
    anomalies = []

    if rate:
        for k, v in rate.items():
            u = v.get("utilization", 0)
            if u >= 80:
                label = v.get("label", k)
                anomalies.append(f"Rate limit {label} {u}% — 조절 필요")

    if pcd["agents"]:
        total_all = sum(a["stats_tokens"] for a in pcd["agents"])
        if total_all > 0:
            top = pcd["agents"][0]
            top_pct = top["stats_tokens"] / total_all * 100
            if top_pct > 70:
                anomalies.append(f"{top['name']}이 전체의 {top_pct:.0f}%를 차지 — 컨텍스트 효율 점검 권고")

    if jsonl["total"] > 10_000_000:
        anomalies.append(f"일일 토큰 {format_tokens(jsonl['total'])} — 평소 대비 높음")

    if jsonl["count"] > 0:
        output_ratio = jsonl["total_output"] / max(jsonl["total_input"], 1)
        if output_ratio > 15:
            anomalies.append(f"출력/입력 비율 {output_ratio:.0f}x — 불필요한 대량 생성 가능성")

    if anomalies:
        lines.append("**특이사항**")
        for a in anomalies:
            lines.append(f"- {a}")
    else:
        lines.append("**특이사항**: 없음 — 정상 범위")

    return "\n".join(lines)


def send_to_discord(message: str) -> bool:
    """PCD API로 Discord 채널에 리포트 전송."""
    # 2000자 제한 처리
    chunks = []
    current = ""
    for para in message.split("\n\n"):
        candidate = (current + "\n\n" + para).strip() if current else para
        if len(candidate) <= 1900:
            current = candidate
        else:
            if current:
                chunks.append(current)
            current = para[:1900] if len(para) > 1900 else para
    if current:
        chunks.append(current)

    import urllib.request

    success = True
    for chunk in chunks:
        payload = json.dumps({
            "target": f"channel:{TOKEN_MANAGER_CHANNEL}",
            "content": chunk,
            "source": "token-manager",
            "bot": "notify",
        }).encode()
        req = urllib.request.Request(
            ADK_API,
            data=payload,
            headers={"Content-Type": "application/json"},
        )
        try:
            resp = urllib.request.urlopen(req, timeout=10)
            result = json.loads(resp.read())
            if not result.get("ok"):
                print(f"ADK send not ok: {result}", file=sys.stderr)
                success = False
        except Exception as e:
            print(f"Send failed: {e}", file=sys.stderr)
            success = False
    return success


def collect_raw_data(yesterday_str: str) -> dict:
    """원시 데이터를 JSON으로 수집 (Claude 분석용)."""
    now_kst = datetime.now(KST)
    pcd = query_pcd_db(yesterday_str)
    rate = get_rate_limit()
    jsonl = analyze_jsonl(yesterday_str)

    # rate limit에 남은 시간 추가
    rate_with_remaining = {}
    if rate:
        for k, v in rate.items():
            entry = dict(v)
            reset = v.get("resets_at", "")
            if reset:
                try:
                    reset_dt = datetime.fromisoformat(reset.replace("Z", "+00:00"))
                    entry["remaining_hours"] = round((reset_dt - now_kst).total_seconds() / 3600, 1)
                except Exception:
                    pass
            rate_with_remaining[k] = entry
    rate = rate_with_remaining

    return {
        "report_date": yesterday_str,
        "rate_limit": rate,
        "jsonl_sessions": {
            "count": jsonl["count"],
            "total_input": jsonl["total_input"],
            "total_output": jsonl["total_output"],
            "total": jsonl["total"],
            "top5": jsonl["top5"],
        },
        "pcd_agents": pcd["agents"],
        "pcd_sessions_yesterday": [
            {k: s[k] for k in ["name", "provider", "model", "tokens", "connected", "last_seen"]}
            for s in pcd["sessions"] if s.get("tokens", 0) > 0
        ][:10],
        "daily_activity": pcd["daily"],
    }


def main():
    now_kst = datetime.now(KST)
    yesterday = (now_kst - timedelta(days=1)).strftime("%Y-%m-%d")

    if "--raw-json" in sys.argv:
        data = collect_raw_data(yesterday)
        print(json.dumps(data, ensure_ascii=False, indent=2))
        return

    dry_run = "--dry-run" in sys.argv

    report = build_report(yesterday)

    if dry_run:
        print(report)
        return

    if send_to_discord(report):
        print(f"Token daily report sent for {yesterday}")
    else:
        print("Failed to send report", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
