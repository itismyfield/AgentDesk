import type { Campaign } from "../../api/campaigns";
import type { RoutineRecord } from "../../api/routines";
import type { ServerCampaignDocument, ServerCampaignSummary, ServerCampaignView } from "../../api/serverCampaigns";
import type { HealthResponse, RuntimeConfigResponse } from "../../api/settingsRuntime";
import { formatRelativeTime } from "../../app/shellFormatting";
import type { Agent, DashboardStats, DispatchedSession, KanbanCard } from "../../types";
import { cardTitle, coerceTimestampMs, isManualInterventionCard, isOpenKanbanCard } from "../agent-manager/kanban-utils";
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

export type HomeAgentState = "working" | "background" | "resting" | "noSession";
export const AGENT_STATES: HomeAgentState[] = ["working", "background", "resting", "noSession"];

export interface HomeAgent {
  agent: Agent;
  state: HomeAgentState;
  /** First line of the live session's summary, for working and background agents. */
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

/** One state per agent from its own sessions and the server's visual status. With no session list at all,
 * nothing proves a session is gone, so idle agents read as resting rather than without a session. */
export function classifyAgents(agents: Agent[], sessions: DispatchedSession[]): HomeAgent[] {
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
      const visual = (agent as Agent & { visual_status_code?: string }).visual_status_code;
      let state: HomeAgentState;
      if (live.some((session) => TURN_STATUSES.has(session.status))) state = "working";
      else if (live.some((session) => session.status === "awaiting_bg") || visual === "idle-bg-wait") state = "background";
      else if (agent.status === "working" || visual === "active" || visual === "stuck-suspect") state = "working";
      else if (live.length > 0 || sessions.length === 0 || agent.id.startsWith("dispatched:")) state = "resting";
      else state = "noSession";
      const busy = state === "working" || state === "background";
      return {
        agent,
        state,
        info: busy ? agent.session_info?.split("\n")[0]?.trim() || null : null,
        seenAt: busy ? null : newestSeen(state === "resting" ? live : own),
      };
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
    default: return tr("세션 없음", "No session");
  }
}

export function agentLine({ state, info, seenAt }: HomeAgent, tr: Tr, isKo: boolean): string {
  const when = seenAt === null ? null : formatRelativeTime(seenAt, isKo);
  if (state === "noSession" && when) return tr(`세션 없음 · 마지막 세션 ${when}`, `No session · last session ${when}`);
  return join(agentStateLabel(state, tr), info ?? when);
}

// ── To do ──

export interface TodoItem {
  key: string;
  tone: HomeTone;
  chip: string;
  title: string;
  detail: string;
  to: string;
  /** How many things the row stands for; a count row stands for many cards. */
  count: number;
}

const FAILED_STATUS = /(^|_)failed$/;

function repoName(repo: string | null): string | null {
  return repo ? repo.split("/").pop() || repo : null;
}

/** Things only a person can move: cards that need a decision, failed or waiting for review, the server's
 * stuck counts, and server campaigns that need inspection. Approval and deploy waits have no server field yet. */
