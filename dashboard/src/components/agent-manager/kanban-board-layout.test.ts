import { describe, expect, it } from "vitest";

import {
  BACKLOG_PAGE_SIZE,
  buildKanbanBacklogEntries,
  countActiveKanbanAdvancedFilters,
  paginateKanbanBacklogEntries,
} from "./kanban-board-layout";

describe("kanban-board-layout", () => {
  it("places tracked backlog cards before raw GitHub backlog issues", () => {
    const entries = buildKanbanBacklogEntries(
      [
        { id: "card-1", title: "Card one" },
        { id: "card-2", title: "Card two" },
      ] as any,
      [
        { number: 101, title: "Issue one" },
        { number: 102, title: "Issue two" },
      ] as any,
    );

    expect(entries.map((entry) => entry.kind)).toEqual(["card", "card", "issue", "issue"]);
    expect(entries.map((entry) => entry.key)).toEqual([
      "card-card-1",
      "card-card-2",
      "issue-101",
      "issue-102",
    ]);
  });

  it("paginates backlog entries and clamps out-of-range pages", () => {
    const entries = buildKanbanBacklogEntries(
      Array.from({ length: BACKLOG_PAGE_SIZE + 2 }, (_, index) => ({
        id: `card-${index + 1}`,
        title: `Card ${index + 1}`,
      })) as any,
      [],
    );

    const first = paginateKanbanBacklogEntries(entries, 0);
    const second = paginateKanbanBacklogEntries(entries, 1);
    const clamped = paginateKanbanBacklogEntries(entries, 9);

    expect(first.pageCount).toBe(2);
    expect(first.items).toHaveLength(BACKLOG_PAGE_SIZE);
    expect(second.items).toHaveLength(2);
    expect(clamped.page).toBe(1);
    expect(clamped.items).toHaveLength(2);
  });

  it("counts only non-default advanced filters", () => {
    expect(
      countActiveKanbanAdvancedFilters({
        showClosed: false,
        agentFilter: "all",
        deptFilter: "all",
        cardTypeFilter: "all",
        signalStatusFilter: "all",
      }),
    ).toBe(0);

    expect(
      countActiveKanbanAdvancedFilters({
        showClosed: true,
        agentFilter: "agent-1",
        deptFilter: "dept-1",
        cardTypeFilter: "review",
        signalStatusFilter: "stalled",
      }),
    ).toBe(5);
  });
});
