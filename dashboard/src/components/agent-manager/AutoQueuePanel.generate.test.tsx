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

it("generates one queue per agent from the ready cards and reports the ones that fail", async () => {
  vi.mocked(generateAutoQueue)
    .mockResolvedValueOnce({ run: null, entries: [], message: "No dispatchable cards found" })
    .mockResolvedValueOnce({ run: {} as never, entries: [] });
  await act(async () =>
    root.render(
      <AutoQueuePanel
        tr={tr}
        locale="en"
        agents={[]}
        selectedRepo="itismyfield/AgentDesk"
        readyEntries={[
          { agentId: "agent-b", issueNumber: 7 },
          { agentId: "agent-a", issueNumber: 5 },
          { agentId: "agent-a", issueNumber: 3 },
        ]}
      />,
    ),
  );
  const button = [...container.querySelectorAll("button")].find(
    (candidate) => candidate.textContent === "Generate",
  );
  await act(async () => button!.click());

  expect(vi.mocked(generateAutoQueue).mock.calls).toEqual([
    [{ repo: "itismyfield/AgentDesk", agentId: "agent-a", issueNumbers: [3, 5] }],
    [{ repo: "itismyfield/AgentDesk", agentId: "agent-b", issueNumbers: [7] }],
  ]);
  expect(container.textContent).toContain("Queue not created: agent-a: No dispatchable cards found");
});
