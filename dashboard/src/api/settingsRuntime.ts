import type { CompanySettings, VoiceConfigPutBody, VoiceConfigResponse } from "../types";
import { clearCachedGet, readCachedGet, request, type CachedGetEntry } from "./httpClient";

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
  clearCachedGet("/api/voice/config");
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

// #2050 P3 finding 18 — normalize optional fields so consumers don't have
// to defensively `??` everything. Adds default `providers: []` /
// `degraded_reasons: []` / `dispatch_outbox` zero shape so UI render paths
// can rely on consistent typing under transient server omissions.
function normalizeHealth(raw: unknown): HealthResponse {
  const source = (raw ?? {}) as Partial<HealthResponse>;
  return {
    status: source.status ?? "unhealthy",
    version: source.version,
    uptime_secs: source.uptime_secs,
    global_active: source.global_active,
    global_finalizing: source.global_finalizing,
    deferred_hooks: source.deferred_hooks,
    queue_depth: source.queue_depth,
    watcher_count: source.watcher_count,
    recovery_duration: source.recovery_duration,
    degraded_reasons: Array.isArray(source.degraded_reasons)
      ? source.degraded_reasons
      : [],
    providers: Array.isArray(source.providers) ? source.providers : [],
    db: source.db,
    dashboard: source.dashboard,
    outbox_age: source.outbox_age,
    dispatch_outbox: source.dispatch_outbox ?? {
      pending: 0,
      retrying: 0,
      permanent_failures: 0,
      oldest_pending_age: 0,
    },
  };
}

export async function getHealth(): Promise<HealthResponse> {
  const raw = await request<unknown>("/api/health");
  return normalizeHealth(raw);
}

export function getCachedHealth(): CachedGetEntry<HealthResponse> | null {
  const cached = readCachedGet<unknown>("/api/health");
  if (!cached) return null;
  return {
    data: normalizeHealth(cached.data),
    fetchedAt: cached.fetchedAt,
  };
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
