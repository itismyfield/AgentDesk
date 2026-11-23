import { describe, expect, it } from "vitest";
import type { Campaign } from "../../api/campaigns";
import type { RoutineRecord } from "../../api/routines";
import type { ServerCampaignDocument, ServerCampaignSummary } from "../../api/serverCampaigns";
import { serverCampaignFixtures } from "../../api/serverCampaignsFixtures";
import { mergeAgentStatus } from "../../contexts/session-overlay";
import type { Agent, DispatchedSession, KanbanCard } from "../../types";
import {
  agentLine, autoPauseRule, buildCampaignRows, buildSystemRows, buildTodayRoutines, buildTodoItems, buildUsageRows, classifyAgents, monitorView, systemNeedsAttention, todoStatus, usageDetail,
  type HomeHealth,
} from "./homeModel";

const tr = (ko: string) => ko;
const agent = (id: string, extra: Partial<Agent> = {}) => ({ id, name: id, name_ko: id, status: "idle", ...extra }) as Agent;
const session = (agentId: string, status: string, lastSeen = "2026-11-18T00:00:00Z", info: string | null = null) =>
  ({ id: `${agentId}-${status}`, linked_agent_id: agentId, status, last_seen_at: lastSeen, session_info: info }) as unknown as DispatchedSession;

describe("home agents", () => {
  it("orders busy agents first and takes each state and line from one session", () => {
    // The session overlay copied the newer CI wait's summary onto ada, who is reviewing a PR.
    const entries = classifyAgents(
      [agent("rest"), agent("gone"), agent("ada", { status: "working", session_info: "CI 대기" }), agent("bg")],
      [
        session("rest", "idle"), session("gone", "disconnected"), session("bg", "awaiting_bg", undefined, "CI 대기"),
        session("ada", "turn_active", "2026-11-18T00:00:00Z", "PR 리뷰\n둘째 줄"), session("ada", "awaiting_bg", "2026-11-18T01:00:00Z", "CI 대기"),
      ],
      false,
    );
    expect(entries.map(({ agent: { id }, state, info }) => [id, state, info])).toEqual([
      ["ada", "working", "PR 리뷰"], ["bg", "background", "CI 대기"], ["rest", "resting", null], ["gone", "noSession", null],
    ]);
    expect(agentLine(entries[3], tr, true)).toMatch(/^세션 없음 · 마지막 세션 \d+(초|분|시간|일) 전$/);
  });

  it("drops a finished background wait once its session turns idle and the agent list's snapshot goes with it", () => {
    const listed = [agent("child", { visual_status_code: "idle-bg-wait" } as Partial<Agent>)];
    const waiting = session("child", "awaiting_bg");
    expect(classifyAgents(listed, [waiting], false)[0].state).toBe("background");
    // Background children show only in the snapshot, so it holds until the next session change.
    expect(classifyAgents(listed, [{ ...waiting, status: "idle" }], false)[0].state).toBe("background");
    // That change arrives as a session update plus agent_status, which carries no visual status.
    expect(classifyAgents(mergeAgentStatus(listed, agent("child")), [{ ...waiting, status: "idle" }], false)[0].state).toBe("resting");
  });

  it("monitors a working agent's live turn and times it only from an in-flight snapshot", () => {
    const [ada] = classifyAgents([agent("ada", { status: "working", session_info: "PR 리뷰" })], [], false);
    const turn = { status: "working", started_at: "2026-11-18T00:00:00Z", updated_at: null, provider: "codex", current_tool_line: null };
    expect(monitorView(turn, ada, tr)).toEqual({ started: null, label: "PR 리뷰", kind: "think", role: "codex" });
    const live = { ...turn, updated_at: "2026-11-18T00:05:00Z", current_tool_line: "npm test", tool_events: [{ kind: "tool", tool_name: "Bash" }] };
    const now = Date.parse("2026-11-18T00:06:00Z");
    expect(monitorView(live, ada, tr, now)).toMatchObject({ started: Date.parse(turn.started_at), label: "npm test", kind: "run" });
    expect(monitorView({ ...turn, status: "idle" }, ada, tr, now)).toBeNull();
    // A start well after the snapshot (another time zone) or 13 hours back (a leftover file) hides the clock.
    expect(monitorView({ ...live, started_at: "2026-11-18T09:00:00Z" }, ada, tr, now)?.started).toBeNull();
    expect(monitorView(live, ada, tr, now + 13 * 3_600_000)?.started).toBeNull();
  });

  it("tells an empty session list from one that failed to load", () => {
    expect(classifyAgents([agent("a")], [], false)[0].state).toBe("noSession");
    const [unknown] = classifyAgents([agent("a")], [], true);
    expect([unknown.state, agentLine(unknown, tr, true)]).toEqual(["unknown", "확인 불가"]);
  });
});

