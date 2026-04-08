import type { DragEvent } from "react";
import type { GitHubIssue, KanbanRepoSource } from "../../api";
import type {
  KanbanCard,
  KanbanCardStatus,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import KanbanColumn from "./KanbanColumn";
import {
  COLUMN_DEFS,
  TERMINAL_STATUSES,
  QA_STATUSES,
} from "./kanban-utils";

interface ColumnDef {
  status: KanbanCardStatus;
  labelKo: string;
  labelEn: string;
  accent: string;
}

interface KanbanBoardProps {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  selectedRepo: string;
  compactBoard: boolean;
  showClosed: boolean;
  initialLoading: boolean;
  loadingIssues: boolean;
  hasQaCards: boolean;
  effectiveColumnDefs: ColumnDef[];
  visibleColumns: ColumnDef[];
  cardsByStatus: Map<KanbanCardStatus, KanbanCard[]>;
  backlogIssues: GitHubIssue[];
  recentDoneCards: KanbanCard[];
  recentDonePage: number;
  recentDoneOpen: boolean;
  mobileColumnStatus: KanbanCardStatus;
  draggingCardId: string | null;
  dragOverStatus: KanbanCardStatus | null;
  dragOverCardId: string | null;
  closingIssueNumber: number | null;
  assigningIssue: boolean;
  dispatchMap: Map<string, TaskDispatch>;
  dispatches: TaskDispatch[];
  repoSources: KanbanRepoSource[];
  setRecentDonePage: React.Dispatch<React.SetStateAction<number>>;
  setRecentDoneOpen: React.Dispatch<React.SetStateAction<boolean>>;
  setMobileColumnStatus: React.Dispatch<React.SetStateAction<KanbanCardStatus>>;
  setDraggingCardId: React.Dispatch<React.SetStateAction<string | null>>;
  setDragOverStatus: React.Dispatch<React.SetStateAction<KanbanCardStatus | null>>;
  setDragOverCardId: React.Dispatch<React.SetStateAction<string | null>>;
  setActionError: React.Dispatch<React.SetStateAction<string | null>>;
  getAgentLabel: (agentId: string | null | undefined) => string;
  resolveAgentFromLabels: (labels: Array<{ name: string; color: string }>) => import("../../types").Agent | null;
  onCardClick: (cardId: string) => void;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
  onDrop: (targetStatus: KanbanCardStatus, beforeCardId: string | null, event: DragEvent<HTMLElement>) => Promise<void>;
  onCloseIssue: (issue: GitHubIssue) => Promise<void>;
  onDirectAssignIssue: (issue: GitHubIssue, agentId: string) => Promise<void>;
  onOpenAssignModal: (issue: GitHubIssue) => void;
  onUpdateCardStatus: (cardId: string, targetStatus: KanbanCardStatus) => Promise<void>;
}

export default function KanbanBoard({
  tr,
  locale,
  selectedRepo,
  compactBoard,
  showClosed,
  initialLoading,
  loadingIssues,
  hasQaCards,
  effectiveColumnDefs,
  visibleColumns,
  cardsByStatus,
  backlogIssues,
  recentDoneCards,
  recentDonePage,
  recentDoneOpen,
  mobileColumnStatus,
  draggingCardId,
  dragOverStatus,
  dragOverCardId,
  closingIssueNumber,
  assigningIssue,
  dispatchMap,
  dispatches,
  repoSources,
  setRecentDonePage,
  setRecentDoneOpen,
  setMobileColumnStatus,
  setDraggingCardId,
  setDragOverStatus,
  setDragOverCardId,
  setActionError,
  getAgentLabel,
  resolveAgentFromLabels,
  onCardClick,
  onBacklogIssueClick,
  onDrop,
  onCloseIssue,
  onDirectAssignIssue,
  onOpenAssignModal,
  onUpdateCardStatus,
}: KanbanBoardProps) {
  return (
    <>
      {/* ── Recent completions ── */}
      {recentDoneCards.length > 0 && (() => {
        const PAGE_SIZE = 10;
        const totalPages = Math.ceil(recentDoneCards.length / PAGE_SIZE);
        const page = Math.min(recentDonePage, totalPages - 1);
        const pageCards = recentDoneCards.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
        return (
          <section className="rounded-2xl border px-4 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "rgba(34,197,94,0.04)" }}>
            <button
              onClick={() => setRecentDoneOpen((v) => !v)}
              className="flex w-full items-center gap-2 text-left"
            >
              <span className="text-xs font-semibold uppercase" style={{ color: "var(--th-text-muted)" }}>
                {tr("최근 완료", "Recent Completions")}
              </span>
              <span className="rounded-full px-1.5 py-0.5 text-xs font-bold" style={{ background: "rgba(34,197,94,0.18)", color: "#4ade80" }}>
                {recentDoneCards.length}
              </span>
              <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
                {recentDoneOpen ? "▲" : "▼"}
              </span>
            </button>
            {recentDoneOpen && (
              <div className="mt-2 space-y-1.5">
                {pageCards.map((card) => {
                  const statusDef = COLUMN_DEFS.find((c) => c.status === card.status);
                  const agentName = getAgentLabel(card.assignee_agent_id);
                  const completedDate = card.completed_at
                    ? new Date(card.completed_at).toLocaleDateString(locale === "ko" ? "ko-KR" : "en-US", { month: "short", day: "numeric" })
                    : "";
                  return (
                    <button
                      key={card.id}
                      onClick={() => onCardClick(card.id)}
                      className="flex w-full items-center gap-2 rounded-xl px-3 py-2 text-left text-sm transition-colors hover:brightness-125"
                      style={{ background: "rgba(148,163,184,0.06)" }}
                    >
                      <span
                        className="shrink-0 rounded-full px-1.5 py-0.5 text-xs font-semibold"
                        style={{ background: `${statusDef?.accent ?? "#22c55e"}22`, color: statusDef?.accent ?? "#22c55e" }}
                      >
                        {card.status === "done" ? tr("완료", "Done") : tr("취소", "Cancelled")}
                      </span>
                      {card.github_issue_number && (
                        <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>#{card.github_issue_number}</span>
                      )}
                      <span className="min-w-0 flex-1 truncate" style={{ color: "var(--th-text-primary)" }}>{card.title}</span>
                      <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>{agentName}</span>
                      <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>{completedDate}</span>
                    </button>
                  );
                })}
                {totalPages > 1 && (
                  <div className="flex items-center justify-center gap-3 pt-1">
                    <button
                      disabled={page === 0}
                      onClick={() => setRecentDonePage((p) => Math.max(0, p - 1))}
                      className="rounded px-2 py-0.5 text-xs disabled:opacity-30"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      ← {tr("이전", "Prev")}
                    </button>
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {page + 1} / {totalPages}
                    </span>
                    <button
                      disabled={page >= totalPages - 1}
                      onClick={() => setRecentDonePage((p) => Math.min(totalPages - 1, p + 1))}
                      className="rounded px-2 py-0.5 text-xs disabled:opacity-30"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {tr("다음", "Next")} →
                    </button>
                  </div>
                )}
              </div>
            )}
          </section>
        );
      })()}

      {!selectedRepo ? (
        <div className="rounded-2xl border border-dashed px-4 py-10 text-center text-sm" style={{ borderColor: "rgba(148,163,184,0.22)", color: "var(--th-text-muted)" }}>
          {tr("repo를 추가하면 repo별 backlog와 칸반을 볼 수 있습니다.", "Add a repo to view its backlog and board.")}
        </div>
      ) : (
        <div className="space-y-3">
          {compactBoard && (
            <>
              <div className="flex gap-2 overflow-x-auto pb-1">
                {effectiveColumnDefs.filter((column) => (showClosed || !TERMINAL_STATUSES.has(column.status)) && (!QA_STATUSES.has(column.status) || hasQaCards)).map((column) => (
                  <button
                    key={column.status}
                    onClick={() => setMobileColumnStatus(column.status)}
                    className="shrink-0 rounded-full px-3 py-1.5 text-xs font-medium border"
                    style={{
                      borderColor: mobileColumnStatus === column.status ? `${column.accent}88` : "rgba(148,163,184,0.24)",
                      backgroundColor: mobileColumnStatus === column.status ? `${column.accent}22` : "rgba(255,255,255,0.04)",
                      color: mobileColumnStatus === column.status ? "white" : "var(--th-text-secondary)",
                    }}
                  >
                    {tr(column.labelKo, column.labelEn)}
                  </button>
                ))}
              </div>
              <div className="rounded-xl border px-3 py-2 text-xs" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", backgroundColor: "var(--th-overlay-subtle)" }}>
                {tr("모바일에서는 카드를 탭해 상세 패널에서 상태를 변경하세요.", "On mobile, tap a card and change status in the detail sheet.")}
              </div>
            </>
          )}

          <div className={compactBoard ? "" : "pb-2"} style={compactBoard ? undefined : { overflowX: "auto", overflowY: "visible" }}>
            <div className={compactBoard ? "space-y-4" : "flex items-start gap-4 min-w-max"}>
              {visibleColumns.map((column) => {
                const columnCards = cardsByStatus.get(column.status) ?? [];
                const backlogCount = column.status === "backlog" ? columnCards.length + backlogIssues.length : columnCards.length;
                return (
                  <KanbanColumn
                    key={column.status}
                    column={column}
                    columnCards={columnCards}
                    backlogIssues={backlogIssues}
                    backlogCount={backlogCount}
                    tr={tr}
                    locale={locale}
                    compactBoard={compactBoard}
                    initialLoading={initialLoading}
                    loadingIssues={loadingIssues}
                    draggingCardId={draggingCardId}
                    dragOverStatus={dragOverStatus}
                    dragOverCardId={dragOverCardId}
                    closingIssueNumber={closingIssueNumber}
                    assigningIssue={assigningIssue}
                    dispatchMap={dispatchMap}
                    dispatches={dispatches}
                    repoSources={repoSources}
                    selectedRepo={selectedRepo}
                    getAgentLabel={getAgentLabel}
                    resolveAgentFromLabels={resolveAgentFromLabels}
                    onCardClick={onCardClick}
                    onBacklogIssueClick={onBacklogIssueClick}
                    onSetDraggingCardId={setDraggingCardId}
                    onSetDragOverStatus={setDragOverStatus}
                    onSetDragOverCardId={setDragOverCardId}
                    onDrop={onDrop}
                    onCloseIssue={onCloseIssue}
                    onDirectAssignIssue={onDirectAssignIssue}
                    onOpenAssignModal={onOpenAssignModal}
                    onUpdateCardStatus={onUpdateCardStatus}
                    onSetActionError={setActionError}
                  />
                );
              })}
            </div>
          </div>
        </div>
      )}
    </>
  );
}
