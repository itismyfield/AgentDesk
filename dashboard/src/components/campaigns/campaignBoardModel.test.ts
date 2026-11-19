import { describe, expect, it } from "vitest";
import { serverCampaignFixtures } from "../../api/serverCampaignsFixtures";
import { legacyBoard, serverBoard } from "./campaignBoardModel";
import { legacyTask, serverTasks } from "./campaignOverviewModel";
import { makeCampaignNode } from "./campaignTestFixtures";

const tr = (ko: string) => ko;
const ids = (cards: { task: { id: string } }[]) => cards.map((card) => card.task.id);

describe("stage board columns", () => {
  it("lists a ledger by status, adds blocked or failed only when held, and leads with what needs someone", () => {
    const columns = legacyBoard([
      makeCampaignNode("old", { status: "running", updated_at: "2026-09-20T00:00:00Z" }),
      makeCampaignNode("new", { status: "running", updated_at: "2026-09-21T00:00:00Z" }),
      makeCampaignNode("stuck", { status: "running", blocker: "외부 확인", updated_at: "2026-09-19T00:00:00Z" }),
      makeCampaignNode("done", { status: "completed" }),
    ].map((node) => legacyTask(node)));
    expect(columns.map((column) => [column.key, column.done])).toEqual([["pending", false], ["running", false], ["completed", true], ["skipped", true]]);
    expect(ids(columns[1].cards)).toEqual(["stuck", "new", "old"]);
    expect(legacyBoard([legacyTask(makeCampaignNode("f", { status: "failed" }))]).map((column) => column.key)).toContain("failed");
  });

  it("follows the server's stage order, shows tries only once started and offers retry on failed tasks", () => {
    const doc = serverCampaignFixtures.failedRunning;
    const columns = serverBoard(doc, serverTasks(doc, [], tr), tr);
    expect(columns.map((column) => column.key)).toEqual(["requirements", "design", "design_review", "implementation", "review", "landing", "main_verification", "final_verification", "complete"]);
    expect(columns[3].cards.map((card) => [card.task.id, card.line, card.retry])).toEqual([["A", "1번째 시도 · 수리 3회", true], ["C", "1번째 시도", false]]);
    const prepared = serverCampaignFixtures.prepared;
    expect(serverBoard(prepared, serverTasks(prepared, [], tr), tr)[0].cards.map((card) => card.line)).toEqual([null, null, null]);
  });

  it("keeps tasks the server has not placed in their own leading column", () => {
    const doc = structuredClone(serverCampaignFixtures.prepared);
    doc.view.nodes = [];
    const [first] = serverBoard(doc, serverTasks(doc, [], tr), tr);
    expect([first.key, ids(first.cards)]).toEqual(["unplaced", ["A", "B", "C"]]);
  });
});
