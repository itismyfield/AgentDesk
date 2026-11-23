import type { Campaign } from "../../api/campaigns";
import type { RoutineRecord } from "../../api/routines";
import type { ServerCampaignDocument, ServerCampaignSummary, ServerCampaignView } from "../../api/serverCampaigns";
import type { HealthResponse, RuntimeConfigResponse } from "../../api/settingsRuntime";
import { formatRelativeTime } from "../../app/shellFormatting";
import type { AgentMonitorStepKind } from "../../kit/pro/agent-monitor";
import type { Agent, DispatchedSession, KanbanCard } from "../../types";
import { cardTitle, coerceTimestampMs, isOpenKanbanCard } from "../agent-manager/kanban-utils";
import { campaignProgress } from "../campaigns/campaignModel";
import { queueLocationQuery } from "../campaigns/campaignQueueModel";
import { DISPLAY_LABELS, REASON_LABELS, STAGE_LABELS, campaignLocationQuery, displayText } from "../campaigns/serverCampaignModel";
import {
  formatRateLimitWindowLabel,
  transformRLProviders,
  type RateLimitLevel,
  type RawRLProvider,
} from "../dashboard/MiniRateLimitBarModel";
import { parseTime } from "../dashboard/routineModel";

export type Tr = (ko: string, en: string) => string;
export type HomeTone = "ok" | "info" | "warn" | "err" | "neutral";

export const QUEUE_PATH = `/campaigns${queueLocationQuery(null)}`;

const join = (...parts: Array<string | null | undefined | false>) => parts.filter(Boolean).join(" · ");

// ── Agents ──

export type HomeAgentState = "working" | "background" | "resting" | "noSession" | "unknown";
/** Display and sort order. `unknown` only appears when the session list failed to load. */
export const AGENT_STATES: HomeAgentState[] = ["working", "background", "resting", "noSession", "unknown"];

export interface HomeAgent {
  agent: Agent;
  state: HomeAgentState;
  /** First line of the summary of the session that set a working or background state. */
  info: string | null;
  /** Newest session signal: the live one for resting agents, the last one for agents without a session. */
  seenAt: number | null;
}

const TURN_STATUSES = new Set(["turn_active", "working"]);
const LIVE_STATUSES = new Set(["turn_active", "working", "awaiting_bg", "awaiting_user", "idle"]);

function newestSeen(sessions: DispatchedSession[]): number | null {
  const times = sessions.map((session) => coerceTimestampMs(session.last_seen_at)).filter((time): time is number => time !== null);
  return times.length ? Math.max(...times) : null;
}

function newestSession(sessions: DispatchedSession[]): DispatchedSession | null {
  const seen = (session: DispatchedSession) => coerceTimestampMs(session.last_seen_at) ?? 0;
  return sessions.reduce<DispatchedSession | null>((best, session) => (best && seen(best) >= seen(session) ? best : session), null);
}

const firstLine = (text: string | null | undefined) => text?.split("\n")[0]?.trim() || null;

/** One state per agent. Its own sessions set the state and the line together, so a session update replaces both.
 * The agent list's visual status is a snapshot (cleared on the next session change): with sessions known it only adds
 * background children, which no session status shows. A failed session list leaves idle agents unknown. */
