export interface AutoQueueResetScope {
  runId?: string | null;
  repo?: string | null;
  agentId: string;
}

interface AutoQueueGenerateApi {
  resetAutoQueue(scope?: AutoQueueResetScope): Promise<unknown>;
  generateAutoQueue(
    repo: string | null,
    agentId?: string | null,
  ): Promise<Record<string, unknown>>;
}

interface AutoQueueResetApi {
  resetAutoQueue(scope?: AutoQueueResetScope): Promise<unknown>;
}

export async function generateAutoQueueForSelection(
  api: AutoQueueGenerateApi,
  repo: string | null,
  agentId: string | null | undefined,
): Promise<Record<string, unknown>> {
  const resetAgentId = agentId?.trim();
  if (!resetAgentId) {
    throw new Error("agent_id is required for reset");
  }

  await api.resetAutoQueue({ repo, agentId: resetAgentId });
  return api.generateAutoQueue(repo, agentId);
}

export async function resetAutoQueueForSelection(
  api: AutoQueueResetApi,
  repo: string | null,
  agentId: string | null | undefined,
  runId?: string | null,
): Promise<unknown> {
  const resetAgentId = agentId?.trim();
  if (!resetAgentId) {
    throw new Error("agent_id is required for reset");
  }

  return api.resetAutoQueue({
    repo,
    agentId: resetAgentId,
    runId: runId ?? null,
  });
}
