// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { setCampaignAutoQueue } from "../../api/campaigns";
import CampaignAutoQueueToggle from "./CampaignAutoQueueToggle";
import { makeLargeCampaign } from "./campaignTestFixtures";

vi.mock("../../api/campaigns", () => ({ setCampaignAutoQueue: vi.fn() }));

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.mocked(setCampaignAutoQueue).mockReset(); });

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
  vi.mocked(setCampaignAutoQueue).mockRejectedValue(new Error("campaign revision conflict; reload before retrying"));
  await act(async () => root.render(<CampaignAutoQueueToggle campaign={campaign} tr={(_, en) => en} onSaved={onSaved} />));
  await act(async () => container.querySelector("button")!.click());
  expect(setCampaignAutoQueue).toHaveBeenCalledWith(campaign, false);
  expect(onSaved).not.toHaveBeenCalled();
  expect(container.querySelector("button")?.textContent).toBe("Auto-run on");
  expect(container.querySelector("[role=status]")?.textContent).toContain("revision conflict");
});
