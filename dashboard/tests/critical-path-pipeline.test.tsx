// @vitest-environment happy-dom
import { act, useEffect, type CSSProperties, type ComponentType, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { campaignSchema } from "../src/api/campaigns";
import { serverCampaignDocumentSchema } from "../src/api/serverCampaigns";
import {
  campaignPipelineSchema,
  getDefaultPipelineTemplate,
  getPipelineConditionFields,
  getServerCampaignPipeline,
  saveServerCampaignPipeline,
  validateServerCampaignPipeline,
} from "../src/api/serverCampaignPipeline";
import {
  compareCampaignCriticalPath,
  computeCampaignCriticalPath,
} from "../src/components/campaigns/campaignCriticalPath";
import { rawEdgeId } from "../src/components/campaigns/campaignGraphRouting";
import { legacyTask } from "../src/components/campaigns/campaignOverviewModel";
import CampaignPipeline from "../src/components/campaigns/CampaignPipeline";
import CampaignRelations from "../src/components/campaigns/CampaignRelations";
import CampaignRelationsEditor from "../src/components/campaigns/CampaignRelationsEditor";
import { installMemoryStorage, makeCampaignNode } from "../src/components/campaigns/campaignTestFixtures";
import {
  adkCriticalPathDemoCampaign,
  adkCriticalPathDemoLive,
  demoPipelineConditionFields,
  demoPipelineTemplate,
  srvPipelineBottleneckDetail,
  srvPipelineBottleneckDocument,
} from "../src/components/campaigns/fixtures/criticalPathPipelineDemoFixtures";
import { insertStateBetween, spliceStateBetween } from "../src/components/campaigns/pipelineEdit";
import { resetOrphanedEditorEntries } from "../src/components/campaigns/useEditorHistory";

vi.mock("../src/app/shellFocus", () => ({
  useShellFocus: vi.fn(),
}));

vi.mock("../src/api/serverCampaignPipeline", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../src/api/serverCampaignPipeline")>()),
  getDefaultPipelineTemplate: vi.fn(),
  getServerCampaignPipeline: vi.fn(),
  getPipelineConditionFields: vi.fn(),
  validateServerCampaignPipeline: vi.fn(),
  saveServerCampaignPipeline: vi.fn(),
}));

vi.mock("@xyflow/react", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@xyflow/react")>();
  return {
    ...actual,
    useStore: <T,>(selector: (state: { transform: [number, number, number] }) => T) =>
      selector({ transform: [0, 0, 1] }),
    BaseEdge: ({ id, path, style }: { id?: string; path?: string; style?: CSSProperties }) => (
      <svg data-edge-svg={id}>
        <path d={path} style={style} />
      </svg>
    ),
    EdgeLabelRenderer: ({ children }: { children?: ReactNode }) => (
      <div data-testid="edge-label-layer">{children}</div>
    ),
    ReactFlow: ({
      nodes,
      edges = [],
      nodeTypes = {},
      edgeTypes = {},
      onInit,
      onNodesChange,
      onNodeDoubleClick,
      children,
    }: {
      nodes: Array<{ id: string; type?: string; data?: unknown; selected?: boolean }>;
      edges?: Array<{ id: string; source: string; target: string; type?: string; data?: unknown }>;
      nodeTypes?: Record<string, ComponentType<Record<string, unknown>>>;
      edgeTypes?: Record<string, ComponentType<Record<string, unknown>>>;
      onInit?: (instance: unknown) => void;
      onNodesChange?: (changes: unknown[]) => void;
      onNodeDoubleClick?: (event: unknown, node: { id: string }) => void;
      children?: ReactNode;
    }) => {
      useEffect(() => {
        onInit?.({
          fitView: vi.fn(),
          setViewport: vi.fn(),
          getViewport: () => ({ x: 0, y: 0, zoom: 1 }),
          zoomIn: vi.fn(),
          zoomOut: vi.fn(),
        });
      }, []); // eslint-disable-line react-hooks/exhaustive-deps
      return (
        <div data-testid="mock-reactflow" data-node-count={nodes.length} data-edge-count={edges.length}>
          {nodes.map((node) => {
            const NodeComp = node.type ? nodeTypes[node.type] : undefined;
            return (
              <div
                key={node.id}
                className="react-flow__node"
                data-id={node.id}
                data-rf-node={node.id}
                onClick={() => onNodesChange?.([{ type: "select", id: node.id, selected: true }])}
                onDoubleClick={() => onNodeDoubleClick?.({}, node)}
              >
                {NodeComp ? (
                  <NodeComp id={node.id} data={node.data} selected={Boolean(node.selected)} isConnectable />
                ) : null}
              </div>
            );
          })}
          {edges.map((edge, idx) => {
            const EdgeComp = edge.type ? edgeTypes[edge.type] : undefined;
            return (
              <div key={edge.id} data-rf-edge={edge.id}>
                {EdgeComp ? (
                  <EdgeComp
                    id={edge.id}
                    source={edge.source}
                    target={edge.target}
                    sourceX={idx * 40}
                    sourceY={20}
                    targetX={idx * 40 + 120}
                    targetY={20}
                    sourcePosition={actual.Position.Right}
                    targetPosition={actual.Position.Left}
                    data={edge.data}
                  />
                ) : null}
              </div>
            );
          })}
          {children}
        </div>
      );
    },
    Background: () => null,
    MiniMap: () => null,
    Handle: () => null,
  };
});

