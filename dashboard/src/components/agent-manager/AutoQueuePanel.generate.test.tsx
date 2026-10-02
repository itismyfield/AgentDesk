// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { ApiRequestError, generateAutoQueue, getAutoQueueStatus, type AutoQueueRun, type AutoQueueStatus } from "../../api";
import AutoQueuePanel from "./AutoQueuePanel";

vi.mock("../../api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api")>()),
  getAutoQueueStatus: vi.fn(),
  generateAutoQueue: vi.fn(),
}));

let root: Root;
let container: HTMLDivElement;
const tr = (_ko: string, en: string) => en;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.resetAllMocks();
  vi.mocked(getAutoQueueStatus).mockResolvedValue({ run: null, entries: [], agents: {} });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

async function render(readyEntries: Array<{ agentId: string; issueNumber: number }>, repo = "itismyfield/AgentDesk") {
  await act(async () =>
    root.render(
      <AutoQueuePanel tr={tr} locale="en" agents={[]} selectedRepo={repo} readyEntries={readyEntries} />,
    ),
  );
}
const run = (id: string, status: AutoQueueRun["status"]): AutoQueueRun => ({
  id,
  repo: "itismyfield/AgentDesk",
  agent_id: "agent-a",
  status,
  ai_model: null,
  ai_rationale: null,
  timeout_minutes: 120,
  unified_thread: false,
  unified_thread_id: null,
  created_at: 0,
  completed_at: null,
});
const statusWith = (r: AutoQueueRun | null): AutoQueueStatus => ({ run: r, entries: [], agents: {} });
function deferred() {
  let resolve!: (value: AutoQueueStatus) => void;
  const promise = new Promise<AutoQueueStatus>((res) => (resolve = res));
  return { promise, resolve };
}
const generateButton = () =>
  [...container.querySelectorAll("button")].find((button) =>
    ["Generate", "Generating..."].includes(button.textContent ?? ""),
  );

it("generates one queue per agent from the ready cards and reports the ones that fail", async () => {
  vi.mocked(generateAutoQueue)
    .mockResolvedValueOnce({ run: null, entries: [], message: "No dispatchable cards found" })
    .mockResolvedValueOnce({ run: run("run-b", "generated"), entries: [] });
  await render([
    { agentId: "agent-b", issueNumber: 7 },
    { agentId: "agent-a", issueNumber: 5 },
    { agentId: "agent-a", issueNumber: 3 },
  ]);
  await act(async () => generateButton()!.click());

  expect(vi.mocked(generateAutoQueue).mock.calls).toEqual([
    [{ repo: "itismyfield/AgentDesk", agentId: "agent-a", issueNumbers: [3, 5] }],
    [{ repo: "itismyfield/AgentDesk", agentId: "agent-b", issueNumbers: [7] }],
  ]);
  expect(container.textContent).toContain("Queue not created: agent-a: No dispatchable cards found");
});

it("names the cards a created queue left out", async () => {
  vi.mocked(generateAutoQueue).mockResolvedValueOnce({
    run: run("run-a", "generated"),
    entries: [],
    skipped_due_to_active_dispatch: [{ issue_number: 5 }],
    skipped_due_to_filter: [{ issue_number: 7, reason: "card status 'done' is not enqueueable" }],
  });
  await render([{ agentId: "agent-a", issueNumber: 5 }]);
  await act(async () => generateButton()!.click());

  expect(container.textContent).toContain(
    "Cards left out: agent-a: #5 already dispatched, #7 card status 'done' is not enqueueable",
  );
});

it("keeps the newest status when an older read lands after it", async () => {
  const stale = deferred();
  const fresh = deferred();
  vi.mocked(getAutoQueueStatus).mockReturnValueOnce(stale.promise).mockReturnValueOnce(fresh.promise);
  vi.mocked(generateAutoQueue).mockResolvedValue({ run: run("run-new", "generated"), entries: [] });
  await render([{ agentId: "agent-a", issueNumber: 5 }]);
  await act(async () => generateButton()!.click());
  expect(vi.mocked(getAutoQueueStatus).mock.calls[1]).toEqual(["itismyfield/AgentDesk", undefined, { fresh: true }]);

  await act(async () => fresh.resolve(statusWith(run("run-new", "generated"))));
  expect(generateButton()).toBeUndefined();
  await act(async () => stale.resolve(statusWith(null)));
  expect(generateButton()).toBeUndefined();
});

it("drops a generate that finishes after the repo changed", async () => {
  let finish!: (value: Awaited<ReturnType<typeof generateAutoQueue>>) => void;
  vi.mocked(generateAutoQueue).mockReturnValue(new Promise((res) => (finish = res)));
  await render([{ agentId: "agent-a", issueNumber: 5 }]);
  await act(async () => generateButton()!.click());
  expect(generateButton()!.disabled).toBe(true);

  await render([{ agentId: "agent-a", issueNumber: 5 }], "itismyfield/Other");
  expect(generateButton()!.disabled).toBe(false);
  vi.mocked(getAutoQueueStatus).mockClear();
  await act(async () => finish({ run: null, entries: [], message: "No dispatchable cards found" }));
  expect(getAutoQueueStatus).not.toHaveBeenCalled();
  expect(generateButton()!.disabled).toBe(false);
  expect(container.textContent).not.toContain("Queue not created");
});

it("says to reset first when a queue already exists", async () => {
  vi.mocked(generateAutoQueue).mockRejectedValue(
    new ApiRequestError("live auto-queue run already exists: run_id=r1, status=generated", { status: 409 }),
  );
  await render([{ agentId: "agent-a", issueNumber: 5 }]);
  await act(async () => generateButton()!.click());
  expect(generateButton()!.disabled).toBe(false);
  expect(container.textContent).toContain("Queue not created: agent-a: a queue already exists; reset it to generate again");
});

it("unlocks after a timed-out generate and shows the error", async () => {
  vi.mocked(generateAutoQueue).mockRejectedValue(new Error("Request timeout: /api/queue/generate"));
  await render([{ agentId: "agent-a", issueNumber: 5 }]);
  await act(async () => generateButton()!.click());
  expect(generateButton()!.disabled).toBe(false);
  expect(container.textContent).toContain("Queue not created: agent-a: Request timeout: /api/queue/generate");
});
