#!/usr/bin/env python3
"""Read-only gate for deleting a legacy body bundle: since a given time, did one host
carry enough business turns and at least one restart while TUI channels sent no legacy body?

Inputs are files so a coordinator can run it on snapshots taken from another host:
  --census  `/api/health/detail` JSON (or its `.legacy_body_census` object)
  --turns   bundle written by `fetch-turns` from `/api/agents` + `/api/agents/{id}/transcripts`

Verdicts: PASS (all three hold on complete evidence), FAIL (a TUI legacy body send was
seen), INCOMPLETE (evidence missing/truncated/not covering `--since`, or thresholds unmet).
UnknownBody sends never decide the verdict; they are listed and flagged REVIEW_NEEDED.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
import urllib.parse
import urllib.request
from collections import Counter

LEGACY_CLASSES = {"LegacyBody", "RecoveredLegacyBody", "LegacyBodyRefresh"}
# Doctor startup inventory does not carry send events, so it cannot hide a legacy send.
NON_BLOCKING_ERRORS = {"START_INVENTORY_UNAVAILABLE"}
# `clamp_api_limit` caps every transcripts page at this many rows.
API_MAX_LIMIT = 2000
TURN_FIELDS = ("id", "turn_id", "channel_id", "session_key", "provider", "created_at", "duration_ms")


def parse_since(text):
    at = dt.datetime.fromisoformat(text)
    if at.tzinfo is None:
        raise argparse.ArgumentTypeError("--since needs an explicit UTC offset")
    return int(at.timestamp() * 1000)


def iso(ms):
    if ms is None:
        return "-"
    zone = dt.timezone(dt.timedelta(hours=9))
    return dt.datetime.fromtimestamp(ms / 1000, zone).isoformat(timespec="seconds")


def load_census(path):
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
    return data.get("legacy_body_census", data) if isinstance(data, dict) else data


def census_part(census, since):
    """Window coverage, restarts, and TUI legacy/unknown body sends inside [since, cut]."""
    blockers = []
    if not isinstance(census, dict):
        return {"blockers": ["CENSUS_NOT_OBJECT"], "restarts": 0, "legacy": [], "unknown": [],
                "tui_channels": set(), "window": (None, None), "host": None}
    files = census.get("boot_files")
    if census.get("evidence_truncated") is not False:
        blockers.append("EVIDENCE_TRUNCATED")
    if census.get("stale_snapshot") is not False:
        blockers.append("STALE_SNAPSHOT")
    if not isinstance(files, list) or not files:
        blockers.append("BOOT_FILES_MISSING")
        files = []
    errors = census.get("errors")
    if not isinstance(errors, list):
        blockers.append("CENSUS_ERRORS_UNKNOWN")
        errors = []
    blockers += [f"CENSUS_ERROR:{e}" for e in errors if e not in NON_BLOCKING_ERRORS]
    start = (census.get("coverage_start") or {}).get("started_at_ms")
    cut = census.get("cut_ms")
    if not isinstance(start, int) or not isinstance(cut, int) or start > since or cut < since:
        blockers.append("WINDOW_NOT_COVERING_SINCE")
    cut = cut if isinstance(cut, int) else since
    boots = {}
    for file in files:
        header = file.get("header") or {}
        boots[header.get("boot_id")] = file
        if (file.get("window") or {}).get("invalid") is not False:
            blockers.append(f"JOURNAL_INVALID:{header.get('boot_id')}")
    # Every boot from the current one back to the one running at `since` must be present.
    cursor, seen = census.get("current_boot_id"), set()
    while True:
        file = boots.get(cursor)
        if file is None or cursor in seen:
            blockers.append(f"BOOT_CHAIN_GAP:{cursor}")
            break
        seen.add(cursor)
        header = file["header"]
        if not isinstance(header.get("started_at_ms"), int):
            blockers.append(f"BOOT_TIME_UNKNOWN:{cursor}")
            break
        if header["started_at_ms"] <= since:
            break
        cursor = header.get("predecessor_boot_id")
    live, current = census.get("live_journal") or {}, boots.get(census.get("current_boot_id")) or {}
    if live.get("invalid") is not False or live.get("enqueued_cut") != (current.get("window") or {}).get("cut"):
        blockers.append("LIVE_JOURNAL_NOT_DRAINED")
    started = [(f.get("header") or {}).get("started_at_ms") for f in files]
    restarts = sum(1 for at in started if isinstance(at, int) and since < at <= cut)
    legacy, unknown, tui_channels = [], [], set()
    for file in files:
        selection = file.get("selection") or {}
        bindings = {int(c) for c in selection.get("tui_bindings") or []}
        tui_channels |= bindings
        for event in file.get("targets") or []:
            tag, at = event.get("census") or {}, event.get("at_ms")
            klass = tag.get("output_class")
            if klass not in LEGACY_CLASSES and klass != "UnknownBody":
                continue
            if not isinstance(at, int):
                blockers.append("EVENT_TIME_UNKNOWN")
                continue
            if not since <= at <= cut:
                continue
            channel = event.get("channel")
            tui = tag.get("binding_mode") == "TUI" or channel in bindings
            row = {"at_ms": at, "channel": channel, "site": tag.get("site"), "op": event.get("op"),
                   "phase": event.get("phase"), "output_class": klass, "binding_mode": tag.get("binding_mode"),
                   "actual_runtime": tag.get("actual_runtime"), "reason": tag.get("reason"), "tui": tui}
            if klass == "UnknownBody":
                unknown.append(row)
            elif tui and not (tag.get("site") == "W17" and tag.get("reason") == "NON_MEMBER"):
                legacy.append(row)
    return {"blockers": sorted(set(blockers)), "restarts": restarts, "legacy": legacy,
            "unknown": unknown, "tui_channels": tui_channels, "window": (start, cut),
            "host": census.get("host")}


def turn_end(row):
    """Completion time; `created_at` is UTC text (to_char of a timestamptz on a UTC session)."""
    created = dt.datetime.strptime(row["created_at"], "%Y-%m-%d %H:%M:%S")
    return int(created.replace(tzinfo=dt.timezone.utc).timestamp() * 1000)


def turns_part(bundle, since, cut, excluded_agents, excluded_channels, tui_channels):
    blockers, total, tui, truncated = [], 0, 0, []
    hosts = Counter()
    limit = bundle.get("limit") if isinstance(bundle, dict) else None
    agents = bundle.get("agents") if isinstance(bundle, dict) else None
    if not isinstance(limit, int) or not isinstance(agents, list):
        return {"blockers": ["TURNS_BUNDLE_INVALID"], "total": 0, "tui": 0, "truncated": [],
                "hosts": hosts}
    for agent in agents:
        agent_id, rows = agent.get("agent_id"), agent.get("transcripts")
        if not isinstance(rows, list):
            blockers.append(f"TURNS_AGENT_INVALID:{agent_id}")
            continue
        if agent_id in excluded_agents:
            continue
        ends = []
        for row in rows:
            try:
                end = turn_end(row)
            except (KeyError, TypeError, ValueError):
                blockers.append(f"TURN_TIME_UNKNOWN:{agent_id}")
                continue
            ends.append(end)
            channel = str(row.get("channel_id") or "")
            if channel in excluded_channels or not since <= end <= cut:
                continue
            total += 1
            tui += channel.isdigit() and int(channel) in tui_channels
            hosts[str(row.get("session_key") or "").rsplit("/", 1)[-1].split(":", 1)[0] or "?"] += 1
        # A full page whose oldest turn is not before `since` may hide older in-window turns.
        if len(rows) >= min(limit, API_MAX_LIMIT) and ends and min(ends) >= since:
            truncated.append(agent_id)
    return {"blockers": sorted(set(blockers)), "total": total, "tui": tui, "truncated": truncated,
            "hosts": hosts}


def evaluate(census, bundle, since, min_turns, excluded_agents=(), excluded_channels=()):
    c = census_part(census, since)
    t = turns_part(bundle, since, c["window"][1] or since, set(excluded_agents),
                   {str(ch) for ch in excluded_channels}, c["tui_channels"])
    reasons = c["blockers"] + t["blockers"]
    if t["total"] < min_turns:
        reasons.append("TURNS_TRUNCATED_BELOW_THRESHOLD" if t["truncated"] else "TURNS_BELOW_THRESHOLD")
    if c["restarts"] < 1:
        reasons.append("NO_RESTART_SINCE")
    verdict = "FAIL" if c["legacy"] else ("INCOMPLETE" if reasons else "PASS")
    return {"verdict": verdict, "reasons": reasons, "review_needed": bool(c["unknown"]),
            "host": c["host"], "since_ms": since, "window": c["window"], "restarts": c["restarts"],
            "turns": t, "legacy": c["legacy"], "unknown": c["unknown"], "min_turns": min_turns}


def render(report):
    t, (start, end) = report["turns"], report["window"]
    flag = " REVIEW_NEEDED" if report["review_needed"] else ""
    lines = [f"VERDICT {report['verdict']}{flag}",
             f"host={report['host']} since={iso(report['since_ms'])} window={iso(start)}..{iso(end)}",
             f"turns={t['total']} (min {report['min_turns']}, tui_channels={t['tui']}, "
             f"hosts={dict(sorted(t['hosts'].items()))}, full_pages={t['truncated']})",
             f"restarts={report['restarts']}",
             f"tui_legacy_sends={len(report['legacy'])}",
             *(f"  {iso(r['at_ms'])} ch={r['channel']} site={r['site']} {r['op']}/{r['phase']} "
               f"{r['output_class']} binding={r['binding_mode']} runtime={r['actual_runtime']}"
               for r in report["legacy"]),
             f"unknown_body={len(report['unknown'])} channels="
             f"{dict(sorted(Counter(r['channel'] for r in report['unknown']).items()))}",
             *(f"  {iso(r['at_ms'])} ch={r['channel']} tui={r['tui']} {r['op']}/{r['phase']}"
               for r in report["unknown"]),
             f"reasons={report['reasons']}"]
    return "\n".join(lines)


def fetch_turns(api, limit):
    def get(path):
        with urllib.request.urlopen(api.rstrip("/") + path, timeout=60) as response:
            return json.load(response)
    agents = []
    for agent in get("/api/agents")["agents"]:
        quoted = urllib.parse.quote(agent["id"], safe="")
        rows = get(f"/api/agents/{quoted}/transcripts?limit={limit}")["transcripts"]
        agents.append({"agent_id": agent["id"],
                       "transcripts": [{k: row.get(k) for k in TURN_FIELDS} for row in rows]})
    fetched = int(dt.datetime.now(dt.timezone.utc).timestamp() * 1000)
    return {"source": api, "fetched_at_ms": fetched, "limit": limit, "agents": agents}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    check = sub.add_parser("check", help="judge census + turn bundle files")
    check.add_argument("--census", required=True)
    check.add_argument("--turns", required=True)
    check.add_argument("--since", required=True, type=parse_since)
    check.add_argument("--min-turns", required=True, type=int)
    check.add_argument("--exclude-agent", action="append", default=[])
    check.add_argument("--exclude-channel", action="append", default=[])
    check.add_argument("--json", action="store_true")
    fetch = sub.add_parser("fetch-turns", help="GET turn history into a bundle file")
    fetch.add_argument("--api", required=True)
    fetch.add_argument("--out", required=True)
    fetch.add_argument("--limit", type=int, default=2000)
    args = parser.parse_args(argv)
    if args.command == "fetch-turns":
        with open(args.out, "w", encoding="utf-8") as handle:
            json.dump(fetch_turns(args.api, args.limit), handle, ensure_ascii=False)
        return 0
    with open(args.turns, encoding="utf-8") as handle:
        bundle = json.load(handle)
    report = evaluate(load_census(args.census), bundle, args.since, args.min_turns,
                      args.exclude_agent, args.exclude_channel)
    if args.json:
        report["turns"]["hosts"] = dict(report["turns"]["hosts"])
        print(json.dumps(report, ensure_ascii=False, indent=1))
    else:
        print(render(report))
    return {"PASS": 0, "FAIL": 1}.get(report["verdict"], 2)


if __name__ == "__main__":
    sys.exit(main())
