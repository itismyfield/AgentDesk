// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaigns, type Campaign } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
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
  vi.useRealTimers();
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

const notFound = () => new ApiRequestError("server campaign not found", { status: 404, body: { error: "server campaign not found", code: "not_found" } });
function copyOf(document: ServerCampaignDocument, id: string, title: string) {
  const next = structuredClone(document);
  next.view.campaign_id = id;
  next.definition.title = title;
  return next;
}

it("marks the snapshot stale when a poll stays unanswered for a whole interval", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "Date"] });
  await render();
  await openServer();
  list.mockImplementation(() => new Promise(() => {}));
  await act(async () => { vi.advanceTimersByTime(15_000); });
  expect(buttonByText("Edit definition")?.disabled).toBe(false);
  await act(async () => { vi.advanceTimersByTime(15_000); });
  expect(container.textContent).toContain("The server is not answering");
  expect(buttonByText("Edit definition")?.disabled).toBe(true);
  // The unanswered poll is waited out, not stacked on.
  expect(list).toHaveBeenCalledTimes(2);
});

it("marks a server that never answers silent one interval after the page opens", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "Date"] });
  list.mockImplementation(() => new Promise(() => {}));
  await render();
  await act(async () => { vi.advanceTimersByTime(15_000); });
  expect(container.textContent).toContain("The server is not answering");
  expect(buttonByText("New server campaign")?.disabled).toBe(true);
});

it("counts poll intervals that pass while the tab is hidden", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "Date"] });
  await render();
  await openServer();
  list.mockImplementation(() => new Promise(() => {}));
  await click(buttonByText("Refresh"));
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" });
  await act(async () => { vi.advanceTimersByTime(30_000); });
  Reflect.deleteProperty(document, "visibilityState");
  expect(container.textContent).toContain("The server is not answering");
});

it("still marks a hung server silent when the user keeps pressing Refresh", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "Date"] });
  await render();
  await openServer();
  list.mockImplementation(() => new Promise(() => {}));
  await act(async () => { vi.advanceTimersByTime(15_000); });
  await act(async () => { vi.advanceTimersByTime(12_000); });
  await click(buttonByText("Refresh"));
  await act(async () => { vi.advanceTimersByTime(3_000); });
  expect(container.textContent).toContain("The server is not answering");
});

it("does not call a refresh the user just started an unanswered server", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "Date"] });
  await render();
  await openServer();
  await act(async () => { vi.advanceTimersByTime(14_990); });
  list.mockImplementation(() => new Promise(() => {}));
  await click(buttonByText("Refresh"));
  await act(async () => { vi.advanceTimersByTime(10); });
  expect(container.textContent).not.toContain("The server is not answering");
  expect(buttonByText("Edit definition")?.disabled).toBe(false);
  await act(async () => { vi.advanceTimersByTime(15_000); });
  expect(container.textContent).toContain("The server is not answering");
});

it("keeps a command's progress and refusal with the campaign it was sent for", async () => {
  list.mockResolvedValue([fixtures.failedRunning, copyOf(fixtures.failedRunning, "other", "Other")]);
  let refuse: (reason: unknown) => void = () => {};
  command.mockImplementationOnce(() => new Promise((_, reject) => { refuse = reject; }));
  await render();
  await openServer("Release");
  await click(buttonByText("Stop after current stage"));
  await openServer("Other");
  expect(buttonByText("Stop after current stage")?.disabled).toBe(false);
  await act(async () => refuse(new ApiRequestError("rejected", { status: 409, body: { error: "rejected", code: "x", context: { rejection: "stale_run", target: null } } })));
  expect(controlText()).not.toContain("The target run changed");
  await openServer("Release");
  expect(controlText()).toContain("The target run changed");
});

it("shows an off-list campaign as stale when its read fails, and as missing only when the server says so", async () => {
  window.history.replaceState(null, "", "/campaigns?mode=server&campaign=elsewhere");
  list.mockImplementation(async () => [fixtures.prepared]); // each poll answers with a new list, as the API does
  let answer: (value: ServerCampaignDocument) => void = () => {};
  vi.mocked(getServerCampaign).mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
  await render();
  expect(container.textContent).not.toContain("This server campaign was not found.");
  await act(async () => answer(copyOf(fixtures.prepared, "elsewhere", "Linked campaign")));
  expect(buttonByText("Edit definition")?.disabled).toBe(false);
  vi.mocked(getServerCampaign).mockRejectedValueOnce(new TypeError("Failed to fetch"));
  await click(buttonByText("Refresh"));
  expect(container.querySelector(".campaign-server-title h3")?.textContent).toBe("Linked campaign");
  expect(container.textContent).toContain("Could not refresh this server campaign.");
  expect(buttonByText("Edit definition")?.disabled).toBe(true);
  vi.mocked(getServerCampaign).mockRejectedValueOnce(notFound());
  await click(buttonByText("Refresh"));
  expect(container.textContent).toContain("This server campaign was not found.");
});

