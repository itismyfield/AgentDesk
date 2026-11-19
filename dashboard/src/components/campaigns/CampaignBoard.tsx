import { ChevronDown } from "lucide-react";
import { useId, useState } from "react";
import { useWorkspaceAgents } from "../../contexts/WorkspaceContext";
import { Badge as KitBadge, type BadgeTone } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { AlertDialog } from "../../kit/free/dialog-clean";
import { Switch } from "../../kit/free/switch-clean";
import type { Agent } from "../../types";
import AgentPersona from "../AgentPersona";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import type { BoardCard, BoardColumn } from "./campaignBoardModel";
import type { TaskView } from "./CampaignOverview";
import { findAgent, taskCounts } from "./campaignOverviewModel";
import { LABELS, LiveChip, type Tr } from "./campaignPresentation";
import { DISPOSITION_LABELS, rejectionText } from "./serverCampaignModel";

// Cards drawn per step, so a ledger with hundreds of finished tasks never draws them all at once.
const PAGE = 20;
const WARN_FRAME = "border-[color-mix(in_srgb,var(--warn)_60%,var(--line))]";

/** The server's retry command for failed tasks, as the campaign's command list offers it. */
export interface BoardRetry { enabled: boolean; reason: string | null; onRetry: (id: string) => Promise<void> }

const CHIPS: Record<NonNullable<BoardCard["chip"]>, { tone: BadgeTone; label: [string, string] }> = {
  pending: { tone: "neutral", label: LABELS.pending }, running: { tone: "info", label: LABELS.running }, blocked: { tone: "warning", label: LABELS.blocked },
  failed: { tone: "danger", label: LABELS.failed }, completed: { tone: "success", label: LABELS.completed }, skipped: { tone: "neutral", label: LABELS.skipped },
  interrupted: { tone: "warning", label: DISPOSITION_LABELS.interrupted },
};

/** The frame marks what needs someone; the chip says the card's own status word, or a block when the card names none. */
function look(task: TaskView, chip?: BoardCard["chip"]): { tone: BadgeTone; label: [string, string]; frame: string } {
  if (task.gate) return { tone: "warning", label: ["승인 대기", "Awaiting approval"], frame: `${WARN_FRAME} bg-[color-mix(in_srgb,var(--warn)_7%,var(--th-card-bg))]` };
  const frame = task.status === "failed" ? "border-[color-mix(in_srgb,var(--err)_60%,var(--line))]" : task.attention ? WARN_FRAME
    : task.status === "running" ? "border-[color-mix(in_srgb,var(--info)_45%,var(--line))]" : "border-[var(--line)]";
  return { ...CHIPS[chip ?? (task.attention && task.status !== "failed" ? "blocked" : task.status)], frame: `${frame} bg-[var(--th-card-bg)]` };
}

