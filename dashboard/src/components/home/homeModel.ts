import type { Campaign, HomeCampaignNodeSummary, HomeLegacyCampaignSummary } from "../../api/campaigns";
import type { RoutineRecord } from "../../api/routines";
import type { ServerCampaignDocument, ServerCampaignSummary, ServerCampaignView } from "../../api/serverCampaigns";
import type { HealthResponse, RuntimeConfigResponse } from "../../api/settingsRuntime";
import { formatRelativeTime } from "../../app/shellFormatting";
import type { AgentMonitorStep, AgentMonitorStepKind, AgentMonitorStepStatus } from "../../kit/pro/agent-monitor";
import type { RunningNowTask } from "../../kit/pro/running-now";
import { isTestAgent } from "../../lib/agentHelpers";
import { aggregateDegradedReasons } from "../../lib/healthReasons";
import type { Agent, DispatchedSession, KanbanCard } from "../../types";
import { parseChannelNameFromSessionKey } from "../agent-manager/discord-routing";
import { cardTitle, coerceTimestampMs, isOpenKanbanCard } from "../agent-manager/kanban-utils";
import { campaignProgress } from "../campaigns/campaignModel";
import { isLedgerHumanApprovalPending } from "../campaigns/campaignOverviewModel";
import { queueLocationQuery } from "../campaigns/campaignQueueModel";
import { DISPLAY_LABELS, REASON_LABELS, STAGE_LABELS, campaignLocationQuery, displayText } from "../campaigns/serverCampaignModel";
import { formatRateLimitWindowLabel, transformRLProviders, type RawRLProvider } from "../dashboard/MiniRateLimitBarModel";
import { formatClock, parseTime } from "../dashboard/routineModel";

export type Tr = (ko: string, en: string) => string;
export type HomeTone = "ok" | "info" | "warn" | "err" | "neutral";

export type HomeLegacyNode = Pick<HomeCampaignNodeSummary, "status"> & Partial<Omit<HomeCampaignNodeSummary, "status">>;

export type HomeLegacySummary = Omit<HomeLegacyCampaignSummary, "nodes"> & {
  nodes: ReadonlyArray<HomeLegacyNode>;
};

export const QUEUE_PATH = `/campaigns${queueLocationQuery(null)}`;
export const CAMPAIGNS_PATH = "/campaigns";

const join = (...parts: Array<string | null | undefined | false>) => parts.filter(Boolean).join(" · ");

// ── Agents & Unlinked Sessions ──

export type HomeAgentState = "working" | "background" | "resting" | "noSession" | "unknown";
/** Display and sort order. `unknown` only appears when the session list failed to load. */
export const AGENT_STATES: HomeAgentState[] = ["working", "background", "resting", "noSession", "unknown"];

export const STALE_SESSION_IDLE_MS = 7 * 24 * 60 * 60 * 1_000;
export const STALE_ACTIVE_TURN_MS = 6 * 60 * 60 * 1_000;

export interface HomeAgent {
  agent: Agent;
  state: HomeAgentState;
  /** First line of the summary of the session that set a working or background state. */
  info: string | null;
  /** Newest session signal: the live one for resting agents, the last one for agents without a session. */
  seenAt: number | null;
  /** Underlying unlinked session when this entry represents a session without `linked_agent_id`. */
  unlinkedSession?: DispatchedSession;
  /** True when an unlinked session is stalled (7+ days idle or zombie stale turn). */
  stalled?: boolean;
  /** Activity timestamp used for recency sorting within the same state bucket. */
  activityAt?: number | null;
}

const TURN_STATUSES = new Set(["turn_active", "working"]);
const LIVE_STATUSES = new Set(["turn_active", "working", "awaiting_bg", "awaiting_user", "idle"]);

function positiveTimestampMs(raw: unknown): number | null {
  const ms = coerceTimestampMs(raw as string | number | null | undefined);
  return ms !== null && ms > 0 ? ms : null;
}

function sessionLastSeenMs(session: DispatchedSession): number | null {
  return positiveTimestampMs(session.last_seen_at) ?? positiveTimestampMs(session.connected_at);
}

function newestSeen(sessions: DispatchedSession[]): number | null {
  const times = sessions.map((session) => positiveTimestampMs(session.last_seen_at)).filter((time): time is number => time !== null);
  return times.length ? Math.max(...times) : null;
}

function newestActivity(sessions: DispatchedSession[]): number | null {
  const times = sessions.map(sessionLastSeenMs).filter((time): time is number => time !== null);
  return times.length ? Math.max(...times) : null;
}

function newestSession(sessions: DispatchedSession[]): DispatchedSession | null {
  const seen = (session: DispatchedSession) => sessionLastSeenMs(session) ?? 0;
  return sessions.reduce<DispatchedSession | null>((best, session) => (best && seen(best) >= seen(session) ? best : session), null);
}

