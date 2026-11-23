import { afterEach, describe, expect, it, vi } from "vitest";
import type { Agent, TaskDispatch } from "../../types";
import type { TodoItem } from "../home/homeModel";
import { buildAttention } from "./agentsAttention";

const NOW = Date.parse("2026-11-23T12:00:00Z");
const tr = (ko: string) => ko;
const agent = (id: string, extra: object = {}) => ({ id, name: id, name_ko: id, status: "working", ...extra }) as Agent;
const failed = (id: string, to: string, at: string, card: string | null = null) =>
  ({ id, to_agent_id: to, kanban_card_id: card, title: `run ${id}`, created_at: at, updated_at: at }) as unknown as TaskDispatch;
const todo = (key: string, agentId: string | null): TodoItem => ({ key, kind: "decide", tone: "err", chip: "판단 필요", title: "#12 카드", detail: "막힘", to: "/campaigns?tab=queue", agentId });

afterEach(() => vi.useRealTimers());

describe("buildAttention", () => {
  it("groups stuck turns, the agent's to-dos and recent failures per agent", () => {
    vi.useFakeTimers({ now: NOW });
    const rows = buildAttention({
      agents: [agent("ada", { visual_status_code: "stuck-suspect" }), agent("luna"), agent("nova")],
      todo: [todo("card:c1", "luna"), todo("card:c2", null)],
      failed: [
        failed("d1", "luna", "2026-11-23T10:00:00Z", "c1"),
        failed("d2", "nova", "2026-11-23T11:00:00Z", "c3"),
        failed("d3", "nova", "2026-11-23T11:30:00Z", "c3"),
        failed("d4", "nova", "2026-11-21T11:00:00Z"),
        failed("d5", "ghost", "2026-11-23T11:00:00Z"),
      ],
      cardsRead: false,
      tr,
      isKo: true,
    });
    expect(rows.map(({ agent: { id }, reasons }) => [id, reasons.map((reason) => reason.key)])).toEqual([
      ["ada", ["stuck:ada"]],
      ["luna", ["card:c1"]],
      ["nova", ["dispatch:d2"]],
    ]);
    expect(rows[0].reasons[0]).toMatchObject({ chip: "막힘 의심", to: null });
    expect(rows[2].reasons[0]).toMatchObject({ chip: "실패", detail: "run d2 · 1시간 전", to: "/campaigns?tab=queue&card=c3" });
  });

  it("drops a failed run whose card moved on once the open cards are read", () => {
    vi.useFakeTimers({ now: NOW });
    const input = { agents: [agent("nova")], todo: [], tr, isKo: true };
    const failures = [failed("d2", "nova", "2026-11-23T11:00:00Z", "c3"), failed("d6", "nova", "2026-11-23T11:10:00Z")];
    // c3 is neither on the to-do list nor (once read) among the open cards: it was retried or closed.
    expect(buildAttention({ ...input, failed: failures, cardsRead: true })[0].reasons.map((reason) => reason.key)).toEqual(["dispatch:d6"]);
    expect(buildAttention({ ...input, failed: failures.slice(0, 1), cardsRead: true })).toEqual([]);
  });

  it("puts a failing card without an owner under the agent its latest failed run went to", () => {
    vi.useFakeTimers({ now: NOW });
    const failures = [failed("d2", "nova", "2026-11-23T11:00:00Z", "c3"), failed("d1", "nova", "2026-11-23T10:00:00Z", "c3")];
    const rows = buildAttention({ agents: [agent("nova")], todo: [todo("card:c3", null)], failed: failures, cardsRead: true, tr, isKo: true });
    expect(rows.map(({ agent: { id }, reasons }) => [id, reasons.map((reason) => reason.chip)])).toEqual([["nova", ["판단 필요"]]]);
  });
});
