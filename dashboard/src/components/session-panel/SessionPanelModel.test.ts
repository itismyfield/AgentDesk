import { describe, expect, it } from "vitest";
import type { DispatchedSession } from "../../types";
import { compareSessions, isStaleIdleSession, sessionState, sessionTitle } from "./SessionPanelModel";

const NOW = Date.now();
const DAY = 86_400_000;
const KEY = "claude/discord_c2/MacBook-Pro-14:AgentDesk-claude-adk-cc-t1570308839910412348";
const row = (id: string, status: string, ago: number | null, key = KEY) =>
  ({ id, session_key: key, name: key, status, last_seen_at: ago === null ? null : new Date(NOW - ago).toISOString(), connected_at: null }) as unknown as DispatchedSession;

describe("SessionPanelModel", () => {
  it("names a session by its channel and marks background children", () => {
    expect(sessionTitle(row("a", "idle", 0))).toEqual({ title: "#adk-cc-t1570308839910412348", child: false });
    expect(sessionTitle(row("b", "idle", 0, `${KEY}:child:toolu_01Dd`))).toEqual({ title: "#adk-cc-t1570308839910412348", child: true });
    expect(sessionTitle(row("c", "idle", 0, "mac-mini:scratch-42"))).toEqual({ title: "mac-mini:scratch-42", child: false });
  });

  it("speaks home's states, calls a week-quiet idle session stale only with a signal, and sorts by state then signal", () => {
    expect(["turn_active", "working", "awaiting_bg", "awaiting_user", "idle"].map((status) => sessionState(row("x", status, 0))))
      .toEqual(["working", "working", "background", "resting", "resting"]);
    expect(isStaleIdleSession(row("old", "idle", 9 * DAY), NOW)).toBe(true);
    expect(isStaleIdleSession(row("quiet", "idle", null), NOW)).toBe(false);
    expect(isStaleIdleSession(row("bg", "awaiting_bg", 9 * DAY), NOW)).toBe(false);
    const sorted = [row("rest-old", "idle", 9 * DAY), row("rest", "idle", DAY), row("rest-new", "idle", 60_000), row("bg", "awaiting_bg", DAY), row("turn", "turn_active", DAY)]
      .sort(compareSessions).map((session) => session.id);
    expect(sorted).toEqual(["turn", "bg", "rest-new", "rest", "rest-old"]);
  });
});
