// @vitest-environment happy-dom

/**
 * #5131 regression guard.
 *
 * `auto_queue_runs.deploy_phases` was retired (migration 0006) and the producer
 * for the derived `deployPhases` set was deleted from `AutoQueuePanel`. The
 * consumer in `createAutoQueuePhaseRenderers` was left behind, and because the
 * shared context was typed `any`, both `tsc -b` and `vite build` stayed green
 * while `renderPhaseGateIndicator` crashed with
 * `TypeError: Cannot read properties of undefined (reading 'has')` for any run
 * that has batch phases — taking the whole AutoQueuePanel down with it.
 *
 * These tests exercise the crash path directly (the renderer) and through the
 * real call sites in `AutoQueuePanelView` (the `hasBatchPhases` branches of both
 * the "all" and "thread" views), using fixtures shaped like the
 * `/api/queue/status` payload. A build cannot catch this defect; only executing
 * the renderer can.
 */

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it } from "vitest";

import type {
  AutoQueueRun,
  DispatchQueueEntry,
  PhaseGateInfo,
} from "../../api";
import AutoQueuePanelView from "./AutoQueuePanelView";
import { createAutoQueuePhaseRenderers } from "./AutoQueuePhaseRenderers";
import { useSortableReorder } from "./AutoQueueSortableRows";
import type {
  AutoQueuePanelCtx,
  AutoQueuePhaseRendererCtx,
} from "./auto-queue-panel-ctx";
import type { ViewMode } from "./auto-queue-panel-utils";

const tr = (ko: string, _en: string) => ko;

function makeEntry(overrides: Partial<DispatchQueueEntry> = {}): DispatchQueueEntry {
  return {
    id: "entry-1",
    agent_id: "agent-1",
    card_id: "card-1",
    priority_rank: 0,
    reason: null,
    status: "pending",
    created_at: 1_700_000_000,
    dispatched_at: null,
    completed_at: null,
    thread_group: 1,
    batch_phase: 1,
    ...overrides,
  };
}

function makeGate(overrides: Partial<PhaseGateInfo> = {}): PhaseGateInfo {
  return {
    id: 1,
    phase: 1,
    status: "pending",
    ...overrides,
  };
}

function makeRun(overrides: Partial<AutoQueueRun> = {}): AutoQueueRun {
  return {
    id: "run-1",
    repo: "itismyfield/AgentDesk",
    agent_id: "agent-1",
    status: "active",
    ai_model: "test-model",
    ai_rationale: null,
    timeout_minutes: 60,
    unified_thread: false,
    unified_thread_id: null,
    created_at: 1_700_000_000,
    completed_at: null,
    max_concurrent_threads: 2,
    thread_group_count: 2,
    ...overrides,
  };
}

/** Mirrors the `gatesByPhase` derivation in `AutoQueuePanel`. */
function groupGatesByPhase(gates: PhaseGateInfo[]): Map<number, PhaseGateInfo[]> {
  const byPhase = new Map<number, PhaseGateInfo[]>();
  for (const gate of gates) {
    const list = byPhase.get(gate.phase) ?? [];
    list.push(gate);
    byPhase.set(gate.phase, list);
  }
  return byPhase;
}

function makeRendererCtx(
  gates: PhaseGateInfo[],
  overrides: Partial<AutoQueuePhaseRendererCtx> = {},
): AutoQueuePhaseRendererCtx {
  return {
    currentBatchPhase: 1,
    gatesByPhase: groupGatesByPhase(gates),
    hasBatchPhases: true,
    handleEntryStatusUpdate: () => {},
    locale: "ko",
    threadGroups: {},
    tr,
    ...overrides,
  };
}

