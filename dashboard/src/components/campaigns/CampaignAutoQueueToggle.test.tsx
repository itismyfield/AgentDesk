// @vitest-environment happy-dom
import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaign, setCampaignAutoQueue, type Campaign } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import CampaignAutoQueueToggle from "./CampaignAutoQueueToggle";
import CampaignsPanel from "./CampaignsPanel";
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

function Harness({ initial }: { initial: Campaign }) {
  const [value, setValue] = useState(initial);
  return <CampaignAutoQueueToggle campaign={value} tr={(_, en) => en} onSaved={(next) => setValue((current) => (next.revision >= current.revision ? next : current))} />;
}
const button = () => container.querySelector("button")!;

it("after a lost response describes what the server has now, not whether the save ran", async () => {
  const campaign = makeLargeCampaign();
  vi.mocked(setCampaignAutoQueue).mockRejectedValue(new Error("Request timeout: /api/campaigns/x"));
  vi.mocked(getCampaign)
    .mockResolvedValueOnce({ ...campaign, auto_queue: true, revision: campaign.revision + 1 })
    .mockResolvedValueOnce({ ...campaign, auto_queue: true, revision: campaign.revision + 1 });
  await act(async () => root.render(<Harness initial={campaign} />));
  await act(async () => button().click());
  expect(statusText()).toBe(
    "No response (Request timeout: /api/campaigns/x). The server has auto-run on now. Check the nodes for what was sent.",
  );
  expect(button().textContent).toBe("Auto-run on");

  await act(async () => button().click());
  expect(statusText()).toBe(
    "No response (Request timeout: /api/campaigns/x). The server has auto-run on now. The request may still apply later.",
  );
  expect(statusText()).not.toContain("Not saved");
});

it("shows the state as unknown until the server is read again", async () => {
  const campaign = makeLargeCampaign();
  vi.mocked(setCampaignAutoQueue).mockRejectedValue(new Error("Request timeout: /api/campaigns/x"));
  vi.mocked(getCampaign).mockRejectedValueOnce(new Error("offline"));
  await act(async () => root.render(<Harness initial={campaign} />));
  await act(async () => button().click());
  expect(statusText()).toContain("Could not confirm the save");
  expect(button().textContent).toBe("Check auto-run");
  expect(button().getAttribute("aria-pressed")).toBe("mixed");

  vi.mocked(getCampaign).mockResolvedValueOnce({ ...campaign, auto_queue: true, revision: campaign.revision + 1 });
  await act(async () => button().click());
  expect(setCampaignAutoQueue).toHaveBeenCalledTimes(1);
  expect(button().textContent).toBe("Auto-run on");
  expect(button().getAttribute("aria-pressed")).toBe("true");
  expect(statusText()).toBe("The server has auto-run on now.");
});

it("leaves the unknown state once a newer campaign arrives from elsewhere", async () => {
  const campaign = makeLargeCampaign();
  vi.mocked(setCampaignAutoQueue).mockRejectedValue(new Error("Request timeout: /api/campaigns/x"));
  vi.mocked(getCampaign).mockRejectedValueOnce(new Error("offline"));
  const render = (value: Campaign) => root.render(<CampaignAutoQueueToggle campaign={value} tr={(_, en) => en} onSaved={vi.fn()} />);
  await act(async () => render(campaign));
  await act(async () => button().click());
  expect(button().getAttribute("aria-pressed")).toBe("mixed");
  await act(async () => render({ ...campaign, auto_queue: true, revision: campaign.revision + 1 }));
  expect(button().textContent).toBe("Auto-run on");
});

it("stays unknown through a poll that began before the failure and settles on the next one", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
  const campaign = { ...makeLargeCampaign(), nodes: [], status: "active" as const };
  const pending: Array<(body: unknown) => void> = [];
  const fetchMock = vi.fn<typeof fetch>(() => new Promise((resolve) => pending.push((body) =>
    resolve(new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } })))));
  vi.stubGlobal("fetch", fetchMock);
  const page = { campaigns: [campaign], live: {}, limit: 100, offset: 0 };
  try {
    await act(async () => root.render(<CampaignsPanel language="en" />));
    await act(async () => pending[0](page));
    await act(async () => { vi.advanceTimersByTime(15_000); }); // poll still in flight below
    expect(fetchMock).toHaveBeenCalledTimes(2);

    vi.mocked(setCampaignAutoQueue).mockRejectedValue(new Error("Request timeout: /api/campaigns/x"));
    vi.mocked(getCampaign).mockRejectedValueOnce(new Error("offline"));
    const toggle = () => container.querySelector<HTMLButtonElement>(".campaign-auto-queue")!;
    await act(async () => toggle().click());
    expect(toggle().getAttribute("aria-pressed")).toBe("mixed");

    await act(async () => { vi.advanceTimersByTime(15_000); });
    expect(fetchMock).toHaveBeenCalledTimes(3); // its own request, not the one in flight
    await act(async () => pending[1](page));
    expect(toggle().getAttribute("aria-pressed")).toBe("mixed");
    await act(async () => pending[2](page));
    expect(toggle().textContent).toBe("Auto-run off");
    expect(toggle().getAttribute("aria-pressed")).toBe("false");
  } finally {
    vi.useRealTimers(); vi.unstubAllGlobals();
  }
});
