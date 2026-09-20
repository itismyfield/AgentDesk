// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaigns, updateCampaignNode, type Campaign } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import CampaignsPanel from "./CampaignsPanel";

vi.mock("../../api/campaigns", () => ({ getCampaigns: vi.fn(), updateCampaignNode: vi.fn() }));
vi.mock("@xyflow/react", () => ({
  ReactFlow: () => <div>Task graph</div>, Background: () => null, Controls: () => null,
  MarkerType: { ArrowClosed: "closed" }, Position: { Right: "right", Left: "left" },
}));

const campaign: Campaign = {
  id: "ongoing", title: "Long campaign", description: "Recover after compaction", status: "active", round: 3, revision: 5,
  created_at: "2026-09-20T00:00:00Z", updated_at: "2026-09-20T00:00:00Z",
  nodes: [{ id: "review", title: "Review current head", status: "running", stage: "review", round: 3,
    assignee: "reviewer", session_id: "session-42", provider: "codex", dependencies: [], issue_url: null, pr_url: null,
    head_sha: "abc123", evidence: ["Unit tests passed"], next_action: "Inspect the latest diff", blocker: null, updated_at: "2026-09-20T00:00:00Z", details: "", acceptance: [], findings: [], evidence_records: [] }],
};
let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  const storage = new Map<string, string>();
  Object.defineProperty(window, "localStorage", { configurable: true, value: {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key), clear: () => storage.clear(),
  } });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  vi.mocked(getCampaigns).mockResolvedValue([campaign]);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.clearAllMocks(); });
async function render() { await act(async () => root.render(<CampaignsPanel language="en" />)); }
function button(label: string) { return Array.from(container.querySelectorAll("button")).find((value) => value.textContent === label)!; }

it("restores the selected campaign and exposes the durable continuation checkpoint", async () => {
  const other = { ...campaign, id: "other", title: "Other", nodes: [] };
  window.localStorage.setItem(STORAGE_KEYS.dashboardActiveCampaign, JSON.stringify(campaign.id));
  vi.mocked(getCampaigns).mockResolvedValue([other, campaign]);
  await render();
  expect(container.querySelector('[aria-current="true"]')?.textContent).toContain("Long campaign");
  expect(container.textContent).toContain("session-42");
  expect(container.textContent).toContain("Inspect the latest diff");
  expect(container.querySelector("progress")?.value).toBe(0);
});

it("retains the prior checkpoint and explicitly marks a failed refresh", async () => {
  await render();
  vi.mocked(getCampaigns).mockRejectedValue(new Error("Offline"));
  await act(async () => button("Refresh").click());
  expect(container.textContent).toContain("Showing the previous snapshot");
  expect(container.textContent).toContain("Inspect the latest diff");
});

it("shows an empty state and distinguishes initial fetch failure", async () => {
  vi.mocked(getCampaigns).mockResolvedValue([]);
  await render();
  expect(container.textContent).toContain("No campaigns yet");
  vi.mocked(getCampaigns).mockRejectedValue(new Error("Forbidden"));
  await act(async () => button("Refresh").click());
  expect(container.querySelector('[role="alert"]')?.textContent).toContain("Forbidden");
  expect(container.textContent).not.toContain("No campaigns yet");
});

it("preserves the edit revision and the draft when another session changes the campaign", async () => {
  await render();
  await act(async () => button("Edit task").click());
  vi.mocked(getCampaigns).mockResolvedValue([{ ...campaign, revision: 6 }]);
  await act(async () => button("Refresh").click());
  vi.mocked(updateCampaignNode).mockRejectedValue(new ApiRequestError("conflict", { status: 409 }));
  await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  expect(vi.mocked(updateCampaignNode).mock.calls[0][0].revision).toBe(5);
  expect(container.querySelector('[role="alert"]')?.textContent).toContain("changed elsewhere");
  expect(container.querySelector("textarea")?.value).toBe("Inspect the latest diff");
});