export function classifyAgents(agents: Agent[], sessions: DispatchedSession[], sessionsFailed: boolean): HomeAgent[] {
  const byAgent = new Map<string, DispatchedSession[]>();
  for (const session of sessions) {
    if (!session.linked_agent_id) continue;
    byAgent.set(session.linked_agent_id, [...(byAgent.get(session.linked_agent_id) ?? []), session]);
  }
  const rank = (state: HomeAgentState) => AGENT_STATES.indexOf(state);
  return agents
    .map((agent): HomeAgent => {
      const own = byAgent.get(agent.id) ?? [];
      const live = own.filter((session) => LIVE_STATUSES.has(session.status));
      const lead = newestSession(live.filter((session) => TURN_STATUSES.has(session.status)))
        ?? newestSession(live.filter((session) => session.status === "awaiting_bg"));
      if (lead) return { agent, state: TURN_STATUSES.has(lead.status) ? "working" : "background", info: firstLine(lead.session_info), seenAt: null };
      const snapshot = (agent as Agent & { visual_status_code?: string }).visual_status_code;
      if (snapshot === "idle-bg-wait") return { agent, state: "background", info: firstLine(agent.session_info), seenAt: null };
      if (live.length) return { agent, state: "resting", info: null, seenAt: newestSeen(live) };
      if (agent.status === "working" || (sessionsFailed && (snapshot === "active" || snapshot === "stuck-suspect"))) {
        return { agent, state: "working", info: firstLine(agent.session_info), seenAt: null };
      }
      // Unlinked sessions shown as agents are live by construction.
      if (agent.id.startsWith("dispatched:")) return { agent, state: "resting", info: null, seenAt: null };
      return { agent, state: own.length || !sessionsFailed ? "noSession" : "unknown", info: null, seenAt: newestSeen(own) };
    })
    .sort((left, right) => rank(left.state) - rank(right.state) || agentName(left.agent, true).localeCompare(agentName(right.agent, true)));
}

export function agentName(agent: Agent, isKo: boolean): string {
  return (isKo ? agent.name_ko || agent.name : agent.name) || agent.id;
}

export function agentStateLabel(state: HomeAgentState, tr: Tr): string {
  switch (state) {
    case "working": return tr("작업 중", "Working");
    case "background": return tr("백그라운드 대기", "Background wait");
    case "resting": return tr("쉬는 중", "Resting");
    case "noSession": return tr("세션 없음", "No session");
    default: return tr("확인 불가", "Unknown");
  }
}

export function agentLine({ state, info, seenAt }: HomeAgent, tr: Tr, isKo: boolean): string {
  const when = seenAt === null ? null : formatRelativeTime(seenAt, isKo);
  if (state === "noSession" && when) return tr(`세션 없음 · 마지막 세션 ${when}`, `No session · last session ${when}`);
  return join(agentStateLabel(state, tr), info ?? when);
}

/** The part of `GET /api/agents/{id}/turn` the home monitor reads. */
export interface HomeTurn {
  status: string;
  started_at: string | null;
  updated_at: string | null;
  provider: string | null;
  current_tool_line: string | null;
  tool_events?: { kind: string; tool_name?: string | null }[];
}

export interface MonitorView {
  started: number | null;
  label: string;
  kind: AgentMonitorStepKind;
  role: string | null;
}

const TOOL_KINDS: Array<[RegExp, AgentMonitorStepKind]> = [
  [/^(Read|NotebookRead)$/, "read"], [/^(Grep|Glob|LS|WebSearch)$/, "search"], [/^(Edit|MultiEdit|Write|NotebookEdit)$/, "edit"],
  [/^(Bash|exec_command)$/, "run"], [/^WebFetch$/, "web"],
];

/** A working agent's live turn for the kit monitor, or null to keep the one-line cell. No steps are passed: the turn
 * reports no step times, and the monitor would show each as zero. */
export function monitorView(turn: HomeTurn | undefined, entry: HomeAgent, tr: Tr): MonitorView | null {
  if (!turn || entry.state !== "working" || turn.status === "idle") return null;
  const tool = turn.current_tool_line ? turn.tool_events?.filter((event) => event.kind === "tool").at(-1)?.tool_name ?? "" : "";
  return {
    // Without an in-flight snapshot the server answers the session's creation time; `updated_at` comes from that snapshot.
    started: turn.updated_at ? coerceTimestampMs(turn.started_at) : null,
    label: turn.current_tool_line ?? entry.info ?? tr("작업 중", "Working"),
    kind: TOOL_KINDS.find(([pattern]) => pattern.test(tool))?.[1] ?? "think",
    role: turn.provider,
  };
}

// ── To do ──

