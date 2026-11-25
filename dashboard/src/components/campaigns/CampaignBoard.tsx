import { ChevronDown, Search } from "lucide-react";
import { useId, useMemo, useState, type CSSProperties } from "react";
import { useWorkspaceAgents } from "../../contexts/WorkspaceContext";
import { Button } from "../../kit/free/button-clean";
import { AlertDialog } from "../../kit/free/dialog-clean";
import { Select } from "../../kit/free/select-clean";
import { Switch } from "../../kit/free/switch-clean";
import { TextField } from "../../kit/free/text-field-clean";
import type { Agent } from "../../types";
import AgentPersona from "../AgentPersona";
import { Cap, Chip, CountBadge, KitCard, Num, RowButton, Well, cn, type ChipTone } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import type { BoardCard, BoardColumn } from "./campaignBoardModel";
import type { TaskView } from "./CampaignOverview";
import { findAgent, findTasks, taskCounts } from "./campaignOverviewModel";
import { LABELS, LiveChip, type Tr } from "./campaignPresentation";
import { DISPOSITION_LABELS, rejectionText } from "./serverCampaignModel";

// Cards drawn per step, so a ledger with hundreds of finished tasks never draws them all at once.
const PAGE = 20;

/** The server's retry command for failed tasks, as the campaign's command list offers it. */
export interface BoardRetry { enabled: boolean; reason: string | null; onRetry: (id: string) => Promise<void> }

const CHIPS: Record<NonNullable<BoardCard["chip"]>, { tone: ChipTone; label: [string, string] }> = {
  pending: { tone: "neutral", label: LABELS.pending }, running: { tone: "info", label: LABELS.running }, blocked: { tone: "warn", label: LABELS.blocked },
  failed: { tone: "err", label: LABELS.failed }, completed: { tone: "ok", label: LABELS.completed }, skipped: { tone: "neutral", label: LABELS.skipped },
  interrupted: { tone: "warn", label: DISPOSITION_LABELS.interrupted },
};

/** The frame marks what needs someone; the chip says the card's own status word, or a block when the card names none. */
function look(task: TaskView, chip?: BoardCard["chip"]): { tone: ChipTone; label: [string, string]; style?: CSSProperties } {
  if (task.gate) return {
    tone: "warn", label: ["승인 대기", "Awaiting approval"],
    style: { borderColor: "var(--warn)", background: "color-mix(in srgb, var(--warn) 7%, var(--bg-2))" },
  };
  const borderColor = task.status === "failed" ? "var(--err)" : task.attention ? "var(--warn)"
    : task.status === "running" ? "var(--info)" : undefined;
  return { ...CHIPS[chip ?? (task.attention && task.status !== "failed" ? "blocked" : task.status)], style: borderColor ? { borderColor } : undefined };
}

