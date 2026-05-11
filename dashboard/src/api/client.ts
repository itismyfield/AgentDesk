import type {
  Agent,
  AuditLogEntry,
  CompanySettings,
  Department,
  KanbanCard,
  KanbanRepoSource,
  Office,
  DispatchedSession,
  DashboardStats,
  TokenAnalyticsResponse,
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
  SkillCatalogEntry,
  DispatchDeliveryEvent,
  TaskDispatch,
  VoiceConfigPutBody,
  VoiceConfigResponse,
} from "../types";
import { resolveAvatarSeed } from "../lib/pixel-avatar";

export type {
  AuditLogEntry,
  DispatchDeliveryEvent,
  KanbanCard,
  KanbanRepoSource,
  TokenAnalyticsResponse,
  VoiceConfigPutBody,
  VoiceConfigResponse,
} from "../types";

const BASE = "";
const REQUEST_TIMEOUT_MS = 15_000;
const TOKEN_ANALYTICS_TIMEOUT_MS = 60_000;
const MAX_RETRIES = 2;
const INITIAL_BACKOFF_MS = 500;

// ── GET deduplication ──
const inflightGets = new Map<string, Promise<unknown>>();
interface CachedGetEntry<T = unknown> {
  data: T;
  fetchedAt: number;
}
const cachedGets = new Map<string, CachedGetEntry>();

// ── Global error listener for toast integration ──
type ApiErrorListener = (url: string, error: Error) => void;
let apiErrorListener: ApiErrorListener | null = null;
export function onApiError(listener: ApiErrorListener | null): void {
  apiErrorListener = listener;
}

interface RequestOptions extends RequestInit {
  timeoutMs?: number;
  maxRetries?: number;
}

function composeRequestSignal(
  timeoutSignal: AbortSignal,
  externalSignal?: AbortSignal,
): { signal: AbortSignal; cleanup: () => void } {
  if (!externalSignal) {
    return {
      signal: timeoutSignal,
      cleanup: () => {},
    };
  }

  const controller = new AbortController();

  const abortFromSource = () => {
    if (controller.signal.aborted) return;
    controller.abort(externalSignal.reason ?? timeoutSignal.reason);
  };

  if (timeoutSignal.aborted || externalSignal.aborted) {
    abortFromSource();
  }

  timeoutSignal.addEventListener("abort", abortFromSource);
  externalSignal.addEventListener("abort", abortFromSource);

  return {
    signal: controller.signal,
    cleanup: () => {
      timeoutSignal.removeEventListener("abort", abortFromSource);
      externalSignal.removeEventListener("abort", abortFromSource);
    },
  };
}

function isRetryable(status: number): boolean {
  return status === 408 || status === 429 || status >= 500;
}

function isAbortError(error: Error): boolean {
  return error.name === "AbortError" || /aborted/i.test(error.message);
}

function readCachedGet<T>(url: string): CachedGetEntry<T> | null {
  const cached = cachedGets.get(url);
  if (!cached) return null;
  return cached as CachedGetEntry<T>;
}

export interface CachedApiSnapshot<T> {
  data: T;
  fetchedAt: number;
}

function readCachedSnapshot<T>(url: string): CachedApiSnapshot<T> | null {
  const cached = readCachedGet<T>(url);
  if (!cached) return null;
  return {
    data: cached.data,
    fetchedAt: cached.fetchedAt,
  };
}

async function request<T>(url: string, opts?: RequestOptions): Promise<T> {
  const method = opts?.method?.toUpperCase() ?? "GET";
  const isGet = method === "GET";
  const shouldDedupe = isGet && !opts?.signal;
  const timeoutMs = opts?.timeoutMs ?? REQUEST_TIMEOUT_MS;
  const maxRetries = opts?.maxRetries ?? MAX_RETRIES;

  if (shouldDedupe) {
    const existing = inflightGets.get(url);
    if (existing) return existing as Promise<T>;
  }

  const execute = async (): Promise<T> => {
    let lastError: Error | null = null;
    for (let attempt = 0; attempt <= maxRetries; attempt++) {
      if (attempt > 0) {
        const delay = INITIAL_BACKOFF_MS * 2 ** (attempt - 1);
        await new Promise((resolve) => setTimeout(resolve, delay));
      }
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), timeoutMs);
      const externalSignal = opts?.signal ?? undefined;
      const { signal, cleanup } = composeRequestSignal(controller.signal, externalSignal);
      try {
        const { timeoutMs: _timeoutMs, signal: _signal, ...fetchOpts } = opts ?? {};
        const res = await fetch(`${BASE}${url}`, {
          credentials: "include",
          ...fetchOpts,
          signal,
          headers: {
            "Content-Type": "application/json",
            ...fetchOpts.headers,
          },
        });
        clearTimeout(timer);
        cleanup();
        if (!res.ok) {
          const err = await res.json().catch(() => ({ error: "unknown" }));
          const error = new Error(err.error || `HTTP ${res.status}`);
          if (isGet && isRetryable(res.status) && attempt < maxRetries) {
            lastError = error;
            continue;
          }
          throw error;
        }
        const payload = await res.json();
        if (isGet) {
          cachedGets.set(url, {
            data: payload,
            fetchedAt: Date.now(),
          });
        }
        return payload;
      } catch (error) {
        clearTimeout(timer);
        cleanup();
        const resolvedError =
          error instanceof Error ? error : new Error(String(error));
        if (resolvedError.name === "AbortError") {
          if (externalSignal?.aborted) throw resolvedError;
          lastError = new Error(`Request timeout: ${url}`);
          if (isGet && attempt < maxRetries) continue;
        } else if (
          isGet &&
          attempt < maxRetries &&
          !resolvedError.message.startsWith("HTTP ")
        ) {
          lastError = resolvedError;
          continue;
        }
        throw lastError ?? resolvedError;
      }
    }
    throw lastError ?? new Error(`Request failed: ${url}`);
  };

  const promise = execute().finally(() => {
    if (shouldDedupe) inflightGets.delete(url);
  });

  if (shouldDedupe) inflightGets.set(url, promise);

  return promise.catch((error) => {
    const resolvedError =
      error instanceof Error ? error : new Error(String(error));
    if (!isAbortError(resolvedError)) {
      apiErrorListener?.(url, resolvedError);
    }
    throw resolvedError;
  });
}

