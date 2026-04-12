import {
  type Dispatch,
  type SetStateAction,
  useEffect,
  useMemo,
  useState,
} from "react";

import type { GitHubIssue } from "../../api";
import type { KanbanCard, KanbanCardStatus } from "../../types";
import KanbanColumn, {
  BacklogIssueCard,
  KanbanCardArticle,
} from "./KanbanColumn";
import {
  BACKLOG_PAGE_SIZE,
  buildKanbanBacklogEntries,
  paginateKanbanBacklogEntries,
} from "./kanban-board-layout";
import {
  QA_STATUSES,
  TERMINAL_STATUSES,
} from "./kanban-utils";

interface ColumnDef {
  status: KanbanCardStatus;
  labelKo: string;
  labelEn: string;
  accent: string;
}

interface KanbanBoardProps {
  tr: (ko: string, en: string) => string;
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
  setRecentDonePage: Dispatch<SetStateAction<number>>;
  setRecentDoneOpen: Dispatch<SetStateAction<boolean>>;
  setMobileColumnStatus: Dispatch<SetStateAction<KanbanCardStatus>>;
  onCardClick: (cardId: string) => void;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
}

export default function KanbanBoard({
  tr,
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
  setRecentDonePage,
  setRecentDoneOpen,
  setMobileColumnStatus,
  onCardClick,
  onBacklogIssueClick,
}: KanbanBoardProps) {
  const [backlogPage, setBacklogPage] = useState(0);

  const backlogCards = cardsByStatus.get("backlog") ?? [];
  const backlogEntries = useMemo(
    () => buildKanbanBacklogEntries(backlogCards, backlogIssues),
    [backlogCards, backlogIssues],
  );
  const pagedBacklog = useMemo(
    () => paginateKanbanBacklogEntries(backlogEntries, backlogPage),
    [backlogEntries, backlogPage],
  );
  const backlogPageLabel = pagedBacklog.pageCount > 0
    ? `${pagedBacklog.page + 1} / ${pagedBacklog.pageCount}`
    : null;

  useEffect(() => {
    setBacklogPage(0);
  }, [selectedRepo]);

  useEffect(() => {
    if (pagedBacklog.page !== backlogPage) {
      setBacklogPage(pagedBacklog.page);
    }
  }, [backlogPage, pagedBacklog.page]);

  const boardVisibleColumns = useMemo(
    () => effectiveColumnDefs.filter((column) =>
      column.status !== "backlog"
      && (showClosed || !TERMINAL_STATUSES.has(column.status))
      && (!QA_STATUSES.has(column.status) || hasQaCards),
    ),
    [effectiveColumnDefs, hasQaCards, showClosed],
  );
  const desktopBoardColumns = useMemo(
    () => visibleColumns.filter((column) => column.status !== "backlog"),
    [visibleColumns],
  );
  const activeMobileColumn =
    boardVisibleColumns.find((column) => column.status === mobileColumnStatus)
    ?? boardVisibleColumns[0]
    ?? null;
  const boardColumns = compactBoard
    ? (activeMobileColumn ? [activeMobileColumn] : [])
    : desktopBoardColumns;
  const boardSummaryColumns = boardVisibleColumns.filter(
    (column) => !TERMINAL_STATUSES.has(column.status),
  );

  return (
    <>
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
                {tr("완료 일감", "Completed Work")}
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
                  const cardNumber = card.github_issue_number ? `#${card.github_issue_number}` : `#${card.id.slice(0, 6)}`;
                  return (
                    <button
                      key={card.id}
                      onClick={() => onCardClick(card.id)}
                      className="flex w-full items-center gap-2 rounded-xl px-3 py-2 text-left text-sm transition-colors hover:brightness-125"
                      style={{ background: "rgba(148,163,184,0.06)" }}
                    >
                      <span className="shrink-0 text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                        {cardNumber}
                      </span>
                      <span className="min-w-0 flex-1 truncate" style={{ color: "var(--th-text-primary)" }}>{card.title}</span>
                      {card.github_issue_url && (
                        <a
                          href={card.github_issue_url}
                          target="_blank"
                          rel="noreferrer"
                          className="shrink-0 text-xs hover:underline"
                          onClick={(event) => event.stopPropagation()}
                          style={{ color: "#93c5fd" }}
                        >
                          GH
                        </a>
                      )}
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
        <div className="space-y-4">
          <section
            className="rounded-2xl border p-4 space-y-4"
            style={{
              borderColor: "rgba(148,163,184,0.24)",
              backgroundColor: "var(--th-bg-surface)",
            }}
          >
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-xs font-semibold uppercase" style={{ color: "var(--th-text-muted)" }}>
                {tr("백로그", "Backlog")}
              </span>
              <span className="rounded-full px-1.5 py-0.5 text-xs font-bold" style={{ background: "rgba(100,116,139,0.18)", color: "#cbd5f5" }}>
                {initialLoading ? "…" : backlogEntries.length}
              </span>
              <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  `한 페이지에 ${BACKLOG_PAGE_SIZE}개씩 보여줍니다.`,
                  `Showing ${BACKLOG_PAGE_SIZE} items per page.`,
                )}
              </span>
              {backlogPageLabel && (
                <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {backlogPageLabel}
                </span>
              )}
            </div>

            {loadingIssues && (
              <div className="rounded-xl border border-dashed px-3 py-3 text-xs" style={{ borderColor: "rgba(148,163,184,0.18)", color: "var(--th-text-muted)" }}>
                {tr("GitHub backlog를 동기화하는 중입니다.", "Syncing GitHub backlog...")}
              </div>
            )}

            {pagedBacklog.items.length > 0 ? (
              <div className="grid gap-3 md:grid-cols-2 2xl:grid-cols-3">
                {pagedBacklog.items.map((entry) => (
                  entry.kind === "card" ? (
                    <KanbanCardArticle
                      key={entry.key}
                      card={entry.card}
                      onCardClick={onCardClick}
                      metaBadge={tr("카드", "Card")}
                    />
                  ) : (
                    <BacklogIssueCard
                      key={entry.key}
                      issue={entry.issue}
                      onBacklogIssueClick={onBacklogIssueClick}
                      metaBadge={tr("GitHub", "GitHub")}
                    />
                  )
                ))}
              </div>
            ) : (
              <div className="rounded-xl border border-dashed px-3 py-6 text-center text-sm" style={{ borderColor: "rgba(148,163,184,0.22)", color: "var(--th-text-muted)" }}>
                {loadingIssues || initialLoading
                  ? tr("백로그를 불러오는 중입니다.", "Loading backlog...")
                  : tr("현재 표시할 백로그가 없습니다.", "No backlog items to show.")}
              </div>
            )}

            {pagedBacklog.pageCount > 1 && (
              <div className="flex items-center justify-center gap-3">
                <button
                  type="button"
                  disabled={pagedBacklog.page === 0}
                  onClick={() => setBacklogPage((page) => Math.max(0, page - 1))}
                  className="rounded px-2 py-1 text-xs disabled:opacity-30"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  ← {tr("이전", "Prev")}
                </button>
                <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {backlogPageLabel}
                </span>
                <button
                  type="button"
                  disabled={pagedBacklog.page >= pagedBacklog.pageCount - 1}
                  onClick={() => setBacklogPage((page) => Math.min(pagedBacklog.pageCount - 1, page + 1))}
                  className="rounded px-2 py-1 text-xs disabled:opacity-30"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("다음", "Next")} →
                </button>
              </div>
            )}
          </section>

          {boardSummaryColumns.length > 0 && (
            <div className="flex flex-wrap gap-2">
              {boardSummaryColumns.map((column) => {
                const count = cardsByStatus.get(column.status)?.length ?? 0;
                return (
                  <div
                    key={`summary-${column.status}`}
                    className="rounded-full border px-3 py-1.5 text-xs"
                    style={{
                      borderColor: `${column.accent}44`,
                      backgroundColor: `${column.accent}18`,
                      color: "var(--th-text-primary)",
                    }}
                  >
                    {tr(column.labelKo, column.labelEn)} {count}
                  </div>
                );
              })}
            </div>
          )}

          {compactBoard && boardVisibleColumns.length > 0 && (
            <>
              <div className="flex gap-2 overflow-x-auto pb-1">
                {boardVisibleColumns.map((column) => {
                  const isActive = activeMobileColumn?.status === column.status;
                  return (
                    <button
                      key={column.status}
                      onClick={() => setMobileColumnStatus(column.status)}
                      className="shrink-0 rounded-full px-3 py-1.5 text-xs font-medium border"
                      style={{
                        borderColor: isActive ? `${column.accent}88` : "rgba(148,163,184,0.24)",
                        backgroundColor: isActive ? `${column.accent}22` : "rgba(255,255,255,0.04)",
                        color: isActive ? "white" : "var(--th-text-secondary)",
                      }}
                    >
                      {tr(column.labelKo, column.labelEn)}
                    </button>
                  );
                })}
              </div>
              <div className="rounded-xl border px-3 py-2 text-xs" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", backgroundColor: "var(--th-overlay-subtle)" }}>
                {tr("모바일에서는 카드를 탭해 상세 패널에서 상태를 변경하세요.", "On mobile, tap a card and change status in the detail sheet.")}
              </div>
            </>
          )}

          {boardColumns.length > 0 ? (
            <div
              className={compactBoard ? "space-y-4" : "grid gap-4"}
              style={compactBoard ? undefined : {
                gridTemplateColumns: "repeat(auto-fit, minmax(min(100%, 17rem), 1fr))",
              }}
            >
              {boardColumns.map((column) => {
                const columnCards = cardsByStatus.get(column.status) ?? [];
                return (
                  <KanbanColumn
                    key={column.status}
                    column={column}
                    columnCards={columnCards}
                    backlogIssues={[]}
                    backlogCount={columnCards.length}
                    tr={tr}
                    compactBoard={compactBoard}
                    initialLoading={initialLoading}
                    loadingIssues={false}
                    onCardClick={onCardClick}
                    onBacklogIssueClick={onBacklogIssueClick}
                  />
                );
              })}
            </div>
          ) : (
            <div className="rounded-2xl border border-dashed px-4 py-10 text-center text-sm" style={{ borderColor: "rgba(148,163,184,0.22)", color: "var(--th-text-muted)" }}>
              {tr("현재 필터에서 보여줄 진행 컬럼이 없습니다.", "No active board columns match the current filters.")}
            </div>
          )}
        </div>
      )}
    </>
  );
}
