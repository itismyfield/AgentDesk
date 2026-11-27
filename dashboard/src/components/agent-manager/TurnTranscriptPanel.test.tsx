// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { SessionTranscript } from "../../api";
import TurnTranscriptPanel from "./TurnTranscriptPanel";

const getCardTranscripts = vi.fn();
vi.mock("../../api", async (importOriginal) => ({ ...(await importOriginal<typeof import("../../api")>()), getCardTranscripts: (...args: unknown[]) => getCardTranscripts(...args) }));

// happy-dom has no layout, so the step list renders every row instead of a measured window.
vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: ({ count }: { count: number }) => ({
    getTotalSize: () => count * 44,
    getVirtualItems: () => Array.from({ length: count }, (_, index) => ({ index, key: index, start: index * 44, size: 44 })),
    measureElement: () => {},
  }),
}));

const tr = (_: string, en: string) => en;
const transcript = (turn: string, createdAt: string, durationMs: number, patch: Partial<SessionTranscript> = {}): SessionTranscript => ({
  id: 1, turn_id: turn, session_key: null, channel_id: null, agent_id: null, provider: "codex", dispatch_id: null, kanban_card_id: "card-1",
  dispatch_title: `[impl] ${turn}`, card_title: null, github_issue_number: null, user_message: "Fix the relay", assistant_message: "", duration_ms: durationMs,
  created_at: createdAt, events: [], ...patch,
});
const TRANSCRIPTS = [
  transcript("turn-2", "2026-11-17T10:30:00Z", 10 * 60_000, {
    events: [
      { kind: "tool_use", tool_name: "Bash", summary: null, content: "cargo test -p relay", status: null, is_error: false },
      { kind: "tool_result", tool_name: "Bash", summary: null, content: "test result: ok", status: null, is_error: false },
      { kind: "assistant", tool_name: null, summary: null, content: "Done.", status: null, is_error: false },
    ],
  }),
  transcript("turn-1", "2026-11-17T10:05:00Z", 5 * 60_000, {
    provider: "claude",
    events: [{ kind: "error", tool_name: null, summary: null, content: "tmux session missing", status: null, is_error: true }],
  }),
];

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  getCardTranscripts.mockReset();
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); });

const byTestId = (id: string) => container.querySelector<HTMLElement>(`[data-testid="${id}"]`);

it("lays turns on one axis, summarises the newest turn and shows the chosen step's input", async () => {
  getCardTranscripts.mockResolvedValue(TRANSCRIPTS);
  await act(async () => root.render(<TurnTranscriptPanel source={{ type: "card", id: "card-1" }} tr={tr} isKo={false} />));
  expect(getCardTranscripts).toHaveBeenCalledWith("card-1", 10);
  const turns = [...container.querySelectorAll<HTMLButtonElement>("[data-turn]")];
  expect(turns.map((turn) => turn.dataset.turn)).toEqual(["turn-2", "turn-1"]);
  expect(turns[0].getAttribute("aria-current")).toBe("true");
  expect(turns[1].getAttribute("aria-label")).toContain("has errors");
  expect(byTestId("turn-transcript-time-axis")?.textContent).toContain("0s");
  expect(byTestId("turn-transcript-summary")?.textContent).toBe("Duration10mEvents3Tools1Errors0");
  expect(byTestId("turn-transcript-prompt")?.querySelector("pre")?.textContent).toBe("Fix the relay");
  // The first step is chosen; a tool call's body is its input.
  expect(byTestId("turn-transcript-step-body")?.textContent).toContain("Input");
  expect(byTestId("turn-transcript-step-body")?.querySelector("pre")?.textContent).toBe("cargo test -p relay");

  const second = container.querySelector<HTMLButtonElement>('[data-step="1"]')!;
  await act(async () => second.click());
  expect(second.getAttribute("aria-current")).toBe("true");
  expect(byTestId("turn-transcript-step-body")?.textContent).toContain("Output");

  await act(async () => turns[1].click());
  expect(byTestId("turn-transcript-summary")?.textContent).toContain("Errors1");
  expect(byTestId("turn-transcript-step-body")?.textContent).toContain("Error");
  expect(byTestId("turn-transcript-step-body")?.querySelector("pre")?.textContent).toBe("tmux session missing");
});

it("reports an empty card and a failed read", async () => {
  getCardTranscripts.mockResolvedValueOnce([]);
  await act(async () => root.render(<TurnTranscriptPanel source={{ type: "card", id: "card-1" }} tr={tr} isKo={false} />));
  expect(container.textContent).toContain("No transcript available yet.");

  getCardTranscripts.mockRejectedValueOnce(new Error("offline"));
  await act(async () => root.render(<TurnTranscriptPanel source={{ type: "card", id: "card-2" }} tr={tr} isKo={false} />));
  expect(container.querySelector('[role="alert"]')?.textContent).toBe("offline");
});