const firstLine = (text: string | null | undefined) => text?.split("\n")[0]?.trim() || null;

/** Human-friendly label for an unlinked session (`#adk-cc-*`, `#게임-ag`, etc.). */
export function unlinkedSessionTitle(session: DispatchedSession): string {
  const [parent] = session.session_key.split(":child:");
  const normalized = parent.replace(/^([^:]+):tmux:/, "$1:");
  const channel = parseChannelNameFromSessionKey(normalized);
  if (channel) return `#${channel}`;
  if (session.name && session.name !== session.session_key) return session.name;
  return parent.slice(parent.lastIndexOf("/") + 1) || session.session_key;
}

/** Stalled unlinked session: 7+ days idle, or an active/waiting turn silent for 6+ hours. */
export function isStaleUnlinkedSession(session: DispatchedSession, now = Date.now()): boolean {
  if (session.linked_agent_id || session.status === "disconnected") return false;
  const last = sessionLastSeenMs(session);
  if (last === null) return false;
  if (TURN_STATUSES.has(session.status) || session.status === "awaiting_bg") {
    return now - last >= STALE_ACTIVE_TURN_MS;
  }
  return now - last >= STALE_SESSION_IDLE_MS;
}

function sortBucket(entry: HomeAgent): number {
  if (entry.state === "working") return 0;
  if (entry.state === "background" || entry.stalled) return 1;
  if (entry.state === "resting") return 2;
  if (entry.state === "noSession") return 3;
  return 4;
}

function compareHomeAgents(left: HomeAgent, right: HomeAgent): number {
  const bucketDiff = sortBucket(left) - sortBucket(right);
  if (bucketDiff !== 0) return bucketDiff;
  if (Boolean(left.stalled) !== Boolean(right.stalled)) return left.stalled ? -1 : 1;
  const leftTime = left.activityAt ?? left.seenAt;
  const rightTime = right.activityAt ?? right.seenAt;
  if (leftTime !== rightTime) {
    if (leftTime == null) return 1;
    if (rightTime == null) return -1;
    return rightTime - leftTime;
  }
  return agentName(left.agent, true).localeCompare(agentName(right.agent, true));
}

/** One state per agent, sorted working -> background/stalled -> recent activity -> name. */
export function classifyAgents(agents: Agent[], sessions: DispatchedSession[], sessionsFailed: boolean): HomeAgent[] {
  const byAgent = new Map<string, DispatchedSession[]>();
  const unlinkedById = new Map<string, DispatchedSession>();
  for (const session of sessions) {
    if (session.linked_agent_id) {
      byAgent.set(session.linked_agent_id, [...(byAgent.get(session.linked_agent_id) ?? []), session]);
    } else if (session.status !== "disconnected") {
      unlinkedById.set(`dispatched:${session.id}`, session);
    }
  }
  return agents
    .map((agent): HomeAgent => {
      if (agent.id.startsWith("dispatched:")) {
        const raw = unlinkedById.get(agent.id);
        if (raw) {
          const seenAt = sessionLastSeenMs(raw);
          const stalled = isStaleUnlinkedSession(raw);
          const state: HomeAgentState = TURN_STATUSES.has(raw.status)
            ? "working"
            : raw.status === "awaiting_bg"
              ? "background"
              : "resting";
          const info = firstLine(raw.session_info) ?? (stalled ? "장기 멈춤" : null);
          return { agent, state, info, seenAt: state === "resting" ? seenAt : null, unlinkedSession: raw, stalled, activityAt: seenAt };
        }
        return { agent, state: agent.status === "working" ? "working" : "resting", info: firstLine(agent.session_info), seenAt: null };
      }
      const own = byAgent.get(agent.id) ?? [];
      const live = own.filter((session) => LIVE_STATUSES.has(session.status));
      const snapshot = (agent as Agent & { visual_status_code?: string }).visual_status_code;
      const stuck = snapshot === "stuck-suspect";
      const stalledProp = stuck ? { stalled: true } : {};
      const lead = newestSession(live.filter((session) => TURN_STATUSES.has(session.status)))
        ?? newestSession(live.filter((session) => session.status === "awaiting_bg"));
      if (lead) {
        return {
          agent,
          state: TURN_STATUSES.has(lead.status) ? "working" : "background",
          info: firstLine(lead.session_info),
          seenAt: null,
          ...stalledProp,
          activityAt: sessionLastSeenMs(lead),
        };
      }
      if (snapshot === "idle-bg-wait") {
        return { agent, state: "background", info: firstLine(agent.session_info), seenAt: null, activityAt: newestActivity(own) };
      }
      if (live.length) {
        const seenAt = newestSeen(live);
        return { agent, state: "resting", info: null, seenAt, ...stalledProp, activityAt: seenAt ?? newestActivity(live) };
      }
      if (agent.status === "working" || (sessionsFailed && (snapshot === "active" || snapshot === "stuck-suspect"))) {
        return { agent, state: "working", info: firstLine(agent.session_info), seenAt: null, ...stalledProp, activityAt: newestActivity(own) };
      }
      const seenAt = newestSeen(own);
      return { agent, state: own.length || !sessionsFailed ? "noSession" : "unknown", info: null, seenAt, ...stalledProp, activityAt: seenAt };
    })
    .sort(compareHomeAgents);
}

