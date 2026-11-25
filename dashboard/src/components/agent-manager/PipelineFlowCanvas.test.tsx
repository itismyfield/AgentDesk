// @vitest-environment happy-dom
import { act, useEffect, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { buildPipelineGraph } from "./pipeline-visual-editor-model";

const flowState = vi.hoisted(() => ({
  colorMode: null as string | null,
  zoomIn: vi.fn(),
  zoomOut: vi.fn(),
  fitView: vi.fn(),
}));

vi.mock("@xyflow/react", () => ({
  Background: () => <div data-testid="rf-background" />,
  Handle: () => <span data-testid="rf-handle" />,
  MarkerType: { ArrowClosed: "arrowclosed" },
  MiniMap: () => <div data-testid="rf-minimap" />,
  Position: { Left: "left", Right: "right" },
  ReactFlow: ({
    colorMode,
    onInit,
    children,
  }: {
    colorMode?: string;
    onInit?: (instance: unknown) => void;
    children?: ReactNode;
  }) => {
    flowState.colorMode = colorMode ?? null;
    useEffect(() => {
      onInit?.({
        zoomIn: flowState.zoomIn,
        zoomOut: flowState.zoomOut,
        fitView: flowState.fitView,
      });
    }, [onInit]);
    return <div data-testid="rf-mock" data-color-mode={colorMode}>{children}</div>;
  },
}));

import PipelineFlowCanvas from "./PipelineFlowCanvas";

const sampleGraph = buildPipelineGraph(
  {
    name: "default",
    version: 1,
    states: [
      { id: "ready", label: "Ready" },
      { id: "done", label: "Done", terminal: true },
    ],
    transitions: [{ from: "ready", to: "done", type: "free", gates: [] }],
    gates: {},
    hooks: {},
    events: {},
    clocks: {},
    phase_gate: { dispatch_to: "self", dispatch_type: "phase-gate" },
  },
  false,
);

describe("PipelineFlowCanvas", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    if (!Element.prototype.scrollIntoView) {
      Element.prototype.scrollIntoView = () => {};
    }
    vi.useFakeTimers();
    flowState.colorMode = null;
    flowState.zoomIn.mockReset();
    flowState.zoomOut.mockReset();
    flowState.fitView.mockReset();
    document.documentElement.dataset.theme = "dark";
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    container.remove();
    delete document.documentElement.dataset.theme;
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("tracks document.documentElement.dataset.theme for ReactFlow colorMode", async () => {
    await act(async () => {
      root.render(
        <PipelineFlowCanvas
          compactGraph={false}
          fsmEdgeBindings={{}}
          graph={sampleGraph}
          graphPanelNote="note"
          isFsmVariant={false}
          onConnectTransition={vi.fn()}
          onSelectionChange={vi.fn()}
          selection={null}
          tr={(_ko, en) => en}
          useScrollableMobileFsmCanvas={false}
        />,
      );
    });

    expect(flowState.colorMode).toBe("dark");
    expect(container.querySelector("[data-testid='pipeline-flow-canvas']")).toBeTruthy();

    await act(async () => {
      document.documentElement.dataset.theme = "light";
      await Promise.resolve();
    });
    expect(flowState.colorMode).toBe("light");
  });

  it("defaults MiniMap to hidden, toggles it via Map button, and wires zoom/fit controls", async () => {
    const onSelectionChange = vi.fn();
    await act(async () => {
      root.render(
        <PipelineFlowCanvas
          compactGraph={false}
          fsmEdgeBindings={{}}
          graph={sampleGraph}
          graphPanelNote="note"
          isFsmVariant={false}
          onConnectTransition={vi.fn()}
          onSelectionChange={onSelectionChange}
          selection={null}
          tr={(_ko, en) => en}
          useScrollableMobileFsmCanvas={false}
        />,
      );
    });

    expect(container.querySelector("select")).toBeNull();
    const picker = container.querySelector<HTMLButtonElement>('button[role="combobox"][aria-label="State or transition to edit"]');
    expect(picker).not.toBeNull();
    await act(async () => {
      picker!.click();
    });
    const edgeOption = [...document.querySelectorAll<HTMLElement>('[role="option"]')].find((el) =>
      el.textContent?.includes("ready → done"),
    )!;
    await act(async () => {
      edgeOption.click();
    });
    expect(onSelectionChange).toHaveBeenCalledWith({ kind: "transition", index: 0 });

    expect(container.querySelector("[data-testid='rf-minimap']")).toBeNull();

    const byLabel = (label: string) =>
      container.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`)!;

    await act(async () => {
      byLabel("Toggle minimap").click();
    });
    expect(container.querySelector("[data-testid='rf-minimap']")).toBeTruthy();
    await act(async () => {
      vi.advanceTimersByTime(5_000);
    });
    expect(container.querySelector("[data-testid='rf-minimap']")).toBeTruthy();

    await act(async () => {
      byLabel("Zoom out").click();
      byLabel("Zoom in").click();
      byLabel("Fit to screen").click();
    });
    expect(flowState.zoomOut).toHaveBeenCalledTimes(1);
    expect(flowState.zoomIn).toHaveBeenCalledTimes(1);
    expect(flowState.fitView).toHaveBeenCalledWith({ padding: 0.18 });

    await act(async () => {
      byLabel("Toggle minimap").click();
    });
    expect(container.querySelector("[data-testid='rf-minimap']")).toBeNull();
  });
});
