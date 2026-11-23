import { describe, expect, it } from "vitest";
import type { DispatchedSession } from "../../types";
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
});
