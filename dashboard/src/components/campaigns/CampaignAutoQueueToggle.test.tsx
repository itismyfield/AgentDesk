// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaign, setCampaignAutoQueue } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import CampaignAutoQueueToggle from "./CampaignAutoQueueToggle";
import { makeLargeCampaign } from "./campaignTestFixtures";

vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()),
  setCampaignAutoQueue: vi.fn(),
  getCampaign: vi.fn(),
}));

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.mocked(setCampaignAutoQueue).mockReset(); vi.mocked(getCampaign).mockReset();
});
const statusText = () => container.querySelector("[role=status]")?.textContent;

it("turns auto-run on and says what was sent and why the rest is held back", async () => {
  const campaign = makeLargeCampaign();
  const onSaved = vi.fn();
  vi.mocked(setCampaignAutoQueue).mockResolvedValue({
    campaign: { ...campaign, auto_queue: true, revision: 6 },
    handoff: {
      queued: [{ node_id: "task-0", card_id: "card-0", run_id: "run-1" }],
      waiting: [{ node_id: "task-1", reason: "no_assigned_agent" }, { node_id: "task-2", reason: "no_assigned_agent" }, { node_id: "task-3", reason: "run_paused" }],
    },
    handoffError: null,
  });
  await act(async () => root.render(<CampaignAutoQueueToggle campaign={campaign} tr={(_, en) => en} onSaved={onSaved} />));
  const button = container.querySelector("button")!;
  expect(button.getAttribute("aria-pressed")).toBe("false");
  await act(async () => button.click());
  expect(setCampaignAutoQueue).toHaveBeenCalledWith(campaign, true);
  expect(onSaved).toHaveBeenCalledWith(expect.objectContaining({ auto_queue: true, revision: 6 }));
  expect(container.querySelector("[role=status]")?.textContent).toBe(
    "Sent 1 tasks to auto-queue. Held back: no assigned agent 2, agent queue paused 1",
  );
});

it("shows a rejected save, such as a stale revision, instead of pretending it switched", async () => {
  const campaign = { ...makeLargeCampaign(), auto_queue: true };
  const onSaved = vi.fn();
  vi.mocked(setCampaignAutoQueue).mockRejectedValue(
    new ApiRequestError("campaign revision conflict; reload before retrying", { status: 409 }),
  );
  await act(async () => root.render(<CampaignAutoQueueToggle campaign={campaign} tr={(_, en) => en} onSaved={onSaved} />));
  await act(async () => container.querySelector("button")!.click());
  expect(setCampaignAutoQueue).toHaveBeenCalledWith(campaign, false);
  expect(onSaved).not.toHaveBeenCalled();
  expect(container.querySelector("button")?.textContent).toBe("Auto-run on");
  expect(statusText()).toContain("revision conflict");
  expect(getCampaign).not.toHaveBeenCalled();
});

it("says turning it off only stops new handoffs", async () => {
  const campaign = { ...makeLargeCampaign(), auto_queue: true };
  vi.mocked(setCampaignAutoQueue).mockResolvedValue({ campaign: { ...campaign, auto_queue: false }, handoff: null, handoffError: null });
  await act(async () => root.render(<CampaignAutoQueueToggle campaign={campaign} tr={(_, en) => en} onSaved={vi.fn()} />));
  await act(async () => container.querySelector("button")!.click());
  expect(statusText()).toBe("Stopped sending new tasks. Tasks already sent keep running in auto-queue.");
});

it("reloads the campaign when the save times out, since it may have gone through", async () => {
  const campaign = makeLargeCampaign();
  const onSaved = vi.fn();
  vi.mocked(setCampaignAutoQueue).mockRejectedValue(new Error("Request timeout: /api/campaigns/x"));
  vi.mocked(getCampaign).mockResolvedValueOnce({ ...campaign, auto_queue: true, revision: 6 });
  await act(async () => root.render(<CampaignAutoQueueToggle campaign={campaign} tr={(_, en) => en} onSaved={onSaved} />));
  await act(async () => container.querySelector("button")!.click());
  expect(onSaved).toHaveBeenCalledWith(expect.objectContaining({ auto_queue: true, revision: 6 }));
  expect(statusText()).toContain("Saved, but the response was lost");

  vi.mocked(getCampaign).mockRejectedValueOnce(new Error("offline"));
  await act(async () => container.querySelector("button")!.click());
  expect(statusText()).toContain("Could not confirm the save");
});