const koTr = (ko: string, _en: string) => ko;

let container: HTMLDivElement;
let root: Root;

function mockViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, writable: true, value: width });
  window.matchMedia = vi.fn().mockImplementation((query: string) => {
    const minMatch = query.match(/\(min-width:\s*(\d+)px\)/);
    const maxMatch = query.match(/\(max-width:\s*(\d+)px\)/);
    const matches = minMatch
      ? width >= Number(minMatch[1])
      : maxMatch
        ? width <= Number(maxMatch[1])
        : false;
    return {
      matches,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    };
  });
}

async function setInputValue(input: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  installMemoryStorage();
  mockViewportWidth(1440);
  resetOrphanedEditorEntries();
  vi.mocked(getDefaultPipelineTemplate).mockResolvedValue(structuredClone(demoPipelineTemplate));
  vi.mocked(getServerCampaignPipeline).mockResolvedValue(structuredClone(srvPipelineBottleneckDetail));
  vi.mocked(getPipelineConditionFields).mockResolvedValue(structuredClone(demoPipelineConditionFields));
  vi.mocked(validateServerCampaignPipeline).mockResolvedValue({ errors: [], warnings: [] });
  vi.mocked(saveServerCampaignPipeline).mockResolvedValue({ definition_revision: 5 });

  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.clearAllMocks();
});

