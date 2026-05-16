import { request } from "./httpClient";

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

// #2050 P3 finding 10 — server actually returns DispatchQueueEntry objects
// + active/pending group counts; previous KanbanCard[] declaration was wrong.
// Callers reading `result.dispatched[0].title` were silently undefined.
export async function activateAutoQueue(
  repo?: string | null,
  agentId?: string | null,
): Promise<{
  dispatched: DispatchQueueEntry[];
  count: number;
  active_groups?: number;
  pending_groups?: number;
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

// ── Phase-gate catalog (#2125) ──

export interface PhaseGateKind {
  id: string;
  label: { ko: string; en: string };
  description: string;
  checks: string[];
}

export interface PhaseGateCatalog {
  kinds: PhaseGateKind[];
  default_kind: string;
}

export async function getPhaseGateCatalog(): Promise<PhaseGateCatalog> {
  return request("/api/queue/phase-gates/catalog");
}

// ── Auto-queue request-generate (#2126) ──

export interface RequestGenerateAutoQueueBody {
  repo: string;
  agentId: string;
  issueNumbers: number[];
  allowedGateKinds?: string[];
  force?: boolean;
}

export interface RequestGenerateAutoQueueResponse {
  request_id: string;
  target: string;
  channel_id?: string | null;
  dispatched_at: string;
  instruction_preview?: string;
}

export async function requestGenerateAutoQueue(
  input: RequestGenerateAutoQueueBody,
): Promise<RequestGenerateAutoQueueResponse> {
  const body: Record<string, unknown> = {
    repo: input.repo,
    agent_id: input.agentId,
    issue_numbers: input.issueNumbers,
  };
  if (input.allowedGateKinds && input.allowedGateKinds.length > 0) {
    body.allowed_gate_kinds = input.allowedGateKinds;
  }
  if (typeof input.force === "boolean") {
    body.force = input.force;
  }
  return request("/api/queue/request-generate", {
    method: "POST",
    body: JSON.stringify(body),
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
