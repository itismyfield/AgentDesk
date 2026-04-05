export type AutoQueueGenerateMode =
  | "priority-sort"
  | "dependency-aware"
  | "similarity-aware"
  | "pm-assisted";

interface AutoQueueGenerateApi {
  resetAutoQueue(agentId?: string | null): Promise<unknown>;
  generateAutoQueue(
    repo: string | null,
    agentId?: string | null,
    mode?: AutoQueueGenerateMode,
  ): Promise<Record<string, unknown>>;
}

interface AutoQueueResetApi {
  resetAutoQueue(agentId?: string | null): Promise<unknown>;
}

export async function generateAutoQueueForSelection(
  api: AutoQueueGenerateApi,
  repo: string | null,
  agentId: string | null | undefined,
  mode: AutoQueueGenerateMode,
): Promise<Record<string, unknown>> {
  await api.resetAutoQueue(agentId);
  return api.generateAutoQueue(repo, agentId, mode);
}

export async function resetAutoQueueForSelection(
  api: AutoQueueResetApi,
  agentId: string | null | undefined,
): Promise<unknown> {
  return api.resetAutoQueue(agentId);
}