function normalizeAgent(agent: Agent): Agent {
  return {
    ...agent,
    name_ko: agent.name_ko ?? agent.name,
    avatar_emoji: agent.avatar_emoji ?? "",
    avatar_seed: resolveAvatarSeed(agent),
    department_name: agent.department_name ?? null,
    department_name_ko: agent.department_name_ko ?? agent.department_name ?? null,
  };
}

// Auth
export async function getSession(): Promise<{
  ok: boolean;
  csrf_token: string;
}> {
  return request("/api/auth/session");
}

// ── Offices ──

export async function getOffices(): Promise<Office[]> {
  const data = await request<{ offices: Office[] }>("/api/offices");
  return data.offices;
}

export async function createOffice(office: Partial<Office>): Promise<Office> {
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
    body: JSON.stringify({
      agent_id: agentId,
      department_id: departmentId ?? null,
    }),
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

export function getCachedAgents(officeId?: string): CachedApiSnapshot<Agent[]> | null {
  const q = officeId ? `?officeId=${officeId}` : "";
  const cached = readCachedSnapshot<{ agents: Agent[] }>(`/api/agents${q}`);
  if (!cached) return null;
  return {
    data: cached.data.agents.map(normalizeAgent),
    fetchedAt: cached.fetchedAt,
  };
}

export async function getAgents(
  officeId?: string,
  opts?: RequestOptions,
): Promise<Agent[]> {
  const q = officeId ? `?officeId=${officeId}` : "";
  const data = await request<{ agents: Agent[] }>(`/api/agents${q}`, opts);
  return data.agents.map(normalizeAgent);
}

export async function getAgent(id: string): Promise<Agent> {
  const data = await request<{ agent: Agent }>(`/api/agents/${id}`);
  return normalizeAgent(data.agent);
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
  patch: Partial<Agent> & {
    prompt_content?: string;
    auto_commit?: boolean;
    commit_message?: string;
  },
): Promise<Agent> {
  const data = await request<{ agent: Agent } | Agent>(`/api/agents/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
  return normalizeAgent("agent" in data ? data.agent : data);
}

export async function deleteAgent(id: string): Promise<void> {
  await request(`/api/agents/${id}`, { method: "DELETE" });
}

export interface AgentSetupRequest {
  agent_id: string;
  channel_id: string;
  provider?: string;
  prompt_template_path?: string;
  skills?: string[];
  dry_run?: boolean;
}

export interface AgentSetupResponse {
  ok?: boolean;
  dry_run?: boolean;
  agent_id?: string;
  role_id?: string;
  channel_id?: string;
  provider?: string;
  steps?: Array<{ name?: string; status?: string; detail?: string }>;
  errors?: string[];
  warnings?: string[];
  rollback?: unknown;
}

export async function setupAgent(
  body: AgentSetupRequest,
): Promise<AgentSetupResponse> {
  return request("/api/agents/setup", {
    method: "POST",
    body: JSON.stringify(body),
    timeoutMs: 60_000,
  });
}

export interface ArchiveAgentRequest {
  reason?: string;
  discord_action?: "none" | "readonly" | "move";
  archive_category_id?: string;
}

export interface ArchiveAgentResponse {
  ok: boolean;
  agent_id: string;
  status?: string;
  archive_state?: string;
  discord?: unknown;
}

export async function archiveAgent(
  id: string,
  body: ArchiveAgentRequest = {},
): Promise<ArchiveAgentResponse> {
  return request(`/api/agents/${id}/archive`, {
    method: "POST",
    body: JSON.stringify(body),
    timeoutMs: 60_000,
  });
}

export async function unarchiveAgent(id: string): Promise<ArchiveAgentResponse> {
  return request(`/api/agents/${id}/unarchive`, {
    method: "POST",
    body: JSON.stringify({}),
    timeoutMs: 60_000,
  });
}

export interface DuplicateAgentRequest {
  new_agent_id: string;
  channel_id: string;
  provider?: string;
  name?: string;
  name_ko?: string;
  department_id?: string | null;
  skills?: string[];
  dry_run?: boolean;
}

export async function duplicateAgent(
  id: string,
  body: DuplicateAgentRequest,
): Promise<AgentSetupResponse & { agent?: Agent }> {
  const data = await request<AgentSetupResponse & { agent?: Agent }>(
    `/api/agents/${id}/duplicate`,
    {
      method: "POST",
      body: JSON.stringify(body),
      timeoutMs: 60_000,
    },
  );
  if (data.agent) {
    return { ...data, agent: normalizeAgent(data.agent) };
  }
  return data;
}

export interface AgentOfficeMembership extends Office {
  assigned: boolean;
  office_department_id?: string | null;
  joined_at?: number | null;
}

export async function getAgentOffices(
  agentId: string,
): Promise<AgentOfficeMembership[]> {
  const data = await request<{ offices: AgentOfficeMembership[] }>(
    `/api/agents/${agentId}/offices`,
  );
  return data.offices;
}

// ── Audit Logs ──

export async function getAuditLogs(
  limit = 20,
  filter?: { entityType?: string; entityId?: string; agentId?: string },
): Promise<AuditLogEntry[]> {
  const params = new URLSearchParams();
  params.set("limit", String(limit));
  if (filter?.entityType) params.set("entityType", filter.entityType);
  if (filter?.entityId) params.set("entityId", filter.entityId);
  if (filter?.agentId) params.set("agentId", filter.agentId);
  const data = await request<{ logs: AuditLogEntry[] }>(
    `/api/audit-logs?${params.toString()}`,
  );
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

export async function getSettings(): Promise<Partial<CompanySettings>> {
  return request("/api/settings");
}

export async function saveSettings(
  settings: Partial<CompanySettings>,
): Promise<{ ok: boolean }> {
  return request("/api/settings", {
    method: "PUT",
    body: JSON.stringify(settings),
  });
}

export class VoiceConfigApiError extends Error {
  status: number;
  payload: unknown;

  constructor(status: number, payload: unknown) {
    const message =
      typeof payload === "object" &&
      payload !== null &&
      "message" in payload &&
      typeof (payload as { message?: unknown }).message === "string"
        ? (payload as { message: string }).message
        : `HTTP ${status}`;
    super(message);
    this.name = "VoiceConfigApiError";
    this.status = status;
    this.payload = payload;
  }
}

export async function getVoiceConfig(): Promise<VoiceConfigResponse> {
  return request("/api/voice/config");
}

export async function saveVoiceConfig(
  body: VoiceConfigPutBody,
): Promise<VoiceConfigResponse> {
  const response = await fetch("/api/voice/config", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const payload = await response.json().catch(() => ({ error: "unknown" }));
  if (!response.ok) {
    throw new VoiceConfigApiError(response.status, payload);
  }
  cachedGets.delete("/api/voice/config");
  return payload as VoiceConfigResponse;
}

// ── Runtime Config ──

export interface RuntimeConfigResponse {
  current: Record<string, number>;
  defaults: Record<string, number>;
}

export type EscalationMode = "pm" | "user" | "scheduled";

export interface EscalationSettings {
  mode: EscalationMode;
  owner_user_id: number | null;
  pm_channel_id: string | null;
  schedule: {
    pm_hours: string;
    timezone: string;
  };
}

export interface EscalationSettingsResponse {
  current: EscalationSettings;
  defaults: EscalationSettings;
}

export async function getRuntimeConfig(): Promise<RuntimeConfigResponse> {
  return request("/api/settings/runtime-config");
}

export async function saveRuntimeConfig(
  patch: Record<string, number>,
): Promise<{ ok: boolean }> {
  return request("/api/settings/runtime-config", {
    method: "PUT",
    body: JSON.stringify(patch),
  });
}

export async function getEscalationSettings(): Promise<EscalationSettingsResponse> {
  return request("/api/settings/escalation");
}

export async function saveEscalationSettings(
  settings: EscalationSettings,
): Promise<EscalationSettingsResponse> {
  return request("/api/settings/escalation", {
    method: "PUT",
    body: JSON.stringify(settings),
  });
}

// ── Runtime Health ──

export interface HealthProviderStatus {
  name: string;
  connected: boolean;
  active_turns: number;
  queue_depth: number;
  sessions: number;
  restart_pending: boolean;
  last_turn_at: string | null;
}

export interface HealthDispatchOutboxStats {
  pending: number;
  retrying: number;
  permanent_failures: number;
  oldest_pending_age: number;
}

export interface HealthResponse {
  status: "healthy" | "degraded" | "unhealthy" | string;
  version?: string;
  uptime_secs?: number;
  global_active?: number;
  global_finalizing?: number;
  deferred_hooks?: number;
  queue_depth?: number;
  watcher_count?: number;
  recovery_duration?: number;
  degraded_reasons?: string[];
  providers?: HealthProviderStatus[];
  db?: boolean;
  dashboard?: boolean;
  outbox_age?: number;
  dispatch_outbox?: HealthDispatchOutboxStats;
}

export async function getHealth(): Promise<HealthResponse> {
  return request("/api/health");
}

export function getCachedHealth(): CachedGetEntry<HealthResponse> | null {
  return readCachedGet<HealthResponse>("/api/health");
}

export interface PromptManifestRetentionStatus {
  total_stored_bytes: number;
  total_original_bytes: number;
  truncated_count: number;
  manifest_count: number;
  layer_count: number;
  oldest_full_content_at: string | null;
  retention_horizon_at: string | null;
  retention_days: number;
  per_layer_max_bytes_adk_provided: number;
  per_layer_max_bytes_user_derived: number;
  enabled: boolean;
  restart_required_for_config_changes: boolean;
  config_applied_at: string;
  config_source: string;
  hot_reload: boolean;
}

export async function getPromptManifestRetention(): Promise<PromptManifestRetentionStatus> {
  return request("/api/prompt-manifest/retention");
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

export async function getTokenAnalytics(
  period: "7d" | "30d" | "90d" = "30d",
  opts?: { signal?: AbortSignal; forceRefresh?: boolean },
): Promise<TokenAnalyticsResponse> {
  // The endpoint now ships with `Cache-Control: max-age=15, swr=300` so a
  // background re-entry to /stats can paint instantly. The Refresh button
  // and any other "user explicitly asked for fresh data" caller passes
  // forceRefresh=true so we bypass:
  //   - the browser cache via fetch's `cache: "reload"` directive
  //   - the new server-side in-process cache via `&fresh=1` query param
  //     (the server short-circuits to its 30s memo without it).
  // Together these restore the explicit-refresh contract Codex flagged on
  // PR #1258 while still letting background traffic skip the ~9s scan.
  const force = opts?.forceRefresh ? "&fresh=1" : "";
  return request(`/api/token-analytics?period=${period}${force}`, {
    signal: opts?.signal,
    timeoutMs: TOKEN_ANALYTICS_TIMEOUT_MS,
    cache: opts?.forceRefresh ? "reload" : "default",
  });
}

export function getCachedTokenAnalytics(
  period: "7d" | "30d" | "90d" = "30d",
): CachedGetEntry<TokenAnalyticsResponse> | null {
  return readCachedGet<TokenAnalyticsResponse>(
    `/api/token-analytics?period=${period}`,
  );
}

// ── Home KPI sparklines (#1242) ──
export interface HomeKpiSeries {
  label: string;
  unit: string;
  values: number[];
}

export interface HomeKpiRateLimitProvider {
  provider: string;
  current_pct: number | null;
  unsupported: boolean;
  stale: boolean;
  reason: string | null;
  values: number[];
}

export interface HomeKpiRateLimit {
  label: string;
  unit: string;
  providers: HomeKpiRateLimitProvider[];
}

export interface HomeKpiTrendsResponse {
  days: number;
  generated_at: string;
  dates: string[];
  tokens: HomeKpiSeries;
  cost: HomeKpiSeries;
  in_progress: HomeKpiSeries;
  rate_limit: HomeKpiRateLimit;
}

export async function getHomeKpiTrends(
  days: number = 14,
  opts?: { signal?: AbortSignal },
): Promise<HomeKpiTrendsResponse> {
  return request(`/api/home/kpi-trends?days=${days}`, {
    signal: opts?.signal,
  });
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

export interface KanbanDispatchMutationResponse {
  card: KanbanCard;
  new_dispatch_id: string | null;
  cancelled_dispatch_id: string | null;
  next_action: string;
}

function isObjectRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function hasOwn(value: Record<string, unknown>, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(value, key);
}

function requireFields(
  endpoint: string,
  value: Record<string, unknown>,
  keys: string[],
): void {
  for (const key of keys) {
    if (!hasOwn(value, key)) {
      throw new Error(
        `${endpoint} response contract invalid: missing required field '${key}'`,
      );
    }
  }
}

function parseKanbanDispatchMutationResponse(
  endpoint: string,
  raw: unknown,
): KanbanDispatchMutationResponse {
  if (!isObjectRecord(raw)) {
    throw new Error(`${endpoint} response contract invalid: expected object`);
  }
  requireFields(endpoint, raw, [
    "card",
    "new_dispatch_id",
    "cancelled_dispatch_id",
    "next_action",
  ]);
  if (!isObjectRecord(raw.card)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'card' must be an object`,
    );
  }
  if (raw.new_dispatch_id !== null && typeof raw.new_dispatch_id !== "string") {
    throw new Error(
      `${endpoint} response contract invalid: field 'new_dispatch_id' must be string or null`,
    );
  }
  if (
    raw.cancelled_dispatch_id !== null &&
    typeof raw.cancelled_dispatch_id !== "string"
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'cancelled_dispatch_id' must be string or null`,
    );
  }
  if (typeof raw.next_action !== "string" || raw.next_action.trim() === "") {
    throw new Error(
      `${endpoint} response contract invalid: field 'next_action' must be a non-empty string`,
    );
  }
  return {
    card: raw.card as unknown as KanbanCard,
    new_dispatch_id: raw.new_dispatch_id,
    cancelled_dispatch_id: raw.cancelled_dispatch_id,
    next_action: raw.next_action,
  };
}

export async function retryKanbanCard(
  id: string,
  payload?: { assignee_agent_id?: string | null; request_now?: boolean },
): Promise<KanbanDispatchMutationResponse> {
  const endpoint = `/api/kanban-cards/${id}/retry`;
  const res = await request<unknown>(endpoint, {
    method: "POST",
    body: JSON.stringify(payload ?? {}),
  });
  return parseKanbanDispatchMutationResponse(endpoint, res);
}

export async function redispatchKanbanCard(
  id: string,
  payload?: { reason?: string | null },
): Promise<KanbanDispatchMutationResponse> {
  const endpoint = `/api/kanban-cards/${id}/redispatch`;
  const res = await request<unknown>(endpoint, {
    method: "POST",
    body: JSON.stringify(payload ?? {}),
  });
  return parseKanbanDispatchMutationResponse(endpoint, res);
}

export async function patchKanbanDeferDod(
  id: string,
  payload: {
    items?: Array<{ label: string }>;
    verify?: string;
    unverify?: string;
    remove?: string;
  },
): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>(
    `/api/kanban-cards/${id}/defer-dod`,
    {
      method: "PATCH",
      body: JSON.stringify(payload),
    },
  );
  return res.card;
}

export interface AssignmentResult {
  ok: boolean;
  agent_id: string;
}

export interface AssignmentTransitionResult {
  attempted: boolean;
  ok: boolean;
  from?: string;
  to?: string;
  target?: string;
  target_status: string;
  next_action: string;
  steps?: string[];
  completed_steps?: Array<{ from?: string; to?: string; changed?: boolean }>;
  failed_step?: string;
  error: string | null;
}

export interface AssignKanbanIssueResponse {
  card: KanbanCard;
  deduplicated?: boolean;
  assignment: AssignmentResult;
  transition: AssignmentTransitionResult;
}

function parseAssignKanbanIssueResponse(
  endpoint: string,
  raw: unknown,
): AssignKanbanIssueResponse {
  if (!isObjectRecord(raw)) {
    throw new Error(`${endpoint} response contract invalid: expected object`);
  }
  requireFields(endpoint, raw, ["card", "assignment", "transition"]);

  const assignment = raw.assignment;
  const transition = raw.transition;
  if (!isObjectRecord(raw.card)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'card' must be an object`,
    );
  }
  if (!isObjectRecord(assignment)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'assignment' must be an object`,
    );
  }
  if (!isObjectRecord(transition)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition' must be an object`,
    );
  }

  requireFields(endpoint, assignment, ["ok", "agent_id"]);
  requireFields(endpoint, transition, [
    "attempted",
    "ok",
    "target_status",
    "error",
    "next_action",
  ]);

  if (typeof assignment.ok !== "boolean") {
    throw new Error(
      `${endpoint} response contract invalid: field 'assignment.ok' must be boolean`,
    );
  }
  if (
    typeof assignment.agent_id !== "string" ||
    assignment.agent_id.trim() === ""
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'assignment.agent_id' must be a non-empty string`,
    );
  }
  if (
    typeof transition.attempted !== "boolean" ||
    typeof transition.ok !== "boolean"
  ) {
    throw new Error(
      `${endpoint} response contract invalid: transition booleans must be boolean`,
    );
  }
  if (
    typeof transition.target_status !== "string" ||
    transition.target_status.trim() === ""
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition.target_status' must be a non-empty string`,
    );
  }
  if (transition.error !== null && typeof transition.error !== "string") {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition.error' must be string or null`,
    );
  }
  if (
    typeof transition.next_action !== "string" ||
    transition.next_action.trim() === ""
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition.next_action' must be a non-empty string`,
    );
  }

  return {
    card: raw.card as unknown as KanbanCard,
    deduplicated:
      typeof raw.deduplicated === "boolean" ? raw.deduplicated : undefined,
    assignment: assignment as unknown as AssignmentResult,
    transition: transition as unknown as AssignmentTransitionResult,
  };
}