/** What the row's button does: approve (disabled for now), open the decision or deploy, or no button. */
export type TodoKind = "approve" | "decide" | "deploy" | "other";

export interface TodoItem {
  key: string;
  kind: TodoKind;
  tone: HomeTone;
  chip: string;
  title: string;
  detail: string;
  to: string;
  agentId: string | null;
}

const FAILED_STATUS = /(^|_)failed$/;
// `src/manual_intervention.rs`: a dilemma or a blocked reason outside these progress prefixes waits for a person.
// The queue's shared helper also treats deploy states as benign for its own lanes, so home keeps the server's list.
const BENIGN_BLOCKED_PREFIXES = ["ci:waiting", "ci:running", "ci:rerunning", "ci:rework", "pr:creating"];

function needsPerson({ review_status, blocked_reason }: KanbanCard): boolean {
  const reason = blocked_reason ?? "";
  return review_status === "dilemma_pending" || (reason.trim() !== "" && !BENIGN_BLOCKED_PREFIXES.some((prefix) => reason.startsWith(prefix)));
}

function repoName(repo: string | null): string | null {
  return repo ? repo.split("/").pop() || repo : null;
}

// `human_status` of server campaign nodes (docs/dashboard-campaign-decisions-api.md). `awaiting_start` stays out:
// the scheduler assigns that work.
const HUMAN_TODO = new Map<string, Pick<TodoItem, "kind" | "tone"> & { chip: [string, string] }>([
  ["awaiting_approval", { kind: "approve", tone: "warn", chip: ["승인 대기", "Approval"] }],
  ["decision_required", { kind: "decide", tone: "err", chip: ["판단 필요", "Decide"] }],
  ["awaiting_deploy", { kind: "deploy", tone: "info", chip: ["배포 대기", "Deploy"] }],
]);
const KIND_ORDER: TodoKind[] = ["approve", "decide", "deploy", "other"];

/** Things the server leaves to a person: server campaign nodes waiting for an approval, a decision or a deploy,
 * manual intervention as `src/manual_intervention.rs` defines it (a dilemma or a non-benign blocked reason), failed
 * cards, and server campaigns that need inspection. Reviews, new requests and stalled work stay out: policies move
 * them, and the timeout policy turns stalled work into manual intervention. */
export function buildTodoItems({ cards, campaigns, documents = [], tr, isKo }: {
  cards: KanbanCard[];
  campaigns: ServerCampaignSummary[] | null;
  documents?: ServerCampaignDocument[];
  tr: Tr;
  isKo: boolean;
}): TodoItem[] {
  const items: TodoItem[] = [];
  const ago = (value: string | null | undefined) => {
    const time = coerceTimestampMs(value);
    return time === null ? null : formatRelativeTime(time, isKo);
  };
  for (const card of cards.filter(isOpenKanbanCard)) {
    const base = {
      key: `card:${card.id}`,
      title: `${card.github_issue_number ? `#${card.github_issue_number} ` : ""}${cardTitle(card.title)}`,
      to: `/campaigns${queueLocationQuery(card.id)}`,
      agentId: card.assignee_agent_id ?? card.owner_agent_id ?? null,
    };
    if (needsPerson(card)) {
      items.push({ ...base, kind: "decide", tone: "err", chip: tr("판단 필요", "Decide"), detail: card.blocked_reason?.trim() || tr("사람 판단을 기다려요", "Waiting for a decision") });
    } else if (FAILED_STATUS.test(card.status)) {
      items.push({ ...base, kind: "other", tone: "err", chip: tr("실패", "Failed"), detail: join(repoName(card.github_repo), card.latest_dispatch_result_summary ?? ago(card.updated_at)) });
    }
  }
  for (const { definition, view } of documents) {
    for (const node of view.nodes) {
      const status = node.runtime.human_status;
      const human = typeof status === "string" ? HUMAN_TODO.get(status) : undefined;
      if (!human) continue;
      const spec = definition.nodes.find((entry) => entry.id === node.id);
      const issue = spec?.issue_url.match(/\/issues\/(\d+)/)?.[1];
      const waitedAt = node.runtime.wait_started_at;
      items.push({
        key: `node:${view.campaign_id}:${node.id}`,
        kind: human.kind,
        tone: human.tone,
        chip: tr(...human.chip),
        title: `${issue ? `#${issue} ` : ""}${spec?.title ?? node.id}`,
        detail: join(definition.title, typeof waitedAt === "string" && ago(waitedAt)),
        to: `/campaigns${campaignLocationQuery({ mode: "server", campaign: view.campaign_id, node: node.id })}`,
        agentId: null,
      });
    }
  }
  for (const summary of campaigns ?? []) {
    const { display, campaign_id } = summary.view;
    if (display !== "needs_inspection" && display !== "unable_to_progress") continue;
    items.push({
      key: `campaign:${campaign_id}`,
      kind: "other",
      tone: "err",
      chip: tr(...DISPLAY_LABELS[display]),
      title: summary.definition.title,
      detail: displayText(summary.view, tr),
      to: `/campaigns${campaignLocationQuery({ mode: "server", campaign: campaign_id, node: null })}`,
      agentId: null,
    });
  }
  return items.sort((left, right) => KIND_ORDER.indexOf(left.kind) - KIND_ORDER.indexOf(right.kind));
}

