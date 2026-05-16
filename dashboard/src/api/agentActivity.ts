import type { DispatchedSession, DispatchDeliveryEvent, TaskDispatch } from "../types";
import { readCachedSnapshot, request, type CachedApiSnapshot, type RequestOptions } from "./httpClient";

export async function getCardTranscripts(
  cardId: string,
  limit = 10,
): Promise<SessionTranscript[]> {
  const data = await request<{ transcripts: SessionTranscript[] }>(
    `/api/pipeline/cards/${cardId}/transcripts?limit=${limit}`,
  );
  return data.transcripts;
}

export async function getTaskDispatches(filters?: {
  status?: string;
  from_agent_id?: string;
  to_agent_id?: string;
  limit?: number;
}): Promise<TaskDispatch[]> {
  const params = new URLSearchParams();
  if (filters?.status) params.set("status", filters.status);
  if (filters?.from_agent_id)
    params.set("from_agent_id", filters.from_agent_id);
  if (filters?.to_agent_id) params.set("to_agent_id", filters.to_agent_id);
  if (filters?.limit) params.set("limit", String(filters.limit));
  const q = params.toString();
  const data = await request<{ dispatches: TaskDispatch[] }>(
    `/api/dispatches${q ? `?${q}` : ""}`,
  );
  return data.dispatches;
}

export interface DispatchDeliveryEventsResponse {
  dispatch_id: string;
  events: DispatchDeliveryEvent[];
}

export async function getDispatchDeliveryEvents(
  dispatchId: string,
): Promise<DispatchDeliveryEventsResponse> {
  return request(`/api/dispatches/${encodeURIComponent(dispatchId)}/events`);
}

// ── Dispatched Sessions ──

export async function getDispatchedSessions(
  includeMerged = false,
): Promise<DispatchedSession[]> {
  const q = includeMerged ? "?includeMerged=1" : "";
  const data = await request<{ sessions: DispatchedSession[] }>(
    `/api/dispatched-sessions${q}`,
  );
  return data.sessions;
}

