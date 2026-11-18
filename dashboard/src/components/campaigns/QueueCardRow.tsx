import { localeName } from "../../i18n";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import type { Agent, KanbanCard, UiLanguage } from "../../types";
import { cardTitle, coerceTimestampMs, formatAgeLabel, isManualInterventionCard, labelForStatus, priorityLabel } from "../agent-manager/kanban-utils";
import type { Tr } from "./campaignPresentation";
import { StatusMarker, cardMarker } from "./StatusMarker";
import { TaskRow } from "./TaskListParts";

const PRIORITY_TONE: Record<string, "danger" | "warning"> = { urgent: "danger", high: "warning" };

function changedAgo(ms: number, tr: Tr) {
  if (ms < 60_000) return tr("방금 변경", "changed just now");
  const age = formatAgeLabel(ms, tr);
  return tr(`${age} 전 변경`, `changed ${age} ago`);
}

/** One queued card as a list row: progress marker, number and title, then status and assignee (a second line in a
 * narrow list), with the needs-you and priority flags and the age at the end. */
export default function QueueCardRow({ card, agent, locale, selected, busy, controls, tr, onOpen, onReady }: {
  card: KanbanCard; agent: Agent | undefined; locale: UiLanguage; selected: boolean; busy: boolean;
  /** The docked detail pane the row opens into, when there is one. */
  controls?: string; tr: Tr;
  onOpen: () => void; onReady: () => void;
}) {
  const updated = coerceTimestampMs(card.updated_at);
  const age = updated === null ? null : Date.now() - updated;
  const title = cardTitle(card.title);
  const assignee = agent ? localeName(locale, agent) : card.assignee_agent_id ?? tr("담당 없음", "Unassigned");
  const needsYou = isManualInterventionCard(card);
  const priorityTone = PRIORITY_TONE[card.priority];
  return <li data-card-id={card.id} className="min-w-0">
    <TaskRow selected={selected} onClick={onOpen} aria-current={selected || undefined} aria-controls={controls}
      marker={<StatusMarker {...cardMarker(card.status)} />} number={card.github_issue_number ? `#${card.github_issue_number}` : null} title={title}
      meta={<>{labelForStatus(card.status, tr)} · {assignee}</>}
      flags={<>
        {needsYou && <Badge tone="warning" title={card.blocked_reason ?? undefined}>{tr("판단 필요", "Needs you")}</Badge>}
        {priorityTone && <Badge tone={priorityTone}>{priorityLabel(card.priority, tr)}</Badge>}
      </>}
      trailing={age !== null && <span className="shrink-0 text-right text-xs tabular-nums text-[var(--fg-faint)]" title={changedAgo(age, tr)}>
        <span aria-hidden>{age < 60_000 ? tr("방금", "now") : formatAgeLabel(age, tr)}</span><span className="sr-only">{changedAgo(age, tr)}</span>
      </span>}
      action={card.status === "backlog" && <Button variant="secondary" size="sm" disabled={busy} onClick={onReady} className="shrink-0">{tr("준비됨으로", "To ready")}</Button>} />
  </li>;
}
