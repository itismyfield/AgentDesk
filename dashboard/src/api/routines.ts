import { request } from "./httpClient";

export type RoutineStatus = "enabled" | "paused" | "detached";
export type RoutineRunStatus =
  | "running"
  | "succeeded"
  | "failed"
  | "skipped"
  | "paused"
  | "interrupted";

export interface RoutineRecord {
  id: string;
  agent_id: string | null;
  script_ref: string;
  name: string;
  status: RoutineStatus | string;
  execution_strategy: "fresh" | "persistent" | string;
  schedule: string | null;
  next_due_at: string | null;
  last_run_at: string | null;
  last_result: string | null;
  checkpoint: unknown | null;
  discord_thread_id: string | null;
  timeout_secs: number | null;
  in_flight_run_id: string | null;
  created_at: string;
  updated_at: string;
}

export interface RoutineRunRecord {
  id: string;
  routine_id: string;
  status: RoutineRunStatus | string;
  action: string | null;
  turn_id: string | null;
  lease_expires_at: string | null;
  result_json: unknown | null;
  error: string | null;
  discord_log_status: string | null;
  discord_log_error: string | null;
  discord_message_id: string | null;
  discord_log_sections: unknown;
  started_at: string;
  finished_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface RoutineMetrics {
  routines_total: number;
  routines_enabled: number;
  routines_paused: number;
  routines_detached: number;
  runs_total: number;
  runs_running: number;
  runs_succeeded: number;
  runs_failed: number;
  runs_skipped: number;
  runs_paused: number;
  runs_interrupted: number;
  runs_error: number;
  avg_latency_ms: number | null;
}

export async function getRoutines(filters?: {
  agentId?: string;
  status?: RoutineStatus;
}): Promise<RoutineRecord[]> {
  const params = new URLSearchParams();
  if (filters?.agentId) params.set("agent_id", filters.agentId);
  if (filters?.status) params.set("status", filters.status);
  const query = params.toString();
  const data = await request<{ routines: RoutineRecord[] }>(
    `/api/routines${query ? `?${query}` : ""}`,
  );
  return data.routines;
}

export async function getRoutineRuns(
  routineId: string,
  limit = 20,
): Promise<RoutineRunRecord[]> {
  const data = await request<{ runs: RoutineRunRecord[] }>(
    `/api/routines/${encodeURIComponent(routineId)}/runs?limit=${limit}`,
  );
  return data.runs;
}

export async function getRoutineMetrics(filters?: {
  agentId?: string;
  since?: string;
}): Promise<RoutineMetrics> {
  const params = new URLSearchParams();
  if (filters?.agentId) params.set("agent_id", filters.agentId);
  if (filters?.since) params.set("since", filters.since);
  const query = params.toString();
  const data = await request<{ metrics: RoutineMetrics }>(
    `/api/routines/metrics${query ? `?${query}` : ""}`,
  );
  return data.metrics;
}