function Card({ card, selected, controls, onOpen, roster, retry, tr }: {
  card: BoardCard; selected: boolean; controls?: string; onOpen: (id: string) => void; roster: Agent[] | null; retry?: BoardRetry; tr: Tr;
}) {
  const { task } = card;
  const agent = findAgent(roster, task.agent);
  const { tone, label, style } = look(task, card.chip);
  const waiting = tr("승인 API 준비 중(#8249)", "Approval API in progress (#8249)");
  const who = [agent && (agent.name_ko || agent.name), task.number].filter(Boolean).join(" · ");
  const cardStyle = selected
    ? { borderColor: "var(--accent)", background: "color-mix(in srgb, var(--accent) 7%, var(--bg-2))", "--ui-crdc-shadow-selected": "none" } as CSSProperties
    : style;
  return <KitCard as="article" data-board-card={task.id} pad="sm" selected={selected} style={{ "--ui-crdc-pad": "10px", gap: 6, ...cardStyle } as CSSProperties}>
    <button type="button" data-task-id={task.id} aria-current={selected || undefined} aria-controls={controls} onClick={() => onOpen(task.id)}
      className="m-0 grid w-full min-w-0 cursor-pointer gap-1.5 border-0 bg-transparent p-0 text-left font-inherit text-inherit">
      {who && <Cap className="flex min-w-0 items-center gap-1.5">
        {agent && <AgentPersona agent={agent} size={24} />}<span className="min-w-0 truncate tabular-nums">{who}</span>
      </Cap>}
      <strong className="line-clamp-3 text-[13px] font-semibold leading-[18px] text-[var(--fg)] [overflow-wrap:anywhere] [word-break:keep-all]">{task.title}</strong>
      {card.line && <Cap className="line-clamp-2 [overflow-wrap:anywhere]" title={card.line}>{card.line}</Cap>}
      {task.blocker && (task.attention || task.status === "pending") && <span className={`line-clamp-2 text-xs leading-4 [overflow-wrap:anywhere] ${task.attention ? "text-[var(--warn)]" : "text-[var(--fg-muted)]"}`}>
        {task.attention ? tr("막힘", "Blocked") : tr("대기 사유", "Waiting on")}: {task.blocker}
      </span>}
      <span className="flex min-w-0 flex-wrap items-center gap-1.5"><Chip data-status-chip tone={tone} dot={tone === "info"}>{tr(...label)}</Chip><LiveChip live={task.live} tr={tr} /></span>
    </button>
    {task.gate && <>
      <Button variant="primary" size="sm" disabled title={waiting} className="w-full">{tr("승인", "Approve")}</Button>
      <Cap>{waiting}</Cap>
    </>}
    {card.retry && retry && <>
      <AlertDialog title={tr("다시 시도를 준비할까요?", "Prepare a retry?")} description={tr("실패한 작업에 새 시도를 준비해요. 지난 실패 기록은 그대로 남아요.", "A new try is prepared for this failed task. Its past failures stay on record.")}
        confirmLabel={tr("다시 시도 준비", "Prepare retry")} cancelLabel={tr("취소", "Cancel")} onConfirm={() => retry.onRetry(task.id)}
        trigger={<Button variant="secondary" size="sm" disabled={!retry.enabled} className="w-full">{tr("다시 시도 준비", "Prepare retry")}</Button>} />
      {retry.reason && <Cap>{rejectionText(retry.reason, tr)}</Cap>}
    </>}
  </KitCard>;
}

const UNGROUPED_KEY = "__ungrouped__";

