// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaigns, type Campaign } from "../../api/campaigns";
import {
  editServerCampaignDefinition, getServerCampaign, getServerCampaignEvents, getServerCampaigns, sendServerCampaignCommand,
  type ServerCampaignDocument,
} from "../../api/serverCampaigns";
import { serverCampaignFixtures as fixtures } from "../../api/serverCampaignsFixtures";
import CampaignsPanel from "./CampaignsPanel";

vi.mock("../../api/campaigns", () => ({ getCampaigns: vi.fn(), updateCampaignNode: vi.fn(), setCampaignAutoQueue: vi.fn() }));
vi.mock("../../api/serverCampaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/serverCampaigns")>()),
  getServerCampaigns: vi.fn(), getServerCampaign: vi.fn(), getServerCampaignEvents: vi.fn(),
  sendServerCampaignCommand: vi.fn(), createServerCampaign: vi.fn(), editServerCampaignDefinition: vi.fn(),
}));
vi.mock("@xyflow/react", () => ({
  ReactFlow: () => <div>graph</div>, Background: () => null, Controls: () => null,
  MarkerType: { ArrowClosed: "closed" }, Position: { Right: "right", Left: "left" },
}));

const list = vi.mocked(getServerCampaigns);
const command = vi.mocked(sendServerCampaignCommand);
const legacy: Campaign = {
  id: "legacy-1", title: "Legacy campaign", description: "", status: "active", round: 1, revision: 1, auto_queue: false,
  created_at: "2026-09-20T00:00:00Z", updated_at: "2026-09-20T00:00:00Z", nodes: [],
};
const accepted = { definition_revision: 7, run_id: "run-1", control_version: 2, event_sequence: 22, control: "stopping" as const };
let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  window.history.replaceState(null, "", "/campaigns");
  const storage = new Map<string, string>();
  Object.defineProperty(window, "localStorage", {
    configurable: true,
    value: { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => storage.set(key, value), removeItem: (key: string) => storage.delete(key), clear: () => storage.clear() },
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  vi.mocked(getCampaigns).mockResolvedValue({ campaigns: [legacy], live: {}, read: 1 });
  list.mockResolvedValue([fixtures.prepared]);
  vi.mocked(getServerCampaign).mockResolvedValue(fixtures.prepared);
  vi.mocked(getServerCampaignEvents).mockResolvedValue([]);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.clearAllMocks();
});

async function render() {
  await act(async () => root.render(<CampaignsPanel language="en" />));
}
function buttonByText(text: string) {
  return [...container.querySelectorAll("button")].find((candidate) => candidate.textContent === text);
}
async function click(element: Element | undefined | null) {
  if (!element) throw new Error("missing element");
  await act(async () => { (element as HTMLElement).click(); });
}
async function openServer(title = "Release") {
  await click([...container.querySelectorAll(".campaign-server-list button")].find((button) => button.querySelector("strong")?.textContent === title));
}
function controlText() {
  return container.querySelector(".campaign-server-controls")?.textContent ?? "";
}

it("keeps legacy campaigns on their existing controls and gives server campaigns only server commands", async () => {
  await render();
  expect(container.textContent).toContain("Legacy mode");
  expect(container.querySelector(".campaign-auto-queue")).not.toBeNull();
  await openServer();
  expect(container.querySelector(".campaign-auto-queue")).toBeNull();
  expect(container.textContent).not.toContain("Legacy mode");
  expect(buttonByText("Start")?.disabled).toBe(true);
  expect(controlText()).toContain("Execution is not available yet");
  expect(buttonByText("Edit definition")?.disabled).toBe(false);
});

it("keeps the last server snapshot when a refresh fails and then allows only stop", async () => {
  // Every command offered, so only the lost connection can disable one.
  const offered = structuredClone(fixtures.failedRunning);
  offered.view.commands = offered.view.commands.map((entry) => ({ ...entry, rejection: null }));
  list.mockResolvedValue([offered]);
  await render();
  await openServer();
  list.mockRejectedValue(new Error("Offline"));
  await click(buttonByText("Refresh"));
  expect(container.textContent).toContain("Could not refresh server campaigns.");
  expect(container.textContent).toContain("Running · 1 failed · 1 blocked");
  expect(buttonByText("Stop now")?.disabled).toBe(false);
  expect(buttonByText("Stop after current stage")?.disabled).toBe(false);
  for (const label of ["Start", "Resume", "Edit definition"]) expect(buttonByText(label)?.disabled, label).toBe(true);
  expect(container.textContent).toContain("Disconnected: the latest state is unknown");
});

