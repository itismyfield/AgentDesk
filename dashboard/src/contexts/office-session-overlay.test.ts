import { describe, expect, it } from "vitest";

import type { Agent, DispatchedSession } from "../types";
import {
  applySessionOverlay,
  deriveDispatchedAsAgents,
  deriveSubAgents,
} from "./office-session-overlay";

function makeAgent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "project-agentdesk",
    name: "AgentDesk",
    name_ko: "AgentDesk",
    department_id: "engineering",
    avatar_emoji: "🤖",
    personality: null,
    status: "idle",
    stats_tasks_done: 0,
    stats_xp: 0,
    stats_tokens: 0,
    created_at: 0,
    ...overrides,
  };
}

function makeSession(overrides: Partial<DispatchedSession> = {}): DispatchedSession {
  return {
    id: "session-1",
    session_key: "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011",
    name: "adk-cdx-t1485506232256168011",
    department_id: "engineering",
    linked_agent_id: "project-agentdesk",
    provider: "codex",
    model: null,
    status: "working",
    session_info: "리뷰 중",
    sprite_number: null,
    avatar_emoji: "🤖",
    stats_xp: 0,
    tokens: 10,
    connected_at: 0,
    last_seen_at: 0,
    thread_channel_id: "1485506232256168011",
    ...overrides,
  };
}

describe("office-session-overlay", () => {
  it("linked thread session overlays parent agent and stays out of dispatched staff", () => {
  const agent = makeAgent();
  const session = makeSession();

    const overlaid = applySessionOverlay([agent], [session]);
    const subAgents = deriveSubAgents([session]);
    const dispatched = deriveDispatchedAsAgents([session]);

    expect(overlaid[0].status).toBe("working");
    expect(overlaid[0].activity_source).toBe("agentdesk");
    expect(overlaid[0].current_thread_channel_id).toBe("1485506232256168011");
    expect(subAgents).toHaveLength(1);
    expect(subAgents[0].parentAgentId).toBe("project-agentdesk");
    expect(dispatched).toHaveLength(0);
  });

  it("multiple working sessions: newest by timestamp thread_channel_id wins", () => {
    const agent = makeAgent();
    const newerSession = makeSession({
      id: "session-2",
      session_info: "rework 중",
      thread_channel_id: "9999999999",
      last_seen_at: 200,
      connected_at: 100,
    });
    const olderSession = makeSession({
      id: "session-1",
      session_info: "리뷰 중",
      thread_channel_id: "1111111111",
      last_seen_at: 50,
      connected_at: 10,
    });
    const overlaid = applySessionOverlay([agent], [newerSession, olderSession]);
    expect(overlaid[0].current_thread_channel_id).toBe("9999999999");
    expect(overlaid[0].session_info).toBe("rework 중");
    expect(overlaid[0].agentdesk_working_count).toBe(2);

    const overlaidReverse = applySessionOverlay([agent], [olderSession, newerSession]);
    expect(overlaidReverse[0].current_thread_channel_id).toBe("9999999999");
    expect(overlaidReverse[0].session_info).toBe("rework 중");
    expect(overlaidReverse[0].agentdesk_working_count).toBe(2);
  });

  it("direct channel session still overlays agent without requiring a thread id", () => {
    const agent = makeAgent();
    const session = makeSession({
      name: "adk-cdx",
      session_key: "mac-mini:AgentDesk-codex-adk-cdx",
      thread_channel_id: null,
    });

    const overlaid = applySessionOverlay([agent], [session]);

    expect(overlaid[0].status).toBe("working");
    expect(overlaid[0].current_thread_channel_id).toBeNull();
  });
});
