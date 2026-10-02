// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { generateAutoQueue, getAutoQueueStatus } from "../../api";
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

async function render(readyEntries: Array<{ agentId: string; issueNumber: number }>) {
  await act(async () =>
    root.render(
      <AutoQueuePanel
        tr={tr}
        locale="en"
        agents={[]}
        selectedRepo="itismyfield/AgentDesk"
        readyEntries={readyEntries}
      />,
    ),
  );
}
const generateButton = () =>
  [...container.querySelectorAll("button")].find((button) =>
    ["Generate", "Generating..."].includes(button.textContent ?? ""),
  )!;

it("generates one queue per agent from the ready cards and reports the ones that fail", async () => {
  vi.mocked(generateAutoQueue)
    .mockResolvedValueOnce({ run: null, entries: [], message: "No dispatchable cards found" })
    .mockResolvedValueOnce({ run: {} as never, entries: [] });
  await render([
    { agentId: "agent-b", issueNumber: 7 },
    { agentId: "agent-a", issueNumber: 5 },
    { agentId: "agent-a", issueNumber: 3 },
  ]);
  await act(async () => generateButton().click());

  expect(vi.mocked(generateAutoQueue).mock.calls).toEqual([
    [{ repo: "itismyfield/AgentDesk", agentId: "agent-a", issueNumbers: [3, 5] }],
    [{ repo: "itismyfield/AgentDesk", agentId: "agent-b", issueNumbers: [7] }],
  ]);
  expect(container.textContent).toContain("Queue not created: agent-a: No dispatchable cards found");
});

it("names the cards a created queue left out", async () => {
  vi.mocked(generateAutoQueue).mockResolvedValueOnce({
    run: {} as never,
    entries: [],
    skipped_due_to_active_dispatch: [{ issue_number: 5 }],
    skipped_due_to_filter: [{ issue_number: 7, reason: "card status 'done' is not enqueueable" }],
  });
  await render([{ agentId: "agent-a", issueNumber: 5 }]);
  await act(async () => generateButton().click());

  expect(container.textContent).toContain(
    "Cards left out: agent-a: #5 already dispatched, #7 card status 'done' is not enqueueable",
  );
});

it("keeps Generate locked after a created queue until the status refresh lands", async () => {
  vi.mocked(generateAutoQueue).mockResolvedValue({ run: {} as never, entries: [] });
  await render([{ agentId: "agent-a", issueNumber: 5 }]);
  let finishRefresh!: () => void;
  vi.mocked(getAutoQueueStatus).mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finishRefresh = () => resolve({ run: null, entries: [], agents: {} });
      }),
  );
  await act(async () => generateButton().click());
  await act(async () => generateButton().click());

  expect(generateButton().disabled).toBe(true);
  expect(generateAutoQueue).toHaveBeenCalledTimes(1);
  await act(async () => finishRefresh());
  expect(generateButton().disabled).toBe(false);
});
