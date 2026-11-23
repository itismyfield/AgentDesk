import { describe, expect, it } from "vitest";
import type { Agent, DispatchedSession } from "../../types";
import { classifyAgents } from "../home/homeModel";
import { isTurnSession, splitLinkedSessions } from "./AgentInfoCardModel";

const session = (id: string, status: DispatchedSession["status"]) => ({ id, status }) as DispatchedSession;

describe("splitLinkedSessions", () => {
  it("keeps live sessions and counts the finished ones", () => {
    const rows = [session("a", "aborted"), session("b", "idle"), session("c", "disconnected"), session("d", "turn_active"), session("e", "awaiting_bg")];
    const { live, closed } = splitLinkedSessions(rows);
    expect(live.map((row) => row.id)).toEqual(["b", "d", "e"]);
    expect(closed).toBe(2);
    expect(live.filter(isTurnSession).map((row) => row.id)).toEqual(["d"]);
  });

  it("agrees with home on which session statuses are live", () => {
    const agent = { id: "a", name: "a", name_ko: "a", status: "idle" } as Agent;
    for (const status of ["turn_active", "working", "awaiting_bg", "awaiting_user", "idle", "disconnected", "aborted", "connected"]) {
      const row = { ...session("s", status as DispatchedSession["status"]), linked_agent_id: "a" };
      const live = classifyAgents([agent], [row], false)[0].state !== "noSession";
      expect([status, splitLinkedSessions([row]).live.length === 1]).toEqual([status, live]);
    }
  });
});
