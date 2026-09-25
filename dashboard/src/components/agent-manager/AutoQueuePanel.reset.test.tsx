// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import type { AutoQueueRun, AutoQueueStatus, DispatchQueueEntry } from "../../api";
import { getAutoQueueStatus, resetAutoQueue } from "../../api";
import AutoQueuePanel from "./AutoQueuePanel";

vi.mock("../../api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api")>()),
  getAutoQueueStatus: vi.fn(),
  resetAutoQueue: vi.fn(),
}));

let root: Root;
let container: HTMLDivElement;
const tr = (_ko: string, en: string) => en;

function makeEntry(id: string, agentId: string): DispatchQueueEntry {
  return {
    id,
    agent_id: agentId,
    card_id: `card-${id}`,
    priority_rank: 0,
    reason: null,
    status: "pending",
    created_at: 1_700_000_000,
    dispatched_at: null,
    completed_at: null,
  };
}

// A NULL-agent run whose entries span two agents: the pre-#6243 handler looped
// one reset per entry agent here.
function makeStatus(status: AutoQueueRun["status"]): AutoQueueStatus {
  return {
    run: {
      id: "run-1",
      repo: "itismyfield/AgentDesk",
      agent_id: null,
      status,
      ai_model: null,
      ai_rationale: null,
      timeout_minutes: 60,
      unified_thread: false,
      unified_thread_id: null,
      created_at: 1_700_000_000,
      completed_at: null,
    },
    entries: [makeEntry("e1", "agent-a"), makeEntry("e2", "agent-b")],
    agents: {
      "agent-a": { pending: 1, dispatched: 0, done: 0, skipped: 0, failed: 0 },
      "agent-b": { pending: 1, dispatched: 0, done: 0, skipped: 0, failed: 0 },
    },
  };
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.resetAllMocks();
  vi.mocked(resetAutoQueue).mockResolvedValue({ ok: true, deleted_entries: 2, completed_runs: 1 });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

async function render(status: AutoQueueRun["status"]) {
  vi.mocked(getAutoQueueStatus).mockResolvedValue(makeStatus(status));
  await act(async () =>
    root.render(
      <AutoQueuePanel tr={tr} locale="en" agents={[]} selectedRepo="itismyfield/AgentDesk" />,
    ),
  );
}
const resetButton = () =>
  [...container.querySelectorAll("button")].find((button) => button.textContent === "Reset");

it.each<AutoQueueRun["status"]>(["generated", "pending"])(
  "resets a %s run spanning several agents with one run-pinned call",
  async (status) => {
    await render(status);
    await act(async () => resetButton()!.click());

    expect(resetAutoQueue).toHaveBeenCalledExactlyOnceWith({
      runId: "run-1",
      repo: "itismyfield/AgentDesk",
      agentId: undefined,
    });
  },
);

// The server refuses a live-run reset (409); the operator ends the run instead.
it.each<AutoQueueRun["status"]>(["active", "paused", "restoring"])(
  "hides Reset and keeps End for a %s run",
  async (status) => {
    await render(status);

    expect(resetButton()).toBeUndefined();
    expect(container.textContent).toContain("End queue");
  },
);
