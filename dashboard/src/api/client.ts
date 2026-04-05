import type {
  Agent,
  AuditLogEntry,
  Department,
  KanbanCard,
  KanbanRepoSource,
  Office,
  DispatchedSession,
  DashboardStats,
  RoundTableMeeting,
  SkillCatalogEntry,
  TaskDispatch,
} from "../types";

export type { AuditLogEntry, KanbanCard, KanbanRepoSource } from "../types";

const BASE = "";
const REQUEST_TIMEOUT_MS = 15_000;
const MAX_RETRIES = 2;
const INITIAL_BACKOFF_MS = 500;

// ── GET deduplication ──
const inflightGets = new Map<string, Promise<unknown>>();

// ── Global error listener for toast integration ──
type ApiErrorListener = (url: string, error: Error) => void;
let apiErrorListener: ApiErrorListener | null = null;
export function onApiError(listener: ApiErrorListener | null): void {
  apiErrorListener = listener;
}

function isRetryable(status: number): boolean {
  return status === 0 || status === 408 || status === 429 || status >= 500;
}

async function request<T>(
  url: string,
  opts?: RequestInit,
): Promise<T> {
  const method = opts?.method?.toUpperCase() ?? "GET";
  const isGet = method === "GET";

  // Deduplicate identical concurrent GET requests
  if (isGet) {
    const existing = inflightGets.get(url);
    if (existing) return existing as Promise<T>;
  }

  const execute = async (): Promise<T> => {
    let lastError: Error | null = null;
    for (let attempt = 0; attempt <= MAX_RETRIES; attempt++) {
      if (attempt > 0) {
        const delay = INITIAL_BACKOFF_MS * 2 ** (attempt - 1);
        await new Promise((r) => setTimeout(r, delay));
      }
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
      try {
        const res = await fetch(`${BASE}${url}`, {
          credentials: "include",
          ...opts,
          signal: controller.signal,
          headers: {
            "Content-Type": "application/json",
            ...opts?.headers,
          },
        });
        clearTimeout(timer);
        if (!res.ok) {
          const err = await res.json().catch(() => ({ error: "unknown" }));
          const error = new Error(err.error || `HTTP ${res.status}`);
          if (isGet && isRetryable(res.status) && attempt < MAX_RETRIES) {
            lastError = error;
            continue;
          }
          throw error;
        }
        return await res.json();
      } catch (e) {
        clearTimeout(timer);
        const error = e instanceof Error ? e : new Error(String(e));
        if (error.name === "AbortError") {
          lastError = new Error(`Request timeout: ${url}`);
          if (isGet && attempt < MAX_RETRIES) continue;
        } else if (isGet && attempt < MAX_RETRIES && !error.message.startsWith("HTTP ")) {
          lastError = error;
          continue;
        }
        throw lastError ?? error;
      }
    }
    throw lastError ?? new Error(`Request failed: ${url}`);
  };

  const promise = execute().finally(() => {
    if (isGet) inflightGets.delete(url);
  });

  if (isGet) inflightGets.set(url, promise);

  return promise.catch((e) => {
    const error = e instanceof Error ? e : new Error(String(e));
    apiErrorListener?.(url, error);
    throw error;
  });
}

// Auth
export async function getSession(): Promise<{ ok: boolean; csrf_token: string }> {
  return request("/api/auth/session");
}

// ── Offices ──

export async function getOffices(): Promise<Office[]> {
  const data = await request<{ offices: Office[] }>("/api/offices");
  return data.offices;
}

export async function createOffice(
  office: Partial<Office>,
): Promise<Office> {
  return request("/api/offices", {
    method: "POST",
    body: JSON.stringify(office),
  });
}

