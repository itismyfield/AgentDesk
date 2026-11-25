// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DispatchedSession } from "../../types";
import { SessionInfoCard, inferSessionContextLimit } from "./SessionInfoCard";

vi.mock("./SessionPanelModel", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./SessionPanelModel")>()),
  fetchSessionOutput: vi.fn().mockResolvedValue({
    recent_output: "ready",
    backend: "tmux",
    available: true,
    output_format: "plain",
    captured_at_ms: 1_700_000_000_000,
  }),
}));

let container: HTMLDivElement;
let root: Root;
const tr = (_ko: string, en: string) => en;

function makeSession(patch: Partial<DispatchedSession> = {}): DispatchedSession {
  return {
    id: "sess-1",
    session_key: "claude/discord/adk-cc",
    name: "adk-cc",
    department_id: null,
    linked_agent_id: null,
    provider: "claude",
    model: "claude-sonnet-4-5",
    status: "working",
    session_info: "Running tests",
    avatar_emoji: "🤖",
    tokens: 50_000,
    connected_at: 1_700_000_000_000,
    last_seen_at: 1_700_000_060_000,
    ...patch,
  };
}

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

describe("SessionInfoCard", () => {
  it("renders ContextCompaction when session tokens > 0 and hides it at 0", async () => {
    await act(async () => {
      root.render(
        <SessionInfoCard
          id="info-1"
          session={makeSession({ tokens: 50_000, model: "claude-sonnet-4-5" })}
          departments={[]}
          locale="en"
          tr={tr}
          isKo={false}
        />,
      );
    });

    const ctx = container.querySelector<HTMLElement>('[data-testid="session-context-sess-1"]')!;
    expect(ctx).not.toBeNull();
    expect(ctx.querySelector(".aui-context-compaction")).not.toBeNull();
    expect(ctx.textContent).toContain("Context usage");
    expect(ctx.textContent).toContain("claude-sonnet-4-5");
    expect(ctx.textContent).toContain("25%");
    expect(ctx.textContent).toContain("50K of 200K");
    expect(ctx.textContent).toContain("compacts at 160K");

    await act(async () => {
      root.render(
        <SessionInfoCard
          id="info-1"
          session={makeSession({ tokens: 0 })}
          departments={[]}
          locale="en"
          tr={tr}
          isKo={false}
        />,
      );
    });
    expect(container.querySelector('[data-testid="session-context-sess-1"]')).toBeNull();
  });

  it("infers context limits from model identifiers", () => {
    expect(inferSessionContextLimit(null)).toBe(200_000);
    expect(inferSessionContextLimit("claude-opus-4-6[1m]")).toBe(1_000_000);
    expect(inferSessionContextLimit("qwen-coder")).toBe(256_000);
    expect(inferSessionContextLimit("gpt-4o")).toBe(128_000);
  });
});
