// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { KanbanProvider } from "../../contexts/KanbanContext";
import type { KanbanCard } from "../../types";
import QueueCardDetail from "./QueueCardDetail";
import type { useQueueCardActions } from "./useQueueCardActions";

// The transcript panel reads the API on its own and is not under test here.
vi.mock("../agent-manager/TurnTranscriptPanel", () => ({ default: () => null }));
vi.mock("../agent-manager/useKanbanCardActivity", () => ({
  reviewDecisionMap: () => ({}),
  useKanbanCardActivity: () => ({
    auditLog: [], clearCardReviews: vi.fn(), ghComments: [], githubIssueBody: null, invalidateCardActivity: vi.fn(), reviewData: null,
  }),
}));

function fullCard(updatedAt: string, description: string): KanbanCard {
  return {
    id: "c", title: "Relay restart", description, status: "backlog", github_repo: "itismyfield/AgentDesk", owner_agent_id: null,
    requester_agent_id: null, assignee_agent_id: null, parent_card_id: null, latest_dispatch_id: null, sort_order: 0, priority: "medium",
    depth: 0, blocked_reason: null, review_notes: null, github_issue_number: null, github_issue_url: null, metadata_json: null,
    pipeline_stage_id: null, review_status: null, created_at: "2026-10-01T00:00:00Z", updated_at: updatedAt, started_at: null,
    requested_at: null, completed_at: null,
  };
}
// The list's summary view leaves the body out.
function summaryOf(card: KanbanCard): KanbanCard {
  const summary: Partial<KanbanCard> = { ...card };
  delete summary.description;
  return summary as KanbanCard;
}

const tr = (_: string, en: string) => en;
let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.clearAllMocks(); });
const button = (text: string) => [...document.body.querySelectorAll("button")].find((value) => value.textContent === text);

it("locks saving while a newer version is unread, and after a failed re-read, so the old body cannot overwrite the new one", async () => {
  const older = fullCard("2026-10-01T00:00:00Z", "old body");
  const newer = fullCard("2026-10-02T00:00:00Z", "new body");
  let failReread: (cause: Error) => void = () => {};
  const load = vi.fn()
    .mockResolvedValueOnce(older)
    .mockReturnValueOnce(new Promise((_, reject) => { failReread = reject; }))
    .mockResolvedValueOnce(newer);
  const update = vi.fn().mockResolvedValue(undefined);
  const actions = { load, update, cancel: vi.fn(), retry: vi.fn(), redispatch: vi.fn(), remove: vi.fn() } as unknown as ReturnType<typeof useQueueCardActions>;
  const render = (card: KanbanCard) => act(async () => root.render(<KanbanProvider initialCards={[]} initialDispatches={[]}>
    <QueueCardDetail cardId="c" card={card} agents={[]} campaigns={[]} locale="en" tr={tr} actions={actions}
      onClose={vi.fn()} onNeedsAssignee={vi.fn()} onCampaignSaved={vi.fn()} />
  </KanbanProvider>));

  await render(summaryOf(older));
  expect(button("Save")?.disabled).toBe(false);

  await render(summaryOf(newer));
  expect(button("Save")?.disabled).toBe(true);
  expect(document.body.textContent).toContain("Checking a newer version");

  await act(async () => failReread(new Error("offline")));
  expect(document.body.textContent).toContain("Saving is locked: the newest card did not load.");
  await act(async () => button("Save")?.click());
  expect(update).not.toHaveBeenCalled();

  await act(async () => button("Load again")?.click());
  expect(button("Save")?.disabled).toBe(false);
  await act(async () => button("Save")?.click());
  expect(update).toHaveBeenCalledOnce();
  expect(update.mock.calls[0][1]).toMatchObject({ description: "new body" });
});
