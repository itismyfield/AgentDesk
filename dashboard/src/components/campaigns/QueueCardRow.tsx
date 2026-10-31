import type { Agent, KanbanCard, UiLanguage } from "../../types";
import { localeName } from "../../i18n";
import { cardTitle, coerceTimestampMs, formatAgeLabel, labelForStatus, priorityLabel } from "../agent-manager/kanban-utils";
import type { Tr } from "./campaignPresentation";

const PRIORITY_TONE: Record<string, string> = {
  urgent: "text-[var(--th-accent-danger)]",
  high: "text-[var(--th-badge-amber-text)]",
};

function changedAgo(ms: number, tr: Tr) {
  if (ms < 60_000) return tr("방금 변경", "changed just now");
  const age = formatAgeLabel(ms, tr);
  return tr(`${age} 전 변경`, `changed ${age} ago`);
}

/** One queued card: number and title first, then repo, assignee, priority and age as one muted line. */
export default function QueueCardRow({ card, agent, locale, selected, busy, tr, onOpen, onReady }: {
  card: KanbanCard; agent: Agent | undefined; locale: UiLanguage; selected: boolean; busy: boolean; tr: Tr;
  onOpen: () => void; onReady: () => void;
}) {
  const updated = coerceTimestampMs(card.updated_at);
  return <li data-card-id={card.id} className={`campaign-queue-row flex min-w-0 items-start gap-2 border-t border-th-border-subtle px-3 py-2 first:border-t-0 ${selected ? "bg-[color-mix(in_srgb,var(--th-accent-info)_10%,var(--th-card-bg))]" : ""}`}>
    <button type="button" onClick={onOpen} aria-current={selected || undefined}
      className="grid min-w-0 flex-1 gap-1 !border-0 !bg-transparent !px-0 text-left">
      <span className="flex min-w-0 items-baseline gap-2">
        {card.github_issue_number && <span className="shrink-0 text-xs tabular-nums text-th-text-muted">#{card.github_issue_number}</span>}
        <strong className="min-w-0 text-sm font-medium leading-6 [overflow-wrap:anywhere]">{cardTitle(card.title)}</strong>
      </span>
      <span className="flex min-w-0 flex-wrap gap-x-2 gap-y-0.5 text-xs text-th-text-muted">
        {card.status !== "backlog" && <span className="font-medium text-th-text-primary">{labelForStatus(card.status, tr)}</span>}
        {card.github_repo && <span className="min-w-0 truncate">{card.github_repo.split("/").pop()}</span>}
        <span className="min-w-0 truncate">{agent ? localeName(locale, agent) : card.assignee_agent_id ?? tr("담당 없음", "Unassigned")}</span>
        <span className={PRIORITY_TONE[card.priority] ?? ""}>{priorityLabel(card.priority, tr)}</span>
        {updated !== null && <span className="tabular-nums">{changedAgo(Date.now() - updated, tr)}</span>}
      </span>
    </button>
    {card.status === "backlog" && <button type="button" disabled={busy} onClick={onReady}
      className="shrink-0 whitespace-nowrap !text-xs">{tr("준비됨으로", "To ready")}</button>}
  </li>;
}