export function buildTodoItems({ cards, stats, campaigns, tr, isKo }: {
  cards: KanbanCard[];
  stats: DashboardStats | null;
  campaigns: ServerCampaignSummary[] | null;
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
      count: 1,
    };
    if (isManualInterventionCard(card)) {
      items.push({ ...base, tone: "err", chip: tr("판단 필요", "Decide"), detail: card.blocked_reason?.trim() || tr("사람 판단을 기다려요", "Waiting for a decision") });
    } else if (FAILED_STATUS.test(card.status)) {
      items.push({ ...base, tone: "err", chip: tr("실패", "Failed"), detail: join(repoName(card.github_repo), card.latest_dispatch_result_summary ?? ago(card.updated_at)) });
    } else if (card.status === "review") {
      items.push({ ...base, tone: "warn", chip: tr("검토 대기", "Review"), detail: join(repoName(card.github_repo), ago(card.review_entered_at ?? card.updated_at)) });
    }
  }
  for (const summary of campaigns ?? []) {
    const { display, campaign_id } = summary.view;
    if (display !== "needs_inspection" && display !== "unable_to_progress") continue;
    items.push({
      key: `campaign:${campaign_id}`,
      tone: "err",
      chip: tr(...DISPLAY_LABELS[display]),
      title: summary.definition.title,
      detail: displayText(summary.view, tr),
      to: `/campaigns${campaignLocationQuery({ mode: "server", campaign: campaign_id, node: null })}`,
      count: 1,
    });
  }
  const accepting = stats?.kanban.waiting_acceptance ?? 0;
  if (accepting > 0) {
    items.push({
      key: "count:acceptance", tone: "warn", chip: tr("수락 대기", "Unaccepted"), to: QUEUE_PATH, count: accepting,
      title: tr(`에이전트가 아직 받지 않은 작업 ${accepting}건`, `${accepting} tasks not accepted yet`),
      detail: tr("대기열에서 담당을 확인해요", "Check the owner in the queue"),
    });
  }
  const stalled = stats?.kanban.stale_in_progress ?? 0;
  if (stalled > 0) {
    items.push({
      key: "count:stalled", tone: "warn", chip: tr("진행 멈춤", "Stalled"), to: QUEUE_PATH, count: stalled,
      title: tr(`진행이 멈춘 작업 ${stalled}건`, `${stalled} stalled tasks`),
      detail: tr("최근 움직임이 없어요", "No recent activity"),
    });
  }
  // Red rows first; the sort is stable, so each source keeps its own order.
  return items.sort((left, right) => Number(right.tone === "err") - Number(left.tone === "err"));
}

// ── System ──

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
  health: HealthResponse | null;
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

  const queueDepth = detail?.queue_depth;
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

// ── Campaigns ──

export type StageSegment = "done" | "now" | "fail" | "todo";