describe("home to-dos", () => {
  const card = (id: string, status: string, extra: Partial<KanbanCard> = {}) =>
    ({ id, title: `card ${id}`, status, github_repo: "o/AgentDesk", github_issue_number: 7, blocked_reason: null, review_status: null, updated_at: "2026-11-18T00:00:00Z", ...extra }) as KanbanCard;

  it("keeps what the server leaves to a person and drops automatic reviews, PR creation, new requests and CI waits", () => {
    const items = buildTodoItems({
      cards: [
        card("r", "review", { review_status: "reviewing" }), card("pr", "review", { blocked_reason: "pr:creating" }), card("q", "requested"),
        card("ci", "in_progress", { blocked_reason: "ci:waiting" }), card("rerun", "in_progress", { blocked_reason: "ci:rerunning" }),
        card("d", "review", { review_status: "dilemma_pending" }), card("b", "in_progress", { blocked_reason: "dependency cycle", assignee_agent_id: "ada" }),
        card("f", "qa_failed"), card("x", "done", { blocked_reason: "old" }),
      ],
      campaigns: [{ definition: { title: "릴레이", goal: "", repository: "adk" }, view: { campaign_id: "relay", display: "needs_inspection", as_of: "", stop_mode: null, failed_count: 0, blocked_count: 0 } }] as ServerCampaignSummary[],
      tr,
      isKo: true,
    });
    expect(items.map((item) => [item.key, item.chip])).toEqual([["card:d", "판단 필요"], ["card:b", "판단 필요"], ["card:f", "실패"], ["campaign:relay", "상태 확인 필요"]]);
    expect(items[1]).toMatchObject({ title: "#7 card b", detail: "dependency cycle", to: "/campaigns?tab=queue&card=b", agentId: "ada" });
    expect(items[3].agentId).toBeNull();
  });

  it("lists server nodes waiting for approval, a decision or a deploy first, and leaves work the scheduler assigns", () => {
    const document = structuredClone(serverCampaignFixtures.prepared);
    const statuses = ["awaiting_deploy", "awaiting_start", "awaiting_approval"];
    document.view.nodes.forEach((node, index) => { node.runtime = { ...node.runtime, human_status: statuses[index], wait_started_at: null }; });
    const items = buildTodoItems({ cards: [card("d", "review", { review_status: "dilemma_pending" })], campaigns: [], documents: [document], tr, isKo: true });
    expect(items.map((item) => [item.key, item.kind, item.chip])).toEqual([
      ["node:release:C", "approve", "승인 대기"], ["card:d", "decide", "판단 필요"], ["node:release:A", "deploy", "배포 대기"],
    ]);
    expect(items[0]).toMatchObject({ title: "#7440 Independent work", detail: "Release", to: "/campaigns?mode=server&campaign=release&node=C" });
  });

  it("shows the empty state only once every source answered", () => {
    expect(todoStatus(0, ["ok", "loading"])).toBe("loading");
    expect(todoStatus(0, ["ok", "failed"])).toBe("partial");
    expect(todoStatus(0, ["ok", "partial"])).toBe("partial");
    expect(todoStatus(0, ["ok", "ok"])).toBe("empty");
  });
});

