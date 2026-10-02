import type {
  AutoQueueResetScope,
  GenerateAutoQueueResponse,
  GenerateSkip,
} from "../../api/autoQueue";

interface AutoQueueResetApi {
  resetAutoQueue(scope: AutoQueueResetScope): Promise<unknown>;
}

export interface ReadyAutoQueueEntry {
  repo?: string | null;
  agentId: string;
  issueNumber: number;
}

export interface GenerateGroup {
  repo: string;
  agentId: string;
  issueNumbers: number[];
}

export function buildGenerateGroups(
  readyEntries: ReadyAutoQueueEntry[],
  fallbackRepo: string | null | undefined,
): GenerateGroup[] {
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

/** "#5 already dispatched, #7 filtered" for the cards a generate call left out. */
export function describeGenerateSkips(
  result: GenerateAutoQueueResponse,
  tr: (ko: string, en: string) => string,
): string {
  const groups: Array<[GenerateSkip[] | undefined, string]> = [
    [result.skipped_due_to_active_dispatch, tr("이미 실행 중", "already dispatched")],
    [result.skipped_due_to_dependency, tr("선행 이슈 미완료", "dependency not done")],
    [result.skipped_due_to_filter, tr("대상 아님", "filtered")],
  ];
  return groups
    .flatMap(([skips, label]) =>
      (skips ?? []).map((skip) => `#${skip.issue_number} ${skip.reason ?? label}`),
    )
    .join(", ");
}

/**
 * Resets the shown run in one call, since `run_id` pins every server write.
 * Returns `false` without calling the API when there is no run.
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
