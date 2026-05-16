import type { AutoQueueThreadLink, DispatchQueueEntry as DispatchQueueEntryType } from "../../api";
import type { UiLanguage } from "../../types";
import { getQueueGroupColor } from "../../theme/statusTokens";

const REQUEST_GROUP_KEY_SEPARATOR = "\u0000";

export type ViewMode = "thread" | "all" | "agent";

export function requestGroupKey(repo: string, agentId: string): string {
  return `${repo}${REQUEST_GROUP_KEY_SEPARATOR}${agentId}`;
}

export function formatRequestGroupKey(key: string): string {
  return key.split(REQUEST_GROUP_KEY_SEPARATOR).join("/");
}

export function formatTs(
  value: number | null | undefined,
  locale: UiLanguage,
): string {
  if (!value) return "-";
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(value);
}

export function reorderPendingIds(ids: string[], fromId: string, toId: string): string[] | null {
  const fromIdx = ids.indexOf(fromId);
  const toIdx = ids.indexOf(toId);
  if (fromIdx === -1 || toIdx === -1 || fromIdx === toIdx) return null;

  const nextIds = [...ids];
  nextIds.splice(fromIdx, 1);
  nextIds.splice(toIdx, 0, fromId);
  return nextIds;
}

export function shiftPendingId(
  ids: string[],
  entryId: string,
  offset: -1 | 1,
): string[] | null {
  const fromIdx = ids.indexOf(entryId);
  if (fromIdx === -1) return null;
  const toIdx = fromIdx + offset;
  if (toIdx < 0 || toIdx >= ids.length) return null;
  return reorderPendingIds(ids, entryId, ids[toIdx]);
}

export function threadGroupColor(group: number): string {
  return getQueueGroupColor(group);
}

export function batchPhaseLabel(phase: number): string {
  return `P${phase}`;
}

export function isCompletedEntry(entry: DispatchQueueEntryType): boolean {
  return (
    entry.status === "done"
    || entry.status === "skipped"
    || entry.status === "failed"
  );
}

export function sortEntriesForDisplay(entries: DispatchQueueEntryType[]): DispatchQueueEntryType[] {
  const statusOrder: Record<string, number> = {
    dispatched: 0,
    pending: 1,
    failed: 2,
    done: 3,
    skipped: 4,
  };

  return [...entries].sort((a, b) => {
    const sa = statusOrder[a.status] ?? 1;
    const sb = statusOrder[b.status] ?? 1;
    if (sa !== sb) return sa - sb;
    return a.priority_rank - b.priority_rank;
  });
}

export function formatThreadLinkLabel(
  link: AutoQueueThreadLink,
  tr: (ko: string, en: string) => string,
): string {
  const key = (link.label || link.role || "").trim().toLowerCase();
  if (key === "work") return tr("작업", "Work");
  if (key === "review") return tr("리뷰", "Review");
  if (key === "active") return tr("활성", "Active");
  return (link.label || link.role || tr("스레드", "Thread")).trim();
}