export async function assignKanbanIssue(payload: {
  github_repo: string;
  github_issue_number: number;
  github_issue_url?: string | null;
  title: string;
  description?: string | null;
  assignee_agent_id: string;
}): Promise<AssignKanbanIssueResponse> {
  const endpoint = "/api/kanban-cards/assign-issue";
  const res = await request<unknown>(endpoint, {
    method: "POST",
    body: JSON.stringify(payload),
  });
  return parseAssignKanbanIssueResponse(endpoint, res);
}

export async function getStalledCards(): Promise<KanbanCard[]> {
  return request("/api/kanban-cards/stalled");
}

// #1064: bulk-action consolidated into per-card POST /kanban-cards/{id}/transition.
// Server-side pipeline lookup for terminal/initial states is now the caller's
// responsibility — each action resolves a concrete target_status before iteration.
export async function bulkKanbanAction(
  action: "pass" | "reset" | "cancel" | "transition",
  card_ids: string[],
  targetStatus?: string,
): Promise<{
  action: string;
  results: Array<{ id: string; ok: boolean; error?: string }>;
}> {
  let resolvedTarget: string | undefined = targetStatus;
  if (action === "pass" || action === "cancel") {
    resolvedTarget = "done";
  } else if (action === "reset") {
    resolvedTarget = "backlog";
  } else if (action === "transition") {
    if (!resolvedTarget) {
      throw new Error("transition action requires target_status");
    }
  }

  const results = await Promise.all(
    card_ids.map(async (id) => {
      try {
        await request(`/api/kanban-cards/${encodeURIComponent(id)}/transition`, {
          method: "POST",
          body: JSON.stringify({ status: resolvedTarget }),
        });
        return { id, ok: true };
      } catch (error) {
        return {
          id,
          ok: false,
          error: error instanceof Error ? error.message : String(error),
        };
      }
    }),
  );

  return { action, results };
}

