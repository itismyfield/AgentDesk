import { describe, expect, it } from "vitest";
import type { Campaign } from "../../api/campaigns";
import type { RoutineRecord } from "../../api/routines";
import type { ServerCampaignDocument, ServerCampaignSummary } from "../../api/serverCampaigns";
import type { HealthResponse } from "../../api/settingsRuntime";
import type { Agent, DashboardStats, DispatchedSession, KanbanCard } from "../../types";
import {
  agentLine, autoPauseRule, buildCampaignRows, buildSystemRows, buildTodayRoutines, buildTodoItems, buildUsageRows, classifyAgents,
} from "./homeModel";

const tr = (ko: string) => ko;
const agent = (id: string, extra: Partial<Agent> = {}) => ({ id, name: id, name_ko: id, status: "idle", ...extra }) as Agent;
const session = (agentId: string, status: string, lastSeen = "2026-11-18T00:00:00Z") =>
  ({ linked_agent_id: agentId, status, last_seen_at: lastSeen }) as unknown as DispatchedSession;

describe("home agents", () => {
  it("reads each agent's state from its sessions and orders busy agents first", () => {
    const entries = classifyAgents(
      [agent("rest"), agent("gone"), agent("bg", { status: "working", session_info: "CI 대기\n둘째 줄" }), agent("run"), agent("child", { visual_status_code: "idle-bg-wait" } as Partial<Agent>)],
      [session("rest", "idle"), session("gone", "disconnected"), session("bg", "awaiting_bg"), session("run", "turn_active"), session("child", "idle")],
    );
    expect(entries.map(({ agent: { id }, state }) => [id, state])).toEqual([
      ["run", "working"], ["bg", "background"], ["child", "background"], ["rest", "resting"], ["gone", "noSession"],
    ]);
    expect(entries[1].info).toBe("CI 대기");
    expect(agentLine(entries[4], tr, true)).toMatch(/^세션 없음 · 마지막 세션 \d+(초|분|시간|일) 전$/);
  });

  it("does not claim a lost session when no session list arrived", () => {
    expect(classifyAgents([agent("a")], [])[0].state).toBe("resting");
  });
});

describe("home to-dos", () => {
  const card = (id: string, status: string, extra: Partial<KanbanCard> = {}) =>
    ({ id, title: `card ${id}`, status, github_repo: "o/AgentDesk", github_issue_number: 7, blocked_reason: null, review_status: null, updated_at: "2026-11-18T00:00:00Z", ...extra }) as KanbanCard;

  it("lists decisions and failures before reviews, adds server counts and stuck server campaigns", () => {
    const items = buildTodoItems({
      cards: [card("r", "review"), card("b", "in_progress", { blocked_reason: "dependency cycle" }), card("d", "done", { blocked_reason: "old" })],
      stats: { kanban: { waiting_acceptance: 2, stale_in_progress: 0 } } as DashboardStats,
      campaigns: [{ definition: { title: "릴레이", goal: "", repository: "adk" }, view: { campaign_id: "relay", display: "needs_inspection", as_of: "", stop_mode: null, failed_count: 0, blocked_count: 0 } }] as ServerCampaignSummary[],
      tr,
      isKo: true,
    });
    expect(items.map((item) => [item.chip, item.count])).toEqual([["판단 필요", 1], ["상태 확인 필요", 1], ["검토 대기", 1], ["수락 대기", 2]]);
    expect(items[0]).toMatchObject({ title: "#7 card b", detail: "dependency cycle", to: "/campaigns?tab=queue&card=b" });
  });
});

describe("home system", () => {
  it("marks detail-only values unknown on a public summary and reads the server campaign executor", () => {
    const view = { execution_readiness: "unavailable" } as ServerCampaignDocument["view"];
    const rows = buildSystemRows({
      health: { status: "degraded", publicOnly: "forbidden", queue_depth: 4 } as HealthResponse,
      healthFailed: false,
      legacy: [{ status: "active", auto_queue: true }] as Campaign[],
      documents: [{ view } as ServerCampaignDocument],
      tr,
    });
    expect(Object.fromEntries(rows.map((row) => [row.key, [row.value, row.tone]]))).toEqual({
      server: ["주의", "warn"], discord: ["확인 불가", "neutral"], memento: ["확인 불가", "neutral"],
      campaign: ["서버 실행 기능 꺼짐 · 자동 진행 1개", "warn"], queue: ["확인 불가", "neutral"],
    });
  });

  it("counts connected bots and the queue from detail health", () => {
    const health = { status: "healthy", uptime_secs: 7_200, queue_depth: 0, providers: [{ connected: true }, { connected: false }] } as HealthResponse;
    const rows = buildSystemRows({ health, healthFailed: false, legacy: null, documents: [], tr });
    expect(rows.map((row) => row.value)).toEqual(["정상 · 2시간째", "1/2개 연결", "확인 불가", "확인 불가", "0건"]);
  });
});