describe("Demo fixtures & pure critical-path / What-if engine", () => {
  it("validates demo fixtures against API Zod schemas", () => {
    expect(() => campaignSchema.parse(adkCriticalPathDemoCampaign)).not.toThrow();
    expect(() => serverCampaignDocumentSchema.parse(srvPipelineBottleneckDocument)).not.toThrow();
    expect(() => campaignPipelineSchema.parse(srvPipelineBottleneckDetail)).not.toThrow();
  });

  it("computes remaining vs structural critical paths, exact slack values, What-if absorption, and path shifts", () => {
    const remaining = computeCampaignCriticalPath(adkCriticalPathDemoCampaign.nodes, { mode: "remaining" });
    expect(remaining.totalDuration).toBe(4);
    expect([...remaining.criticalNodeIds].sort()).toEqual(["n2", "n3", "n4", "n5"]);
    expect(remaining.nodes.get("n1")?.duration).toBe(0);
    expect(remaining.nodes.get("n1")?.critical).toBe(false);

    // Exact slack: 0 on Chain A, 1 on Chain B (n6 -> n8), 2 on Chain C (n7)
    expect(remaining.nodes.get("n2")?.slack).toBe(0);
    expect(remaining.nodes.get("n3")?.slack).toBe(0);
    expect(remaining.nodes.get("n4")?.slack).toBe(0);
    expect(remaining.nodes.get("n5")?.slack).toBe(0);
    expect(remaining.nodes.get("n6")?.slack).toBe(1);
    expect(remaining.nodes.get("n8")?.slack).toBe(1);
    expect(remaining.nodes.get("n7")?.slack).toBe(2);

    const structural = computeCampaignCriticalPath(adkCriticalPathDemoCampaign.nodes, { mode: "structural" });
    expect(structural.totalDuration).toBe(5);
    expect(structural.nodes.get("n1")?.critical).toBe(true);
    expect([...structural.criticalNodeIds].sort()).toEqual(["n1", "n2", "n3", "n4", "n5"]);

    // What-if +1 on n6: absorbed by slack = 1 -> durationDelta === 0
    const plusOne = compareCampaignCriticalPath(adkCriticalPathDemoCampaign.nodes, "remaining", { n6: 1 });
    expect(plusOne.hasWhatIf).toBe(true);
    expect(plusOne.durationDelta).toBe(0);
    expect(plusOne.simulated.totalDuration).toBe(4);

    // What-if +2 on n6: exceeds slack -> durationDelta === +1 and critical path shifts onto n6 -> n8 -> n5
    const plusTwo = compareCampaignCriticalPath(adkCriticalPathDemoCampaign.nodes, "remaining", { n6: 2 });
    expect(plusTwo.durationDelta).toBe(1);
    expect(plusTwo.simulated.totalDuration).toBe(5);
    expect(plusTwo.pathShifted).toBe(true);
    expect(plusTwo.newlyCriticalIds.sort()).toEqual(["n6", "n8"]);
    expect([...plusTwo.simulated.criticalNodeIds].sort()).toEqual(["n5", "n6", "n8"]);
    expect(plusTwo.simulated.nodes.get("n2")?.slack).toBe(1);
  });

  it("handles cyclic subgraphs safely without infinite recursion and preserves critical edges across zero-duration completed bridges", () => {
    const cyclicNodes = [
      makeCampaignNode("a", { status: "running", dependencies: ["b"] }),
      makeCampaignNode("b", { status: "running", dependencies: ["a"] }),
      makeCampaignNode("u", { status: "running", dependencies: [] }),
      makeCampaignNode("m", { status: "completed", dependencies: ["u"] }),
      makeCampaignNode("v", { status: "pending", dependencies: ["m"] }),
    ];
    const result = computeCampaignCriticalPath(cyclicNodes, { mode: "remaining" });
    expect([...result.cyclicIds].sort()).toEqual(["a", "b"]);
    expect(result.nodes.get("a")?.cyclic).toBe(true);
    expect(result.nodes.get("a")?.critical).toBe(false);
    expect(result.totalDuration).toBe(2);
    expect([...result.criticalNodeIds].sort()).toEqual(["u", "v"]);
    expect(result.criticalEdgeKeys.has(rawEdgeId(["u", "m"]))).toBe(true);
    expect(result.criticalEdgeKeys.has(rawEdgeId(["m", "v"]))).toBe(true);
  });
});