export async function getKanbanRepoSources(): Promise<KanbanRepoSource[]> {
  const data = await request<{ repos: KanbanRepoSource[] }>(
    "/api/kanban-repos",
  );
  return data.repos;
}

export async function addKanbanRepoSource(
  repo: string,
): Promise<KanbanRepoSource> {
  return request("/api/kanban-repos", {
    method: "POST",
    body: JSON.stringify({ repo }),
  });
}

export async function updateKanbanRepoSource(
  id: string,
  data: { default_agent_id?: string | null },
): Promise<KanbanRepoSource> {
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

export async function getKanbanReviews(
  cardId: string,
): Promise<KanbanReview[]> {
  const data = await request<{ reviews: KanbanReview[] }>(
    `/api/kanban-cards/${cardId}/reviews`,
  );
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

export async function triggerDecidedRework(
  reviewId: string,
): Promise<{ ok: boolean }> {
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

export async function getCardAuditLog(
  cardId: string,
): Promise<CardAuditLogEntry[]> {
  const data = await request<{ logs: CardAuditLogEntry[] }>(
    `/api/kanban-cards/${cardId}/audit-log`,
  );
  return data.logs;
}

export interface CardGitHubCommentsResult {
  comments: GitHubComment[];
  body: string;
}

export async function getCardGitHubComments(
  cardId: string,
): Promise<CardGitHubCommentsResult> {
  const data = await request<{ comments: GitHubComment[]; body?: string }>(
    `/api/kanban-cards/${cardId}/comments`,
  );
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
  applies_to_agent_id?: string | null;
  trigger_after?: "ready" | "review_pass";
}

export async function getPipelineStages(
  repo: string,
): Promise<import("../types").PipelineStage[]> {
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
  await request(`/api/pipeline/stages?repo=${encodeURIComponent(repo)}`, {
    method: "DELETE",
  });
}

export async function getCardPipelineStatus(cardId: string): Promise<{
  stages: import("../types").PipelineStage[];
  history: import("../types").PipelineHistoryEntry[];
  current_stage: import("../types").PipelineStage | null;
}> {
  return request(`/api/pipeline/cards/${cardId}`);
}

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

export interface SkillTrendPoint {
  day: string;
  count: number;
}

export async function getSkillTrend(days = 30): Promise<SkillTrendPoint[]> {
  const data = await request<{ trend: SkillTrendPoint[] }>(
    `/api/skills-trend?days=${days}`,
  );
  return data.trend;
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
  avatar_seed?: number | null;
  rarity?: string | null;
  progress?: AchievementProgress | null;
}

export interface AchievementProgress {
  current_xp: number;
  threshold: number;
  next_threshold: number | null;
  percent: number;
}

export interface DailyMission {
  id: string;
  label: string;
  current: number;
  target: number;
  completed: boolean;
}

export interface AchievementsResponse {
  achievements: Achievement[];
  daily_missions: DailyMission[];
}

function normalizeAchievement(raw: unknown): Achievement {
  const source = (raw ?? {}) as Record<string, unknown>;
  const rawEarnedAt = source.earned_at;
  const earnedAt =
    typeof rawEarnedAt === "number"
      ? rawEarnedAt
      : typeof rawEarnedAt === "string"
        ? Date.parse(rawEarnedAt) || 0
        : 0;
  const rawProgress =
    source.progress && typeof source.progress === "object"
      ? (source.progress as Record<string, unknown>)
      : null;

  return {
    id: String(source.id ?? ""),
    agent_id: String(source.agent_id ?? ""),
    type: String(source.type ?? ""),
    name: String(source.name ?? ""),
    description: typeof source.description === "string" ? source.description : null,
    earned_at: earnedAt,
    agent_name: String(source.agent_name ?? ""),
    agent_name_ko: String(source.agent_name_ko ?? ""),
    avatar_emoji: typeof source.avatar_emoji === "string" ? source.avatar_emoji : "🤖",
    avatar_seed:
      typeof source.avatar_seed === "number" ? source.avatar_seed : null,
    rarity: typeof source.rarity === "string" ? source.rarity : null,
    progress: rawProgress
      ? {
          current_xp:
            typeof rawProgress.current_xp === "number" ? rawProgress.current_xp : 0,
          threshold:
            typeof rawProgress.threshold === "number" ? rawProgress.threshold : 0,
          next_threshold:
            typeof rawProgress.next_threshold === "number"
              ? rawProgress.next_threshold
              : null,
          percent:
            typeof rawProgress.percent === "number" ? rawProgress.percent : 0,
        }
      : null,
  };
}

function normalizeDailyMission(raw: unknown): DailyMission {
  const source = (raw ?? {}) as Record<string, unknown>;
  return {
    id: String(source.id ?? ""),
    label: String(source.label ?? ""),
    current: typeof source.current === "number" ? source.current : 0,
    target: typeof source.target === "number" ? source.target : 0,
    completed: Boolean(source.completed),
  };
}

export async function getAchievements(
  agentId?: string,
): Promise<AchievementsResponse> {
  const q = agentId ? `?agentId=${agentId}` : "";
  const data = await request<{
    achievements?: unknown[];
    daily_missions?: unknown[];
  }>(`/api/v1/achievements${q}`);
  return {
    achievements: Array.isArray(data.achievements)
      ? data.achievements.map(normalizeAchievement)
      : [],
    daily_missions: Array.isArray(data.daily_missions)
      ? data.daily_missions.map(normalizeDailyMission)
      : [],
  };
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
  if (opts?.messageType && opts.messageType !== "all")
    params.set("messageType", opts.messageType);
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
  return request(
    `/api/github-issues/${owner}/${repoName}/${issueNumber}/close`,
    {
      method: "PATCH",
    },
  );
}

// ── Round Table Meetings ──

export async function getRoundTableMeetings(): Promise<RoundTableMeeting[]> {
  const data = await request<{ meetings: RoundTableMeeting[] }>(
    "/api/round-table-meetings",
  );
  return data.meetings;
}

export async function getRoundTableMeeting(
  id: string,
): Promise<RoundTableMeeting> {
  const data = await request<{ meeting: RoundTableMeeting }>(
    `/api/round-table-meetings/${id}`,
  );
  return data.meeting;
}

export async function getRoundTableMeetingChannels(): Promise<
  RoundTableMeetingChannelOption[]
> {
  const data = await request<{ channels: RoundTableMeetingChannelOption[] }>(
    "/api/round-table-meetings/channels",
  );
  return data.channels;
}

export async function deleteRoundTableMeeting(
  id: string,
): Promise<{ ok: boolean }> {
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

export async function createRoundTableIssues(
  id: string,
  repo?: string,
): Promise<RoundTableIssueCreationResponse> {
  return request(`/api/round-table-meetings/${id}/issues`, {
    method: "POST",
    body: JSON.stringify({ repo }),
  });
}

export async function discardRoundTableIssue(
  id: string,
  key: string,
): Promise<{
  ok: boolean;
  meeting: RoundTableMeeting;
  summary: RoundTableIssueCreationResponse["summary"];
}> {
  return request(`/api/round-table-meetings/${id}/issues/discard`, {
    method: "POST",
    body: JSON.stringify({ key }),
  });
}

export async function discardAllRoundTableIssues(id: string): Promise<{
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
  primaryProvider: string,
  reviewerProvider: string,
  fixedParticipants: string[] = [],
): Promise<{ ok: boolean; message?: string }> {
  return request("/api/round-table-meetings/start", {
    method: "POST",
    body: JSON.stringify({
      agenda,
      channel_id: channelId,
      primary_provider: primaryProvider,
      reviewer_provider: reviewerProvider,
      fixed_participants: fixedParticipants,
    }),
  });
}

// ── Skill Catalog ──

export async function getSkillCatalog(): Promise<SkillCatalogEntry[]> {
  const data = await request<{ catalog: SkillCatalogEntry[] }>(
    "/api/skills/catalog",
  );
  return data.catalog;
}

// ── Auto-Queue ──

export interface AutoQueueRun {
  id: string;
  repo: string | null;
  agent_id: string | null;
  status: "generated" | "pending" | "active" | "paused" | "completed" | "cancelled";
  ai_model: string | null;
  ai_rationale: string | null;
  timeout_minutes: number;
  unified_thread: boolean;
  unified_thread_id: string | null;
  created_at: number;
  completed_at: number | null;
  max_concurrent_threads?: number;
  thread_group_count?: number;
  deploy_phases?: number[];
}

export interface AutoQueueThreadLink {
  role: string;
  label: string;
  channel_id?: string | null;
  thread_id: string;
  url?: string | null;
}

export interface DispatchQueueEntry {
  id: string;
  agent_id: string;
  card_id: string;
  priority_rank: number;
  reason: string | null;
  status: "pending" | "dispatched" | "done" | "skipped" | "failed";
  created_at: number;
  dispatched_at: number | null;
  completed_at: number | null;
  card_title?: string;
  github_issue_number?: number | null;
  github_repo?: string | null;
  retry_count?: number;
  thread_group?: number;
  batch_phase?: number;
  thread_links?: AutoQueueThreadLink[];
  card_status?: string;
  review_round?: number;
}

export interface ThreadGroupStatus {
  pending: number;
  dispatched: number;
  done: number;
  skipped: number;
  failed: number;
  status: string;
  reason?: string | null;
  entries: {
    id: string;
    card_id: string;
    github_issue_number?: number | null;
    status: string;
  }[];
}

export interface PhaseGateInfo {
  id: number;
  phase: number;
  status: "pending" | "passed" | "failed";
  dispatch_id?: string | null;
  failure_reason?: string | null;
  created_at?: string | null;
  updated_at?: string | null;
}

export interface AutoQueueStatus {
  run: AutoQueueRun | null;
  entries: DispatchQueueEntry[];
  agents: Record<
    string,
    { pending: number; dispatched: number; done: number; skipped: number; failed: number }
  >;
  thread_groups?: Record<string, ThreadGroupStatus>;
  phase_gates?: PhaseGateInfo[];
}

export interface AutoQueueHistoryRun {
  id: string;
  repo: string | null;
  agent_id: string | null;
  status: AutoQueueRun["status"] | (string & {});
  created_at: number;
  completed_at: number | null;
  duration_ms: number;
  entry_count: number;
  done_count: number;
  skipped_count: number;
  pending_count: number;
  dispatched_count: number;
  success_rate: number;
  failure_rate: number;
}

export interface AutoQueueHistorySummary {
  total_runs: number;
  completed_runs: number;
  success_rate: number;
  failure_rate: number;
}

export interface AutoQueueHistoryResponse {
  summary: AutoQueueHistorySummary;
  runs: AutoQueueHistoryRun[];
}

export async function generateAutoQueue(
  repo?: string | null,
  agentId?: string | null,
): Promise<{
  run: AutoQueueRun;
  entries: DispatchQueueEntry[];
}> {
  const body: Record<string, unknown> = {
    repo: repo ?? null,
    agent_id: agentId ?? null,
  };
  return request("/api/queue/generate", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export async function activateAutoQueue(
  repo?: string | null,
  agentId?: string | null,
): Promise<{
  dispatched: KanbanCard[];
  count: number;
}> {
  const body: Record<string, unknown> = {};
  if (repo) body.repo = repo;
  if (agentId) body.agent_id = agentId;
  return request("/api/queue/dispatch-next", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export async function getAutoQueueStatus(
  repo?: string | null,
  agentId?: string | null,
): Promise<AutoQueueStatus> {
  const params = new URLSearchParams();
  if (repo) params.set("repo", repo);
  if (agentId) params.set("agent_id", agentId);
  const qs = params.toString();
  return request(`/api/queue/status${qs ? `?${qs}` : ""}`);
}

export async function getAutoQueueHistory(
  limit = 8,
  repo?: string | null,
  agentId?: string | null,
): Promise<AutoQueueHistoryResponse> {
  const params = new URLSearchParams();
  params.set("limit", String(limit));
  if (repo) params.set("repo", repo);
  if (agentId) params.set("agent_id", agentId);
  return request(`/api/queue/history?${params.toString()}`);
}

export async function getPipelineStagesForAgent(
  repo: string,
  agentId: string,
): Promise<import("../types").PipelineStage[]> {
  const params = new URLSearchParams({ repo, agent_id: agentId });
  const data = await request<{ stages: import("../types").PipelineStage[] }>(
    `/api/pipeline/stages?${params}`,
  );
  return data.stages;
}

export async function skipAutoQueueEntry(id: string): Promise<{ ok: boolean }> {
  return request(`/api/queue/entries/${id}/skip`, { method: "PATCH" });
}

export async function updateAutoQueueEntry(
  id: string,
  patch: {
    status?: "pending" | "skipped";
    thread_group?: number;
    priority_rank?: number;
    batch_phase?: number;
  },
): Promise<{ ok: boolean; entry: DispatchQueueEntry }> {
  return request(`/api/queue/entries/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

export async function updateAutoQueueRun(
  id: string,
  status?: "paused" | "active" | "completed",
): Promise<{ ok: boolean }> {
  const body: Record<string, unknown> = {};
  if (status !== undefined) body.status = status;
  return request(`/api/queue/runs/${id}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

export async function reorderAutoQueueEntries(
  orderedIds: string[],
  agentId?: string | null,
): Promise<{ ok: boolean }> {
  return request("/api/queue/reorder", {
    method: "PATCH",
    body: JSON.stringify({
      ordered_ids: orderedIds,
      agent_id: agentId ?? undefined,
    }),
  });
}

export interface AutoQueueResetScope {
  runId?: string | null;
  repo?: string | null;
  agentId: string;
}

export async function resetAutoQueue(
  scope: AutoQueueResetScope,
): Promise<{ ok: boolean; deleted_entries: number; completed_runs: number }> {
  return request("/api/queue/reset", {
    method: "POST",
    body: JSON.stringify({
      run_id: scope.runId ?? undefined,
      repo: scope.repo ?? undefined,
      agent_id: scope.agentId,
    }),
  });
}

export async function resetGlobalAutoQueue(
  confirmationToken = "confirm-global-reset",
): Promise<{ ok: boolean; deleted_entries: number; completed_runs: number }> {
  return request("/api/queue/reset-global", {
    method: "POST",
    body: JSON.stringify({
      confirmation_token: confirmationToken,
    }),
  });
}

// ── Pipeline Config Hierarchy (#135) ──

export interface PipelineConfigResponse {
  pipeline: import("../types").PipelineConfigFull;
  layers: { default: boolean; repo: boolean; agent: boolean };
}

export async function getDefaultPipeline(): Promise<
  import("../types").PipelineConfigFull
> {
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

export async function getRepoPipeline(
  repo: string,
): Promise<{ repo: string; pipeline_config: unknown }> {
  // Server expects /repo/{owner}/{repo} as two segments, not one encoded segment
  const [owner, name] = repo.split("/");
  return request(
    `/api/pipeline/config/repo/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`,
  );
}

export async function setRepoPipeline(
  repo: string,
  config: unknown,
): Promise<{ ok: boolean }> {
  const [owner, name] = repo.split("/");
  return request(
    `/api/pipeline/config/repo/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`,
    {
      method: "PUT",
      body: JSON.stringify({ config }),
    },
  );
}

export async function getAgentPipeline(
  agentId: string,
): Promise<{ agent_id: string; pipeline_config: unknown }> {
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