/** Full home roster: real agents plus active or stalled unlinked sessions (and department stand-ins). */
export function buildHomeAgents(
  agents: Agent[],
  sessions: DispatchedSession[],
  sessionsFailed: boolean,
  now = Date.now(),
): HomeAgent[] {
  const realAgents = agents.filter((agent) => !agent.id.startsWith("dispatched:") && (!isTestAgent(agent) || agent.status === "working"));
  const unlinkedAgents: Agent[] = sessions
    .filter((session) => {
      if (session.linked_agent_id || session.status === "disconnected") return false;
      const activeOrWaiting = TURN_STATUSES.has(session.status) || session.status === "awaiting_bg" || session.status === "awaiting_user";
      return activeOrWaiting || isStaleUnlinkedSession(session, now) || Boolean(session.department_id);
    })
    .map((session) => {
      const title = unlinkedSessionTitle(session);
      const working = TURN_STATUSES.has(session.status) || session.status === "awaiting_bg";
      return {
        id: `dispatched:${session.id}`,
        name: title,
        name_ko: title,
        department_id: session.department_id,
        cli_provider: session.provider,
        avatar_emoji: session.avatar_emoji || "📡",
        personality: null,
        status: working ? "working" : "idle",
        current_task_id: null,
        stats_tasks_done: 0,
        stats_tokens: session.tokens || 0,
        created_at: session.connected_at,
        session_info: session.session_info,
        department_name: session.department_name,
        department_name_ko: session.department_name_ko,
        department_color: session.department_color,
      };
    });

  const combined = classifyAgents([...realAgents, ...unlinkedAgents], sessions, sessionsFailed);
  return combined.map((entry) => {
    if (!entry.unlinkedSession) return entry;
    const stalled = isStaleUnlinkedSession(entry.unlinkedSession, now);
    return stalled === Boolean(entry.stalled) ? entry : { ...entry, stalled };
  }).sort(compareHomeAgents);
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

export function agentLine(entry: HomeAgent, tr: Tr, isKo: boolean): string {
  const { state, info, seenAt, unlinkedSession, stalled } = entry;
  const when = seenAt === null ? null : formatRelativeTime(seenAt, isKo);
  if (stalled && unlinkedSession) {
    const last = sessionLastSeenMs(unlinkedSession);
    const age = last !== null && Date.now() - last >= STALE_SESSION_IDLE_MS
      ? tr("7일 넘게 멈춤", "Idle 7+ days")
      : tr("멈춤 의심", "Suspected stall");
    return join(age, firstLine(unlinkedSession.session_info) ?? (last !== null ? formatRelativeTime(last, isKo) : null));
  }
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
  tool_events?: {
    kind: string;
    status?: string | null;
    tool_name?: string | null;
    summary?: string | null;
    line?: string | null;
  }[];
}

export interface MonitorView {
  started: number | null;
  label: string;
  kind: AgentMonitorStepKind;
  role: string | null;
  steps?: AgentMonitorStep[];
}

const TOOL_KINDS: Array<[RegExp, AgentMonitorStepKind]> = [
  [/^(Read|NotebookRead)$/i, "read"], [/^(Grep|Glob|LS|WebSearch)$/i, "search"], [/^(Edit|MultiEdit|Write|NotebookEdit)$/i, "edit"],
  [/^(Bash|exec_command)$/i, "run"], [/^WebFetch$/i, "web"],
];

function classifyToolKind(toolName: string | null | undefined, rawKind?: string): AgentMonitorStepKind {
  if (rawKind === "thinking") return "think";
  const name = toolName ?? "";
  return TOOL_KINDS.find(([pattern]) => pattern.test(name))?.[1] ?? (rawKind === "tool" ? "run" : "think");
}

function mapTurnSteps(turn: HomeTurn): AgentMonitorStep[] {
  const events = turn.tool_events ?? [];
  if (events.length === 0) return [];
  return events.map((event, index) => {
    const kind = classifyToolKind(event.tool_name, event.kind);
    const isLast = index === events.length - 1;
    const status: AgentMonitorStepStatus = event.status === "error"
      ? "failed"
      : event.status === "running" || (isLast && event.status !== "success")
        ? "running"
        : "done";
    const label = event.summary?.trim() || event.line?.trim() || event.tool_name?.trim() || (kind === "think" ? "생각 중" : "도구 실행");
    return {
      id: `step-${index}-${event.tool_name ?? event.kind}`,
      label,
      kind,
      status,
      tool: event.tool_name ?? undefined,
    };
  });
}

const CLOCK_AHEAD_MS = 60_000;
const CLOCK_MAX_MS = 12 * 3_600_000;

function resolveMonitorRole(entry: HomeAgent, turn?: HomeTurn): string | null {
  const agentProvider = !entry.unlinkedSession && !entry.agent.id.startsWith("dispatched:")
    ? entry.agent.cli_provider?.trim() || null
    : null;
  return agentProvider ?? turn?.provider ?? entry.unlinkedSession?.provider ?? entry.agent.cli_provider ?? null;
}

/** A working agent's live turn for the kit monitor, with a compact fallback when turn details have not arrived. */
export function monitorView(turn: HomeTurn | undefined, entry: HomeAgent, tr: Tr, now = Date.now()): MonitorView | null {
  if (entry.state !== "working") return null;
  const role = resolveMonitorRole(entry, turn);
  if (!turn || (!turn.status && !turn.current_tool_line && !(turn.tool_events?.length))) {
    return {
      started: null,
      label: entry.info ?? tr("작업 중", "Working"),
      kind: "think",
      role,
    };
  }
  if (turn.status === "idle") return null;
  const tool = turn.current_tool_line ? turn.tool_events?.filter((event) => event.kind === "tool").at(-1)?.tool_name ?? "" : "";
  const updated = coerceTimestampMs(turn.updated_at);
  const started = updated === null ? null : coerceTimestampMs(turn.started_at);
  const plausible = started !== null && updated !== null && started <= updated + CLOCK_AHEAD_MS && now - started <= CLOCK_MAX_MS;
  const steps = mapTurnSteps(turn);
  return {
    started: plausible ? started : null,
    label: turn.current_tool_line ?? entry.info ?? tr("작업 중", "Working"),
    kind: TOOL_KINDS.find(([pattern]) => pattern.test(tool))?.[1] ?? "think",
    role,
    ...(steps.length > 0 ? { steps } : {}),
  };
}

/** Synthesizes a live monitor view for an unlinked working lane (`#adk-cc-*`, etc.). */
export function unlinkedMonitorView(entry: HomeAgent, tr: Tr, now = Date.now()): MonitorView | null {
  const session = entry.unlinkedSession;
  if (!session || entry.state !== "working") return null;
  const started = coerceTimestampMs(session.connected_at);
  const last = sessionLastSeenMs(session) ?? now;
  const plausible = started !== null && started <= last + CLOCK_AHEAD_MS && now - started <= CLOCK_MAX_MS;
  return {
    started: plausible ? started : null,
    label: firstLine(session.session_info) ?? tr("연결되지 않은 레인 실행 중", "Unlinked lane running"),
    kind: "run",
    role: session.provider,
  };
}

/** Shared builder for the top-bar `RunningNow` badge across all screens. */
export function buildRunningNowTasks({
  agents,
  sessions,
  sessionsFailed,
  turns,
  tr,
  isKo,
}: {
  agents: Agent[];
  sessions: DispatchedSession[];
  sessionsFailed: boolean;
  turns?: Map<string, HomeTurn>;
  tr: Tr;
  isKo: boolean;
}): RunningNowTask[] {
  const realEntries = classifyAgents(
    agents.filter((agent) => !agent.id.startsWith("dispatched:") && (!isTestAgent(agent) || agent.status === "working")),
    sessions,
    sessionsFailed,
  );
  const tasks: RunningNowTask[] = [];

  for (const entry of realEntries) {
    if (entry.state !== "working" && entry.state !== "background") continue;
    const turn = turns?.get(entry.agent.id);
    const events = turn?.tool_events ?? [];
    const doneSteps = events.filter((ev) => ev.status === "success").length;
    tasks.push({
      id: `agent:${entry.agent.id}`,
      title: agentName(entry.agent, isKo),
      step: turn?.current_tool_line ?? entry.info ?? agentStateLabel(entry.state, tr),
      status: entry.state === "working" ? "running" : "waiting",
      ...(events.length > 0 ? { done: doneSteps, total: events.length } : {}),
    });
  }

  for (const session of sessions) {
    if (session.linked_agent_id || session.status === "disconnected") continue;
    const isRunning = TURN_STATUSES.has(session.status);
    const isWaiting = session.status === "awaiting_bg" || session.status === "awaiting_user";
    if (!isRunning && !isWaiting) continue;
    tasks.push({
      id: `session:${session.session_key}`,
      title: unlinkedSessionTitle(session),
      step: firstLine(session.session_info) ?? (isRunning ? tr("미연결 레인 실행 중", "Unlinked lane running") : tr("백그라운드 대기", "Waiting")),
      status: isRunning ? "running" : "waiting",
    });
  }

  return tasks;
}

// ── To do & 3-Second Triage ──

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

export type TriageAlertKind = "quota" | "stale-session" | "stalled-agent" | "health";

export interface TriageAlert {
  key: string;
  kind: TriageAlertKind;
  tone: "warn" | "err";
  chip: string;
  title: string;
  detail: string;
  to: string;
  sessionKey?: string;
  reasons?: string[];
}

const FAILED_STATUS = /(^|_)failed$/;
const BENIGN_BLOCKED_PREFIXES = ["ci:waiting", "ci:running", "ci:rerunning", "ci:rework", "pr:creating"];

function needsPerson({ review_status, blocked_reason }: KanbanCard): boolean {
  const reason = blocked_reason ?? "";
  return review_status === "dilemma_pending" || (reason.trim() !== "" && !BENIGN_BLOCKED_PREFIXES.some((prefix) => reason.startsWith(prefix)));
}

function repoName(repo: string | null): string | null {
  return repo ? repo.split("/").pop() || repo : null;
}

const HUMAN_TODO = new Map<string, Pick<TodoItem, "kind" | "tone"> & { chip: [string, string] }>([
  ["awaiting_approval", { kind: "approve", tone: "warn", chip: ["승인 대기", "Approval"] }],
  ["decision_required", { kind: "decide", tone: "err", chip: ["판단 필요", "Decide"] }],
  ["awaiting_deploy", { kind: "deploy", tone: "info", chip: ["배포 대기", "Deploy"] }],
]);
const KIND_ORDER: TodoKind[] = ["approve", "decide", "deploy", "other"];

function formatTodoIssueTitle(rawTitle: string, issueNumber?: string | number | null): string {
  const trimmed = rawTitle.trim();
  const leading = trimmed.match(/^#(\d+)\b[\s:·-]*/);
  const issue = issueNumber != null && String(issueNumber) !== "" ? String(issueNumber) : leading?.[1] ?? null;
  if (!issue) return trimmed;
  let clean = trimmed;
  const prefixRe = new RegExp(`^#${issue}\\b[\\s:·-]*`);
  while (prefixRe.test(clean)) {
    clean = clean.replace(prefixRe, "");
  }
  return clean ? `#${issue} ${clean}` : `#${issue}`;
}

export function buildTodoItems({ cards, campaigns, documents = [], legacy, tr, isKo }: {
  cards: KanbanCard[];
  campaigns: ServerCampaignSummary[] | null;
  documents?: ServerCampaignDocument[];
  legacy?: ReadonlyArray<HomeLegacySummary> | null;
  tr: Tr;
  isKo: boolean;
}): TodoItem[] {
  const items: TodoItem[] = [];
  const ago = (value: string | null | undefined) => {
    const time = coerceTimestampMs(value);
    return time === null ? null : formatRelativeTime(time, isKo);
  };
  for (const card of cards.filter(isOpenKanbanCard)) {
    const cleanCardTitle = cardTitle(card.title);
    const base = {
      key: `card:${card.id}`,
      title: card.github_issue_number ? formatTodoIssueTitle(cleanCardTitle, card.github_issue_number) : cleanCardTitle,
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
        title: formatTodoIssueTitle(cardTitle(spec?.title ?? node.id), issue),
        detail: join(definition.title, typeof waitedAt === "string" && ago(waitedAt)),
        to: `/campaigns${campaignLocationQuery({ mode: "server", campaign: view.campaign_id, node: node.id })}`,
        agentId: null,
      });
    }
  }
  for (const campaign of legacy ?? []) {
    if (campaign.status === "completed" || campaign.status === "cancelled") continue;
    for (const node of campaign.nodes) {
      if (!node.id || node.status === "completed" || node.status === "skipped") continue;
      if (!isLedgerHumanApprovalPending(node.stage, node.next_action, node.blocker)) continue;
      const issue = node.issue_url?.match(/\/issues\/(\d+)/)?.[1];
      items.push({
        key: `legacy:${campaign.id}:${node.id}`,
        kind: "approve",
        tone: "warn",
        chip: tr("승인 대기", "Approval"),
        title: formatTodoIssueTitle(cardTitle(node.title ?? node.id), issue),
        detail: join(campaign.title, ago(node.updated_at)),
        to: `/campaigns${campaignLocationQuery({ mode: "legacy", campaign: campaign.id, node: node.id })}`,
        agentId: node.assignee ?? null,
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

/** Formats epoch ms explicitly in KST (`Asia/Seoul`, +9). */
export function formatKstReset(resetAtMs: number | null | undefined, tr: Tr): string | null {
  if (resetAtMs == null || !Number.isFinite(resetAtMs) || resetAtMs <= 0) return null;
  const parts = new Intl.DateTimeFormat("en-CA", {
    timeZone: "Asia/Seoul",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).formatToParts(new Date(resetAtMs));
  const pick = (type: string) => parts.find((p) => p.type === type)?.value ?? "";
  const text = `${Number(pick("month"))}/${Number(pick("day"))} ${pick("hour")}:${pick("minute")} KST`;
  return tr(`${text} 초기화`, `Resets ${text}`);
}

/** Human-readable, aggregated explanation of why `/api/health/detail` reports degraded or unhealthy. */
export function describeHealthReasons(health: HomeHealth | null | undefined, tr: Tr): string[] {
  if (!health) return [];
  const isKo = tr("ko", "en") === "ko";
  const reasons: string[] = [];
  if (health.db === false) {
    reasons.push(tr("PostgreSQL 데이터베이스 연결 실패", "PostgreSQL database disconnected"));
  }
  const rawDegraded = [...(health.degraded_reasons ?? []), ...(health.startup_degraded_reasons ?? [])];
  for (const group of aggregateDegradedReasons(rawDegraded, isKo)) {
    reasons.push(group.summary);
  }
  for (const provider of health.providers ?? []) {
    if (!provider.connected) {
      reasons.push(tr(`${provider.name} Discord 봇 연결 끊김`, `${provider.name} Discord bot disconnected`));
    } else if (provider.restart_pending) {
      reasons.push(tr(`${provider.name} 재시작 대기 중`, `${provider.name} restart pending`));
    }
  }
  if (health.dispatch_outbox) {
    const { permanent_failures, retrying, pending } = health.dispatch_outbox;
    if (permanent_failures > 0) {
      reasons.push(tr(`전달 영구 실패 ${permanent_failures}건`, `${permanent_failures} permanent outbox failures`));
    }
    if (retrying > 0) {
      reasons.push(tr(`전달 재시도 중 ${retrying}건`, `${retrying} outbox deliveries retrying`));
    } else if (pending > 5) {
      reasons.push(tr(`전달 대기 ${pending}건`, `${pending} outbox deliveries pending`));
    }
  }
  if ((health.queue_depth ?? 0) >= 3) {
    reasons.push(tr(`채널 대기열 적체 ${health.queue_depth}건`, `Queue backlog: ${health.queue_depth}`));
  }
  if ((health.mailbox_observation_failures?.length ?? 0) > 0) {
    reasons.push(tr(`메일박스 관측 실패 ${health.mailbox_observation_failures!.length}건`, `${health.mailbox_observation_failures!.length} mailbox observation failures`));
  }
  return [...new Set(reasons)];
}

/** Dynamic header link for `HomeTodoCard` based on campaign/queue to-dos and triage alerts. */
export function todoHeaderLink(
  items: ReadonlyArray<TodoItem>,
  alerts: ReadonlyArray<TriageAlert>,
  tr: Tr,
): { to: string; label: string } {
  if (items.length > 0 && items.every((item) => item.to.startsWith("/campaigns?mode=") && !item.key.startsWith("card:"))) {
    return { to: CAMPAIGNS_PATH, label: tr("캠페인에서 보기", "Open campaigns") };
  }
  if (items.length === 0 && alerts.length > 0) {
    const first = alerts[0].kind;
    if (first === "quota") return { to: "/stats", label: tr("통계에서 보기", "Open stats") };
    if (first === "health") return { to: "/settings?category=connections", label: tr("연결 상태 보기", "View connections") };
    if (first === "stale-session" || first === "stalled-agent") {
      return { to: "/agents", label: tr("에이전트에서 보기", "Open agents") };
    }
  }
  return { to: QUEUE_PATH, label: tr("대기열에서 보기", "Open queue") };
}

/** Promoted 3-second triage alerts: quota danger/warning, degraded server, stalled linked agents, and stalled unlinked sessions. */
export function buildTriageAlerts({
  usageRows,
  rule,
  sessions,
  agentEntries,
  reconciledKeys,
  health,
  tr,
  isKo,
  now = Date.now(),
}: {
  usageRows: UsageRow[];
  rule: ReturnType<typeof autoPauseRule>;
  sessions: DispatchedSession[];
  agentEntries?: ReadonlyArray<HomeAgent>;
  reconciledKeys?: ReadonlySet<string>;
  health: HomeHealth | null;
  tr: Tr;
  isKo: boolean;
  now?: number;
}): TriageAlert[] {
  const alerts: TriageAlert[] = [];
  const pauseThreshold = rule?.enabled && rule.pct !== null ? rule.pct : 99;

  for (const row of usageRows) {
    if (row.pct === null || row.unmeasured || row.pct < 80) continue;
    const paused = row.pct >= pauseThreshold || row.pct >= 99;
    const providerTitle = [row.group, row.label].filter(Boolean).join(" ");
    const resetText = formatKstReset(row.resetAtMs, tr) ?? tr("초기화 시각 미확인", "Reset time unavailable");
    alerts.push({
      key: `quota:${row.group ?? ""}:${row.label}`,
      kind: "quota",
      tone: paused ? "err" : "warn",
      chip: paused ? tr("자동 진행 멈춤", "Auto-run paused") : tr("쿼터 주의", "Quota warn"),
      title: `${providerTitle} ${row.pct}%`,
      detail: join(
        paused ? tr("한도 임계치 도달로 새 자동 배치가 멈췄어요", "New auto-dispatches are paused") : tr("한도 소진 임박", "Approaching quota limit"),
        resetText,
      ),
      to: "/stats",
    });
  }

  if (health && (health.status === "degraded" || health.status === "unhealthy")) {
    const reasons = describeHealthReasons(health, tr);
    const isErr = health.status === "unhealthy";
    alerts.push({
      key: `health:${health.status}`,
      kind: "health",
      tone: isErr ? "err" : "warn",
      chip: isErr ? tr("서버 장애", "Server down") : tr("서버 주의", "Server degraded"),
      title: isErr ? tr("서버가 장애 상태예요", "Server is unhealthy") : tr("서버가 주의 상태예요", "Server is degraded"),
      detail: reasons[0] ?? tr("원인 보기에서 상세 상태를 확인해요", "Inspect detailed health reasons"),
      to: "/settings?category=connections",
      reasons,
    });
  }

  for (const entry of agentEntries ?? []) {
    if (!entry.stalled || entry.unlinkedSession) continue;
    alerts.push({
      key: `stuck:${entry.agent.id}`,
      kind: "stalled-agent",
      tone: "warn",
      chip: tr("막힘 의심", "Likely stuck"),
      title: agentName(entry.agent, isKo),
      detail: join(
        entry.info,
        tr("도구를 실행한 뒤 응답이 오래 없어요", "No response for an extended period after tool run"),
      ),
      to: `/agents?agent=${encodeURIComponent(entry.agent.id)}`,
    });
  }

  for (const session of sessions) {
    if (reconciledKeys?.has(session.session_key)) continue;
    if (!isStaleUnlinkedSession(session, now)) continue;
    const last = sessionLastSeenMs(session);
    const ageLabel = last !== null && now - last >= STALE_SESSION_IDLE_MS
      ? tr("7일 넘게 멈춤", "Idle 7+ days")
      : tr("장기 멈춤 턴", "Stale turn");
    alerts.push({
      key: `stale:${session.session_key}`,
      kind: "stale-session",
      tone: "warn",
      chip: tr("멈춘 세션", "Stalled"),
      title: `${unlinkedSessionTitle(session)} · ${ageLabel}`,
      detail: join(
        session.provider,
        last !== null ? tr(`마지막 신호 ${formatRelativeTime(last, isKo)}`, `Last signal ${formatRelativeTime(last, isKo)}`) : null,
      ),
      to: "/agents",
      sessionKey: session.session_key,
    });
  }

  return alerts;
}

/** `partial`: the source answered but left part of its to-dos unread. */
export type SourceState = "loading" | "ok" | "partial" | "failed";

/** The to-do list is empty only when every source answered in full; a failed or partial source leaves it partial. */
export function todoStatus(count: number, sources: SourceState[]): "loading" | "empty" | "partial" | "ready" {
  if (sources.some((source) => source === "failed" || source === "partial")) return "partial";
  if (count > 0) return "ready";
  return sources.includes("loading") ? "loading" : "empty";
}

// ── System ──

export type HomeHealth = HealthResponse & {
  mailbox_observation_failures?: unknown[];
  startup_degraded_reasons?: string[];
};

export interface HomeMementoStatus {
  connected: boolean;
  status?: "connected" | "disconnected" | "unknown" | "unconfigured";
}

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

export function buildSystemRows({ health, healthFailed, legacy, documents, memento, tr }: {
  health: HomeHealth | null;
  healthFailed: boolean;
  legacy: ReadonlyArray<HomeLegacySummary> | null;
  documents: ServerCampaignDocument[];
  memento?: HomeMementoStatus | null;
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

  const mementoRow: Pick<SystemRow, "value" | "tone"> = !memento || memento.status === "unknown"
    ? { value: unknown, tone: "neutral" }
    : memento.connected
      ? { value: tr("연결됨", "Connected"), tone: "ok" }
      : memento.status === "unconfigured"
        ? { value: tr("미설정", "Not configured"), tone: "neutral" }
        : { value: tr("연결 끊김", "Disconnected"), tone: "warn" };

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

  const queueDepth = detail?.mailbox_observation_failures?.length ? null : detail?.queue_depth;
  return [
    { key: "server", label: tr("서버", "Server"), value: server, tone: serverTone },
    { key: "discord", label: tr("Discord 봇", "Discord bots"), ...discord },
    { key: "memento", label: "Memento", ...mementoRow },
    { key: "campaign", label: tr("캠페인 실행", "Campaign runs"), value: campaignParts.join(" · ") || unknown, tone: campaignTone },
    {
      key: "queue",
      label: tr("대기열", "Queue"),
      value: queueDepth == null ? unknown : tr(`${queueDepth}건`, `${queueDepth}`),
      tone: queueDepth == null || queueDepth === 0 ? "neutral" : queueDepth >= 3 ? "err" : "warn",
    },
  ];
}

export function systemNeedsAttention(rows: SystemRow[], tr: Tr): boolean {
  const unknown = tr("확인 불가", "Unknown");
  return rows.some((row) => row.tone === "warn" || row.tone === "err" || (row.key !== "memento" && row.value === unknown));
}

// ── Campaigns ──

export type StageSegment = "done" | "now" | "fail" | "todo";

export interface CampaignRow {
  key: string;
  live: boolean;
  title: string;
  status: string;
  tone: HomeTone;
  to: string;
  segments: StageSegment[] | null;
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

function legacyCampaignRow(campaign: HomeLegacySummary, tr: Tr): CampaignRow {
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

export function buildCampaignRows({ legacy, summaries, documents, tr, limit }: {
  legacy: ReadonlyArray<HomeLegacySummary> | null;
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
  finished: boolean;
}

function sameLocalDay(left: number, right: number): boolean {
  return new Date(left).toDateString() === new Date(right).toDateString();
}

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
  group: string | null;
  label: string;
  pct: number | null;
  unmeasured: boolean;
  stale: boolean;
  measuredAt: number | null;
  note: string | null;
  resetAtMs?: number | null;
}

const unmeasuredBucket = (provider: string | undefined, bucket: string) => provider === "gemini" && (bucket === "rpm" || bucket === "rpd");

export function buildUsageRows(raw: RawUsageProvider[], isKo: boolean): UsageRow[] {
  return raw.flatMap((source) => transformRLProviders([source]).flatMap((provider): UsageRow[] => {
    const fetchedAt = coerceTimestampMs(source.fetched_at ?? null);
    const name = provider.profileId === "default" ? provider.provider : `${provider.provider} ${provider.profileId}`;
    if (provider.unsupported || provider.buckets.length === 0) {
      return [{ group: null, label: name, pct: null, unmeasured: false, stale: provider.stale, measuredAt: null, note: provider.reason, resetAtMs: null }];
    }
    return provider.buckets.map((bucket) => {
      const unmeasured = unmeasuredBucket(source.provider, bucket.id);
      const pct = unmeasured ? null : bucket.utilization;
      const label = formatRateLimitWindowLabel(bucket.label, isKo);
      return {
        group: name,
        label,
        pct,
        unmeasured,
        stale: provider.stale,
        measuredAt: pct === null ? null : fetchedAt,
        note: null,
        resetAtMs: unmeasured ? null : bucket.resetAtMs,
      };
    });
  }));
}

export function autoPauseRule(config: RuntimeConfigResponse | null): { enabled: boolean; pct: number | null } | null {
  const enabled = config?.current?.dispatchRateLimitGateEnabled;
  if (typeof enabled !== "boolean") return null;
  const pct = config?.current?.dispatchRateLimitGateDangerPct;
  return { enabled, pct: typeof pct === "number" ? pct : null };
}

export function pauseMeterScale(rule: ReturnType<typeof autoPauseRule>): { warnAt: number; dangerAt: number } {
  if (!rule?.enabled || rule.pct === null || rule.pct >= 100) return { warnAt: 0.8, dangerAt: 0.99 };
  const dangerAt = Math.min(0.99, Math.max(0, rule.pct) / 100);
  return { warnAt: Math.min(0.8, dangerAt), dangerAt };
}

export function usageDetail(row: UsageRow, rule: ReturnType<typeof autoPauseRule>, tr: Tr): string | undefined {
  if (row.unmeasured) return tr("측정 안 됨", "Not measured");
  if (row.pct === null) return row.note ?? tr("확인 불가", "Unknown");
  const old = row.stale && row.measuredAt !== null ? tr(`${formatClock(row.measuredAt)} 값`, `as of ${formatClock(row.measuredAt)}`) : null;
  const shown = Boolean(rule?.enabled && rule.pct !== null && rule.pct < 100);
  const paused = !shown && rule?.enabled && rule.pct !== null && row.pct >= rule.pct ? tr("자동 진행 멈춤 기준 이상", "At or past the auto-run pause") : null;
  const kstReset = row.pct >= 80 ? formatKstReset(row.resetAtMs, tr) : null;
  return join(old, paused, kstReset) || undefined;
}
