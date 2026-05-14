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

export interface ReadyAutoQueueEntry {
  repo?: string | null;
  agentId: string;
  issueNumber: number;
}

export interface RequestGenerateGroup {
  repo: string;
  agentId: string;
  issueNumbers: number[];
}

export function buildRequestGenerateGroups(
  readyEntries: ReadyAutoQueueEntry[],
  fallbackRepo: string | null | undefined,
): RequestGenerateGroup[] {
  const byRepoAgent = new Map<string, { repo: string; agentId: string; issues: Set<number> }>();
  for (const entry of readyEntries) {
    const repo = (entry.repo || fallbackRepo || "").trim();
    const agentId = entry.agentId.trim();
    if (!repo || !agentId || !Number.isFinite(entry.issueNumber)) continue;
    const key = `${repo}\u0000${agentId}`;
    const bucket = byRepoAgent.get(key) ?? { repo, agentId, issues: new Set<number>() };
    bucket.issues.add(entry.issueNumber);
    byRepoAgent.set(key, bucket);
  }
  return [...byRepoAgent.values()]
    .map(({ repo, agentId, issues }) => ({
      repo,
      agentId,
      issueNumbers: [...issues].sort((a, b) => a - b),
    }))
    .sort((a, b) => a.repo.localeCompare(b.repo) || a.agentId.localeCompare(b.agentId));
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