/** The 단계 보드 tab: instant search + group/status filters at top; finished columns (`완료` · `건너뜀`) start folded as compact summaries so active columns get full width even at 1032px tablet width. */
export default function CampaignBoard({ columns, tasks, selectedId, controls, onOpen, retry, tr }: {
  columns: BoardColumn[]; tasks: TaskView[]; selectedId: string | null; controls?: string; onOpen: (id: string) => void; retry?: BoardRetry; tr: Tr;
}) {
  const phone = useIsMobile();
  const roster = useWorkspaceAgents();
  const id = useId();
  const [hideDone, setHideDone] = useState(true);
  const [shown, setShown] = useState<Record<string, number>>({});
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const [query, setQuery] = useState("");
  const [groupFilter, setGroupFilter] = useState("all");
  const [statusFilter, setStatusFilter] = useState("all");

  const availableGroups = useMemo(() => {
    const named = new Set<string>();
    let hasUngrouped = false;
    for (const task of tasks) {
      if (task.group?.trim()) named.add(task.group.trim());
      else hasUngrouped = true;
    }
    return { list: [...named].sort((a, b) => a.localeCompare(b)), hasUngrouped };
  }, [tasks]);

  const matchingIds = useMemo(() => {
    const base = query.trim() ? findTasks(tasks, query) : tasks;
    return new Set(
      base.filter((task) => {
        if (groupFilter !== "all") {
          const g = task.group?.trim() || UNGROUPED_KEY;
          if (g !== groupFilter) return false;
        }
        if (statusFilter === "attention") return Boolean(task.attention || task.gate);
        if (statusFilter !== "all" && task.status !== statusFilter) return false;
        return true;
      }).map((task) => task.id),
    );
  }, [tasks, query, groupFilter, statusFilter]);

  const isFiltered = Boolean(query.trim() || groupFilter !== "all" || statusFilter !== "all");
  const filteredColumns = useMemo(
    () => isFiltered ? columns.map((col) => ({ ...col, cards: col.cards.filter(({ task }) => matchingIds.has(task.id)) })) : columns,
    [columns, isFiltered, matchingIds],
  );

  const counts = taskCounts(tasks);
  const pending = tasks.filter((task) => task.status === "pending").length;
  const limit = (column: BoardColumn) => shown[column.key] ?? (column.done && hideDone ? 0 : PAGE);
  // Phones open the columns with running work or something that needs someone, and finished ones once shown.
  const urgent = (column: BoardColumn) => !column.done && column.cards.some(({ task }) => task.status === "running" || task.attention || task.gate);
  const isOpen = (column: BoardColumn) => open[column.key] ?? (urgent(column) || (column.done && !hideDone));
  // Phones also list those columns first, each group keeping its order; wide screens keep the pipeline order.
  const groups = [...filteredColumns.filter(urgent), ...filteredColumns.filter((column) => !urgent(column))];
  const toggle = (column: BoardColumn) => {
    if (!isOpen(column) && limit(column) === 0) setShown((current) => ({ ...current, [column.key]: PAGE }));
    setOpen((current) => ({ ...current, [column.key]: !isOpen(column) }));
  };
  const stat = (value: number, label: string, color = "") => <span className={`whitespace-nowrap ${value ? color : "text-[var(--fg-muted)]"}`}><Num className="font-semibold">{value}</Num> {label}</span>;

  const body = (column: BoardColumn) => {
    const visible = column.cards.slice(0, limit(column));
    const rest = column.cards.length - visible.length;
    const step = Math.min(PAGE, rest);
    // Ledger tasks carry a write time, so the first ones shown are the latest; server tasks keep the definition order.
    const recent = column.done && column.cards.some(({ task }) => task.updatedAt);
    return <>
      {column.cards.length === 0 && <Cap className="block py-3 text-center">{tr("비어 있어요", "Empty")}</Cap>}
      {visible.map((card) => <Card key={card.task.id} card={card} selected={card.task.id === selectedId} controls={controls} onOpen={onOpen} roster={roster} retry={retry} tr={tr} />)}
      {rest > 0 && <Button variant="ghost" size="sm" onClick={() => setShown((current) => ({ ...current, [column.key]: visible.length + step }))}>
        {visible.length ? tr(`${step}개 더 보기`, `Show ${step} more`) : recent ? tr(`최근 ${step}개 보기`, `Show latest ${step}`) : tr(`${step}개 보기`, `Show ${step}`)}
      </Button>}
    </>;
  };

  const resetFilters = () => { setQuery(""); setGroupFilter("all"); setStatusFilter("all"); };

  return <div className="campaign-board @container/board grid min-w-0 gap-3">
    <Well className="grid min-w-0 gap-2.5 px-4 py-2.5 text-[13px]">
      <div className="flex min-w-0 flex-wrap items-center gap-x-5 gap-y-2">
        <span className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1">
          {stat(tasks.length, tr("작업", "tasks"))}{stat(counts.running, tr("진행 중", "running"))}{stat(pending, tr("대기", "waiting"))}
          {stat(counts.approvals, tr("승인 대기", "awaiting approval"), "text-[var(--warn)]")}{stat(counts.failed, tr("실패", "failed"), "text-[var(--err)]")}{stat(counts.blocked, tr("막힘", "blocked"), "text-[var(--warn)]")}
          {isFiltered && <Cap className="text-[var(--accent)]"><Num>{tr(`검색 결과 ${matchingIds.size}건`, `${matchingIds.size} matched`)}</Num></Cap>}
        </span>
        <Switch className="ml-auto" size="sm" label={tr("완료 숨기기", "Hide done")} checked={hideDone} onChange={(event) => { setHideDone(event.target.checked); setShown({}); setOpen({}); }} />
      </div>
      <div className="grid min-w-0 grid-cols-2 items-center gap-2 @[40rem]/board:grid-cols-[minmax(0,1fr)_auto_auto_auto]">
        <div className="col-span-2 @[40rem]/board:col-span-1">
          <TextField
            type="search"
            size="sm"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            aria-label={tr("보드 작업 검색", "Search board tasks")}
            placeholder={tr("제목, #번호, ID, 담당 검색…", "Search title, #number, ID, assignee…")}
            prefix={<Search aria-hidden />}
          />
        </div>
        {availableGroups.list.length > 0 && (
          <div className="min-w-[9.5rem]">
            <Select
              aria-label={tr("그룹 필터", "Filter by group")}
              size="sm"
              value={groupFilter}
              onChange={setGroupFilter}
              options={[
                { value: "all", label: tr("모든 그룹", "All groups") },
                ...availableGroups.list.map((g) => ({ value: g, label: g })),
                ...(availableGroups.hasUngrouped ? [{ value: UNGROUPED_KEY, label: tr("미분류", "Ungrouped") }] : []),
              ]}
            />
          </div>
        )}
        <div className="min-w-[9rem]">
          <Select
            aria-label={tr("상태 필터", "Filter by status")}
            size="sm"
            value={statusFilter}
            onChange={setStatusFilter}
            options={[
              { value: "all", label: tr("모든 상태", "All statuses") },
              { value: "running", label: tr("진행 중", "Running") },
              { value: "attention", label: tr("막힘 · 실패 · 승인", "Needs attention") },
              { value: "pending", label: tr("대기", "Pending") },
              { value: "completed", label: tr("완료", "Completed") },
              { value: "skipped", label: tr("건너뜀", "Skipped") },
            ]}
          />
        </div>
        {isFiltered && (
          <Button variant="ghost" size="sm" onClick={resetFilters}>{tr("초기화", "Reset")}</Button>
        )}
      </div>
    </Well>
    {phone ? <div className="grid min-w-0 gap-2">
      {groups.map((column) => <Well as="section" key={column.key} aria-label={tr(...column.label)} className="overflow-hidden">
        <RowButton aria-expanded={isOpen(column)} aria-controls={`${id}-${column.key}`} onClick={() => toggle(column)} className="gap-2 py-2.5 text-sm font-semibold">
          <ChevronDown aria-hidden className={`size-4 shrink-0 text-[var(--fg-muted)] transition-transform motion-reduce:transition-none ${isOpen(column) ? "" : "-rotate-90"}`} />
          <span className="min-w-0 flex-1 truncate">{tr(...column.label)}</span>
          <CountBadge count={column.cards.length} max={9999} />
        </RowButton>
        {isOpen(column) && <div id={`${id}-${column.key}`} className="grid min-w-0 gap-2 px-2 pb-2">{body(column)}</div>}
      </Well>)}
    </div> : <div className="flex min-w-0 flex-wrap items-start gap-2 pb-2 min-[1360px]:flex-nowrap min-[1360px]:overflow-x-auto">
      {filteredColumns.map((column) => {
        const foldedDone = column.done && limit(column) === 0;
        const step = Math.min(PAGE, column.cards.length);
        const recent = column.done && column.cards.some(({ task }) => task.updatedAt);
        return <Well as="section" key={column.key} aria-label={tr(...column.label)}
          data-folded-column={foldedDone || undefined}
          className={cn(
            "flex flex-col gap-2 p-2",
            foldedDone
              ? "w-auto min-w-[6.75rem] max-w-[10.5rem] shrink-0 @max-[46rem]/board:order-first @max-[46rem]/board:max-w-none @max-[46rem]/board:flex-row @max-[46rem]/board:items-center @max-[46rem]/board:py-1.5"
              : "min-h-[20rem] min-w-[11rem] flex-[1_1_12rem]",
          )}>
          <h3 className="m-0 flex items-center justify-between gap-2 px-1 pt-1 !text-[13px] !font-semibold text-[var(--fg)]">
            <span className="min-w-0 truncate">{tr(...column.label)}</span>
            <span className="flex shrink-0 items-center gap-1">
              <CountBadge count={column.cards.length} max={9999} />
              {column.done && !foldedDone && (
                <Button variant="ghost" size="sm" className="!h-6 !min-h-6 !px-1.5 !text-[11px]" onClick={() => setShown((current) => ({ ...current, [column.key]: 0 }))}>
                  {tr("접기", "Fold")}
                </Button>
              )}
            </span>
          </h3>
          {foldedDone ? (
            <div className="grid min-w-0 gap-1">
              {column.cards.length === 0 ? (
                <Cap className="block px-1 py-1 text-center">{tr("비어 있어요", "Empty")}</Cap>
              ) : (
                <Button
                  variant="ghost"
                  size="sm"
                  className="whitespace-nowrap [&_.aui-button-clean__label]:whitespace-nowrap"
                  aria-expanded={false}
                  onClick={() => setShown((current) => ({ ...current, [column.key]: step }))}
                >
                  {recent ? tr(`최근 ${step}개 보기`, `Show latest ${step}`) : tr(`${step}개 보기`, `Show ${step}`)}
                </Button>
              )}
            </div>
          ) : (
            <div className="grid max-h-[min(70dvh,46rem)] min-w-0 content-start gap-2 overflow-y-auto">{body(column)}</div>
          )}
        </Well>;
      })}
    </div>}
  </div>;
}
