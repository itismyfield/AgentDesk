import { describe, expect, it } from "vitest";
import type { DispatchedSession } from "../../types";
import { canCleanupStaleIdleSession, canReconcileStaleSession, compareSessions, isStaleIdleSession, sessionState, sessionSummaryCounts, sessionTitle } from "./SessionPanelModel";

const NOW = Date.now();
const DAY = 86_400_000;
const KEY = "claude/discord_c2/MacBook-Pro-14:AgentDesk-claude-adk-cc-t1570308839910412348";
const row = (id: string, status: string, ago: number | null, key = KEY, session_info: string | null = null) =>
  ({ id, session_key: key, name: key, status, session_info, last_seen_at: ago === null ? null : new Date(NOW - ago).toISOString(), connected_at: null }) as unknown as DispatchedSession;

describe("SessionPanelModel", () => {
  it("names a session by its channel and distinguishes background children", () => {
    expect(sessionTitle(row("a", "idle", 0))).toEqual({ title: "#adk-cc-t1570308839910412348", child: false });
    expect(sessionTitle(row("b", "idle", 0, `${KEY}:child:toolu_01Dd`))).toEqual({ title: "#adk-cc-t1570308839910412348 · toolu_01Dd", child: true });
    expect(sessionTitle(row("c", "idle", 0, "mac-mini:scratch-42"))).toEqual({ title: "mac-mini:scratch-42", child: false });
  });

  it("judges staleness by signal age and active-turn heartbeat only, not reconciled session_info notes", () => {
    expect(["turn_active", "working", "awaiting_bg", "awaiting_user", "idle", "disconnected"].map((status) => sessionState(row("x", status, 0))))
      .toEqual(["working", "working", "background", "resting", "resting", "closed"]);
    expect(isStaleIdleSession(row("old", "idle", 9 * DAY), NOW)).toBe(true);
    expect(canReconcileStaleSession(row("old", "idle", 9 * DAY), NOW)).toBe(false);
    expect(canCleanupStaleIdleSession(row("old", "idle", 9 * DAY), NOW)).toBe(true);
    expect(canCleanupStaleIdleSession({ ...row("old-dispatched", "idle", 9 * DAY), active_dispatch_id: "d-1" }, NOW)).toBe(false);
    expect(canCleanupStaleIdleSession(row("fresh", "idle", DAY), NOW)).toBe(false);
    expect(isStaleIdleSession(row("reconciled-fresh", "idle", 60_000, KEY, "reconciled stale turn_active"), NOW)).toBe(false);
    expect(canReconcileStaleSession(row("reconciled-fresh", "idle", 60_000, KEY, "reconciled stale turn_active"), NOW)).toBe(false);
    expect(canReconcileStaleSession(row("stuck-turn", "turn_active", 45 * 60_000), NOW)).toBe(true);
    expect(isStaleIdleSession(row("quiet", "idle", null), NOW)).toBe(false);
    const list = [row("rest-old", "idle", 9 * DAY), row("rest", "idle", DAY), row("rest-new", "idle", 60_000, KEY, "reconciled stale turn_active"), row("bg", "awaiting_bg", DAY), row("turn", "turn_active", 60_000)];
    expect(sessionSummaryCounts(list, (k) => k, NOW)).toBe("작업 중 1 · 백그라운드 대기 1 · 멈춤 1 · 쉬는 중 2");
    expect(list.sort(compareSessions).map((s) => s.id)).toEqual(["turn", "bg", "rest-new", "rest", "rest-old"]);
  });
});