it("renders AgentTiming spans and toggles AgentToolCalls summary when tool events exist", async () => {
  const richTranscripts = [
    transcript("turn-tools", "2026-11-17T11:00:00Z", 120_000, {
      agent_id: "adk-cc",
      provider: "claude",
      events: [
        { kind: "thinking", tool_name: null, summary: null, content: "Inspect failing test", status: null, is_error: false },
        { kind: "tool_use", tool_name: "Bash", summary: "cargo test", content: "cargo test", status: "done", is_error: false },
        { kind: "tool_result", tool_name: "Bash", summary: null, content: "ok", status: "done", is_error: false },
        { kind: "tool_use", tool_name: "Write", summary: "patch helper", content: "patch", status: "pending", is_error: false },
        { kind: "assistant", tool_name: null, summary: null, content: "All set.", status: null, is_error: false },
      ],
    }),
    TRANSCRIPTS[1],
  ];
  getCardTranscripts.mockResolvedValueOnce(richTranscripts);
  await act(async () => root.render(<TurnTranscriptPanel source={{ type: "card", id: "card-1" }} tr={tr} isKo={false} />));

  const timing = byTestId("turn-transcript-timing")!;
  expect(timing.querySelector(".aui-agent-timing")).not.toBeNull();
  const kinds = [...timing.querySelectorAll<HTMLElement>(".aui-agent-timing-span")].map((el) => el.dataset.kind);
  expect(kinds).toEqual(["thinking", "tool", "tool", "waiting"]);

  const toggle = byTestId("turn-transcript-tool-calls-toggle") as HTMLButtonElement;
  expect(toggle.textContent).toContain("Tool call summary (2)");
  expect(byTestId("turn-transcript-tool-calls")).toBeNull();

  await act(async () => toggle.click());
  const toolCalls = byTestId("turn-transcript-tool-calls")!;
  const embeddedScreen = toolCalls.querySelector<HTMLElement>(".aui-agent-tool-calls")!;
  expect(embeddedScreen).not.toBeNull();
  expect(embeddedScreen.dataset.embedded).toBe("true");
  expect(embeddedScreen.querySelector("nav")).toBeNull();
  expect(toolCalls.querySelector(".aui-agent-tool-calls-live")).toBeNull();
  expect(toolCalls.textContent).toContain("Bash");
  expect(toolCalls.textContent).toContain("Write");
  const headers = [...embeddedScreen.querySelectorAll("th")].map((th) => th.textContent).filter(Boolean);
  expect(headers).toEqual(["TIME", "TOOL", "INPUT", "STATUS", "DURATION", "TOKENS", "RUN"]);
  const statusEls = [...toolCalls.querySelectorAll<HTMLElement>(".aui-agent-tool-calls-status")];
  expect(statusEls.map((el) => el.dataset.status)).toEqual(expect.arrayContaining(["done", "waiting"]));
  expect(statusEls.map((el) => el.textContent)).toEqual(expect.arrayContaining(["Done", "Waiting"]));

  // Switching to a turn with no tool events hides the tool summary section.
  const secondTurn = container.querySelectorAll<HTMLButtonElement>("[data-turn]")[1]!;
  await act(async () => secondTurn.click());
  expect(byTestId("turn-transcript-tool-calls-section")).toBeNull();
});

it("shows quota/reply diagnostics and filters steps via in-memory search", async () => {
  const diagTranscripts = [
    transcript("turn-ask", "2026-11-27T05:00:00Z", 45_000, {
      provider: "claude",
      assistant_message: "Verification succeeded. Shall I merge the pull request?",
      events: [
        { kind: "tool_use", tool_name: "Read", summary: "src/config.rs", content: "src/config.rs", status: "done", is_error: false },
        { kind: "tool_result", tool_name: "Read", summary: null, content: "port = 8791", status: "done", is_error: false },
        { kind: "assistant", tool_name: null, summary: null, content: "Verification succeeded. Shall I merge the pull request?", status: "done", is_error: false },
      ],
    }),
    transcript("turn-quota", "2026-11-27T04:00:00Z", 10_000, {
      provider: "claude",
      assistant_message: "You've hit your limit · resets 5pm (Asia/Seoul)",
      events: [],
    }),
  ];
  getCardTranscripts.mockResolvedValueOnce(diagTranscripts);
  await act(async () => root.render(<TurnTranscriptPanel source={{ type: "card", id: "card-1" }} tr={tr} isKo={false} />));

  expect(byTestId("turn-transcript-diagnostics")?.textContent).toContain("Shall I merge the pull request?");
  const searchInput = byTestId("turn-transcript-search-input") as HTMLInputElement;
  expect(searchInput).not.toBeNull();

  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    setter?.call(searchInput, "8791");
    searchInput.dispatchEvent(new Event("input", { bubbles: true }));
    searchInput.dispatchEvent(new Event("change", { bubbles: true }));
  });
  expect(byTestId("turn-transcript-search-nav")?.textContent).toContain("1 / 1");
  expect(byTestId("turn-transcript-step-body")?.textContent).toContain("8791");

  const turns = [...container.querySelectorAll<HTMLButtonElement>("[data-turn]")];
  await act(async () => turns[1]!.click());
  expect(byTestId("turn-transcript-diagnostics")?.textContent).toContain("Quota hit");
});