describe("home system", () => {
  it("marks detail-only values unknown on a public summary and reads the server campaign executor", () => {
    const view = { execution_readiness: "unavailable" } as ServerCampaignDocument["view"];
    const rows = buildSystemRows({
      health: { status: "degraded", publicOnly: "forbidden", queue_depth: 4 } as HomeHealth,
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

  it("counts connected bots and the queue from detail health, unless a mailbox went unread", () => {
    const health = { status: "healthy", uptime_secs: 7_200, queue_depth: 0, providers: [{ connected: true }, { connected: false }] } as HomeHealth;
    const rows = buildSystemRows({ health, healthFailed: false, legacy: null, documents: [], tr });
    expect(rows.map((row) => row.value)).toEqual(["정상 · 2시간째", "1/2개 연결", "확인 불가", "확인 불가", "0건"]);
    const blind = buildSystemRows({ health: { ...health, mailbox_observation_failures: [{ channel_id: "1" }] }, healthFailed: false, legacy: null, documents: [], tr });
    expect(blind.at(-1)?.value).toBe("확인 불가");
  });

  it("needs attention for an off or unknown line but not for Memento's standing unknown", () => {
    const rows = (patch: Partial<HomeHealth>, legacy: Campaign[] | null = []) =>
      buildSystemRows({ health: { status: "healthy", queue_depth: 0, providers: [{ connected: true }], ...patch } as HomeHealth, healthFailed: false, legacy, documents: [], tr });
    expect(systemNeedsAttention(rows({}), tr)).toBe(false);
    expect(systemNeedsAttention(rows({ queue_depth: 1 }), tr)).toBe(true);
    expect(systemNeedsAttention(rows({}, null), tr)).toBe(true);
  });
});

describe("home campaigns", () => {
  const summary = (id: string, display: ServerCampaignSummary["view"]["display"]) =>
    ({ definition: { title: id, goal: "goal", repository: "adk" }, view: { campaign_id: id, display, as_of: "", stop_mode: null, failed_count: 0, blocked_count: 0 } }) as ServerCampaignSummary;
  const ledger = (id: string, status: string, nodes: string[]) =>
    ({ id, title: id, status, updated_at: "2026-11-18T00:00:00Z", nodes: nodes.map((value) => ({ status: value })) }) as unknown as Campaign;

  it("puts a server campaign at its slowest task's stage and shows ledger completion", () => {
    const node = (stage: string) => ({ runtime: { stage } });
    const document = { view: { campaign_id: "v3", nodes: [node("review"), node("landing")], reasons: [{ code: "review_resources", target: "a" }] } } as unknown as ServerCampaignDocument;
    const { rows, shown } = buildCampaignRows({
      legacy: [ledger("planned", "planned", []), ledger("done", "completed", ["completed"]), ledger("live", "active", ["completed", "running", "pending", "pending"])],
      summaries: [summary("v3", "running"), summary("old", "succeeded")],
      documents: [document],
      tr,
      limit: 2,
    });
    expect([rows.map((row) => row.key), shown]).toEqual([["server:v3", "legacy:live", "legacy:planned"], 2]);
    expect(rows[0]).toMatchObject({ segments: ["done", "done", "done", "done", "now", "todo", "todo", "todo"], line: "리뷰 · 리뷰 세션 자원 대기", status: "실행 중 · 작업 2개" });
    expect(rows[1]).toMatchObject({ ratio: 0.25, line: "4개 중 1개 끝남 · 진행 중 1" });
    expect(rows[2]).toMatchObject({ ratio: null, line: "0개 중 0개 끝남" });
  });

  it("leads with campaigns under way, keeps all of them shown and says when stages were not read", () => {
    const prepared = ["p1", "p2", "p3", "p4"].map((id) => summary(id, "prepared"));
    const mixed = buildCampaignRows({ legacy: [ledger("live", "active", ["running"])], summaries: prepared, documents: [], tr, limit: 4 });
    expect([mixed.rows.map((row) => row.key).slice(0, 2), mixed.shown]).toEqual([["legacy:live", "server:p1"], 4]);
    const running = buildCampaignRows({ legacy: [], summaries: ["a", "b", "c", "d", "e"].map((id) => summary(id, "running")), documents: [], tr, limit: 4 });
    expect(running.shown).toBe(5);
    expect(running.rows[0]).toMatchObject({ segments: null, ratio: null, line: "단계 확인 불가" });
  });
});

describe("home routines and usage", () => {
  it("keeps today's runs and later due times, a routine that ran and runs again today in two rows", () => {
    const now = new Date(2026, 10, 19, 12, 0).getTime();
    const at = (hour: number, day = 19) => new Date(2026, 10, day, hour).toISOString();
    const routine = (id: string, extra: Partial<RoutineRecord>) => ({ id, name: id, status: "enabled", in_flight_run_id: null, next_due_at: null, last_run_at: null, ...extra }) as RoutineRecord;
    const run = (status: string, hour: number, action: string | null = null) => ({ status, action, started_at: at(hour) }) as RoutineRecord["last_run"];
    const rows = buildTodayRoutines([
      routine("brief", { last_run: run("succeeded", 6, "agent") }),
      routine("twice", { last_run: run("succeeded", 11, "complete"), next_due_at: at(13) }),
      routine("late", { next_due_at: at(22) }),
      routine("tomorrow", { last_run: { status: "failed", started_at: at(9, 18) } as RoutineRecord["last_run"], next_due_at: at(9, 20) }),
    ], now, tr);
    expect(rows.map((row) => [row.key, row.chip, row.finished])).toEqual([
      ["brief:run", "보냄", true], ["twice:run", "끝남", true], ["twice:due", "예정", false], ["late:due", "예정", false],
    ]);
  });

  it("turns tracker buckets into bars, leaves Gemini's limit-only buckets unmeasured and reads the auto-run pause", () => {
    const rows = buildUsageRows([
      { provider: "claude", buckets: [{ name: "7d", limit: 100, used: 83, remaining: 17, reset: 0 }, { name: "5h", limit: 100, used: 0, remaining: 100, reset: 0 }], stale: true, fetched_at: 1_795_033_577 },
      { provider: "gemini", buckets: [{ name: "rpm", limit: 15, used: 0, remaining: 15, reset: 0 }], stale: false, fetched_at: 1_795_033_600 },
      { provider: "github", buckets: [{ name: "core", limit: 5000, used: 3, remaining: 4997, reset: 0 }], stale: false },
    ], true);
    expect(rows.map((row) => [row.group, row.label, row.pct, row.unmeasured, row.measuredAt])).toEqual([
      ["Claude", "7일", 83, false, 1_795_033_577_000], ["Claude", "5시간", 0, false, 1_795_033_577_000], ["Gemini", "rpm", null, true, null],
    ]);
    expect(rows[0].stale).toBe(true);
    expect(autoPauseRule({ current: { dispatchRateLimitGateEnabled: true, dispatchRateLimitGateDangerPct: 90 }, defaults: {} })).toEqual({ enabled: true, pct: 90 });
    expect(autoPauseRule(null)).toBeNull();
  });

  it("names the auto-run pause on a row at or past its threshold only", () => {
    const [high, low] = buildUsageRows([{ provider: "claude", buckets: [{ name: "7d", limit: 100, used: 92, remaining: 8, reset: 0 }, { name: "5h", limit: 100, used: 85, remaining: 15, reset: 0 }], stale: false }], true);
    expect([usageDetail(high, { enabled: true, pct: 90 }, tr), usageDetail(low, { enabled: true, pct: 90 }, tr)]).toEqual(["자동 진행 멈춤 기준 이상", undefined]);
    expect(usageDetail(high, { enabled: false, pct: 90 }, tr)).toBeUndefined();
  });
});