export type SourceState = "loading" | "ok" | "failed";

/** The to-do list is empty only when every source answered; a failed source leaves it partial. */
export function todoStatus(count: number, sources: SourceState[]): "loading" | "empty" | "partial" | "ready" {
  if (sources.includes("failed")) return "partial";
  if (count > 0) return "ready";
  return sources.includes("loading") ? "loading" : "empty";
}

// ── System ──

/** Health as home reads it. `mailbox_observation_failures` lists mailboxes the queue total leaves out. */
export type HomeHealth = HealthResponse & { mailbox_observation_failures?: unknown[] };

export interface SystemRow {
  key: "server" | "discord" | "memento" | "campaign" | "queue";
  label: string;
  value: string;
  tone: HomeTone;
}

function uptimeText(seconds: number, tr: Tr): string {
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor(seconds / 3_600);
  const minutes = Math.max(1, Math.floor(seconds / 60));
  if (days > 0) return tr(`${days}일째`, `up ${days}d`);
  if (hours > 0) return tr(`${hours}시간째`, `up ${hours}h`);
  return tr(`${minutes}분째`, `up ${minutes}m`);
}

export function buildSystemRows({ health, healthFailed, legacy, documents, tr }: {
  health: HomeHealth | null;
  healthFailed: boolean;
  legacy: Campaign[] | null;
  documents: ServerCampaignDocument[];
  tr: Tr;
}): SystemRow[] {
  const unknown = tr("확인 불가", "Unknown");
  const detail = health && !health.publicOnly ? health : null;
  const status = health?.status;
  const serverTone: HomeTone = status === "healthy" ? "ok" : status === "degraded" ? "warn" : status === "unhealthy" ? "err" : "neutral";
  const statusWord = status === "healthy" ? tr("정상", "Healthy") : status === "degraded" ? tr("주의", "Degraded") : status === "unhealthy" ? tr("장애", "Unhealthy") : status;
  const server = health
    ? join(statusWord, detail?.uptime_secs != null && uptimeText(detail.uptime_secs, tr))
    : healthFailed ? unknown : tr("확인 중", "Checking");

  const providers = detail?.providers ?? [];
  const connected = providers.filter((provider) => provider.connected).length;
  const discord: Pick<SystemRow, "value" | "tone"> = !detail
    ? { value: unknown, tone: "neutral" }
    : providers.length === 0
      ? { value: tr("연결된 봇 없음", "No bots"), tone: "err" }
      : connected === providers.length
        ? { value: tr(`${connected}개 연결`, `${connected} connected`), tone: "ok" }
        : { value: tr(`${connected}/${providers.length}개 연결`, `${connected}/${providers.length} connected`), tone: connected === 0 ? "err" : "warn" };

  // Server campaigns report their executor; ledger campaigns run through auto-run.
  const campaignParts: string[] = [];
  let campaignTone: HomeTone = "neutral";
  if (documents.some(({ view }) => view.execution_readiness === "unavailable")) {
    campaignParts.push(tr("서버 실행 기능 꺼짐", "Server execution off"));
    campaignTone = "warn";
  }
  if (legacy) {
    const autoRun = legacy.filter((campaign) => campaign.status === "active" && campaign.auto_queue).length;
    campaignParts.push(autoRun ? tr(`자동 진행 ${autoRun}개`, `Auto-run on ${autoRun}`) : tr("자동 진행 꺼짐", "Auto-run off"));
    if (autoRun && campaignTone === "neutral") campaignTone = "ok";
  }

  // The server adds only the mailboxes it could read and lists the rest as observation failures.
  const queueDepth = detail?.mailbox_observation_failures?.length ? null : detail?.queue_depth;
  return [
    { key: "server", label: tr("서버", "Server"), value: server, tone: serverTone },
    { key: "discord", label: tr("Discord 봇", "Discord bots"), ...discord },
    { key: "memento", label: "Memento", value: unknown, tone: "neutral" },
    { key: "campaign", label: tr("캠페인 실행", "Campaign runs"), value: campaignParts.join(" · ") || unknown, tone: campaignTone },
    {
      key: "queue",
      label: tr("대기열", "Queue"),
      value: queueDepth == null ? unknown : tr(`${queueDepth}건`, `${queueDepth}`),
      tone: queueDepth == null || queueDepth === 0 ? "neutral" : queueDepth >= 3 ? "err" : "warn",
    },
  ];
}