export interface CampaignRow {
  key: string;
  title: string;
  status: string;
  tone: HomeTone;
  to: string;
  /** Server campaigns: one segment per stage up to final verification. */
  segments: StageSegment[] | null;
  /** Ledger campaigns: share of tasks completed. */
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

/** A campaign stands at its slowest task's stage, so done segments are stages every task has passed. */
function serverCampaignRow(summary: ServerCampaignSummary, document: ServerCampaignDocument | undefined, tr: Tr): CampaignRow {
  const { display, campaign_id, failed_count } = summary.view;
  const nodes = document?.view.nodes ?? [];
  const at = nodes.length ? Math.min(...nodes.map(({ runtime }) => (runtime.stage === "complete" ? STAGES.length : STAGES.indexOf(runtime.stage)))) : null;
  const current: StageSegment = display === "unable_to_progress" || failed_count > 0 ? "fail" : display === "prepared" ? "todo" : "now";
  const reasons = [...new Set((document?.view.reasons ?? []).map((reason) => reason.code))].slice(0, 2);
  return {
    key: `server:${campaign_id}`,
    title: summary.definition.title,
    status: join(displayText(summary.view, tr), nodes.length > 0 && tr(`작업 ${nodes.length}개`, `${nodes.length} tasks`)),
    tone: display === "running" ? "info" : display === "unable_to_progress" || display === "needs_inspection" ? "err" : "neutral",
    to: `/campaigns${campaignLocationQuery({ mode: "server", campaign: campaign_id, node: null })}`,
    segments: at === null ? null : STAGES.map((_, index) => (index < at ? "done" : index === at ? current : "todo")),
    ratio: null,
    line: join(at !== null && at < STAGES.length && tr(...STAGE_LABELS[STAGES[at]]), ...reasons.map((code) => tr(...REASON_LABELS[code]))) || summary.definition.goal,
  };
}

function legacyCampaignRow(campaign: Campaign, tr: Tr): CampaignRow {
  const { counts, total } = campaignProgress(campaign.nodes);
  const [label, tone]: [string, HomeTone] = campaign.status === "active"
    ? [tr("진행 중", "Active"), "info"]
    : campaign.status === "paused" ? [tr("멈춤", "Paused"), "warn"] : [tr("준비됨", "Planned"), "neutral"];
  return {
    key: `legacy:${campaign.id}`,
    title: campaign.title,
    status: join(label, tr(`작업 ${total}개`, `${total} tasks`)),
    tone,
    to: `/campaigns${campaignLocationQuery({ mode: "legacy", campaign: campaign.id, node: null })}`,
    segments: null,
    ratio: total ? counts.completed / total : 0,
    line: join(
      tr(`${total}개 중 ${counts.completed}개 끝남`, `${counts.completed} of ${total} done`),
      counts.running > 0 && tr(`진행 중 ${counts.running}`, `${counts.running} running`),
      counts.blocked > 0 && tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`),
    ),
  };
}

/** Open server campaigns first, then open ledger campaigns: active, paused, planned, newest first. */
export function buildCampaignRows({ legacy, summaries, documents, tr, limit }: {
  legacy: Campaign[] | null;
  summaries: ServerCampaignSummary[] | null;
  documents: ServerCampaignDocument[];
  tr: Tr;
  limit: number;
}): { rows: CampaignRow[]; total: number } {
  const server = (summaries ?? []).filter(isOpenServerCampaign).map((summary) =>
    serverCampaignRow(summary, documents.find((document) => document.view.campaign_id === summary.view.campaign_id), tr));
  const ledger = (legacy ?? [])
    .filter((campaign) => LEGACY_ORDER.includes(campaign.status))
    .sort((left, right) => LEGACY_ORDER.indexOf(left.status) - LEGACY_ORDER.indexOf(right.status) || right.updated_at.localeCompare(left.updated_at))
    .map((campaign) => legacyCampaignRow(campaign, tr));
  const rows = [...server, ...ledger];
  return { rows: rows.slice(0, limit), total: rows.length };
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

/** Server times only: a run started today, else the next due time when it falls later today. No run times
 * are derived from the cron expression. */
export function buildTodayRoutines(routines: RoutineRecord[], now: number, tr: Tr): RoutineRow[] {
  const rows: RoutineRow[] = [];
  for (const routine of routines) {
    if (routine.status === "detached") continue;
    const run = routine.last_run ?? null;
    const runAt = parseTime(run?.started_at) ?? parseTime(routine.last_run_at);
    const base = { key: routine.id, routine };
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
    } else {
      const due = parseTime(routine.next_due_at);
      if (routine.status === "enabled" && due !== null && due >= now && sameLocalDay(due, now)) {
        rows.push({ ...base, at: due, chip: tr("예정", "Scheduled"), tone: "neutral", finished: false });
      }
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
  level: RateLimitLevel;
  stale: boolean;
  measuredAt: number | null;
  note: string | null;
}

/** Limit tracker windows as bars; GitHub and hidden buckets stay out, as on the old usage widget. */
export function buildUsageRows(raw: RawUsageProvider[], isKo: boolean): UsageRow[] {
  return raw.flatMap((source) => transformRLProviders([source]).flatMap((provider): UsageRow[] => {
    const measuredAt = coerceTimestampMs(source.fetched_at ?? null);
    const common = { stale: provider.stale, measuredAt, level: "normal" as RateLimitLevel };
    if (provider.unsupported || provider.buckets.length === 0) {
      return [{ ...common, key: `${provider.provider}:${provider.profileId}`, label: provider.provider, pct: null, note: provider.reason }];
    }
    return provider.buckets.map((bucket) => ({
      ...common,
      key: `${provider.provider}:${provider.profileId}:${bucket.id}`,
      label: `${provider.provider} ${formatRateLimitWindowLabel(bucket.label, isKo)}`,
      pct: bucket.utilization,
      level: bucket.level,
      note: null,
    }));
  }));
}

/** The auto-run pause from settings: the dispatch gate's switch and its threshold. */
export function autoPauseRule(config: RuntimeConfigResponse | null): { enabled: boolean; pct: number | null } | null {
  const enabled = config?.current?.dispatchRateLimitGateEnabled;
  if (typeof enabled !== "boolean") return null;
  const pct = config?.current?.dispatchRateLimitGateDangerPct;
  return { enabled, pct: typeof pct === "number" ? pct : null };
}