function Card({ card, selected, controls, onOpen, roster, retry, tr }: {
  card: BoardCard; selected: boolean; controls?: string; onOpen: (id: string) => void; roster: Agent[] | null; retry?: BoardRetry; tr: Tr;
}) {
  const { task } = card;
  const agent = findAgent(roster, task.agent);
  const { tone, label, frame } = look(task, card.chip);
  const waiting = tr("승인 API 준비 중(#8249)", "Approval API in progress (#8249)");
  const who = [agent && (agent.name_ko || agent.name), task.number].filter(Boolean).join(" · ");
  return <article data-board-card={task.id} className={`grid min-w-0 gap-2 rounded-[var(--r-md)] border p-2.5 ${selected ? "border-[var(--accent)] bg-[var(--accent-soft)]" : frame}`}>
    <button type="button" data-task-id={task.id} aria-current={selected || undefined} aria-controls={controls} onClick={() => onOpen(task.id)}
      className="!m-0 grid min-w-0 gap-1.5 !border-0 !bg-transparent !p-0 text-left">
      {who && <span className="flex min-w-0 items-center gap-1.5 text-[11px] text-[var(--fg-muted)]">
        {agent && <AgentPersona agent={agent} size={20} />}<span className="min-w-0 truncate tabular-nums">{who}</span>
      </span>}
      <strong className="line-clamp-3 text-[13px] font-semibold leading-[18px] text-[var(--fg)] [overflow-wrap:anywhere] [word-break:keep-all]">{task.title}</strong>
      {card.line && <span className="line-clamp-2 text-xs text-[var(--fg-muted)] [overflow-wrap:anywhere]" title={card.line}>{card.line}</span>}
      {task.blocker && (task.attention || task.status === "pending") && <span className={`line-clamp-2 text-xs [overflow-wrap:anywhere] ${task.attention ? "text-[var(--warn)]" : "text-[var(--fg-muted)]"}`}>
        {task.attention ? tr("막힘", "Blocked") : tr("대기 사유", "Waiting on")}: {task.blocker}
      </span>}
      <span className="flex min-w-0 flex-wrap items-center gap-1.5"><KitBadge tone={tone} dot live={tone === "info"}>{tr(...label)}</KitBadge><LiveChip live={task.live} tr={tr} /></span>
    </button>
    {task.gate && <>
      <Button variant="primary" size="sm" disabled title={waiting}>{tr("승인", "Approve")}</Button>
      <span className="text-[11px] text-[var(--fg-muted)]">{waiting}</span>
    </>}
    {card.retry && retry && <>
      <AlertDialog title={tr("다시 시도를 준비할까요?", "Prepare a retry?")} description={tr("실패한 작업에 새 시도를 준비해요. 지난 실패 기록은 그대로 남아요.", "A new try is prepared for this failed task. Its past failures stay on record.")}
        confirmLabel={tr("다시 시도 준비", "Prepare retry")} cancelLabel={tr("취소", "Cancel")} onConfirm={() => retry.onRetry(task.id)}
        trigger={<Button variant="secondary" size="sm" disabled={!retry.enabled}>{tr("다시 시도 준비", "Prepare retry")}</Button>} />
      {retry.reason && <span className="text-[11px] text-[var(--fg-muted)]">{rejectionText(retry.reason, tr)}</span>}
    </>}
  </article>;
}