/** A line reads off (warn, err) or unknown. Memento is left out: home has no source for it, so it is always unknown. */
export function systemNeedsAttention(rows: SystemRow[], tr: Tr): boolean {
  const unknown = tr("확인 불가", "Unknown");
  return rows.some((row) => row.key !== "memento" && (row.tone === "warn" || row.tone === "err" || row.value === unknown));
}

// ── Campaigns ──

export type StageSegment = "done" | "now" | "fail" | "todo";

export interface CampaignRow {
  key: string;
  /** Under way: a server campaign past preparation or an active ledger campaign. */
  live: boolean;
  title: string;
  status: string;
  tone: HomeTone;
  to: string;
  /** Server campaigns: one segment per stage up to final verification. */
  segments: StageSegment[] | null;
  /** Ledger campaigns: share of tasks completed, null without tasks. */
  ratio: number | null;
  line: string;
}

type Stage = ServerCampaignView["nodes"][number]["runtime"]["stage"];
const STAGES = (Object.keys(STAGE_LABELS) as Stage[]).filter((stage) => stage !== "complete");
const CLOSED_DISPLAYS = new Set<ServerCampaignSummary["view"]["display"]>(["succeeded", "stopped"]);
const LEGACY_ORDER: Campaign["status"][] = ["active", "paused", "planned"];

export function isOpenServerCampaign(summary: ServerCampaignSummary): boolean {
  return !CLOSED_DISPLAYS.has(summary.view.display);
}

/** A campaign stands at its slowest task's stage, so done segments are stages every task has passed. Without its
 * detail the stages are unknown, never an empty track. */
