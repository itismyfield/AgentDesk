import { describe, expect, it } from "vitest";
import type { CampaignNodeLive } from "../../api/campaigns";
import { serverCampaignFixtures } from "../../api/serverCampaignsFixtures";
import { legacyBoard, serverBoard } from "./campaignBoardModel";
import { serverTasks } from "./campaignOverviewModel";
import { makeCampaignNode } from "./campaignTestFixtures";

const tr = (ko: string) => ko;
const ids = (cards: { task: { id: string } }[]) => cards.map((card) => card.task.id);
const running = { card_id: "c", card_status: "in_progress", dispatch_type: "implementation", running: true } as CampaignNodeLive;

describe("stage board columns", () => {
  it("lists a ledger by its own status, leads with what needs someone and keeps live work as a mark only", () => {
    const columns = legacyBoard([
      makeCampaignNode("old", { status: "running", updated_at: "2026-09-20T00:00:00Z" }),
      makeCampaignNode("new", { status: "running", updated_at: "2026-09-21T00:00:00Z" }),
      makeCampaignNode("stuck", { status: "running", blocker: "외부 확인", updated_at: "2026-09-19T00:00:00Z" }),
      makeCampaignNode("failed", { status: "failed" }), makeCampaignNode("blocked", { status: "blocked" }), makeCampaignNode("done", { status: "completed" }),
    ], { failed: running, blocked: running });
    expect(columns.map((column) => [column.key, column.done])).toEqual([["pending", false], ["running", false], ["blocked", false], ["failed", false], ["completed", true], ["skipped", true]]);
    expect(ids(columns[1].cards)).toEqual(["stuck", "new", "old"]);
    for (const [index, status] of [[2, "blocked"], [3, "failed"]] as const) expect(columns[index].cards.map((card) => [card.task.status, card.task.live?.running])).toEqual([[status, true]]);
    expect(legacyBoard([makeCampaignNode("p")]).map((column) => column.key)).toEqual(["pending", "running", "completed", "skipped"]);
  });

  it("follows the server's stage order and keeps the server's words: series once started, repair cycle, a user stop", () => {
    const blocked = serverCampaignFixtures.failedRunning; // B is blocked by A's failure before it ever started
    expect(serverBoard(blocked, serverTasks(blocked, [], tr), tr)[0].cards.map((card) => [card.task.id, card.line])).toEqual([["B", null]]);
    const doc = structuredClone(blocked);
    doc.view.nodes[1].runtime = { ...doc.view.nodes[1].runtime, stage: "implementation", disposition: "pending", repair_cycles: 1 }; // a repair was prepared
    const columns = serverBoard(doc, serverTasks(doc, [], tr), tr);
    expect(columns.map((column) => column.key)).toEqual(["requirements", "design", "design_review", "implementation", "review", "landing", "main_verification", "final_verification", "complete"]);
    expect(columns[3].cards.map((card) => [card.task.id, card.line, card.retry])).toEqual([["A", "계열 1 · 수리 차수 3", true], ["C", "계열 1", false], ["B", "계열 1 · 수리 차수 1", false]]);
    doc.view.nodes[2].runtime.disposition = "interrupted";
    expect(serverBoard(doc, serverTasks(doc, [], tr), tr)[3].cards.find((card) => card.task.id === "C")).toMatchObject({ task: { status: "pending" }, chip: ["사용자 중단", "Stopped by user"] });
  });

  it("keeps finished server tasks in definition order and unplaced tasks in their own leading column", () => {
    const doc = structuredClone(serverCampaignFixtures.prepared);
    for (const node of doc.view.nodes.reverse()) node.runtime = { ...node.runtime, stage: "complete", disposition: "complete" };
    expect(ids(serverBoard(doc, serverTasks(doc, [], tr), tr).at(-1)!.cards)).toEqual(["A", "B", "C"]);
    doc.view.nodes = [];
    const [first] = serverBoard(doc, serverTasks(doc, [], tr), tr);
    expect([first.key, ids(first.cards)]).toEqual(["unplaced", ["A", "B", "C"]]);
  });
});
