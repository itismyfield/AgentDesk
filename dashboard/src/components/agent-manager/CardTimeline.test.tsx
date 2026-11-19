// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { CardAuditLogEntry, GitHubComment } from "../../api";
import type { TaskDispatch } from "../../types";
import CardTimeline from "./CardTimeline";

const tr = (_: string, en: string) => en;
const base = Date.parse("2026-11-17T10:00:00Z");
const dispatch = (id: string, at: number, patch: Partial<TaskDispatch> = {}): TaskDispatch => ({
  id, kanban_card_id: "card-1", from_agent_id: "pipeline", to_agent_id: "agent-ada", dispatch_type: "implementation", status: "completed",
  title: "", context_file: null, result_file: null, result_summary: null, parent_dispatch_id: null, chain_depth: 0, created_at: at,
  dispatched_at: at, completed_at: null, ...patch,
});
const DISPATCHES = [
  dispatch("d-1", base - 2 * 3_600_000, { status: "failed", title: "First attempt", result_summary: "tmux session missing\nretry with restore" }),
  dispatch("d-2", base - 60_000, { status: "in_progress", title: "Second attempt" }),
];
const AUDIT: CardAuditLogEntry[] = [
  { id: 1, card_id: "card-1", from_status: "ready", to_status: "requested", source: "pipeline", result: null, created_at: new Date(base - 3 * 3_600_000).toISOString() },
];
const COMMENTS: GitHubComment[] = [
  { author: { login: "luna" }, createdAt: new Date(base - 30 * 60_000).toISOString(), body: "리뷰했습니다. blocking finding 1건\n\n1. `src/a.rs:1` retry is too short" },
];

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); });

const rows = () => [...container.querySelectorAll<HTMLButtonElement>("[data-activity-row]")];
const tab = (name: string) => [...container.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((node) => node.textContent?.startsWith(name));

async function render(setTimelineFilter = vi.fn(), timelineFilter: "review" | null = null) {
  await act(async () => root.render(
    <CardTimeline ghComments={COMMENTS} timelineFilter={timelineFilter} setTimelineFilter={setTimelineFilter} tr={tr} locale="en" onRefresh={vi.fn()}
      card={{ id: "card-1", latest_dispatch_id: "d-2" }} dispatches={DISPATCHES} auditLog={AUDIT} getAgentLabel={(id) => id ?? ""} now={base} />,
  ));
}

it("lists dispatches, transitions and comments newest first with a count per source tab", async () => {
  await render();
  expect(rows().map((row) => row.closest("li")?.dataset.activityId)).toEqual(["dispatch:d-2", expect.stringMatching(/^comment:/), "dispatch:d-1", "status:1"]);
  expect(tab("All")?.getAttribute("aria-selected")).toBe("true");
  expect(tab("All")?.textContent).toBe("All4");
  expect(tab("Dispatch")?.textContent).toBe("Dispatch2");
  expect(container.textContent).toContain("Today");
});

it("filters by source tab, mirrors GitHub kinds to the parent filter, and searches the visible sentence", async () => {
  const setTimelineFilter = vi.fn();
  await render(setTimelineFilter);
  await act(async () => tab("Dispatch")!.click());
  expect(rows()).toHaveLength(2);
  expect(setTimelineFilter).toHaveBeenLastCalledWith(null);
  await act(async () => tab("Review")!.click());
  expect(setTimelineFilter).toHaveBeenLastCalledWith("review");

  await render(setTimelineFilter, "review");
  expect(rows()).toHaveLength(1);
  await act(async () => tab("All")!.click());
  expect(setTimelineFilter).toHaveBeenLastCalledWith(null);

  await render(setTimelineFilter, null);
  const search = container.querySelector<HTMLInputElement>('input[type="search"]')!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(search, "second");
    search.dispatchEvent(new Event("input", { bubbles: true }));
  });
  expect(rows()).toHaveLength(1);
  expect(rows()[0].getAttribute("aria-label")).toContain("Second attempt");
});

it("opens one record at a time with its full text", async () => {
  await render();
  const failed = rows().find((row) => row.closest("li")?.dataset.activityId === "dispatch:d-1")!;
  await act(async () => failed.click());
  expect(failed.getAttribute("aria-expanded")).toBe("true");
  expect(container.querySelector('[data-testid="card-activity-detail"]')?.textContent).toContain("retry with restore");
  const review = rows().find((row) => row.closest("li")?.dataset.source === "review")!;
  await act(async () => review.click());
  expect(failed.getAttribute("aria-expanded")).toBe("false");
  expect(container.querySelectorAll('[data-testid="card-activity-detail"]')).toHaveLength(1);
});

it("says when there is no activity yet", async () => {
  await act(async () => root.render(
    <CardTimeline ghComments={[]} timelineFilter={null} setTimelineFilter={vi.fn()} tr={tr} locale="en" onRefresh={vi.fn()} />,
  ));
  expect(container.textContent).toContain("No activity yet");
  expect(rows()).toHaveLength(0);
});
