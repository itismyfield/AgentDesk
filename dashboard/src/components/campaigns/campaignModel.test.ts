import { describe, expect, it } from "vitest";
import type { CampaignNode } from "../../api/campaigns";
import { CAMPAIGN_STAGES, campaignIssueLabel, campaignProgress, campaignStageStep, dependencyNeighborhood, groupCampaignNodes, safeCampaignLink } from "./campaignModel";
import { legacyEntry, legacyTask, openGates, statusSentence } from "./campaignOverviewModel";
import { makeCampaignNode, makeLargeCampaign } from "./campaignTestFixtures";

function node(id: string, dependencies: string[] = [], status: CampaignNode["status"] = "pending"): CampaignNode {
  return makeCampaignNode(id, { dependencies, status });
}

describe("campaign progress", () => {
  it("never shows 100 percent while a task remains unfinished", () => {
    const nodes = Array.from({ length: 300 }, (_, index) => node(String(index), [], index ? "completed" : "running"));
    expect(campaignProgress(nodes).percent).toBe(99);
  });
  it("keeps skipped, failed and blocked tasks distinct from actual completion", () => {
    const result = campaignProgress([node("a", [], "completed"), node("b", [], "skipped"), node("c", [], "failed"), node("d", [], "blocked")]);
    expect(result.percent).toBe(25);
    expect(result.counts).toEqual({ completed: 1, skipped: 1, failed: 1, blocked: 1, running: 0, pending: 0 });
    expect(campaignProgress([]).percent).toBe(0);
  });
});

describe("large campaign exploration", () => {
  it("groups only explicit categories and keeps unclassified work separate from status and stage", () => {
    const groups = groupCampaignNodes(makeLargeCampaign().nodes);
    expect(groups).toHaveLength(4);
    expect(groups.at(-1)?.key).toBe("");
    expect(groups.every((group) => group.nodes.length === 30)).toBe(true);
    expect(groups.every((group) => group.progress.counts.running === 5)).toBe(true);
  });
  it("bounds a 120-node fan-in/out and reports omitted and external connections", () => {
    const upstream = Array.from({ length: 100 }, (_, index) => node(`up-${index}`));
    const selected = node("selected", upstream.map((value) => value.id));
    const downstream = Array.from({ length: 19 }, (_, index) => node(`down-${index}`, ["selected"]));
    const context = dependencyNeighborhood([...upstream, selected, ...downstream], "selected");
    expect(context.visible).toHaveLength(17);
    expect(context.omittedUpstream).toBe(92);
    expect(context.omittedDownstream).toBe(11);
    expect(context.externalDependencies.get("selected")).toHaveLength(92);
    expect(context.visible.map((value) => value.id)).toEqual(dependencyNeighborhood([...upstream, selected, ...downstream], "selected").visible.map((value) => value.id));
  });
  it("keeps drawn connections as directed raw edges and tells edges hidden by the limit from deleted tasks", () => {
    const context = dependencyNeighborhood([node("u1"), node("u2"), node("s", ["u1", "u2", "deleted"]), node("d1", ["s"])], "s", 1);
    expect(context.edges).toEqual([["u1", "s"], ["s", "d1"]]);
    expect(context.hiddenEdges).toEqual([["u2", "s"]]);
    expect(context.omittedUpstream).toBe(1);
    // A dependency on a task that no longer exists is not a hidden connection.
    expect(context.externalDependencies.get("s")).toEqual(["u2"]);
  });
  it("never turns an unsafe issue URL into a displayed issue number", () => {
    expect(campaignIssueLabel(makeCampaignNode("safe-id", { issue_url: "javascript:/issues/5701" }))).toBe("safe-id");
  });
});

it("allows only safe issue and PR links", () => {
  expect(safeCampaignLink("javascript:alert(1)")).toBeUndefined();
  expect(safeCampaignLink("data:text/html,bad")).toBeUndefined();
  expect(safeCampaignLink("https://github.com/org/repo/pull/1")).toBe("https://github.com/org/repo/pull/1");
});