describe("createAutoQueuePhaseRenderers / renderPhaseGateIndicator", () => {
  it("renders a phase-gate run without touching the retired deploy_phases field", () => {
    const { renderPhaseGateIndicator } = createAutoQueuePhaseRenderers(
      makeRendererCtx([makeGate({ phase: 1, status: "pending" })]),
    );

    // #5131: this call is what used to throw
    // `TypeError: ... reading 'has'` — assert it explicitly rather than
    // relying on the surrounding render to surface it.
    expect(() => renderPhaseGateIndicator(1)).not.toThrow();
  });

  it("drives the indicator off the phase_gates payload, not deploy_phases", () => {
    const { renderPhaseGateIndicator } = createAutoQueuePhaseRenderers(
      makeRendererCtx([makeGate({ phase: 2, status: "passed" })], {
        currentBatchPhase: 2,
      }),
    );

    const markup = renderToStaticMarkup(renderPhaseGateIndicator(2));
    expect(markup).toContain("게이트");
    expect(markup).toContain("통과");
  });

  it("surfaces a failed gate's failure_reason from the phase_gates payload", () => {
    const { renderPhaseGateIndicator } = createAutoQueuePhaseRenderers(
      makeRendererCtx([
        makeGate({ phase: 1, status: "failed", failure_reason: "ci red" }),
      ]),
    );

    const markup = renderToStaticMarkup(renderPhaseGateIndicator(1));
    expect(markup).toContain("실패");
    expect(markup).toContain("ci red");
  });

  it("renders a phase that has no gate row at all", () => {
    const { renderPhaseGateIndicator } = createAutoQueuePhaseRenderers(
      makeRendererCtx([]),
    );

    expect(() => renderPhaseGateIndicator(3)).not.toThrow();
    expect(renderToStaticMarkup(renderPhaseGateIndicator(3))).toContain("게이트");
  });
});

describe("AutoQueuePanelView with a phase-gate run", () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  const entries = [
    makeEntry({ id: "entry-1", batch_phase: 1, thread_group: 1 }),
    makeEntry({
      id: "entry-2",
      batch_phase: 2,
      thread_group: 2,
      status: "dispatched",
    }),
  ];
  const gates = [
    makeGate({ id: 1, phase: 1, status: "passed" }),
    makeGate({ id: 2, phase: 2, status: "pending" }),
  ];

  function makePanelCtx(): Omit<AutoQueuePanelCtx, "allDrag" | "viewMode"> {
    return {
      ...makeRendererCtx(gates),
      activating: false,
      agentStats: {
        "agent-1": { pending: 1, dispatched: 1, done: 0, skipped: 0, failed: 0 },
      },
      allEntriesSorted: entries,
      completedCount: 0,
      dispatchedCount: 1,
      doneCount: 0,
      entries,
      entriesByAgent: new Map([["agent-1", entries]]),
      entriesByThreadGroup: new Map([
        [1, [entries[0]]],
        [2, [entries[1]]],
      ]),
      error: null,
      expanded: true,
      failedCount: 0,
      generating: false,
      getAgentLabel: (agentId: string) => agentId,
      handleActivate: () => {},
      handleFallbackActivate: () => {},
      handleGenerate: () => {},
      handleReorder: async () => {},
      handleReset: () => {},
      handleRunAction: () => {},
      hasThreadGroups: true,
      maxConcurrent: 2,
      pendingCount: 1,
      phaseSections: [
        [1, [entries[0]]],
        [2, [entries[1]]],
      ],
      primaryAction: "dispatch",
      readyEntries: [],
      requestProgress: null,
      run: makeRun(),
      selectedRepo: "itismyfield/AgentDesk",
      setExpanded: () => {},
      setViewMode: () => {},
      showRunStartControls: false,
      skippedCount: 0,
      startActionLabel: "디스패치",
      totalCount: entries.length,
    };
  }

  function Harness({ viewMode }: { viewMode: ViewMode }) {
    const base = makePanelCtx();
    const allDrag = useSortableReorder(base.allEntriesSorted, base.handleReorder);
    return <AutoQueuePanelView ctx={{ ...base, allDrag, viewMode }} />;
  }

  async function render(viewMode: ViewMode) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root?.render(<Harness viewMode={viewMode} />);
    });
    return container;
  }

  afterEach(async () => {
    if (root) {
      await act(async () => {
        root?.unmount();
      });
      root = null;
    }
    container?.remove();
    container = null;
  });

  // AutoQueuePanelView.tsx renders renderPhaseGateIndicator(phase) inside the
  // hasBatchPhases branch of both views; either one crashing takes the panel out.
  it.each<ViewMode>(["all", "thread"])(
    "mounts the %s view and renders the phase-gate indicators",
    async (viewMode) => {
      const mounted = await render(viewMode);
      expect(mounted.textContent).toContain("게이트");
      expect(mounted.textContent).toContain("자동 큐");
    },
  );
});
