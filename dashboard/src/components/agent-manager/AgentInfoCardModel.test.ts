import { describe, expect, it } from "vitest";
import type { Agent, DispatchedSession } from "../../types";
import { classifyAgents } from "../home/homeModel";
import { agentSkillCalls, isTurnSession, splitLinkedSessions } from "./AgentInfoCardModel";

const session = (id: string, status: DispatchedSession["status"]) => ({ id, status }) as DispatchedSession;

describe("splitLinkedSessions", () => {
  it("keeps live sessions and counts the finished ones", () => {
    const rows = [session("a", "aborted"), session("b", "idle"), session("c", "disconnected"), session("d", "turn_active"), session("e", "awaiting_bg")];
    const { live, closed } = splitLinkedSessions(rows);
    expect(live.map((row) => row.id)).toEqual(["b", "d", "e"]);
    expect(closed).toBe(2);
    expect(splitLinkedSessions(rows).finished.map((row) => row.id)).toEqual(["a", "c"]);
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

describe("agentSkillCalls", () => {
  it("keeps this agent's rows by agent id only, most calls first", () => {
    const row = (agent_role_id: string, skill_name: string, calls: number) => ({ agent_role_id, agent_name: "", skill_name, skill_desc_ko: "desc", calls, last_used_at: 0 });
    // "adk-dashboard" is another agent whose id equals Ada's role id: its calls stay apart.
    const rows = [row("ada", "a", 1), row("adk-dashboard", "a", 8), row("luna", "c", 9), row("ada", "d", 3)];
    expect(agentSkillCalls(rows, "ada")).toEqual([{ id: "d", label: "d", value: 3 }, { id: "a", label: "a", value: 1 }]);
    expect(agentSkillCalls(rows, "nova")).toEqual([]);
  });
});