describe("CampaignRelations critical path bar & What-if simulator", () => {
  it("toggles critical path, simulates +1 absorbed and +2 shifted on n6, and switches timeline/graph cleanly", async () => {
    const liveMap = adkCriticalPathDemoLive[adkCriticalPathDemoCampaign.id];
    let selectedId: string | null = "n6";
    const renderView = async () => {
      await act(async () => {
        root.render(
          <CampaignRelations
            campaign={adkCriticalPathDemoCampaign}
            live={liveMap}
            selectedId={selectedId}
            onOpen={(next) => {
              selectedId = next;
            }}
            onEdit={vi.fn()}
            tr={koTr}
          />,
        );
      });
    };

    await renderView();

    const toggleBtn = container.querySelector<HTMLButtonElement>('[data-testid="toggle-critical-path"]');
    expect(toggleBtn).not.toBeNull();
    expect(container.querySelector('[data-testid="critical-path-bar"]')).toBeNull();

    await act(async () => {
      toggleBtn!.click();
    });

    const bar = container.querySelector('[data-testid="critical-path-bar"]');
    expect(bar).not.toBeNull();
    expect(bar!.textContent).toContain("최장 4단계 · 핵심 작업 4개");

    // Simulate +1 on n6 -> absorbed (±0)
    const plus1 = container.querySelector<HTMLButtonElement>('[data-testid="whatif-plus-1"]');
    expect(plus1).not.toBeNull();
    await act(async () => {
      plus1!.click();
    });
    expect(container.querySelector('[data-testid="whatif-delta-chip"]')?.textContent).toContain("여유 단계 내 흡수 (지연 0)");

    // Reset What-if
    const resetBtn = container.querySelector<HTMLButtonElement>('[data-testid="whatif-reset"]');
    expect(resetBtn).not.toBeNull();
    await act(async () => {
      resetBtn!.click();
    });
    expect(container.querySelector('[data-testid="whatif-delta-chip"]')).toBeNull();

    // Simulate +2 on n6 -> +1 delayed & critical path shifted
    const plus2 = container.querySelector<HTMLButtonElement>('[data-testid="whatif-plus-2"]');
    await act(async () => {
      plus2!.click();
    });
    expect(container.querySelector('[data-testid="whatif-delta-chip"]')?.textContent).toContain("전체 +1단계 지연");
    expect(container.querySelector('[data-testid="whatif-shifted-chip"]')?.textContent).toContain("병목 경로 이동됨");

    // Switch to timeline view and verify GanttTimeline critical container renders cleanly
    const timelineRadio = Array.from(container.querySelectorAll<HTMLElement>('[role="radio"], button')).find(
      (el) => el.textContent?.trim() === "타임라인",
    );
    expect(timelineRadio).toBeDefined();
    await act(async () => {
      timelineRadio!.click();
    });
    expect(container.querySelector('[data-critical-timeline="on"]')).not.toBeNull();
  });
});

describe("CampaignRelationsEditor live depth delta & critical path toggle", () => {
  it("renders editor-depth-badge, toggles critical highlight, and updates live with +Δ and -Δ on dependency edits", async () => {
    const leave = { current: null };
    await act(async () => {
      root.render(
        <CampaignRelationsEditor
          campaign={adkCriticalPathDemoCampaign}
          scope={{ kind: "group", key: "관계도 UI" }}
          onScope={vi.fn()}
          onExit={vi.fn()}
          onSaved={vi.fn()}
          leave={leave}
          tr={koTr}
        />,
      );
    });

    const depthBadge = container.querySelector('[data-testid="editor-depth-badge"]');
    expect(depthBadge?.textContent).toBe("직렬 깊이 5단계");

    const critToggle = container.querySelector<HTMLButtonElement>('[data-testid="editor-toggle-critical"]');
    expect(critToggle).not.toBeNull();
    expect(critToggle!.getAttribute("aria-pressed")).toBe("false");
    await act(async () => {
      critToggle!.click();
    });
    expect(critToggle!.getAttribute("aria-pressed")).toBe("true");

    // Select n3 on canvas and add dependency n8 (#8408) -> n1 -> n6 -> n8 -> n3 -> n4 -> n5 raises depth 5 -> 6 (+1)
    const n3Node = container.querySelector<HTMLElement>('[data-rf-node="n3"]');
    expect(n3Node).not.toBeNull();
    await act(async () => {
      n3Node!.click();
    });

    const addSelectTrigger = container.querySelector<HTMLElement>('[aria-label="선행 추가"]');
    expect(addSelectTrigger).not.toBeNull();
    await act(async () => {
      addSelectTrigger!.click();
    });
    const n8Option = Array.from(document.querySelectorAll<HTMLElement>('[role="option"]')).find((opt) =>
      opt.textContent?.includes("#8408"),
    );
    expect(n8Option).toBeDefined();
    await act(async () => {
      n8Option!.click();
    });

    expect(container.querySelector('[data-testid="editor-depth-badge"]')?.textContent).toBe(
      "직렬 깊이 5 → 6단계 (+1)",
    );

    // Undo back to baseline, then select n4 on canvas and remove dependency n3 -> depth drops 5 -> 4 (-1)
    const undoBtn = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
      (btn) => btn.textContent?.trim() === "되돌리기",
    );
    await act(async () => {
      undoBtn!.click();
    });
    expect(container.querySelector('[data-testid="editor-depth-badge"]')?.textContent).toBe("직렬 깊이 5단계");

    const n4Node = container.querySelector<HTMLElement>('[data-rf-node="n4"]');
    expect(n4Node).not.toBeNull();
    await act(async () => {
      n4Node!.click();
    });

    const removeN3Btn = container.querySelector<HTMLButtonElement>(
      '[aria-label*="선행 #8405"][aria-label*="빼기"]',
    );
    expect(removeN3Btn).not.toBeNull();
    await act(async () => {
      removeN3Btn!.click();
    });
    expect(container.querySelector('[data-testid="editor-depth-badge"]')?.textContent).toBe(
      "직렬 깊이 5 → 4단계 (-1)",
    );
  });
});

