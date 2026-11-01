import { request } from "./httpClient";

export interface AutoQueueThreadLink {
  role: string;
  label: string;
  channel_id?: string | null;
  thread_id: string;
  url?: string | null;
}

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
