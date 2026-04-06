export type AutoQueueGenerateMode =
  | "priority-sort"
  | "dependency-aware"
  | "similarity-aware"
  | "pm-assisted";

export interface AutoQueueResetScope {
  runId?: string | null;
  repo?: string | null;
  agentId?: string | null;
}

interface AutoQueueGenerateApi {
  resetAutoQueue(scope?: AutoQueueResetScope): Promise<unknown>;
  generateAutoQueue(
    repo: string | null,
    agentId?: string | null,
    mode?: AutoQueueGenerateMode,
  ): Promise<Record<string, unknown>>;
}

interface AutoQueueResetApi {
  resetAutoQueue(scope?: AutoQueueResetScope): Promise<unknown>;
}

export async function generateAutoQueueForSelection(
  api: AutoQueueGenerateApi,
  repo: string | null,
  agentId: string | null | undefined,
  mode: AutoQueueGenerateMode,
): Promise<Record<string, unknown>> {
  await api.resetAutoQueue({ repo, agentId: agentId ?? null });
  return api.generateAutoQueue(repo, agentId, mode);
}

export async function resetAutoQueueForSelection(
  api: AutoQueueResetApi,
  repo: string | null,
  agentId: string | null | undefined,
  runId?: string | null,
): Promise<unknown> {
  return api.resetAutoQueue({
    repo,
    agentId: agentId ?? null,
    runId: runId ?? null,
  });
}