it("checks an unanswered stop, resends it under the same key and settles it from the server view", async () => {
  list.mockResolvedValue([fixtures.failedRunning]);
  command.mockRejectedValueOnce(new TypeError("Failed to fetch")).mockResolvedValueOnce(accepted);
  await render();
  await openServer();
  await click(buttonByText("Stop after current stage"));
  expect(container.textContent).toContain("Checking the request result");
  await click(buttonByText("Resend the same request"));
  const [first, second] = command.mock.calls;
  expect(second[2]).toBe(first[2]);
  expect(first[1]).toEqual({ command: "stop", expected_definition_revision: 7, expected_run: "run-1", mode: "finish_stage" });
  list.mockResolvedValue([fixtures.stoppingUnknown]);
  await click(buttonByText("Refresh"));
  expect(container.textContent).not.toContain("Checking the request result");
  expect(container.textContent).toContain("Stop accepted · waiting for confirmation");
});

it("never lets an older server list answer replace a newer one", async () => {
  let resolveOld: (value: ServerCampaignDocument[]) => void = () => {};
  await render();
  await openServer();
  list.mockImplementationOnce(() => new Promise((resolve) => { resolveOld = resolve; }));
  await click(buttonByText("Refresh"));
  list.mockResolvedValueOnce([fixtures.failedRunning]);
  await click(buttonByText("Refresh"));
  expect(container.textContent).toContain("Running · 1 failed · 1 blocked");
  await act(async () => resolveOld([fixtures.prepared]));
  expect(container.textContent).toContain("Running · 1 failed · 1 blocked");
});

it("keeps an edit draft, its focus and its base revision across polls", async () => {
  await render();
  await openServer();
  await click(buttonByText("Edit definition"));
  const name = container.querySelector<HTMLInputElement>('input[aria-label="Name"]')!;
  name.focus();
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(name, "Draft name");
    name.dispatchEvent(new Event("input", { bubbles: true }));
  });
  const newer = structuredClone(fixtures.prepared);
  newer.view.definition_revision = 8;
  newer.definition.title = "Renamed elsewhere";
  list.mockResolvedValue([newer]);
  await click(buttonByText("Refresh"));
  name.focus();
  const after = container.querySelector<HTMLInputElement>('input[aria-label="Name"]')!;
  expect(after).toBe(name);
  expect(after.value).toBe("Draft name");
  expect(document.activeElement).toBe(after);
  // The save still targets the revision the draft was based on, so the server can refuse it.
  vi.mocked(editServerCampaignDefinition).mockResolvedValueOnce({ definition_revision: 9, run_id: null, control_version: null, event_sequence: 3, control: "prepared" });
  await click(buttonByText("Save"));
  expect(vi.mocked(editServerCampaignDefinition).mock.calls[0].slice(0, 2)).toEqual(["release", 7]);
});

it("opens a deep link to a server task in a new browser and copies only the feedback fields", async () => {
  window.history.replaceState(null, "", "/campaigns?mode=server&campaign=release&node=B");
  list.mockResolvedValue([fixtures.reviewWaiting]);
  await render();
  expect(container.querySelector(".campaign-inspector h4")?.textContent).toBe("Dependent work");
  await click(container.querySelector('[data-node-id="C"]'));
  expect(window.location.search).toBe("?mode=server&campaign=release&node=C");
  const feedback = container.querySelector<HTMLTextAreaElement>('textarea[aria-label="Copy for feedback"]')!.value.split("\n");
  expect(feedback).toEqual([
    `${window.location.origin}/campaigns?mode=server&campaign=release&node=C`,
    "as_of 2026-10-28T05:00:00Z", "definition revision 7", "event 21", "display Waiting",
  ]);
});

it("opens a deep-linked server campaign that is not in the first list page", async () => {
  window.history.replaceState(null, "", "/campaigns?mode=server&campaign=elsewhere");
  const elsewhere = structuredClone(fixtures.prepared);
  elsewhere.view.campaign_id = "elsewhere";
  elsewhere.definition.title = "Linked campaign";
  vi.mocked(getServerCampaign).mockResolvedValue(elsewhere);
  await render();
  expect(vi.mocked(getServerCampaign)).toHaveBeenCalledWith("elsewhere");
  expect(container.querySelector(".campaign-server-title h3")?.textContent).toBe("Linked campaign");
});
