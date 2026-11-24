// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { DispatchedSession } from "../types";
import { useWorkspace, WorkspaceProvider } from "./WorkspaceContext";

vi.mock("../api/client", () => ({ getDispatchedSessions: vi.fn() }));

import { getDispatchedSessions } from "../api/client";

const session = {
  id: "s1",
  session_key: "mac:session",
  name: "session",
  department_id: null,
  linked_agent_id: null,
  provider: "codex",
  model: null,
  status: "idle",
  session_info: null,
  avatar_emoji: "📡",
  tokens: 0,
  connected_at: 1,
  last_seen_at: 1,
} as DispatchedSession;

describe("workspace session bootstrap", () => {
  let container: HTMLDivElement;
  let root: Root;
  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("retries a missing session list without a toast and then clears the failure", async () => {
    vi.mocked(getDispatchedSessions).mockReset().mockResolvedValue([session]);
    let failed = true;
    let count = 0;
    const Probe = () => {
      const workspace = useWorkspace();
      failed = workspace.sessionsFailed;
      count = workspace.sessions.length;
      return null;
    };
    container = document.createElement("div");
    root = createRoot(container);
    await act(async () => {
      root.render(
        <WorkspaceProvider initialAgents={[]} initialDepartments={[]} initialSessions={null} pushNotification={() => {}}>
          <Probe />
        </WorkspaceProvider>,
      );
    });
    await act(async () => { await Promise.resolve(); });
    expect(getDispatchedSessions).toHaveBeenCalledWith(true, { suppressErrorToast: true, maxRetries: 0 });
    expect(failed).toBe(false);
    expect(count).toBe(1);
  });
});
