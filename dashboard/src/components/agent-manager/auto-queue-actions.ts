import type { AutoQueueResetScope } from "../../api/autoQueue";

interface AutoQueueResetApi {
  resetAutoQueue(scope: AutoQueueResetScope): Promise<unknown>;
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

/**
 * Resets exactly the run the panel shows, in one call: `run_id` pins every
 * server write, so a per-agent loop only repeats it (#6243). Returns `false`
 * without calling the API when there is no run to reset.
 */
export async function resetAutoQueueForSelection(
  api: AutoQueueResetApi,
  repo: string | null,
  agentId: string | null | undefined,
  runId: string | null | undefined,
): Promise<boolean> {
  if (!runId) return false;
  await api.resetAutoQueue({ runId, repo, agentId: agentId?.trim() || undefined });
  return true;
}