describe("home campaigns", () => {
  it("puts a server campaign at its slowest task's stage and shows ledger completion", () => {
    const summary = { definition: { title: "v3", goal: "goal", repository: "adk" }, view: { campaign_id: "v3", display: "running", as_of: "", stop_mode: null, failed_count: 0, blocked_count: 0 } } as ServerCampaignSummary;
    const node = (stage: string) => ({ runtime: { stage } });
    const document = { view: { campaign_id: "v3", nodes: [node("review"), node("landing")], reasons: [{ code: "review_resources", target: "a" }] } } as unknown as ServerCampaignDocument;
    const ledger = (id: string, status: string, nodes: string[]) =>
      ({ id, title: id, status, updated_at: "2026-11-18T00:00:00Z", nodes: nodes.map((value) => ({ status: value })) }) as unknown as Campaign;
    const { rows, total } = buildCampaignRows({
      legacy: [ledger("planned", "planned", []), ledger("done", "completed", ["completed"]), ledger("live", "active", ["completed", "running", "pending", "pending"])],
      summaries: [summary, { ...summary, view: { ...summary.view, campaign_id: "old", display: "succeeded" } }],
      documents: [document],
      tr,
      limit: 2,
    });
    expect(total).toBe(3);
    expect(rows[0]).toMatchObject({ segments: ["done", "done", "done", "done", "now", "todo", "todo", "todo"], line: "리뷰 · 리뷰 세션 자원 대기", status: "실행 중 · 작업 2개" });
    expect(rows[1]).toMatchObject({ key: "legacy:live", ratio: 0.25, line: "4개 중 1개 끝남 · 진행 중 1" });
  });
});

describe("home routines and usage", () => {
  it("keeps today's runs and later due times only", () => {
    const now = new Date(2026, 10, 19, 12, 0).getTime();
    const at = (hour: number, day = 19) => new Date(2026, 10, day, hour).toISOString();
    const routine = (id: string, extra: Partial<RoutineRecord>) => ({ id, name: id, status: "enabled", in_flight_run_id: null, next_due_at: null, last_run_at: null, ...extra }) as RoutineRecord;
    const run = (status: string, hour: number, action: string | null = null) => ({ status, action, started_at: at(hour) }) as RoutineRecord["last_run"];
    const rows = buildTodayRoutines([
      routine("brief", { last_run: run("succeeded", 6, "agent") }),
      routine("pr", { last_run: run("succeeded", 9, "complete") }),
      routine("late", { next_due_at: at(22) }),
      routine("tomorrow", { last_run: { status: "failed", started_at: at(9, 18) } as RoutineRecord["last_run"], next_due_at: at(9, 20) }),
    ], now, tr);
    expect(rows.map((row) => [row.key, row.chip, row.finished])).toEqual([["brief", "보냄", true], ["pr", "끝남", true], ["late", "예정", false]]);
  });

  it("turns tracker buckets into bars with their measurement time and reads the auto-run pause", () => {
    const rows = buildUsageRows([
      { provider: "claude", buckets: [{ name: "7d", limit: 100, used: 83, remaining: 17, reset: 0 }], stale: true, fetched_at: 1_795_033_577 },
      { provider: "github", buckets: [{ name: "core", limit: 5000, used: 3, remaining: 4997, reset: 0 }], stale: false },
    ], true);
    expect(rows).toEqual([expect.objectContaining({ label: "Claude 7일", pct: 83, level: "warning", stale: true, measuredAt: 1_795_033_577_000 })]);
    expect(autoPauseRule({ current: { dispatchRateLimitGateEnabled: true, dispatchRateLimitGateDangerPct: 90 }, defaults: {} })).toEqual({ enabled: true, pct: 90 });
    expect(autoPauseRule(null)).toBeNull();
  });
});