export async function assignDispatchedSession(
  id: string,
  patch: Partial<DispatchedSession>,
): Promise<DispatchedSession> {
  return request(`/api/dispatched-sessions/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

// ── Agent Cron Jobs ──

export interface CronSchedule {
  kind: "every" | "cron" | "at";
  everyMs?: number;
  cron?: string;
  atMs?: number;
}

export interface CronJobState {
  lastStatus?: string;
  lastRunAtMs?: number;
  lastDurationMs?: number;
  nextRunAtMs?: number;
}

export interface CronJob {
  id: string;
  name: string;
  description_ko?: string;
  enabled: boolean;
  schedule: CronSchedule;
  state?: CronJobState;
}

export async function getAgentCron(agentId: string): Promise<CronJob[]> {
  const data = await request<{ jobs: CronJob[] }>(
    `/api/agents/${agentId}/cron`,
  );
  return data.jobs;
}

export async function getAgentDispatchedSessions(
  agentId: string,
): Promise<DispatchedSession[]> {
  const data = await request<{ sessions: DispatchedSession[] }>(
    `/api/agents/${agentId}/dispatched-sessions`,
  );
  return data.sessions;
}

export type SessionTranscriptEventKind =
  | "user"
  | "assistant"
  | "thinking"
  | "tool_use"
  | "tool_result"
  | "result"
  | "error"
  | "task"
  | "system";

export interface SessionTranscriptEvent {
  kind: SessionTranscriptEventKind;
  tool_name?: string | null;
  summary?: string | null;
  content: string;
  status?: string | null;
  is_error: boolean;
}

export interface SessionTranscript {
  id: number;
  turn_id: string;
  session_key: string | null;
  channel_id: string | null;
  agent_id: string | null;
  provider: string | null;
  dispatch_id: string | null;
  kanban_card_id: string | null;
  dispatch_title: string | null;
  card_title: string | null;
  github_issue_number: number | null;
  user_message: string;
  assistant_message: string;
  events: SessionTranscriptEvent[];
  duration_ms: number | null;
  created_at: string;
}

export async function getAgentTranscripts(
  agentId: string,
  limit = 8,
): Promise<SessionTranscript[]> {
  const data = await request<{ transcripts: SessionTranscript[] }>(
    `/api/agents/${agentId}/transcripts?limit=${limit}`,
  );
  return data.transcripts;
}

export interface AgentTurnToolEvent {
  kind: "thinking" | "tool";
  status: "info" | "running" | "success" | "error";
  tool_name?: string | null;
  summary: string;
  line: string;
}

export interface AgentTurnState {
  agent_id: string;
  status: string;
  started_at: string | null;
  updated_at: string | null;
  recent_output: string | null;
  recent_output_source: string;
  session_key: string | null;
  tmux_session: string | null;
  provider: string | null;
  thread_channel_id: string | null;
  active_dispatch_id: string | null;
  last_heartbeat: string | null;
  current_tool_line: string | null;
  prev_tool_status: string | null;
  tool_events: AgentTurnToolEvent[];
  tool_count: number;
}

export async function getAgentTurn(
  agentId: string,
): Promise<AgentTurnState> {
  return request(`/api/agents/${agentId}/turn`);
}

// ── Agent Skills ──

export interface AgentSkill {
  name: string;
  description: string;
  shared: boolean;
}

export interface AgentSkillsResponse {
  skills: AgentSkill[];
  sharedSkills: AgentSkill[];
  totalCount: number;
}

export async function getAgentSkills(
  agentId: string,
): Promise<AgentSkillsResponse> {
  return request(`/api/agents/${agentId}/skills`);
}

// ── Agent Quality ──

export interface AgentQualityWindow {
  days: number;
  sampleSize: number;
  measurementUnavailable: boolean;
  measurementLabel: string | null;
  turnSampleSize: number;
  turnSuccessRate: number | null;
  reviewSampleSize: number;
  reviewPassRate: number | null;
}

export interface AgentQualityDailyRecord {
  agentId: string;
  day: string;
  provider: string | null;
  channelId: string | null;
  turnSuccessCount: number;
  turnErrorCount: number;
  reviewPassCount: number;
  reviewFailCount: number;
  turnSampleSize: number;
  reviewSampleSize: number;
  sampleSize: number;
  turnSuccessRate: number | null;
  reviewPassRate: number | null;
  rolling7d: AgentQualityWindow;
  rolling30d: AgentQualityWindow;
  computedAt: string;
}

export interface AgentQualitySummary {
  generatedAt: string;
  agentId: string;
  latest: AgentQualityDailyRecord | null;
  /** #1102: alias for `latest` — DoD-mandated field. */
  current?: AgentQualityDailyRecord | null;
  daily: AgentQualityDailyRecord[];
  /** #1102: last 7 days of daily rows (newest-first). */
  trend7d?: AgentQualityDailyRecord[];
  /** #1102: last 30 days of daily rows (newest-first). */
  trend30d?: AgentQualityDailyRecord[];
  /** #1102: true when `daily` is synthesized from agent_quality_event. */
  fallbackFromEvents?: boolean;
}

export interface AgentQualityRankingEntry {
  rank: number;
  agentId: string;
  agentName: string | null;
  provider: string | null;
  channelId: string | null;
  latestDay: string;
  rolling7d: AgentQualityWindow;
  rolling30d: AgentQualityWindow;
  /** #1102: value of the chosen (metric, window). null when unavailable. */
  metricValue?: number | null;
}

export type AgentQualityRankingMetric = "turn_success_rate" | "review_pass_rate";
export type AgentQualityRankingWindow = "7d" | "30d";

export interface AgentQualityRankingResponse {
  generatedAt: string;
  /** #1102 */
  metric?: AgentQualityRankingMetric;
  /** #1102 */
  window?: AgentQualityRankingWindow;
  /** #1102: sample_size threshold applied to the requested window. */
  minSampleSize?: number;
  agents: AgentQualityRankingEntry[];
}

export async function getAgentQuality(
  agentId: string,
  days = 30,
  limit = 30,
): Promise<AgentQualitySummary> {
  return request(`/api/agents/${encodeURIComponent(agentId)}/quality?days=${days}&limit=${limit}`);
}

export async function getAgentQualityRanking(
  limit = 20,
  metric?: AgentQualityRankingMetric,
  window?: AgentQualityRankingWindow,
): Promise<AgentQualityRankingResponse> {
  const params = new URLSearchParams({ limit: String(limit) });
  if (metric) params.set("metric", metric);
  if (window) params.set("window", window);
  return request(`/api/agents/quality/ranking?${params.toString()}`);
}

// ── Agent Timeline ──

export interface TimelineEvent {
  id: string;
  source: "dispatch" | "session" | "kanban";
  type: string;
  title: string;
  status: string;
  timestamp: number;
  duration_ms: number | null;
  detail?: Record<string, unknown>;
}

export async function getAgentTimeline(
  agentId: string,
  limit = 30,
): Promise<TimelineEvent[]> {
  const data = await request<{ events: TimelineEvent[] }>(
    `/api/agents/${agentId}/timeline?limit=${limit}`,
  );
  return data.events;
}

// ── Discord Bindings ──

export interface DiscordBinding {
  agentId: string;
  channelId: string;
  counterModelChannelId?: string;
  provider?: string;
  source?: string;
}

export async function getDiscordBindings(): Promise<DiscordBinding[]> {
  const data = await request<{ bindings: DiscordBinding[] }>(
    "/api/discord/bindings",
  );
  return data.bindings;
}

export interface DiscordChannelInfo {
  id: string;
  guild_id?: string | null;
  name?: string | null;
  parent_id?: string | null;
  type?: number | null;
}

export async function getDiscordChannelInfo(
  channelId: string,
): Promise<DiscordChannelInfo> {
  return request(`/api/discord/channels/${channelId}`);
}

export interface GitHubRepoOption {
  nameWithOwner: string;
  updatedAt: string;
  isPrivate: boolean;
  viewerPermission?: string;
}

export interface GitHubReposResponse {
  viewer_login: string;
  repos: GitHubRepoOption[];
}

export function getCachedGitHubRepos(): CachedApiSnapshot<GitHubReposResponse> | null {
  return readCachedSnapshot<GitHubReposResponse>("/api/github-repos");
}

export async function getGitHubRepos(opts?: RequestOptions): Promise<GitHubReposResponse> {
  return request("/api/github-repos", opts);
}

// ── Cron Jobs (global) ──

export interface CronJobGlobal {
  id: string;
  name: string;
  agentId?: string;
  enabled: boolean;
  schedule: CronSchedule;
  state?: CronJobState;
  discordChannelId?: string;
  description_ko?: string;
}

export async function getCronJobs(): Promise<CronJobGlobal[]> {
  const data = await request<{ jobs: CronJobGlobal[] }>("/api/cron-jobs");
  return data.jobs;
}

// ── Machine Status ──

export interface MachineStatus {
  name: string;
  online: boolean;
  lastChecked: number;
}

export async function getMachineStatus(): Promise<MachineStatus[]> {
  const data = await request<{ machines: MachineStatus[] }>(
    "/api/machine-status",
  );
  return data.machines;
}

// ── Activity Heatmap ──

export interface HeatmapData {
  hours: Array<{
    hour: number;
    agents: Record<string, number>; // agentId → event count
  }>;
  date: string;
}

export async function getActivityHeatmap(date?: string): Promise<HeatmapData> {
  const q = date ? `?date=${date}` : "";
  return request(`/api/activity-heatmap${q}`);
}