/** The 단계 보드 tab: one column per server-given stage or ledger status; the server moves cards, nobody drags them. */
export default function CampaignBoard({ columns, tasks, selectedId, controls, onOpen, retry, tr }: {
  columns: BoardColumn[]; tasks: TaskView[]; selectedId: string | null; controls?: string; onOpen: (id: string) => void; retry?: BoardRetry; tr: Tr;
}) {
  const phone = useIsMobile();
  const roster = useWorkspaceAgents();
  const id = useId();
  const [hideDone, setHideDone] = useState(true);
  const [shown, setShown] = useState<Record<string, number>>({});
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const counts = taskCounts(tasks);
  const pending = tasks.filter((task) => task.status === "pending").length;
  const limit = (column: BoardColumn) => shown[column.key] ?? (column.done && hideDone ? 0 : PAGE);
  // Phones open the columns with running work or something that needs someone, and finished ones once shown.
  const urgent = (column: BoardColumn) => !column.done && column.cards.some(({ task }) => task.status === "running" || task.attention || task.gate);
  const isOpen = (column: BoardColumn) => open[column.key] ?? (urgent(column) || (column.done && !hideDone));
  // Phones also list those columns first, each group keeping its order; wide screens keep the pipeline order.
  const groups = [...columns.filter(urgent), ...columns.filter((column) => !urgent(column))];
  const toggle = (column: BoardColumn) => {
    if (!isOpen(column) && limit(column) === 0) setShown((current) => ({ ...current, [column.key]: PAGE }));
    setOpen((current) => ({ ...current, [column.key]: !isOpen(column) }));
  };
  const stat = (value: number, label: string, color = "") => <span className={`whitespace-nowrap ${value ? color : "text-[var(--fg-muted)]"}`}><b className="font-semibold tabular-nums">{value}</b> {label}</span>;

  const body = (column: BoardColumn) => {
    const visible = column.cards.slice(0, limit(column));
    const rest = column.cards.length - visible.length;
    const step = Math.min(PAGE, rest);
    // Ledger tasks carry a write time, so the first ones shown are the latest; server tasks keep the definition order.
    const recent = column.done && column.cards.some(({ task }) => task.updatedAt);
    return <>
      {column.cards.length === 0 && <p className="py-3 text-center text-xs text-[var(--fg-muted)]">{tr("비어 있어요", "Empty")}</p>}
      {visible.map((card) => <Card key={card.task.id} card={card} selected={card.task.id === selectedId} controls={controls} onOpen={onOpen} roster={roster} retry={retry} tr={tr} />)}
      {rest > 0 && <Button variant="ghost" size="sm" onClick={() => setShown((current) => ({ ...current, [column.key]: visible.length + step }))}>
        {visible.length ? tr(`${step}개 더 보기`, `Show ${step} more`) : recent ? tr(`최근 ${step}개 보기`, `Show latest ${step}`) : tr(`${step}개 보기`, `Show ${step}`)}
      </Button>}
    </>;
  };

  return <div className="campaign-board grid min-w-0 gap-3">
    <div className="flex min-w-0 flex-wrap items-center gap-x-5 gap-y-2 rounded-xl border border-[var(--line)] bg-[var(--th-card-bg)] px-4 py-2.5 text-[13px] text-[var(--fg)]">
      <span className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1">
        {stat(tasks.length, tr("작업", "tasks"))}{stat(counts.running, tr("진행 중", "running"))}{stat(pending, tr("대기", "waiting"))}
        {stat(counts.approvals, tr("승인 대기", "awaiting approval"), "text-[var(--warn)]")}{stat(counts.failed, tr("실패", "failed"), "text-[var(--err)]")}{stat(counts.blocked, tr("막힘", "blocked"), "text-[var(--warn)]")}
      </span>
      <Switch className="ml-auto" size="sm" label={tr("완료 숨기기", "Hide done")} checked={hideDone} onChange={(event) => { setHideDone(event.target.checked); setShown({}); setOpen({}); }} />
    </div>
    {phone ? <div className="grid min-w-0 gap-2">
      {groups.map((column) => <section key={column.key} aria-label={tr(...column.label)} className="min-w-0 rounded-xl border border-[var(--line)] bg-[color-mix(in_oklch,var(--fg)_2%,transparent)]">
        <button type="button" aria-expanded={isOpen(column)} aria-controls={`${id}-${column.key}`} onClick={() => toggle(column)}
          className="!m-0 flex w-full min-w-0 items-center gap-2 !border-0 !bg-transparent !px-3 text-left !text-sm font-semibold text-[var(--fg)]">
          <ChevronDown aria-hidden className={`size-4 shrink-0 text-[var(--fg-muted)] transition-transform motion-reduce:transition-none ${isOpen(column) ? "" : "-rotate-90"}`} />
          <span className="min-w-0 flex-1 truncate">{tr(...column.label)}</span>
          <KitBadge count={column.cards.length} max={9999} />
        </button>
        {isOpen(column) && <div id={`${id}-${column.key}`} className="grid min-w-0 gap-2 px-2 pb-2">{body(column)}</div>}
      </section>)}
    </div> : <div className="flex min-w-0 items-start gap-2 overflow-x-auto pb-2">
      {columns.map((column) => <section key={column.key} aria-label={tr(...column.label)}
        className={`flex min-h-[24rem] min-w-0 flex-col gap-2 rounded-xl border border-[var(--line)] bg-[color-mix(in_oklch,var(--fg)_2%,transparent)] p-2 ${column.done && limit(column) === 0 ? "flex-[0_0_8.5rem]" : "flex-[1_0_11rem]"}`}>
        <h3 className="flex items-center justify-between gap-2 px-1 pt-1 !text-[13px] !font-semibold text-[var(--fg)]">
          <span className="min-w-0 truncate">{tr(...column.label)}</span><KitBadge count={column.cards.length} max={9999} />
        </h3>
        <div className="grid max-h-[min(70dvh,46rem)] min-w-0 content-start gap-2 overflow-y-auto">{body(column)}</div>
      </section>)}
    </div>}
  </div>;
}