describe("first-screen glance", () => {
  const stageOf = (stage: string) => { const step = campaignStageStep(stage); return step.index < 0 ? step.raw : CAMPAIGN_STAGES[step.index].key; };
  it("maps real free-text ledger stages to the standard step the writer put first", () => {
    expect(stageOf("merge_ready (relay hold; order #6226 -> #6250)")).toBe("merge");
    expect(stageOf("phase1 read-only audit of 412 tests")).toBe("investigate");
    expect(stageOf("review_clean ci_rerun")).toBe("merge");
    expect(stageOf("implementing: PR1 inert types/codec; design = v6")).toBe("implement");
    expect(stageOf("설계 r5 초벌 DESIGN_BLOCKED → Claude 실행 리뷰")).toBe("design");
    expect(stageOf("PR① #6266 리뷰 P0/P1 0 → CI 대기 후 머지")).toBe("review");
    expect(stageOf("머지됨 — 운영 관측 대기(메인 배포 후)")).toBe("deploy");
    expect(stageOf("주차:ready_to_implement, cargo 대기열")).toBe("implement");
    expect(stageOf("review_fix round 2")).toBe("repair");
    // Substrings of unrelated words are not stage keywords.
    expect(stageOf("OPS_ONLY suffix decision")).toBe("OPS_ONLY suffix decision");
  });
  it("shows an unmapped stage as short plain text without commit hashes", () => {
    expect(campaignStageStep("부분 완료 · 잔여 확인")).toEqual({ index: -1, raw: "부분 완료 · 잔여 확인" });
    const raw = campaignStageStep("주차:운영 잔여 kv 정리와 5분 tick 확인, 98a154dd0f 이후 남은 항목 정리").raw;
    expect(raw).not.toMatch(/98a154dd/);
    expect(raw.length).toBeLessThanOrEqual(28);
    expect(raw.endsWith("…")).toBe(true);
  });
  it("counts running work with a real blocker as blocked; a pending blocker is a precondition and a placeholder is none", () => {
    const blocker = (value: CampaignNode, text: string) => ({ ...value, blocker: text });
    const tasks = [node("b", [], "blocked"), blocker(node("r1", [], "running"), " waits on deploy "), blocker(node("r2", [], "running"), " N/A "),
      blocker(node("p1"), "선행: #1"), blocker(node("p2"), "없음"), node("c", [], "completed")].map((value) => legacyTask(value));
    expect(tasks.filter((task) => task.attention).map((task) => task.id)).toEqual(["b", "r1"]);
    expect(tasks.map((task) => task.blocker)).toEqual([null, "waits on deploy", null, "선행: #1", null, null]);
    expect(statusSentence("Running", tasks, (_, en) => en)).toBe("Running · 2 running, 2 blocked");
  });
  it("counts the list card's attention with the live map, as the overview does", () => {
    const live = { card_id: "card", card_status: "in_progress", dispatch_type: "implementation", dispatch_status: "dispatched", session_status: "turn_active", session_seen_at: null, running: true, queue_status: null };
    const nodes = (["pending", "failed", "blocked", "completed"] as const).flatMap((status) => [0, 1].map((on) => ({ ...node(`${status}-${on}`, [], status), blocker: "waits on deploy" })));
    const working = Object.fromEntries(nodes.filter((value) => value.id.endsWith("-1")).map((value) => [value.id, live]));
    const tr = (_: string, en: string) => en;
    expect(legacyEntry({ ...makeLargeCampaign(), nodes }, working, tr).attention).toBe("1 failed · 4 blocked");
    expect(legacyEntry({ ...makeLargeCampaign(), nodes }, working, tr).split?.map(({ key, count, tone }) => `${key} ${count} ${tone}`)).toEqual(["completed 2 done", "running 3 now", "blocked 1 wait", "failed 1 fail", "pending 1 todo"]);
    expect(statusSentence("Running", nodes.map((value) => legacyTask(value, working[value.id])), tr)).toBe("Running · 3 running, 4 blocked, 1 failed");
  });
  it("keeps a gate open from its opening event until the same run, node and gate is approved", () => {
    const event = (sequence: number, event: string, run_id: string, node_id: string, gate: string) =>
      ({ sequence, run_id, command_id: `c${sequence}`, observed_at: `2026-11-18T07:0${sequence}:00Z`, kind: { event, binding: { run_id, node_id, gate } } });
    const events = [event(1, "approval_gate_opened", "run-1", "A", "review"), event(2, "approval_gate_opened", "run-1", "B", "merge"),
      event(3, "approval_gate_approved", "run-1", "A", "review"), event(4, "approval_gate_opened", "run-0", "C", "deploy"), event(5, "node_failed", "run-1", "D", "start")];
    expect([...openGates(events, "run-1")]).toEqual([["B", { gate: "merge", openedAt: "2026-11-18T07:02:00Z" }]]);
  });
  it("counts open nodes with work running now as running, and leaves closed and quiet ones alone", () => {
    const live = (running: boolean) => ({ card_id: "card", card_status: "in_progress", dispatch_type: "implementation", dispatch_status: "dispatched", session_status: running ? "turn_active" : "idle", session_seen_at: null, running, queue_status: null });
    const status = (value: CampaignNode, running: boolean) => legacyTask(value, live(running)).status;
    expect([status(node("p-live"), true), status(node("p-quiet"), false), status(node("c-live", [], "completed"), true)]).toEqual(["running", "pending", "completed"]);
  });
});