it("keeps the create draft apart from the edit draft of a campaign named new", async () => {
  list.mockResolvedValue([copyOf(fixtures.prepared, "new", "Named new")]);
  await render();
  await click(buttonByText("New server campaign"));
  await openServer("Named new");
  await click(buttonByText("Edit definition"));
  expect(container.querySelectorAll("form.campaign-server-form")).toHaveLength(2);
  expect([...container.querySelectorAll("button")].filter((button) => button.textContent === "Create")).toHaveLength(1);
  expect([...container.querySelectorAll("button")].filter((button) => button.textContent === "Save")).toHaveLength(1);
});

it("selects a missing linked campaign in the selector so every listed campaign stays selectable", async () => {
  window.history.replaceState(null, "", "/campaigns?mode=server&campaign=gone");
  vi.mocked(getServerCampaign).mockRejectedValue(notFound());
  await render();
  const select = container.querySelector<HTMLSelectElement>('select[aria-label="Select campaign"]')!;
  expect(select.value).toBe("server:gone");
  await act(async () => { select.value = "legacy-1"; select.dispatchEvent(new Event("change", { bubbles: true })); });
  expect(container.textContent).toContain("Legacy mode");
});

it("puts the selection back into the URL after a navigation drops the query, but never onto another page", async () => {
  await render();
  await openServer();
  window.history.pushState(null, "", "/campaigns");
  await click(buttonByText("Refresh"));
  expect(window.location.search).toBe("?mode=server&campaign=release");
  window.history.pushState(null, "", "/kanban");
  await click(buttonByText("Refresh"));
  expect(`${window.location.pathname}${window.location.search}`).toBe("/kanban");
});

it("keeps the URL in step on the same page spelled with a trailing slash", async () => {
  window.history.replaceState(null, "", "/campaigns/");
  await render();
  await openServer();
  window.history.pushState(null, "", "/campaigns");
  await click(buttonByText("Refresh"));
  expect(window.location.search).toBe("?mode=server&campaign=release");
});

it("keeps a save locked while its answer is outstanding, even after leaving and reopening the campaign", async () => {
  list.mockResolvedValue([fixtures.prepared, copyOf(fixtures.prepared, "other", "Other")]);
  let answer: (value: Awaited<ReturnType<typeof editServerCampaignDefinition>>) => void = () => {};
  vi.mocked(editServerCampaignDefinition).mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
  await render();
  await openServer("Release");
  await click(buttonByText("Edit definition"));
  await click(buttonByText("Save"));
  await openServer("Other");
  await openServer("Release");
  expect(container.querySelector<HTMLFieldSetElement>("fieldset.campaign-server-fields")?.disabled).toBe(true);
  expect(buttonByText("Saving…")?.disabled).toBe(true);
  await act(async () => answer({ definition_revision: 8, run_id: null, control_version: null, event_sequence: 3, control: "prepared" }));
  expect(container.querySelector("form.campaign-server-form")).toBeNull();
});

it("marks the event list when it could not be read, keeps its count unknown, and retries on the next list read", async () => {
  vi.mocked(getServerCampaignEvents).mockRejectedValueOnce(new TypeError("Failed to fetch"));
  await render();
  await openServer();
  const summary = () => container.querySelector(".campaign-connections summary")?.textContent;
  expect(summary()).toBe("Events · — · Could not refresh events");
  list.mockImplementation(async () => [fixtures.prepared]);
  await click(buttonByText("Refresh"));
  expect(summary()).toBe("Events · 0");
  // A good read is not repeated while nothing moved.
  await click(buttonByText("Refresh"));
  expect(vi.mocked(getServerCampaignEvents)).toHaveBeenCalledTimes(2);
});

it("shows each campaign's own events when reads for several campaigns overlap or fail", async () => {
  const events = vi.mocked(getServerCampaignEvents);
  list.mockResolvedValue([fixtures.prepared, copyOf(fixtures.prepared, "other", "Other")]);
  await render();
  await openServer("Release");
  const summary = () => container.querySelector(".campaign-connections summary")?.textContent;
  expect(summary()).toBe("Events · 0");
  let fail: (reason: unknown) => void = () => {};
  events.mockImplementationOnce(() => new Promise((_, reject) => { fail = reject; }));
  await openServer("Other");
  await openServer("Release");
  // Other's late failure belongs to a read nobody is waiting for any more.
  await act(async () => fail(new TypeError("Failed to fetch")));
  expect(summary()).toBe("Events · 0");
  events.mockRejectedValueOnce(new TypeError("Failed to fetch"));
  await openServer("Other");
  expect(summary()).toBe("Events · — · Could not refresh events");
  await openServer("Release");
  expect(summary()).toBe("Events · 0");
});

