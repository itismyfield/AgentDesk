import type { Agent, AuditLogEntry, Department } from "../types";
import { resolveAvatarSeed } from "../lib/pixel-avatar";
import { getDashboardSession } from "./dashboardAuth";
import { readCachedSnapshot, request, type CachedApiSnapshot, type RequestOptions } from "./httpClient";

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
  return getDashboardSession();
}

// ── Agents ──

export function getCachedAgents(): CachedApiSnapshot<Agent[]> | null {
  const cached = readCachedSnapshot<{ agents: Agent[] }>("/api/agents");
  if (!cached) return null;
  return {
    data: cached.data.agents.map(normalizeAgent),
    fetchedAt: cached.fetchedAt,
  };
}

export async function getAgents(opts?: RequestOptions): Promise<Agent[]> {
  const data = await request<{ agents: Agent[] }>("/api/agents", opts);
  return data.agents.map(normalizeAgent);
}

export async function getAgent(id: string): Promise<Agent> {
  const data = await request<{ agent: Agent }>(`/api/agents/${id}`);
  return normalizeAgent(data.agent);
}

export async function createAgent(agent: Partial<Agent>): Promise<Agent> {
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
    auth_profile?: string | null;
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

export async function getDepartments(): Promise<Department[]> {
  const data = await request<{ departments: Department[] }>("/api/departments");
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
