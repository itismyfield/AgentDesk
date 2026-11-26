// @vitest-environment happy-dom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act } from "react";
import { createRoot } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Agent } from "../../types";
import AuditPageView from "./AuditPageView";

vi.mock("../../api/promptAudit", () => ({
  isAuditBindingMissing: () => false,
  getPromptAudit: vi.fn(async (agentId: string) => ({
    agent_id: agentId,
    provider: "claude",
    session_id: "s1",
    session_started_at: "2026-11-25T00:00:00Z",
    origin: "current_session",
    masked_count: 0,
    sections: [
      { name: "role_prompt", source: "canon_file", path: "agents/adk.md", chars: 80, masked_count: 0, text: "# Role\nBuild cleanly." },
      { name: "runtime", source: "runtime", path: null, chars: 20, masked_count: 0, text: "now=today" },
    ],
  })),
  getAgentAnchors: vi.fn(async (agentId: string) => ({
    agent_id: agentId,
    provider: "claude",
    session_id: "s1",
    session_started_at: "2026-11-25T00:00:00Z",
    source: "current_session",
    memento: { status: "connected", checked_at: "2026-11-25T00:00:00Z" },
    anchor_count: 1,
    anchors: [
      {
        id: "anc-1",
        type: "decision",
        scope: null,
        chars: 42,
        limit: 300,
        text: "단일 문단 의사결정 기록입니다.",
        masked_count: 0,
        overlap_candidates: [],
        receiving_agents: ["adk-main"],
      },
    ],
  })),
}));
vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);

const AGENTS: Agent[] = [
  { id: "adk-main", name: "ADK Main", name_ko: "메인", status: "idle", cli_provider: "claude", discord_channel_cc: "1" } as Agent,
  { id: "relay-e2e", name: "Relay E2E", name_ko: "테스트", status: "idle", cli_provider: "claude", discord_channel_cc: "2" } as Agent,
];

describe("AuditPageView", () => {
  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("hides E2E test agents by default with an include-tests checkbox and avoids duplicate source share rows", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    await act(async () => {
      root.render(
        <QueryClientProvider client={client}>
          <MemoryRouter initialEntries={["/audit"]}>
            <AuditPageView isKo agents={AGENTS} isMobileViewport={false} />
          </MemoryRouter>
        </QueryClientProvider>,
      );
    });
    await act(async () => { await new Promise((r) => setTimeout(r, 10)); });

    expect(container.querySelector('[data-testid="audit-agent-adk-main"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="audit-agent-relay-e2e"]')).toBeNull();

    const includeTestsInput = container.querySelector<HTMLInputElement>('input[data-testid="audit-include-tests"]');
    expect(includeTestsInput).toBeTruthy();
    await act(async () => { includeTestsInput!.click(); });
    expect(container.querySelector('[data-testid="audit-agent-relay-e2e"]')).toBeTruthy();

    expect(container.querySelector('[data-testid="audit-share-bar"]')).toBeTruthy();
    const sourceSummary = container.querySelector('[data-testid="audit-source-summary"]');
    expect(sourceSummary).toBeTruthy();
    expect(sourceSummary?.textContent).not.toContain("0자 · 0%");

    await act(async () => root.unmount());
  });

  it("renders prose anchor content cleanly inside audit-anchor-inspector without CodeBlock gutter", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    await act(async () => {
      root.render(
        <QueryClientProvider client={client}>
          <MemoryRouter initialEntries={["/audit?tab=anchors"]}>
            <AuditPageView isKo agents={AGENTS} isMobileViewport={false} />
          </MemoryRouter>
        </QueryClientProvider>,
      );
    });
    await act(async () => { await new Promise((r) => setTimeout(r, 10)); });

    const inspector = container.querySelector('[data-testid="audit-anchor-inspector"]');
    expect(inspector).toBeTruthy();
    const proseWell = inspector?.querySelector('[data-testid="audit-anchor-text"]');
    expect(proseWell?.textContent).toBe("단일 문단 의사결정 기록입니다.");
    expect(proseWell?.className).toContain("whitespace-pre-wrap");
    expect(proseWell?.className).toContain("max-h-80");
    expect(inspector?.querySelector('[data-testid="audit-anchor-size"]')).toBeTruthy();
    expect(Array.from(inspector?.querySelectorAll("button") ?? []).some((b) => b.textContent?.includes("원문 복사"))).toBe(true);

    await act(async () => root.unmount());
  });

  it("lets mobile users switch from anchors detail back to system prompts via audit-open-prompts", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    await act(async () => {
      root.render(
        <QueryClientProvider client={client}>
          <MemoryRouter initialEntries={["/audit?tab=anchors&agent=adk-main"]}>
            <AuditPageView isKo={false} agents={AGENTS} isMobileViewport={true} />
          </MemoryRouter>
        </QueryClientProvider>,
      );
    });
    await act(async () => { await new Promise((r) => setTimeout(r, 10)); });

    expect(container.querySelector('[data-testid="audit-anchor-detail"]')).toBeTruthy();
    const openPromptsBtn = container.querySelector<HTMLButtonElement>('[data-testid="audit-open-prompts"]');
    expect(openPromptsBtn?.textContent).toBe("System prompts");

    await act(async () => { openPromptsBtn!.click(); });
    await act(async () => { await new Promise((r) => setTimeout(r, 10)); });

    expect(container.querySelector('[data-testid="audit-prompt-detail"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="audit-anchor-detail"]')).toBeNull();

    await act(async () => root.unmount());
  });
});