describe("Pipeline in-place state insertion, multi-step Undo, and FSM bottleneck strips", () => {
  it("preserves conditions/gates/retries in insertStateBetween and wires spliceStateBetween cleanly", () => {
    const base = structuredClone(srvPipelineBottleneckDetail.pipeline);
    const [inserted, newId] = insertStateBetween(base, "t_review_deploy", "auto_check", "배포 전 보안 스캔");
    const newState = inserted.states.find((s) => s.id === newId);
    expect(newState?.title).toBe("배포 전 보안 스캔");
    expect(newState?.kind).toBe("auto_check");

    const rewired = inserted.transitions.find((t) => t.id === "t_review_deploy")!;
    expect(rewired.from).toBe("human_review");
    expect(rewired.to).toBe(newId);
    expect(rewired.gate).toBe("pr_merge");
    expect(rewired.condition).toEqual({ field: "decision.approved", op: "eq", value: true });

    const outgoing = inserted.transitions.find((t) => t.from === newId && t.to === "deploy");
    expect(outgoing).toBeDefined();
    expect(outgoing?.condition).toEqual({ field: "state.outcome", op: "eq", value: "succeeded" });

    const spliced = spliceStateBetween(base, "t_plan_impl", "auto_check");
    expect(spliced.transitions.find((t) => t.id === "t_plan_impl")?.to).toBe("auto_check");
    expect(spliced.transitions.some((t) => t.from === "auto_check" && t.to === "implement")).toBe(true);
  });

  it("renders ledger-bottleneck-strip with bottleneck stage, rework loop share, and blocked count", async () => {
    const liveMap = adkCriticalPathDemoLive[adkCriticalPathDemoCampaign.id];
    const tasks = adkCriticalPathDemoCampaign.nodes.map((n) => legacyTask(n, liveMap?.[n.id]));
    await act(async () => {
      root.render(
        <CampaignPipeline
          source={{ kind: "legacy" }}
          campaign={adkCriticalPathDemoCampaign}
          tasks={tasks}
          tr={koTr}
        />,
      );
    });
    // Wait for async template load
    await act(async () => {
      await Promise.resolve();
    });

    const strip = container.querySelector('[data-testid="ledger-bottleneck-strip"]');
    expect(strip).not.toBeNull();
    expect(strip!.textContent).toContain("병목 · 리뷰 (2개 체류 · 승인 대기 2)");
    expect(strip!.textContent).toContain("재작업 루프 (리뷰 ⇄ 수리) · 1건 (11%)");
    expect(strip!.textContent).toContain("막힘·실패 · 1건");
  });

  it("renders server-bottleneck-strip, focuses bottleneck state, inserts a state in-place on an edge, and undoes step-by-step", async () => {
    const dispositions = Object.fromEntries(
      srvPipelineBottleneckDocument.view.nodes.map((n) => [n.id, n.runtime.disposition]),
    );
    const approvals = {
      pr_merge: false,
      deploy: false,
    };

    await act(async () => {
      root.render(
        <CampaignPipeline
          source={{
            kind: "server",
            id: "srv-pipeline-bottleneck-demo",
            stamp: srvPipelineBottleneckDocument.view.as_of,
            sequence: srvPipelineBottleneckDocument.view.last_event_sequence,
          }}
          title={srvPipelineBottleneckDocument.definition.title}
          dispositions={dispositions}
          approvals={approvals}
          tr={koTr}
        />,
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    const serverStrip = container.querySelector('[data-testid="server-bottleneck-strip"]');
    expect(serverStrip).not.toBeNull();
    expect(serverStrip!.textContent).toContain("활성 체류 5개 · 보류 2개");
    expect(serverStrip!.textContent).toContain("재시도 루프 2개");

    const focusBtn = container.querySelector<HTMLButtonElement>('[data-testid="focus-bottleneck-state"]');
    expect(focusBtn).not.toBeNull();
    expect(focusBtn!.textContent).toContain("병목 · 사람 리뷰 게이트 (3개)");

    await act(async () => {
      focusBtn!.click();
    });

    const bottleneckNote = container.querySelector('[data-testid="inspector-bottleneck-note"]');
    expect(bottleneckNote).not.toBeNull();
    expect(bottleneckNote!.textContent).toContain("보류·막힘 작업 2개가 대기 중이에요");

    // Enter edit mode
    const editBtn = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
      (btn) => btn.textContent?.trim() === "편집",
    );
    expect(editBtn).toBeDefined();
    await act(async () => {
      editBtn!.click();
    });

    // Select `implement` to verify keyboard-accessible inspector button also mounts
    const implNode = container.querySelector<HTMLElement>('[data-rf-node="implement"]');
    expect(implNode).not.toBeNull();
    await act(async () => {
      implNode!.click();
    });
    expect(container.querySelector('[data-testid="inspector-insert-between-t_impl_check"]')).not.toBeNull();

    // Trigger canvas in-place `+` button on `implement -> auto_check`
    const edgeInsertBtn = container.querySelector<HTMLButtonElement>('[data-testid="insert-on-edge-t_impl_check"]');
    expect(edgeInsertBtn).not.toBeNull();
    await act(async () => {
      edgeInsertBtn!.click();
    });

    // Type new state name and pick `자동 검사` (`auto_check`) in PipelineStatePicker popover
    const pickerInput = document.querySelector<HTMLInputElement>('input[aria-label="상태 찾기"]');
    expect(pickerInput).not.toBeNull();
    await setInputValue(pickerInput!, "보안 정적 분석");

    const makeSection = document.querySelector<HTMLElement>('section[aria-label="새 상태 만들기"]');
    expect(makeSection).not.toBeNull();
    const autoCheckBtn = Array.from(makeSection!.querySelectorAll<HTMLButtonElement>("button")).find(
      (btn) => btn.textContent?.trim() === "자동 검사",
    );
    expect(autoCheckBtn).toBeDefined();
    await act(async () => {
      autoCheckBtn!.click();
    });

    // Verify new state was inserted (`8 -> 9` states on canvas) and selected in inspector with `자동 테스트 검증` outgoing target and `코드 구현` incoming
    const rf = container.querySelector('[data-testid="mock-reactflow"]');
    expect(rf?.getAttribute("data-node-count")).toBe("10"); // 9 states + 1 event
    const editorAside = container.querySelector('aside[aria-label="파이프라인 편집"]');
    expect(editorAside?.textContent).toContain("보안 정적 분석");
    expect(editorAside?.textContent).toContain("코드 구현 · t_impl_check");

    // Click `되돌리기` (Undo) and verify restoration to 8 states (`9` nodes including 1 event) and `implement -> auto_check`
    const undoBtn = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
      (btn) => btn.textContent?.trim() === "되돌리기",
    );
    expect(undoBtn).toBeDefined();
    expect(undoBtn!.disabled).toBe(false);
    await act(async () => {
      undoBtn!.click();
    });

    expect(rf?.getAttribute("data-node-count")).toBe("9"); // 8 states + 1 event
    expect(undoBtn!.disabled).toBe(true);
  });
});
