// @vitest-environment happy-dom
import { act, useEffect, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Campaign } from "../src/api/campaigns";
import { request } from "../src/api/httpClient";
import { bulkKanbanAction } from "../src/api/kanban";
import { getDefaultPipelineTemplate, saveServerCampaignPipeline, validateServerCampaignPipeline, type CampaignPipeline as PipelineDetail } from "../src/api/serverCampaignPipeline";
import { serverCampaignFixtures } from "../src/api/serverCampaignsFixtures";
import { CampaignBatchTracker, CampaignGateApprovalCard, buildBatchTracker, isLedgerLandReadyCandidate } from "../src/components/campaigns/CampaignApprovalBanner";
import CampaignBoard from "../src/components/campaigns/CampaignBoard";
import { legacyBoard } from "../src/components/campaigns/campaignBoardModel";
import CampaignOverview, { type TaskView } from "../src/components/campaigns/CampaignOverview";
import { legacyTask, serverTasks } from "../src/components/campaigns/campaignOverviewModel";
import CampaignPipeline from "../src/components/campaigns/CampaignPipeline";
import CampaignQueueTab from "../src/components/campaigns/CampaignQueueTab";
import CampaignRelations from "../src/components/campaigns/CampaignRelations";
import { makeCampaignNode } from "../src/components/campaigns/campaignTestFixtures";
import RelationsInspector from "../src/components/campaigns/RelationsInspector";
import {
  EDITOR_HISTORY_KEY,
  isOrphanedEditorEntry,
  resetOrphanedEditorEntries,
  useEditorHistory,
} from "../src/components/campaigns/useEditorHistory";
import { usePipelineEditor } from "../src/components/campaigns/usePipelineEditor";
import { KanbanProvider } from "../src/contexts/KanbanContext";
import type { Agent, KanbanCard } from "../src/types";

const mockAgent = { id: "agent-1", name: "Builder", name_ko: "빌더" } as Agent;

vi.mock("../src/contexts/WorkspaceContext", () => ({
  useWorkspace: () => ({ agents: [mockAgent] }),
  useWorkspaceAgents: () => [mockAgent],
}));

vi.mock("../src/api/httpClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../src/api/httpClient")>()),
  request: vi.fn(),
}));

vi.mock("../src/api/kanban", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../src/api/kanban")>()),
  bulkKanbanAction: vi.fn(),
  getKanbanCards: vi.fn().mockResolvedValue([]),
}));

vi.mock("../src/api/serverCampaignPipeline", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../src/api/serverCampaignPipeline")>()),
  getDefaultPipelineTemplate: vi.fn(),
  validateServerCampaignPipeline: vi.fn().mockResolvedValue({ errors: [], warnings: [] }),
  saveServerCampaignPipeline: vi.fn(),
}));

vi.mock("@xyflow/react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@xyflow/react")>()),
  ReactFlow: ({
    nodes,
    onInit,
    onNodesChange,
    onNodeDoubleClick,
    children,
  }: {
    nodes: Array<{ id: string }>;
    onInit?: (instance: unknown) => void;
    onNodesChange?: (changes: unknown[]) => void;
    onNodeDoubleClick?: (event: unknown, node: { id: string }) => void;
    children?: ReactNode;
  }) => {
    useEffect(() => {
      onInit?.({ fitView: vi.fn(), setViewport: vi.fn(), zoomIn: vi.fn(), zoomOut: vi.fn() });
    }, []); // eslint-disable-line react-hooks/exhaustive-deps
    return (
      <div data-testid="mock-reactflow" data-node-count={nodes.length}>
        {nodes.map((node) => (
          <button
            key={node.id}
            type="button"
            data-rf-node={node.id}
            onClick={() => onNodesChange?.([{ type: "select", id: node.id, selected: true }])}
            onDoubleClick={() => onNodeDoubleClick?.({}, node)}
          />
        ))}
        {children}
      </div>
    );
  },
  Background: () => null,
  MiniMap: () => null,
  Handle: () => null,
}));

const koTr = (ko: string, _en: string) => ko;