function serverCampaignRow(summary: ServerCampaignSummary, document: ServerCampaignDocument | undefined, tr: Tr): CampaignRow {
  const { display, campaign_id, failed_count } = summary.view;
  const nodes = document?.view.nodes ?? [];
  const at = nodes.length ? Math.min(...nodes.map(({ runtime }) => (runtime.stage === "complete" ? STAGES.length : STAGES.indexOf(runtime.stage)))) : null;
  const current: StageSegment = display === "unable_to_progress" || failed_count > 0 ? "fail" : display === "prepared" ? "todo" : "now";
  const reasons = [...new Set((document?.view.reasons ?? []).map((reason) => reason.code))].slice(0, 2);
  return {
    key: `server:${campaign_id}`,
    live: display !== "prepared",
    title: summary.definition.title,
    status: join(displayText(summary.view, tr), nodes.length > 0 && tr(`작업 ${nodes.length}개`, `${nodes.length} tasks`)),
    tone: display === "running" ? "info" : display === "unable_to_progress" || display === "needs_inspection" ? "err" : "neutral",
    to: `/campaigns${campaignLocationQuery({ mode: "server", campaign: campaign_id, node: null })}`,
    segments: at === null ? null : STAGES.map((_, index) => (index < at ? "done" : index === at ? current : "todo")),
    ratio: null,
    line: document
      ? join(at !== null && at < STAGES.length && tr(...STAGE_LABELS[STAGES[at]]), ...reasons.map((code) => tr(...REASON_LABELS[code]))) || summary.definition.goal
      : tr("단계 확인 불가", "Stages unknown"),
  };
}

