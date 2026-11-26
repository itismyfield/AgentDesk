import { expect, it } from "vitest";
import { campaignNodeSchema, type Campaign } from "../../api/campaigns";
import { serverCampaignFixtures } from "../../api/serverCampaignsFixtures";
import type { ServerCampaignSummary } from "../../api/serverCampaigns";
import {
  currentLedgerStageSegment,
  effectiveRelationCounts,
  groupOverviewProgress,
  isLedgerHumanApprovalPending,
  legacyEntry,
  legacyTask,
  serverEntry,
  serverTasks,
} from "./campaignOverviewModel";
import { makeCampaignNode } from "./campaignTestFixtures";

const tr = (ko: string) => ko;

it("splits a server campaign's bar by stage, drawing a moving stage of a stopping, stopped or inspection-needed campaign as waiting", () => {
  const doc = serverCampaignFixtures.stoppingUnknown;
  const split = (display: string) => {
    const view = { ...doc.view, display, failed_count: 0 } as ServerCampaignSummary["view"];
    return serverEntry({ definition: doc.definition, view } as ServerCampaignSummary, { ...doc, view } as typeof doc, tr).split;
  };
  expect(split("running")?.map(({ key, count, tone }) => `${key} ${count} ${tone}`)).toEqual(["requirements 1 todo", "implementation 1 now", "complete 1 done"]);
  expect(["stopping", "stopped", "needs_inspection", "waiting"].map((display) => split(display)?.[1].tone)).toEqual(["wait", "wait", "wait", "wait"]);
  expect(serverEntry({ definition: doc.definition, view: doc.view } as ServerCampaignSummary, null, tr).split).toBeNull();
});

it("never throws in legacyTask or legacyEntry when partial summary nodes have undefined stage or evidence", () => {
  const partialNode = {
    ...makeCampaignNode("i99", { status: "running", blocker: "CI hold" }),
    stage: undefined as unknown as string,
    evidence: undefined as unknown as string[],
    dependencies: undefined as unknown as string[],
  };
  const campaign: Campaign = {
    id: "partial-summary",
    title: "Partial summary",
    description: "",
    status: "active",
    round: 1,
    revision: 1,
    auto_queue: false,
    nodes_partial: true,
    nodes: [partialNode],
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
  };
  const task = legacyTask(partialNode);
  expect(task.stage).toBe("");
  expect(task.step).toBeNull();
  const entry = legacyEntry(campaign, undefined, tr);
  expect(entry.attention).toBe("막힘 1");
  expect(effectiveRelationCounts([partialNode]).counts.blocked).toBe(1);
  expect(campaignNodeSchema.parse({ id: "s1", title: "Summary", status: "pending", round: 1, updated_at: "2026-10-01T00:00:00Z" }).stage).toBe("");
});

it("summarizes group progress sorted by attention and active work while omitting single-track ungrouped campaigns", () => {
  const ungroupedOnly = [legacyTask(makeCampaignNode("u1", { status: "running" }))];
  expect(groupOverviewProgress(ungroupedOnly, "미분류")).toEqual([]);

  const mixed = [
    legacyTask(makeCampaignNode("d1", { group: "DoneTrack", status: "completed" })),
    legacyTask(makeCampaignNode("r1", { group: "RunTrack", status: "running" })),
    legacyTask(makeCampaignNode("b1", { group: "BlockedTrack", status: "blocked", blocker: "Hold" })),
    legacyTask(makeCampaignNode("n1", { group: null, status: "pending" })),
  ];
  expect(groupOverviewProgress(mixed, "미분류").map((g) => [g.name, g.done, g.total, g.running, g.attention])).toEqual([
    ["BlockedTrack", 0, 1, 0, 1],
    ["RunTrack", 0, 1, 1, 0],
    ["미분류", 0, 1, 0, 0],
    ["DoneTrack", 1, 1, 0, 0],
  ]);

  const doc = structuredClone(serverCampaignFixtures.stoppingUnknown);
  doc.definition.nodes[0].group = "Core";
  const mapped = serverTasks(doc, [], tr);
  expect(mapped[0].group).toBe("Core");
});

it("evaluates human approval pending only on the active final ledger segment and ignores bare LAND_READY", () => {
  expect(currentLedgerStageSegment(null)).toBe("");
  expect(currentLedgerStageSegment("  ")).toBe("");
  expect(
    currentLedgerStageSegment("LAND_READY d5040b28cc → 착지 묶음 1121f(043a36220c) → 배포 뒤 옛 형식 정지 자동 정리·tick 제출 확인"),
  ).toBe("배포 뒤 옛 형식 정지 자동 정리·tick 제출 확인");
  expect(currentLedgerStageSegment("구현 완료 -> 착지 승인 대기")).toBe("착지 승인 대기");

  expect(isLedgerHumanApprovalPending("LAND_READY", "배치 착지 대기", null)).toBe(false);
  expect(
    isLedgerHumanApprovalPending(
      "LAND_READY d5040b28cc → 착지 승인 대기 → 배포 뒤 확인",
      "결정 포럼 승인 대기 -> 머지 완료",
      null,
    ),
  ).toBe(false);

  expect(isLedgerHumanApprovalPending("구현 완료 → 착지 승인 대기")).toBe(true);
  expect(isLedgerHumanApprovalPending("LAND_READY", "결정 포럼 승인 대기")).toBe(true);
  expect(isLedgerHumanApprovalPending("배포 준비", null, "배포 승인 대기")).toBe(true);
  expect(isLedgerHumanApprovalPending("verify -> awaiting landing approval")).toBe(true);
  expect(isLedgerHumanApprovalPending("verify -> awaiting human approval")).toBe(true);

  const bareReady = makeCampaignNode("n-bare", { status: "running", stage: "LAND_READY · 배치 착지 대기" });
  const historical = makeCampaignNode("n-hist", {
    status: "running",
    stage: "LAND_READY d5040b28cc → 착지 묶음 1121f(043a36220c) → 배포 뒤 옛 형식 정지 자동 정리·tick 제출 확인",
    next_action: "착지 승인 대기 → 배포 뒤 확인",
  });
  const activeGate = makeCampaignNode("n-gate", {
    status: "running",
    stage: "LAND_READY d5040b28cc → 착지 승인 대기",
  });
  expect(legacyTask(bareReady).gate).toBeNull();
  expect(legacyTask(historical).gate).toBeNull();
  expect(legacyTask(activeGate).gate?.gate).toBe("landing");
});