function makeKanbanCard(id: string, patch: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id,
    title: `Card ${id}`,
    description: null,
    status: "backlog",
    github_repo: "itismyfield/CookingHeart",
    owner_agent_id: null,
    requester_agent_id: null,
    assignee_agent_id: "agent-1",
    parent_card_id: null,
    latest_dispatch_id: null,
    sort_order: 0,
    priority: "medium",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 186,
    github_issue_url: "https://github.com/itismyfield/CookingHeart/issues/186",
    metadata_json: null,
    pipeline_stage_id: null,
    review_status: null,
    created_at: "2026-11-24T00:00:00Z",
    updated_at: "2026-11-24T00:00:00Z",
    started_at: null,
    requested_at: null,
    completed_at: null,
    ...patch,
  };
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  resetOrphanedEditorEntries();
  window.history.replaceState(null, "", "/campaigns");
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.clearAllMocks();
});

async function setInputValue(input: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

describe("ApprovalCard landing banner & forum approval wiring", () => {
  it("reads structured runtime.pending_gate on server campaigns and renders kit ApprovalCard with disabled reason or forum submission", async () => {
    const doc = structuredClone(serverCampaignFixtures.reviewWaiting);
    Object.assign(doc.view.nodes[2].runtime, {
      human_status: "awaiting_approval",
      wait_started_at: "2026-11-24T03:30:00Z",
      evidence_url: "https://discord.com/channels/1/2/3",
      pending_gate: {
        gate: "landing",
        binding: {
          run_id: "run-1",
          node_id: "C",
          gate: "landing",
          definition_revision: 7,
          target: { kind: "candidate", base_sha: "aaaaaaa1111111", head_sha: "bbbbbbb2222222" },
          policy_hash: "c".repeat(64),
        },
      },
    });

    const tasks: TaskView[] = serverTasks(doc, [], koTr);
    expect(tasks[2].gate?.gate).toBe("landing");
    expect(tasks[2].gate?.binding?.target).toEqual({
      kind: "candidate",
      base_sha: "aaaaaaa1111111",
      head_sha: "bbbbbbb2222222",
    });

    const onApproved = vi.fn();
    await act(async () => {
      root.render(
        <CampaignOverview
          tasks={tasks}
          limit={2}
          showPr={false}
          asOf={doc.view.as_of}
          recent={{ title: "최근 이벤트", entries: [] }}
          selectedId={null}
          onOpen={vi.fn()}
          campaignId="release"
          connection="live"
          onApproved={onApproved}
          language="ko"
          tr={koTr}
        />,
      );
    });

    const banner = container.querySelector('[aria-label="착지 승인 대기"]');
    expect(banner).not.toBeNull();
    expect(banner?.querySelector(".aui-approval-card")).not.toBeNull();
    expect(banner?.textContent).toContain("착지 승인 대기");
    expect(banner?.textContent).toContain("aaaaaaa → bbbbbbb");

    const approveBtn = [...banner!.querySelectorAll("button")].find((b) => b.textContent?.trim() === "승인")!;
    expect(approveBtn.getAttribute("aria-disabled")).toBe("true");
    expect(banner?.textContent).toContain("포럼 승인 근거를 입력하면 승인돼요");

    // Open forum evidence inputs (responseAt starts empty so operator supplies actual forum timestamp) and submit a valid approval
    const toggleForum = [...banner!.querySelectorAll("button")].find((b) => b.textContent?.includes("포럼 승인 근거 입력"))!;
    await act(async () => toggleForum.click());
    const inputs = [...banner!.querySelectorAll("input")];
    expect(inputs[2].value).toBe("");
    await setInputValue(inputs[0], "i7440-landing-approve");
    await setInputValue(inputs[1], "https://discord.com/channels/100/200/300");
    await setInputValue(inputs[2], "2026-11-24T04:00:00Z");

    expect(approveBtn.getAttribute("aria-disabled")).toBeNull();
    vi.mocked(request).mockResolvedValueOnce({ result: { definition_revision: 7 } });
    await act(async () => approveBtn.click());

    expect(request).toHaveBeenCalledWith(
      "/api/server-campaigns/release/commands",
      expect.objectContaining({ method: "POST" }),
    );
    const body = JSON.parse(String(vi.mocked(request).mock.calls[0][1]?.body));
    expect(body.command).toMatchObject({
      command: "approve",
      expected_definition_revision: 7,
      expected_run: "run-1",
      node_id: "C",
      gate: "landing",
      evidence: {
        decision_ref: "i7440-landing-approve",
        response_ref: "https://discord.com/channels/100/200/300",
        response_at: "2026-11-24T04:00:00.000Z",
      },
    });
    expect(onApproved).toHaveBeenCalledOnce();
  });

  it("identifies open LAND_READY ledger tasks without false-positives on completed, blocked, failed, or summary-only text", async () => {
    const openReady = makeCampaignNode("n1", { status: "running", stage: "LAND_READY · 착지 승인 대기", evidence: ["vitest clean"] });
    const bareReady = makeCampaignNode("n-bare", { status: "running", stage: "LAND_READY · 배치 착지 대기" });
    const historicalReady = makeCampaignNode("n-hist", { status: "running", stage: "LAND_READY d5040b28cc → 착지 묶음 1121f → 배포 뒤 확인" });
    const closedOld = makeCampaignNode("n2", { status: "completed", stage: "사용자 승인으로 착지 승인 대기 종료" });
    const blockedNode = makeCampaignNode("n3", { status: "blocked", stage: "LAND_READY", blocker: "CI 실패 확인 필요" });
    const failedNode = makeCampaignNode("n4", { status: "failed", stage: "LAND_READY" });
    const summaryOnly = makeCampaignNode("n5", { status: "running", stage: "구현 중", summary: "완료 후 LAND_READY 예정" });
    expect(legacyTask(openReady).gate?.gate).toBe("landing");
    expect(legacyTask(bareReady).gate).toBeNull();
    expect(legacyTask(historicalReady).gate).toBeNull();
    expect(legacyTask(closedOld).gate).toBeNull();
    expect(legacyTask(blockedNode).gate).toBeNull();
    expect(legacyTask(blockedNode).attention).toBe(true);
    expect(legacyTask(failedNode).gate).toBeNull();
    expect(legacyTask(failedNode).attention).toBe(true);
    expect(legacyTask(summaryOnly).gate).toBeNull();

    expect(isLedgerLandReadyCandidate(historicalReady)).toBe(false);
    expect(isLedgerLandReadyCandidate({ stage: "배포 확인 중", summary: "착지 승인 배너 수리" })).toBe(false);
    expect(isLedgerLandReadyCandidate(bareReady)).toBe(true);
    expect(isLedgerLandReadyCandidate({ stage: "구현 완료 → LAND_READY d5040b28cc" })).toBe(true);

    await act(async () => {
      root.render(
        <CampaignGateApprovalCard
          taskId="n1"
          taskTitle="#8369 캠페인 착지"
          gate={legacyTask(openReady).gate!}
          stageText="착지"
          tr={koTr}
        />,
      );
    });
    expect(container.querySelector(".aui-approval-card")).not.toBeNull();
    expect(container.textContent).toContain("vitest clean");
  });
});

describe("Batch landing header tracker & 486+ scale UX", () => {
  it("parses piece counts, LAND_READY candidates and batch codes into CampaignBatchTracker", async () => {
    const ledger: Campaign = {
      id: "dag-486",
      title: "열린 이슈 DAG",
      description: "병렬 단위 DAG",
      status: "active",
      round: 1,
      revision: 12,
      auto_queue: false,
      created_at: "2026-10-25T00:00:00Z",
      updated_at: "2026-11-24T00:00:00Z",
      nodes: [
        makeCampaignNode("i7353", {
          status: "running",
          stage: "1124b 후보 검증 중",
          summary: "조각: 완료 29/57 — 리뷰·검증 통과",
          pr_url: "https://github.com/itismyfield/AgentDesk/pull/8370",
        }),
        makeCampaignNode("i7323", {
          status: "pending",
          summary: "조각: 완료 1/2",
        }),
        makeCampaignNode("i7400", { status: "completed", post_merge_hold: "24h 관측" }),
      ],
    };
    const tasks: TaskView[] = ledger.nodes.map((n) => ({ ...legacyTask(n), hold: n.post_merge_hold }));
    const parsed = buildBatchTracker(ledger, tasks);
    expect(parsed.piecesDone).toBe(30);
    expect(parsed.piecesTotal).toBe(59);
    expect(parsed.landReadyIds).toEqual(["i7353"]);
    expect(parsed.openPrCount).toBe(1);
    expect(parsed.holdCount).toBe(1);
    expect(parsed.batches.map((b) => b.code)).toContain("1124b");

    const onOpenTask = vi.fn();
    await act(async () => {
      root.render(<CampaignBatchTracker campaign={ledger} tasks={tasks} onOpenTask={onOpenTask} tr={koTr} />);
    });
    expect(container.textContent).toContain("조각 30/59");
    expect(container.textContent).toContain("착지 후보 1");
    const batchChip = [...container.querySelectorAll("button")].find((b) => b.textContent === "1124b")!;
    await act(async () => batchChip.click());
    expect(onOpenTask).toHaveBeenCalledWith("i7353");
  });

  it("folds finished columns on CampaignBoard by default and filters 486+ cards by search and group", async () => {
    const nodes = [
      ...Array.from({ length: 399 }, (_, i) => makeCampaignNode(`done-${i}`, { title: `완료 작업 ${i}`, status: "completed", group: "출력·tui_o" })),
      ...Array.from({ length: 35 }, (_, i) => makeCampaignNode(`skip-${i}`, { title: `건너뜀 ${i}`, status: "skipped", group: "dashboard" })),
      makeCampaignNode("run-1", { title: "#8370 크레이트 묶음", status: "running", group: "statechart", issue_url: "https://github.com/itismyfield/AgentDesk/issues/8370" }),
      makeCampaignNode("pend-1", { title: "#8371 후속 정리", status: "pending", group: "user-blocking" }),
    ];
    const tasks: TaskView[] = nodes.map((n) => legacyTask(n));
    const columns = legacyBoard(nodes);

    await act(async () => {
      root.render(<CampaignBoard columns={columns} tasks={tasks} selectedId={null} onOpen={vi.fn()} tr={koTr} />);
    });

    const doneCol = container.querySelector('section[aria-label="완료"]')!;
    const skipCol = container.querySelector('section[aria-label="건너뜀"]')!;
    expect(doneCol.getAttribute("data-folded-column")).toBe("true");
    expect(skipCol.getAttribute("data-folded-column")).toBe("true");
    expect(doneCol.querySelectorAll("[data-board-card]")).toHaveLength(0);

    // Unfold completed column on click, then fold back
    const unfoldBtn = [...doneCol.querySelectorAll("button")].find((b) => b.textContent?.includes("20개 보기"))!;
    await act(async () => unfoldBtn.click());
    expect(doneCol.getAttribute("data-folded-column")).toBeNull();
    expect(doneCol.querySelectorAll("[data-board-card]")).toHaveLength(20);
    const foldBtn = [...doneCol.querySelectorAll("button")].find((b) => b.textContent === "접기")!;
    await act(async () => foldBtn.click());
    expect(doneCol.getAttribute("data-folded-column")).toBe("true");

    // Instant search narrows board cards immediately
    const searchInput = container.querySelector<HTMLInputElement>('input[aria-label="보드 작업 검색"]')!;
    await setInputValue(searchInput, "#8370");
    expect(container.textContent).toContain("검색 결과 1건");
    expect(container.querySelectorAll("[data-board-card]")).toHaveLength(1);
    expect(container.querySelector('[data-board-card="run-1"]')).not.toBeNull();
  });

  it("hides empty right pane on CampaignQueueTab when unselected and runs bulk status transition", async () => {
    vi.mocked(bulkKanbanAction).mockResolvedValueOnce({
      action: "transition",
      results: [{ id: "c1", ok: true }, { id: "c2", ok: true }],
    });
    const cards = [
      makeKanbanCard("c1", { github_issue_number: 186, title: "NPC 시스템" }),
      makeKanbanCard("c2", { github_issue_number: 187, title: "서빙 시스템" }),
    ];

    await act(async () => {
      root.render(
        <KanbanProvider initialCards={cards} initialDispatches={[]}>
          <CampaignQueueTab
            language="ko"
            tr={koTr}
            campaigns={[]}
            campaignsLoaded
            campaignsError={null}
            onRetryCampaigns={vi.fn()}
            heldIssueUrls={[]}
            serverHoldsIncomplete={false}
            selectedCardId={null}
            onSelectCard={vi.fn()}
            onCampaignSaved={vi.fn()}
          />
        </KanbanProvider>,
      );
    });

    expect(document.querySelector('aside[aria-label="카드 상세"]')).toBeNull();
    expect(document.body.textContent).not.toContain("고른 카드가 없습니다");

    const checkboxes = [...container.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')];
    // First checkbox is "Select all shown"
    await act(async () => checkboxes[0].click());
    const bulkBar = container.querySelector('[role="region"][aria-label="일괄 상태 전이"]');
    expect(bulkBar).not.toBeNull();
    expect(bulkBar?.textContent).toContain("2장 선택됨");

    const runBtn = [...bulkBar!.querySelectorAll("button")].find((b) => b.textContent?.includes("일괄 상태 전이"))!;
    await act(async () => runBtn.click());
    expect(bulkKanbanAction).toHaveBeenCalledWith("transition", ["c1", "c2"], "ready");
  });

  it("folds unused template canvas on ledger CampaignPipeline until toggled, and sizes relation dependency chips for 44px touch", async () => {
    vi.mocked(getDefaultPipelineTemplate).mockResolvedValue({
      template: { id: "campaign-development-9", version: 1 },
      pipeline: {
        schema_version: 1,
        vocabulary_version: 1,
        evaluator_version: 1,
        registry_version: 1,
        start: "requirements",
        states: [
          { id: "requirements", kind: "auto_check", title: "요구사항" },
          { id: "complete", kind: "end_success", title: "완료" },
        ],
        transitions: [
          { id: "t1", from: "requirements", to: "complete", priority: 10, condition: { field: "state.outcome", op: "eq", value: "succeeded" } },
        ],
        events: [],
        effect_gates: [],
      },
    });

    await act(async () => {
      root.render(<CampaignPipeline source={{ kind: "legacy" }} tr={koTr} />);
    });
    await act(async () => { await Promise.resolve(); });

    expect(container.textContent).toContain("원장 캠페인은 서버 파이프라인을 쓰지 않아요");
    expect(container.querySelector('[data-testid="mock-reactflow"]')).toBeNull();

    const previewBtn = [...container.querySelectorAll("button")].find((b) => b.textContent?.includes("기본 9단계 템플릿 미리보기"))!;
    await act(async () => previewBtn.click());
    expect(container.querySelector('[data-testid="mock-reactflow"]')).not.toBeNull();

    // Verify RelationsInspector dependency chip touch-safe sizing
    const nodeA = makeCampaignNode("a", { title: "출력·tui_o" });
    const nodeB = makeCampaignNode("b", { title: "user-blocking", dependencies: ["a"] });
    await act(async () => {
      root.render(
        <RelationsInspector
          node={nodeB}
          edge={null}
          lock={null}
          busy={false}
          nodes={new Map([["a", nodeA], ["b", nodeB]])}
          cycles={[]}
          errors={new Set()}
          problems={[]}
          overview={{ name: "그룹", meta: "2", changes: [], ties: 0 }}
          cycleText={() => ""}
          act={{ field: vi.fn(), add: vi.fn(), remove: vi.fn(), unlink: vi.fn(), focus: vi.fn() }}
          tr={koTr}
        />,
      );
    });
    const removeDepBtn = container.querySelector<HTMLButtonElement>('button[aria-label*="빼기"]')!;
    expect(removeDepBtn.className).toContain("max-[899px]:!size-11");
  });

  it("renders interactive PipelineCanvas and CampaignGroupOverview/CampaignGroupTasks on phone (<900px) with SegmentedControl list toggles", async () => {
    const mqSpy = vi.spyOn(window, "matchMedia").mockImplementation(
      (query: string) =>
        ({
          matches: query.includes("max-width: 899px") || query.includes("max-width"),
          media: query,
          onchange: null,
          addListener: vi.fn(),
          removeListener: vi.fn(),
          addEventListener: vi.fn(),
          removeEventListener: vi.fn(),
          dispatchEvent: vi.fn(),
        }) as unknown as MediaQueryList,
    );

    try {
      vi.mocked(getDefaultPipelineTemplate).mockResolvedValue({
        template: { id: "campaign-development-9", version: 1 },
        pipeline: {
          schema_version: 1,
          vocabulary_version: 1,
          evaluator_version: 1,
          registry_version: 1,
          start: "requirements",
          states: [
            { id: "requirements", kind: "auto_check", title: "요구사항" },
            { id: "complete", kind: "end_success", title: "완료" },
          ],
          transitions: [
            { id: "t1", from: "requirements", to: "complete", priority: 10, condition: { field: "state.outcome", op: "eq", value: "succeeded" } },
          ],
          events: [],
          effect_gates: [],
        },
      });

      const mobileCampaign: Campaign = {
        id: "mobile-dag",
        title: "모바일 캠페인",
        description: "",
        status: "active",
        round: 1,
        revision: 1,
        auto_queue: false,
        created_at: "2026-11-25T00:00:00Z",
        updated_at: "2026-11-25T00:00:00Z",
        nodes: [
          makeCampaignNode("g1", { title: "게이트웨이 설계", group: "Gateway", status: "completed", stage: "done" }),
          makeCampaignNode("g2", { title: "게이트웨이 구현", group: "Gateway", status: "running", stage: "구현 중" }),
          makeCampaignNode("r1", { title: "릴레이 연동", group: "Relay", status: "pending", dependencies: ["g1"] }),
        ],
      };

      // (a) CampaignPipeline on phone: Korean labels on AgentStateMachine + interactive PipelineCanvas + 단계 목록 toggle
      await act(async () => {
        root.render(<CampaignPipeline source={{ kind: "legacy" }} campaign={mobileCampaign} tr={koTr} />);
      });
      await act(async () => { await Promise.resolve(); });

      const machine = container.querySelector(".aui-agent-state-machine")!;
      expect(machine).not.toBeNull();
      expect(machine.textContent).toContain("전환");
      expect(machine.textContent).toContain("도달 전");

      const previewBtn = [...container.querySelectorAll("button")].find((b) => b.textContent?.includes("기본 9단계 템플릿 미리보기"))!;
      await act(async () => previewBtn.click());
      expect(container.textContent).not.toContain("폰에서는 보기만 해요");
      expect(container.querySelector('[data-testid="mock-reactflow"]')).not.toBeNull();

      const stateListTab = [...container.querySelectorAll('[aria-label="파이프라인 보기 방식"] button')].find((b) => b.textContent?.includes("단계 목록"))!;
      await act(async () => (stateListTab as HTMLButtonElement).click());
      expect(container.querySelector('[data-testid="mock-reactflow"]')).toBeNull();
      expect(container.textContent).toContain("요구사항");

      // (b) CampaignRelations on phone: interactive CampaignGroupOverview by default -> drill into CampaignGroupTasks -> switch to 목록 GroupList
      await act(async () => {
        root.render(<CampaignRelations campaign={mobileCampaign} selectedId={null} onOpen={vi.fn()} tr={koTr} />);
      });
      expect(container.querySelector('[data-testid="mock-reactflow"]')?.getAttribute("data-node-count")).toBe("2");
      expect(container.querySelector('[aria-label="그룹 목록"]')).toBeNull();

      const gatewayNode = container.querySelector<HTMLButtonElement>('[data-rf-node="campaign-group:Gateway"]')!;
      await act(async () => gatewayNode.click());
      const openGroupBtn = [...container.querySelectorAll("button")].find((b) => b.textContent?.trim() === "그룹 열기")!;
      await act(async () => openGroupBtn.click());

      expect(container.querySelector('[aria-label="이동 줄"]')?.textContent).toContain("전체 그룹");
      expect(container.querySelector('[aria-label="이동 줄"]')?.textContent).toContain("Gateway");
      expect(container.querySelector('[data-testid="mock-reactflow"]')?.getAttribute("data-node-count")).toBe("3");

      const allGroupsBtn = [...container.querySelectorAll('[aria-label="이동 줄"] button')].find((b) => b.textContent?.trim() === "전체 그룹")!;
      await act(async () => (allGroupsBtn as HTMLButtonElement).click());

      const listLayoutBtn = [...container.querySelectorAll('[aria-label="모바일 그래프 표시"] button')].find((b) => b.textContent?.trim() === "목록")!;
      await act(async () => (listLayoutBtn as HTMLButtonElement).click());
      expect(container.querySelector('[data-testid="mock-reactflow"]')).toBeNull();
      const groupList = container.querySelector('[aria-label="그룹 목록"]')!;
      expect(groupList).not.toBeNull();
      expect(groupList.textContent).toContain("선행 그룹 0 · 다음 그룹 1");
      expect(groupList.textContent).toContain("선행 그룹 1 · 다음 그룹 0");
    } finally {
      mqSpy.mockRestore();
    }
  });
});

describe("Shared editor history & saving-leave guard (#8369 P2-a, P2-c, P2-4)", () => {
  it("absorbs orphaned editor history entries when returning via browser Back so Back never spins (P2-4)", async () => {
    const backSpy = vi.spyOn(window.history, "back").mockImplementation(() => undefined);

    function Harness({ open }: { open: boolean }) {
      useEditorHistory({ active: open, enabled: true, onBack: vi.fn() });
      return <div>{open ? "editing" : "closed"}</div>;
    }

    await act(async () => root.render(<Harness open={false} />));
    await act(async () => root.render(<Harness open={true} />));
    expect(window.history.state?.[EDITOR_HISTORY_KEY]).toMatch(/^ed-/);

    // Unmount while open (simulating Alt+1 / rail navigation to another route)
    await act(async () => root.unmount());
    expect(isOrphanedEditorEntry()).toBe(true);

    // Remount in closed state (simulating browser Back returning to /campaigns)
    root = createRoot(container);
    await act(async () => root.render(<Harness open={false} />));
    expect(backSpy).toHaveBeenCalledOnce();
    expect(window.history.state?.[EDITOR_HISTORY_KEY]).toBeUndefined();
    expect(isOrphanedEditorEntry()).toBe(false);
    backSpy.mockRestore();
  });

  it("blocks discard-and-leave while a pipeline save is in flight and avoids ghost 503 notices after unmount (P2-a/c)", async () => {
    let rejectSave!: (reason: unknown) => void;
    vi.mocked(saveServerCampaignPipeline).mockImplementationOnce(
      () => new Promise((_, reject) => { rejectSave = reject; }),
    );
    const detail: PipelineDetail = {
      definition_revision: 3,
      execution_hash: "a".repeat(64),
      editable: true,
      source: "saved",
      execution: { mode: "legacy_stage", run_id: null, definition_revision: 3, state_ids: {} },
      pipeline: {
        schema_version: 1,
        vocabulary_version: 1,
        evaluator_version: 1,
        registry_version: 1,
        start: "s1",
        states: [{ id: "s1", kind: "agent_task", title: "구현" }, { id: "s2", kind: "end_success", title: "완료" }],
        transitions: [{ id: "t1", from: "s1", to: "s2", priority: 10, condition: { field: "state.outcome", op: "eq", value: "succeeded" } }],
      },
    };

    let api!: ReturnType<typeof usePipelineEditor>;
    function HookHost() {
      api = usePipelineEditor({ id: "rel-1", runtime: "[]", detail, reload: vi.fn(), entry: true, tr: koTr });
      return null;
    }

    await act(async () => root.render(<HookHost />));
    await act(async () => api.begin(detail));
    await act(async () => api.update((draft) => ({ ...draft, states: [{ ...draft.states[0], title: "구현 수정" }, draft.states[1]] })));
    expect(api.dirty).toBe(true);

    let savePromise!: Promise<void>;
    await act(async () => { savePromise = api.save(); });
    expect(api.saving).toBe(true);

    // Attempting to discard while saving must be ignored by the hook guard (P2-c)
    await act(async () => api.decide(true));
    expect(api.edit).not.toBeNull();

    // Unmount while save is in flight, then reject with network/503 error
    await act(async () => root.unmount());
    await act(async () => {
      rejectSave(new Error("503 Service Unavailable"));
      await savePromise;
    });
    // Re-create root for afterEach cleanup
    root = createRoot(container);
  });
});