function legacyCampaignRow(campaign: Campaign, tr: Tr): CampaignRow {
  const { counts, total } = campaignProgress(campaign.nodes);
  const [label, tone]: [string, HomeTone] = campaign.status === "active"
    ? [tr("진행 중", "Active"), "info"]
    : campaign.status === "paused" ? [tr("멈춤", "Paused"), "warn"] : [tr("준비됨", "Planned"), "neutral"];
  return {
    key: `legacy:${campaign.id}`,
    live: campaign.status === "active",
    title: campaign.title,
    status: join(label, tr(`작업 ${total}개`, `${total} tasks`)),
    tone,
    to: `/campaigns${campaignLocationQuery({ mode: "legacy", campaign: campaign.id, node: null })}`,
    segments: null,
    ratio: total ? counts.completed / total : null,
    line: join(
      tr(`${total}개 중 ${counts.completed}개 끝남`, `${counts.completed} of ${total} done`),
      counts.running > 0 && tr(`진행 중 ${counts.running}`, `${counts.running} running`),
      counts.blocked > 0 && tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`),
    ),
  };
}

/** Campaigns under way first, server then ledger, then prepared, paused and planned ones. `shown` keeps every
 * campaign under way visible and at least `limit` rows; home folds the rest. */
export function buildCampaignRows({ legacy, summaries, documents, tr, limit }: {
  legacy: Campaign[] | null;
  summaries: ServerCampaignSummary[] | null;
  documents: ServerCampaignDocument[];
  tr: Tr;
  limit: number;
}): { rows: CampaignRow[]; shown: number } {
  const server = (summaries ?? []).filter(isOpenServerCampaign).map((summary) =>
    serverCampaignRow(summary, documents.find((document) => document.view.campaign_id === summary.view.campaign_id), tr));
  const ledger = (legacy ?? [])
    .filter((campaign) => LEGACY_ORDER.includes(campaign.status))
    .sort((left, right) => LEGACY_ORDER.indexOf(left.status) - LEGACY_ORDER.indexOf(right.status) || right.updated_at.localeCompare(left.updated_at))
    .map((campaign) => legacyCampaignRow(campaign, tr));
  const all = [...server, ...ledger];
  const live = all.filter((row) => row.live);
  return { rows: [...live, ...all.filter((row) => !row.live)], shown: Math.max(limit, live.length) };
}

// ── Today's routines ──

export interface RoutineRow {
  key: string;
  at: number;
  routine: RoutineRecord;
  chip: string;
  tone: HomeTone;
  /** A run that already ended today, whatever its result. */
  finished: boolean;
}

function sameLocalDay(left: number, right: number): boolean {
  return new Date(left).toDateString() === new Date(right).toDateString();
}

/** Server times only: a run started today and the next due time when it falls later today, as separate rows.
 * No run times are derived from the cron expression. */
export function buildTodayRoutines(routines: RoutineRecord[], now: number, tr: Tr): RoutineRow[] {
  const rows: RoutineRow[] = [];
  for (const routine of routines) {
    if (routine.status === "detached") continue;
    const run = routine.last_run ?? null;
    const runAt = parseTime(run?.started_at) ?? parseTime(routine.last_run_at);
    const base = { key: `${routine.id}:run`, routine };
    if (routine.in_flight_run_id || run?.status === "running") {
      rows.push({ ...base, at: runAt ?? now, chip: tr("도는 중", "Running"), tone: "info", finished: false });
    } else if (runAt !== null && sameLocalDay(runAt, now)) {
      const status = run?.status;
      const [chip, tone]: [string, HomeTone] = status === "succeeded"
        ? [run?.action === "agent" ? tr("보냄", "Sent") : tr("끝남", "Done"), "ok"]
        : status === "failed" || status === "interrupted" ? [tr("실패", "Failed"), "err"]
          : status === "skipped" ? [tr("건너뜀", "Skipped"), "neutral"]
            : status === "paused" ? [tr("멈춤", "Paused"), "warn"] : [tr("실행됨", "Ran"), "neutral"];
      rows.push({ ...base, at: runAt, chip, tone, finished: true });
    }
    const due = parseTime(routine.next_due_at);
    if (routine.status === "enabled" && due !== null && due >= now && sameLocalDay(due, now)) {
      rows.push({ key: `${routine.id}:due`, routine, at: due, chip: tr("예정", "Scheduled"), tone: "neutral", finished: false });
    }
  }
  return rows.sort((left, right) => left.at - right.at);
}

// ── Usage ──

export type RawUsageProvider = RawRLProvider & { fetched_at?: number | null };

export interface UsageRow {
  key: string;
  label: string;
  /** Null when the tracker has no measurement for this window. */
  pct: number | null;
  /** The provider reports a limit but no usage, so there is nothing to measure. */
  unmeasured: boolean;
  level: RateLimitLevel;
  stale: boolean;
  measuredAt: number | null;
  note: string | null;
}

// The server reads Gemini's quota limits but no usage and fills `used: 0` (`build_gemini_rate_limit_buckets`).
const unmeasuredBucket = (provider: string | undefined, bucket: string) => provider === "gemini" && (bucket === "rpm" || bucket === "rpd");

/** Limit tracker windows as bars; GitHub and hidden buckets stay out, as on the old usage widget. Only measured
 * rows carry a measurement time. */
export function buildUsageRows(raw: RawUsageProvider[], isKo: boolean): UsageRow[] {
  return raw.flatMap((source) => transformRLProviders([source]).flatMap((provider): UsageRow[] => {
    const fetchedAt = coerceTimestampMs(source.fetched_at ?? null);
    const common = { stale: provider.stale, level: "normal" as RateLimitLevel, unmeasured: false };
    if (provider.unsupported || provider.buckets.length === 0) {
      return [{ ...common, key: `${provider.provider}:${provider.profileId}`, label: provider.provider, pct: null, measuredAt: null, note: provider.reason }];
    }
    return provider.buckets.map((bucket) => {
      const unmeasured = unmeasuredBucket(source.provider, bucket.id);
      const pct = unmeasured ? null : bucket.utilization;
      return {
        ...common,
        key: `${provider.provider}:${provider.profileId}:${bucket.id}`,
        label: `${provider.provider} ${formatRateLimitWindowLabel(bucket.label, isKo)}`,
        pct,
        unmeasured,
        measuredAt: pct === null ? null : fetchedAt,
        level: unmeasured ? "normal" : bucket.level,
        note: null,
      };
    });
  }));
}

/** The auto-run pause from settings: the dispatch gate's switch and its threshold. */
export function autoPauseRule(config: RuntimeConfigResponse | null): { enabled: boolean; pct: number | null } | null {
  const enabled = config?.current?.dispatchRateLimitGateEnabled;
  if (typeof enabled !== "boolean") return null;
  const pct = config?.current?.dispatchRateLimitGateDangerPct;
  return { enabled, pct: typeof pct === "number" ? pct : null };
}