export async function updateOffice(
  id: string,
  patch: Partial<Office>,
): Promise<Office> {
  return request(`/api/offices/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

export async function deleteOffice(id: string): Promise<void> {
  await request(`/api/offices/${id}`, { method: "DELETE" });
}

export async function addAgentToOffice(
  officeId: string,
  agentId: string,
  departmentId?: string | null,
): Promise<void> {
  await request(`/api/offices/${officeId}/agents`, {
    method: "POST",
    body: JSON.stringify({ agent_id: agentId, department_id: departmentId ?? null }),
  });
}

export async function removeAgentFromOffice(
  officeId: string,
  agentId: string,
): Promise<void> {
  await request(`/api/offices/${officeId}/agents/${agentId}`, {
    method: "DELETE",
  });
}

export async function updateOfficeAgent(
  officeId: string,
  agentId: string,
  patch: { department_id?: string | null },
): Promise<void> {
  await request(`/api/offices/${officeId}/agents/${agentId}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

export async function batchAddAgentsToOffice(
  officeId: string,
  agentIds: string[],
): Promise<void> {
  await request(`/api/offices/${officeId}/agents/batch`, {
    method: "POST",
    body: JSON.stringify({ agent_ids: agentIds }),
  });
}

// ── Agents ──

export async function getAgents(officeId?: string): Promise<Agent[]> {
  const q = officeId ? `?officeId=${officeId}` : "";
  const data = await request<{ agents: Agent[] }>(`/api/agents${q}`);
  return data.agents;
}

export async function createAgent(
  agent: Partial<Agent> & { office_id?: string },
): Promise<Agent> {
  return request("/api/agents", {
    method: "POST",
    body: JSON.stringify(agent),
  });
}

export async function updateAgent(
  id: string,
  patch: Partial<Agent>,
): Promise<Agent> {
  return request(`/api/agents/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

export async function deleteAgent(id: string): Promise<void> {
  await request(`/api/agents/${id}`, { method: "DELETE" });
}

export interface AgentOfficeMembership extends Office {
  assigned: boolean;
  office_department_id?: string | null;
  joined_at?: number | null;
}

export async function getAgentOffices(agentId: string): Promise<AgentOfficeMembership[]> {
  const data = await request<{ offices: AgentOfficeMembership[] }>(`/api/agents/${agentId}/offices`);
  return data.offices;
}

// ── Audit Logs ──

export async function getAuditLogs(
  limit = 20,
  filter?: { entityType?: string; entityId?: string },
): Promise<AuditLogEntry[]> {
  const params = new URLSearchParams();
  params.set("limit", String(limit));
  if (filter?.entityType) params.set("entityType", filter.entityType);
  if (filter?.entityId) params.set("entityId", filter.entityId);
  const data = await request<{ logs: AuditLogEntry[] }>(`/api/audit-logs?${params.toString()}`);
  return data.logs;
}

// ── Departments ──

export async function getDepartments(officeId?: string): Promise<Department[]> {
  const q = officeId ? `?officeId=${officeId}` : "";
  const data = await request<{ departments: Department[] }>(
    `/api/departments${q}`,
  );
  return data.departments;
}

export async function createDepartment(
  dept: Partial<Department>,
): Promise<Department> {
  return request("/api/departments", {
    method: "POST",
    body: JSON.stringify(dept),
  });
}

export async function updateDepartment(
  id: string,
  patch: Partial<Department>,
): Promise<Department> {
  return request(`/api/departments/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

export async function deleteDepartment(id: string): Promise<void> {
  await request(`/api/departments/${id}`, { method: "DELETE" });
}

// ── Settings ──

export async function getSettings(): Promise<Record<string, unknown>> {
  return request("/api/settings");
}

export async function saveSettings(
  settings: Record<string, unknown>,
): Promise<void> {
  await request("/api/settings", {
    method: "PUT",
    body: JSON.stringify(settings),
  });
}

// ── Runtime Config ──

export interface RuntimeConfigResponse {
  current: Record<string, number>;
  defaults: Record<string, number>;
}

export async function getRuntimeConfig(): Promise<RuntimeConfigResponse> {
  return request("/api/settings/runtime-config");
}

export async function saveRuntimeConfig(
  patch: Record<string, number>,
): Promise<{ ok: boolean; config: Record<string, number> }> {
  return request("/api/settings/runtime-config", {
    method: "PUT",
    body: JSON.stringify(patch),
  });
}

// ── Dispatches ──

export async function createDispatch(body: {
  kanban_card_id: string;
  to_agent_id: string;
  title: string;
  dispatch_type?: string;
}): Promise<{ dispatch: Record<string, unknown> }> {
  return request("/api/dispatches", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

// ── Stats ──

export async function getStats(officeId?: string): Promise<DashboardStats> {
  const q = officeId ? `?officeId=${officeId}` : "";
  return request(`/api/stats${q}`);
}

// ── Kanban & Dispatches ──

export async function getKanbanCards(): Promise<KanbanCard[]> {
  const data = await request<{ cards: KanbanCard[] }>("/api/kanban-cards");
  return data.cards;
}

export async function createKanbanCard(
  card: Partial<KanbanCard> & { title: string; before_card_id?: string | null },
): Promise<KanbanCard> {
  return request("/api/kanban-cards", {
    method: "POST",
    body: JSON.stringify(card),
  });
}

export async function updateKanbanCard(
  id: string,
  patch: Partial<KanbanCard> & { before_card_id?: string | null },
): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>(`/api/kanban-cards/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
  return res.card;
}

export async function deleteKanbanCard(id: string): Promise<void> {
  await request(`/api/kanban-cards/${id}`, { method: "DELETE" });
}

export async function retryKanbanCard(
  id: string,
  payload?: { assignee_agent_id?: string | null; request_now?: boolean },
): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>(`/api/kanban-cards/${id}/retry`, {
    method: "POST",
    body: JSON.stringify(payload ?? {}),
  });
  return res.card;
}

export async function redispatchKanbanCard(
  id: string,
  payload?: { reason?: string | null },
): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>(`/api/kanban-cards/${id}/redispatch`, {
    method: "POST",
    body: JSON.stringify(payload ?? {}),
  });
  return res.card;
}

export async function patchKanbanDeferDod(
  id: string,
  payload: { items?: Array<{ label: string }>; verify?: string; unverify?: string; remove?: string },
): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>(`/api/kanban-cards/${id}/defer-dod`, {
    method: "PATCH",
    body: JSON.stringify(payload),
  });
  return res.card;
}

export async function assignKanbanIssue(payload: {
  github_repo: string;
  github_issue_number: number;
  github_issue_url?: string | null;
  title: string;
  description?: string | null;
  assignee_agent_id: string;
}): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>("/api/kanban-cards/assign-issue", {
    method: "POST",
    body: JSON.stringify(payload),
  });
  return res.card;
}

export async function getStalledCards(): Promise<KanbanCard[]> {
  return request("/api/kanban-cards/stalled");
}

export async function bulkKanbanAction(
  action: "pass" | "reset" | "cancel",
  card_ids: string[],
): Promise<{ action: string; results: Array<{ id: string; ok: boolean; error?: string }> }> {
  return request("/api/kanban-cards/bulk-action", {
    method: "POST",
    body: JSON.stringify({ action, card_ids }),
  });
}

export async function getKanbanRepoSources(): Promise<KanbanRepoSource[]> {
  const data = await request<{ repos: KanbanRepoSource[] }>("/api/kanban-repos");
  return data.repos;
}

export async function addKanbanRepoSource(repo: string): Promise<KanbanRepoSource> {
  return request("/api/kanban-repos", {
    method: "POST",
    body: JSON.stringify({ repo }),
  });
}

export async function updateKanbanRepoSource(id: string, data: { default_agent_id?: string | null }): Promise<KanbanRepoSource> {
  return request(`/api/kanban-repos/${id}`, {
    method: "PATCH",
    body: JSON.stringify(data),
  });
}

export async function deleteKanbanRepoSource(id: string): Promise<void> {
  await request(`/api/kanban-repos/${id}`, { method: "DELETE" });
}

// ── Kanban Reviews ──

export interface KanbanReview {
  id: string;
  card_id: string;
  round: number;
  original_dispatch_id: string | null;
  original_agent_id: string | null;
  original_provider: string | null;
  review_dispatch_id: string | null;
  reviewer_agent_id: string | null;
  reviewer_provider: string | null;
  verdict: string;
  items_json: string | null;
  github_comment_id: string | null;
  created_at: number;
  completed_at: number | null;
}

export async function getKanbanReviews(cardId: string): Promise<KanbanReview[]> {
  const data = await request<{ reviews: KanbanReview[] }>(`/api/kanban-cards/${cardId}/reviews`);
  return data.reviews;
}

export async function saveReviewDecisions(
  reviewId: string,
  decisions: Array<{ item_id: string; decision: "accept" | "reject" }>,
): Promise<{ review: KanbanReview }> {
  return request(`/api/kanban-reviews/${reviewId}/decisions`, {
    method: "PATCH",
    body: JSON.stringify({ decisions }),
  });
}

export async function triggerDecidedRework(reviewId: string): Promise<{ ok: boolean }> {
  return request(`/api/kanban-reviews/${reviewId}/trigger-rework`, {
    method: "POST",
  });
}

// ── Card Audit Log & Comments ──

export interface CardAuditLogEntry {
  id: number;
  card_id: string;
  from_status: string | null;
  to_status: string | null;
  source: string | null;
  result: string | null;
  created_at: string | null;
}

export interface GitHubComment {
  author: { login: string };
  body: string;
  createdAt: string;
}

export async function getCardAuditLog(cardId: string): Promise<CardAuditLogEntry[]> {
  const data = await request<{ logs: CardAuditLogEntry[] }>(`/api/kanban-cards/${cardId}/audit-log`);
  return data.logs;
}

export interface CardGitHubCommentsResult {
  comments: GitHubComment[];
  body: string;
}

export async function getCardGitHubComments(cardId: string): Promise<CardGitHubCommentsResult> {
  const data = await request<{ comments: GitHubComment[]; body?: string }>(`/api/kanban-cards/${cardId}/comments`);
  return { comments: data.comments, body: data.body ?? "" };
}

// ── Pipeline ──

export interface PipelineStageInput {
  stage_name: string;
  entry_skill?: string | null;
  provider?: string | null;
  agent_override_id?: string | null;
  timeout_minutes?: number;
  on_failure?: "fail" | "retry" | "previous" | "goto";
  on_failure_target?: string | null;
  max_retries?: number;
  skip_condition?: string | null;
  parallel_with?: string | null;
}

export async function getPipelineStages(repo: string): Promise<import("../types").PipelineStage[]> {
  const data = await request<{ stages: import("../types").PipelineStage[] }>(
    `/api/pipeline/stages?repo=${encodeURIComponent(repo)}`,
  );
  return data.stages;
}

export async function savePipelineStages(
  repo: string,
  stages: PipelineStageInput[],
): Promise<import("../types").PipelineStage[]> {
  const data = await request<{ stages: import("../types").PipelineStage[] }>(
    "/api/pipeline/stages",
    { method: "PUT", body: JSON.stringify({ repo, stages }) },
  );
  return data.stages;
}

export async function deletePipelineStages(repo: string): Promise<void> {
  await request(`/api/pipeline/stages?repo=${encodeURIComponent(repo)}`, { method: "DELETE" });
}

export async function getCardPipelineStatus(cardId: string): Promise<{
  stages: import("../types").PipelineStage[];
  history: import("../types").PipelineHistoryEntry[];
  current_stage: import("../types").PipelineStage | null;
}> {
  return request(`/api/pipeline/cards/${cardId}`);
}

export async function getTaskDispatches(filters?: {
  status?: string;
  from_agent_id?: string;
  to_agent_id?: string;
  limit?: number;
}): Promise<TaskDispatch[]> {
  const params = new URLSearchParams();
  if (filters?.status) params.set("status", filters.status);
  if (filters?.from_agent_id) params.set("from_agent_id", filters.from_agent_id);
  if (filters?.to_agent_id) params.set("to_agent_id", filters.to_agent_id);
  if (filters?.limit) params.set("limit", String(filters.limit));
  const q = params.toString();
  const data = await request<{ dispatches: TaskDispatch[] }>(`/api/dispatches${q ? `?${q}` : ""}`);
  return data.dispatches;
}

// ── Dispatched Sessions ──

export async function getDispatchedSessions(includeMerged = false): Promise<DispatchedSession[]> {
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
  const data = await request<{ jobs: CronJob[] }>(`/api/agents/${agentId}/cron`);
  return data.jobs;
}

export async function getAgentDispatchedSessions(agentId: string): Promise<DispatchedSession[]> {
  const data = await request<{ sessions: DispatchedSession[] }>(`/api/agents/${agentId}/dispatched-sessions`);
  return data.sessions;
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

export async function getAgentSkills(agentId: string): Promise<AgentSkillsResponse> {
  return request(`/api/agents/${agentId}/skills`);
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

export async function getAgentTimeline(agentId: string, limit = 30): Promise<TimelineEvent[]> {
  const data = await request<{ events: TimelineEvent[] }>(
    `/api/agents/${agentId}/timeline?limit=${limit}`,
  );
  return data.events;
}

// ── Discord Bindings ──

export interface DiscordBinding {
  agentId: string;
  channelId: string;
  channelName?: string;
  source?: string;
}

export async function getDiscordBindings(): Promise<DiscordBinding[]> {
  const data = await request<{ bindings: DiscordBinding[] }>("/api/discord-bindings");
  return data.bindings;
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

export async function getGitHubRepos(): Promise<GitHubReposResponse> {
  return request("/api/github-repos");
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
  const data = await request<{ machines: MachineStatus[] }>("/api/machine-status");
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

// ── Skill Ranking ──

export interface SkillRankingOverallRow {
  skill_name: string;
  skill_desc_ko: string;
  calls: number;
  last_used_at: number;
}

export interface SkillRankingByAgentRow {
  agent_role_id: string;
  agent_name: string;
  skill_name: string;
  skill_desc_ko: string;
  calls: number;
  last_used_at: number;
}

export interface SkillRankingResponse {
  window: string;
  overall: SkillRankingOverallRow[];
  byAgent: SkillRankingByAgentRow[];
}

export async function getSkillRanking(
  window: "7d" | "30d" | "90d" | "all" = "7d",
  limit = 20,
): Promise<SkillRankingResponse> {
  return request(`/api/skills/ranking?window=${window}&limit=${limit}`);
}

// ── GitHub Issues ──

export interface GitHubIssue {
  number: number;
  title: string;
  body: string;
  state: string;
  url: string;
  labels: Array<{ name: string; color: string }>;
  assignees: Array<{ login: string }>;
  createdAt: string;
  updatedAt: string;
}

export interface GitHubIssuesResponse {
  issues: GitHubIssue[];
  repo: string;
  error?: string;
}

// ── Streaks ──

export interface AgentStreak {
  agent_id: string;
  name: string;
  avatar_emoji: string;
  streak: number;
  last_active: string;
}

export async function getStreaks(): Promise<{ streaks: AgentStreak[] }> {
  return request("/api/streaks");
}

// ── Achievements ──

export interface Achievement {
  id: string;
  agent_id: string;
  type: string;
  name: string;
  description: string | null;
  earned_at: number;
  agent_name: string;
  agent_name_ko: string;
  avatar_emoji: string;
}

export async function getAchievements(agentId?: string): Promise<{ achievements: Achievement[] }> {
  const q = agentId ? `?agentId=${agentId}` : "";
  return request(`/api/achievements${q}`);
}

// ── Messages (Chat) ──

export interface ChatMessage {
  id: number;
  sender_type: "ceo" | "agent" | "system";
  sender_id: string | null;
  receiver_type: "agent" | "department" | "all";
  receiver_id: string | null;
  receiver_name?: string | null;
  receiver_name_ko?: string | null;
  content: string;
  message_type: string;
  created_at: number;
  sender_name?: string | null;
  sender_name_ko?: string | null;
  sender_avatar?: string | null;
}

export async function getMessages(opts?: {
  receiverId?: string;
  receiverType?: string;
  messageType?: string;
  limit?: number;
  before?: number;
}): Promise<{ messages: ChatMessage[] }> {
  const params = new URLSearchParams();
  if (opts?.receiverId) params.set("receiverId", opts.receiverId);
  if (opts?.receiverType) params.set("receiverType", opts.receiverType);
  if (opts?.messageType && opts.messageType !== "all") params.set("messageType", opts.messageType);
  if (opts?.limit) params.set("limit", String(opts.limit));
  if (opts?.before) params.set("before", String(opts.before));
  const q = params.toString();
  return request(`/api/messages${q ? `?${q}` : ""}`);
}

export async function sendMessage(payload: {
  sender_type?: string;
  sender_id?: string | null;
  receiver_type: string;
  receiver_id?: string | null;
  discord_target?: string | null;
  content: string;
  message_type?: string;
}): Promise<ChatMessage> {
  return request("/api/messages", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

// ── GitHub Issues ──

export async function getGitHubIssues(
  repo?: string,
  state: "open" | "closed" | "all" = "open",
  limit = 20,
): Promise<GitHubIssuesResponse> {
  const params = new URLSearchParams({ state, limit: String(limit) });
  if (repo) params.set("repo", repo);
  return request(`/api/github-issues?${params}`);
}

export async function closeGitHubIssue(
  repo: string,
  issueNumber: number,
): Promise<{ ok: boolean; repo: string; number: number }> {
  const [owner, repoName] = repo.split("/");
  return request(`/api/github-issues/${owner}/${repoName}/${issueNumber}/close`, {
    method: "PATCH",
  });
}

// ── Round Table Meetings ──

export async function getRoundTableMeetings(): Promise<RoundTableMeeting[]> {
  const data = await request<{ meetings: RoundTableMeeting[] }>("/api/round-table-meetings");
  return data.meetings;
}

export async function getRoundTableMeeting(id: string): Promise<RoundTableMeeting> {
  return request(`/api/round-table-meetings/${id}`);
}

export async function deleteRoundTableMeeting(id: string): Promise<{ ok: boolean }> {
  return request(`/api/round-table-meetings/${id}`, { method: "DELETE" });
}

export async function updateRoundTableMeetingIssueRepo(
  id: string,
  repo: string | null,
): Promise<{ ok: boolean; meeting: RoundTableMeeting }> {
  return request(`/api/round-table-meetings/${id}/issue-repo`, {
    method: "PATCH",
    body: JSON.stringify({ repo }),
  });
}

export interface RoundTableIssueCreationResponse {
  ok: boolean;
  skipped?: boolean;
  results: Array<{
    key: string;
    title: string;
    assignee: string;
    ok: boolean;
    discarded?: boolean;
    error?: string | null;
    issue_url?: string | null;
    attempted_at: number;
  }>;
  summary: {
    total: number;
    created: number;
    failed: number;
    discarded: number;
    pending: number;
    all_created: boolean;
    all_resolved: boolean;
  };
}

export async function createRoundTableIssues(id: string, repo?: string): Promise<RoundTableIssueCreationResponse> {
  return request(`/api/round-table-meetings/${id}/issues`, {
    method: "POST",
    body: JSON.stringify({ repo }),
  });
}

export async function discardRoundTableIssue(
  id: string,
  key: string,
): Promise<{ ok: boolean; meeting: RoundTableMeeting; summary: RoundTableIssueCreationResponse["summary"] }> {
  return request(`/api/round-table-meetings/${id}/issues/discard`, {
    method: "POST",
    body: JSON.stringify({ key }),
  });
}

export async function discardAllRoundTableIssues(
  id: string,
): Promise<{
  ok: boolean;
  meeting: RoundTableMeeting;
  summary: RoundTableIssueCreationResponse["summary"];
  results: RoundTableIssueCreationResponse["results"];
  skipped?: boolean;
}> {
  return request(`/api/round-table-meetings/${id}/issues/discard-all`, {
    method: "POST",
  });
}

export async function startRoundTableMeeting(
  agenda: string,
  channelId: string,
  primaryProvider?: string,
): Promise<{ ok: boolean }> {
  return request("/api/round-table-meetings/start", {
    method: "POST",
    body: JSON.stringify({ agenda, channel_id: channelId, primary_provider: primaryProvider ?? null }),
  });
}

// ── Skill Catalog ──

export async function getSkillCatalog(): Promise<SkillCatalogEntry[]> {
  const data = await request<{ catalog: SkillCatalogEntry[] }>("/api/skills/catalog");
  return data.catalog;
}

// ── Auto-Queue ──

export interface AutoQueueRun {
  id: string;
  repo: string | null;
  agent_id: string | null;
  status: "pending" | "generated" | "active" | "paused" | "completed";
  ai_model: string | null;
  ai_rationale: string | null;
  timeout_minutes: number;
  unified_thread: boolean;
  unified_thread_id: string | null;
  created_at: number;
  completed_at: number | null;
  max_concurrent_threads?: number;
  max_concurrent_per_agent?: number;
  thread_group_count?: number;
}

export interface DispatchQueueEntry {
  id: string;
  agent_id: string;
  card_id: string;
  priority_rank: number;
  reason: string | null;
  status: "pending" | "dispatched" | "done" | "skipped";
  created_at: number;
  dispatched_at: number | null;
  completed_at: number | null;
  card_title?: string;
  github_issue_number?: number | null;
  github_repo?: string | null;
  thread_group?: number;
}

export interface ThreadGroupStatus {
  pending: number;
  dispatched: number;
  done: number;
  skipped: number;
  status: string;
  reason?: string | null;
  entries: { id: string; card_id: string; github_issue_number?: number | null; status: string }[];
}

export interface AutoQueueStatus {
  run: AutoQueueRun | null;
  entries: DispatchQueueEntry[];
  agents: Record<string, { pending: number; dispatched: number; done: number; skipped: number }>;
  thread_groups?: Record<string, ThreadGroupStatus>;
}

export type AutoQueueGenerateMode =
  | "priority-sort"
  | "dependency-aware"
  | "similarity-aware"
  | "pm-assisted";

export async function generateAutoQueue(
  repo?: string | null,
  agentId?: string | null,
  mode?: AutoQueueGenerateMode | null,
): Promise<{
  run: AutoQueueRun;
  entries: DispatchQueueEntry[];
}> {
  return request("/api/auto-queue/generate", {
    method: "POST",
    body: JSON.stringify({
      repo: repo ?? null,
      agent_id: agentId ?? null,
      mode: mode ?? "priority-sort",
      parallel: mode === "similarity-aware" || undefined,
    }),
  });
}

export async function activateAutoQueue(repo?: string | null, agentId?: string | null, unifiedThread?: boolean): Promise<{
  dispatched: KanbanCard[];
  count: number;
}> {
  const body: Record<string, unknown> = {};
  if (repo) body.repo = repo;
  if (agentId) body.agent_id = agentId;
  if (unifiedThread !== undefined) body.unified_thread = unifiedThread;
  return request("/api/auto-queue/activate", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export async function getAutoQueueStatus(repo?: string | null, agentId?: string | null): Promise<AutoQueueStatus> {
  const params = new URLSearchParams();
  if (repo) params.set("repo", repo);
  if (agentId) params.set("agent_id", agentId);
  const qs = params.toString();
  return request(`/api/auto-queue/status${qs ? `?${qs}` : ""}`);
}

export async function getPipelineStagesForAgent(repo: string, agentId: string): Promise<import("../types").PipelineStage[]> {
  const params = new URLSearchParams({ repo, agent_id: agentId });
  const data = await request<{ stages: import("../types").PipelineStage[] }>(
    `/api/pipeline/stages?${params}`,
  );
  return data.stages;
}

export async function skipAutoQueueEntry(id: string): Promise<{ ok: boolean }> {
  return request(`/api/auto-queue/entries/${id}/skip`, { method: "PATCH" });
}

export async function updateAutoQueueRun(
  id: string,
  status?: "paused" | "active" | "completed",
  unified_thread?: boolean,
): Promise<{ ok: boolean }> {
  const body: Record<string, unknown> = {};
  if (status !== undefined) body.status = status;
  if (unified_thread !== undefined) body.unified_thread = unified_thread;
  return request(`/api/auto-queue/runs/${id}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

export async function reorderAutoQueueEntries(
  orderedIds: string[],
  agentId?: string | null,
): Promise<{ ok: boolean }> {
  return request("/api/auto-queue/reorder", {
    method: "PATCH",
    body: JSON.stringify({ orderedIds, agentId: agentId ?? undefined }),
  });
}

export async function resetAutoQueue(): Promise<{ ok: boolean; deleted_entries: number; completed_runs: number }> {
  return request("/api/auto-queue/reset", { method: "POST" });
}

// ── Pipeline Config Hierarchy (#135) ──

export interface PipelineConfigResponse {
  pipeline: import("../types").PipelineConfigFull;
  layers: { default: boolean; repo: boolean; agent: boolean };
}

export async function getDefaultPipeline(): Promise<import("../types").PipelineConfigFull> {
  return request("/api/pipeline/config/default");
}

export async function getEffectivePipeline(
  repo?: string,
  agentId?: string,
): Promise<PipelineConfigResponse> {
  const params = new URLSearchParams();
  if (repo) params.set("repo", repo);
  if (agentId) params.set("agent_id", agentId);
  return request(`/api/pipeline/config/effective?${params}`);
}

export async function getRepoPipeline(repo: string): Promise<{ repo: string; pipeline_config: unknown }> {
  // Server expects /repo/{owner}/{repo} as two segments, not one encoded segment
  const [owner, name] = repo.split("/");
  return request(`/api/pipeline/config/repo/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`);
}

export async function setRepoPipeline(
  repo: string,
  config: unknown,
): Promise<{ ok: boolean }> {
  const [owner, name] = repo.split("/");
  return request(`/api/pipeline/config/repo/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
    method: "PUT",
    body: JSON.stringify({ config }),
  });
}

export async function getAgentPipeline(agentId: string): Promise<{ agent_id: string; pipeline_config: unknown }> {
  return request(`/api/pipeline/config/agent/${agentId}`);
}

export async function setAgentPipeline(
  agentId: string,
  config: unknown,
): Promise<{ ok: boolean }> {
  return request(`/api/pipeline/config/agent/${agentId}`, {
    method: "PUT",
    body: JSON.stringify({ config }),
  });
}