it("retries a failing events read once per list read, not more", async () => {
  vi.mocked(getServerCampaignEvents).mockRejectedValue(new TypeError("Failed to fetch"));
  list.mockImplementation(async () => [fixtures.prepared]);
  await render();
  await openServer();
  expect(vi.mocked(getServerCampaignEvents)).toHaveBeenCalledTimes(1);
  await click(buttonByText("Refresh"));
  await click(buttonByText("Refresh"));
  await click(buttonByText("Refresh"));
  expect(vi.mocked(getServerCampaignEvents)).toHaveBeenCalledTimes(4);
});

it("lets a slow events read or retry finish instead of restarting it on every list read", async () => {
  const events = vi.mocked(getServerCampaignEvents);
  let answer: (value: Awaited<ReturnType<typeof getServerCampaignEvents>>) => void = () => {};
  const slow = () => new Promise<Awaited<ReturnType<typeof getServerCampaignEvents>>>((resolve) => { answer = resolve; });
  events.mockImplementationOnce(slow);
  list.mockImplementation(async () => [fixtures.prepared]);
  await render();
  await openServer();
  await click(buttonByText("Refresh"));
  await click(buttonByText("Refresh"));
  expect(events).toHaveBeenCalledTimes(1);
  await act(async () => answer([]));
  const summary = () => container.querySelector(".campaign-connections summary")?.textContent;
  expect(summary()).toBe("Events · 0");
  // The same holds for a retry after a failed read.
  const moved = structuredClone(fixtures.prepared);
  moved.view.last_event_sequence += 1;
  list.mockImplementation(async () => [moved]);
  events.mockRejectedValueOnce(new TypeError("Failed to fetch")).mockImplementationOnce(slow);
  await click(buttonByText("Refresh"));
  expect(summary()).toBe("Events · 0 · Could not refresh events");
  await click(buttonByText("Refresh"));
  await click(buttonByText("Refresh"));
  expect(events).toHaveBeenCalledTimes(3);
  await act(async () => answer([]));
  expect(summary()).toBe("Events · 0");
});

it("moves focus into a form only when it is opened, not each time its campaign is shown again", async () => {
  list.mockResolvedValue([fixtures.prepared, copyOf(fixtures.prepared, "other", "Other")]);
  await render();
  await openServer("Release");
  await click(buttonByText("Edit definition"));
  expect(document.activeElement?.getAttribute("aria-label")).toBe("Name");
  (document.activeElement as HTMLElement).blur();
  await openServer("Other");
  await openServer("Release");
  expect(document.activeElement?.getAttribute("aria-label")).not.toBe("Name");
});

it("copies a definition into an untouched create form but never over a draft being written", async () => {
  const done = structuredClone(fixtures.prepared);
  done.view.control = "succeeded";
  const other = copyOf(done, "other", "Other");
  list.mockResolvedValue([done, other]);
  await render();
  await openServer();
  await click(buttonByText("New server campaign"));
  (document.activeElement as HTMLElement | null)?.blur();
  await click(buttonByText("Copy definition to a new campaign"));
  const createName = () => container.querySelector("form.campaign-server-form")!.querySelector<HTMLInputElement>('input[aria-label="Name"]')!;
  expect(createName().value).toBe("Release");
  expect(document.activeElement?.getAttribute("aria-label")).toBe("Campaign ID");
  // A form holding an earlier copy that nobody edited takes the next copy.
  await openServer("Other");
  await click(buttonByText("Copy definition to a new campaign"));
  expect(createName().value).toBe("Other");
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(createName(), "Mine");
    createName().dispatchEvent(new Event("input", { bubbles: true }));
  });
  await openServer("Release");
  await click(buttonByText("Copy definition to a new campaign"));
  expect(createName().value).toBe("Mine");
  expect(container.textContent).toContain("A new campaign is already being written, so nothing was copied.");
});

it("brings a selected task's details into view on a narrow screen", async () => {
  const scroll = vi.fn();
  Object.defineProperty(window, "matchMedia", { configurable: true, value: (query: string) => ({ matches: query === "(max-width: 900px)" }) });
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", { configurable: true, value: scroll });
  list.mockResolvedValue([fixtures.reviewWaiting]);
  await render();
  await openServer();
  await click(container.querySelector('[data-node-id="C"]'));
  expect(scroll).toHaveBeenCalled();
  expect(scroll.mock.contexts.at(-1)).toBe(container.querySelector(".campaign-inspector"));
  Reflect.deleteProperty(window, "matchMedia");
  Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
});
