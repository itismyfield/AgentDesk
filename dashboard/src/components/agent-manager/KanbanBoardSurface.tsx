import AutoQueuePanel from "./AutoQueuePanel";
import KanbanColumn, { type KanbanColumnProps } from "./KanbanColumn";
import { SurfaceCard, SurfaceEmptyState } from "../common/SurfacePrimitives";
import { COLUMN_DEFS } from "./kanban-utils";

interface KanbanBoardSurfaceProps {
  ctx: any;
}

export default function KanbanBoardSurface({ ctx }: KanbanBoardSurfaceProps) {
  const {
    agents,
    assigningIssue,
    backlogIssues,
    cardsByStatus,
    closingIssueNumber,
    compactBoard,
    focusMobileColumn,
    focusedMobileSummary,
    getAgentLabel,
    getAgentProvider,
    handleBacklogIssueOpen,
    handleCardOpen,
    handleCloseIssue,
    handleDirectAssignIssue,
    handleOpenAssignModal,
    handleUpdateCardStatus,
    initialLoading,
    loadingIssues,
    locale,
    mobileColumnStatus,
    mobileColumnSummaries,
    recentDoneCards,
    recentDoneOpen,
    recentDonePage,
    resolveAgentFromLabels,
    selectedAgentId,
    selectedRepo,
    setActionError,
    setRecentDoneOpen,
    setRecentDonePage,
    setSelectedCardId,
    tr,
    visibleColumns,
  } = ctx;

  return (
    <>
      {selectedRepo && (() => {
        // #2128: ready 카드(`status = requested`) 중 assignee + GH 이슈 번호가 있는 것만
        // request-generate 후보로 전달. 그 외 카드는 활성화 카운트에서도 빠진다.
        const readyCards = cardsByStatus.get("requested") ?? [];
        const readyEntries = readyCards
          .filter((card: any) => Boolean(card.assignee_agent_id) && Boolean(card.github_issue_number))
          .map((card: any) => ({
            repo: card.github_repo || selectedRepo,
            agentId: card.assignee_agent_id as string,
            issueNumber: card.github_issue_number as number,
          }));
        return (
          <AutoQueuePanel
            tr={tr}
            locale={locale}
            agents={agents}
            selectedRepo={selectedRepo}
            selectedAgentId={selectedAgentId}
            readyEntries={readyEntries}
          />
        );
      })()}

      <div className="min-w-0">
        <div className="min-w-0 space-y-4">
          {!selectedRepo ? (
            <SurfaceEmptyState
              className="rounded-[24px] px-4 py-10 text-center text-sm"
              style={{ borderColor: "rgba(148,163,184,0.22)", color: "var(--th-text-muted)" }}
            >
              {tr("repo를 추가하면 repo별 backlog와 칸반을 볼 수 있습니다.", "Add a repo to view its backlog and board.")}
            </SurfaceEmptyState>
          ) : (
            <div className="space-y-3">
              {recentDoneCards.length > 0 && (() => {
                const PAGE_SIZE = 10;
                const totalPages = Math.ceil(recentDoneCards.length / PAGE_SIZE);
                const page = Math.min(recentDonePage, totalPages - 1);
                const pageCards = recentDoneCards.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
                return (
                  <SurfaceCard
                    className="rounded-[24px] px-4 py-3"
                    style={{
                      borderColor: "color-mix(in srgb, var(--th-accent-primary) 16%, var(--th-border) 84%)",
                      background: "color-mix(in srgb, var(--th-badge-emerald-bg) 56%, var(--th-card-bg) 44%)",
                    }}
                  >
                    <button
                      onClick={() => setRecentDoneOpen((v: boolean) => !v)}
                      className="flex w-full items-center gap-2 text-left"
                    >
                      <span className="text-xs font-semibold uppercase" style={{ color: "var(--th-text-muted)" }}>
                        {tr("최근 완료", "Recent Completions")}
                      </span>
                      <span className="rounded-full px-1.5 py-0.5 text-[10px] font-bold" style={{ background: "rgba(34,197,94,0.18)", color: "#4ade80" }}>
                        {recentDoneCards.length}
                      </span>
                      <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {recentDoneOpen ? "▲" : "▼"}
                      </span>
                    </button>
                    {recentDoneOpen && (
                      <div className="mt-2 space-y-1.5">
                        {pageCards.map((card: any) => {
                          const statusDef = COLUMN_DEFS.find((c) => c.status === card.status);
                          const agentName = getAgentLabel(card.assignee_agent_id);
                          const completedDate = card.completed_at
                            ? new Date(card.completed_at).toLocaleDateString(locale === "ko" ? "ko-KR" : "en-US", { month: "short", day: "numeric" })
                            : "";
                          return (
                            <button
                              key={card.id}
                              onClick={() => setSelectedCardId(card.id)}
                              className="flex w-full items-center gap-2 rounded-xl px-3 py-2 text-left text-sm transition-colors hover:brightness-125"
                              style={{ background: "rgba(148,163,184,0.06)" }}
                            >
                              <span
                                className="shrink-0 rounded-full px-1.5 py-0.5 text-[10px] font-semibold"
                                style={{ background: `${statusDef?.accent ?? "#22c55e"}22`, color: statusDef?.accent ?? "#22c55e" }}
                              >
                                {card.status === "done" ? tr("완료", "Done") : tr("취소", "Cancelled")}
                              </span>
                              {card.github_issue_number && (
                                <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>#{card.github_issue_number}</span>
                              )}
                              <span className="min-w-0 flex-1 truncate" style={{ color: "var(--th-text-primary)" }}>{card.title}</span>
                              <span className="shrink-0 text-[11px]" style={{ color: "var(--th-text-muted)" }}>{agentName}</span>
                              <span className="shrink-0 text-[11px]" style={{ color: "var(--th-text-muted)" }}>{completedDate}</span>
                            </button>
                          );
                        })}
                        {totalPages > 1 && (
                          <div className="flex items-center justify-center gap-3 pt-1">
                            <button
                              disabled={page === 0}
                              onClick={() => setRecentDonePage((p: number) => Math.max(0, p - 1))}
                              className="rounded px-2 py-0.5 text-xs disabled:opacity-30"
                              style={{ color: "var(--th-text-muted)" }}
                            >
                              ← {tr("이전", "Prev")}
                            </button>
                            <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              {page + 1} / {totalPages}
                            </span>
                            <button
                              disabled={page >= totalPages - 1}
                              onClick={() => setRecentDonePage((p: number) => Math.min(totalPages - 1, p + 1))}
                              className="rounded px-2 py-0.5 text-xs disabled:opacity-30"
                              style={{ color: "var(--th-text-muted)" }}
                            >
                              {tr("다음", "Next")} →
                            </button>
                          </div>
                        )}
                      </div>
                    )}
                  </SurfaceCard>
                );
              })()}
              {/* #1253: at the mobile breakpoint we go back to single-column
                  minimap mode — the pills at the top are the only navigator,
                  and the board renders just the focused column at full width.
                  Previously every column rendered side-by-side via horizontal
                  scroll even on mobile, which forced two-finger panning to
                  get to anything past "backlog" and broke the at-a-glance
                  scan the minimap is supposed to provide. */}
              {compactBoard && (
                <div className="space-y-3">
                  <div
                    data-testid="kanban-mobile-minimap"
                    className="flex min-w-0 gap-2 overflow-x-auto pb-1"
                  >
                    {mobileColumnSummaries.map(({ column, count }: any) => (
                      <button
                        key={column.status}
                        type="button"
                        data-testid={`kanban-mobile-summary-${column.status}`}
                        onClick={() => focusMobileColumn(column.status, false)}
                        className="min-w-[6.5rem] rounded-2xl border px-3 py-2 text-left"
                        style={{
                          borderColor: mobileColumnStatus === column.status ? `${column.accent}88` : "rgba(148,163,184,0.24)",
                          backgroundColor: mobileColumnStatus === column.status ? `${column.accent}22` : "rgba(255,255,255,0.04)",
                          color: mobileColumnStatus === column.status ? "white" : "var(--th-text-secondary)",
                        }}
                      >
                        <div className="text-[11px] font-semibold uppercase" style={{ color: mobileColumnStatus === column.status ? "white" : "var(--th-text-muted)" }}>
                          {tr(column.labelKo, column.labelEn)}
                        </div>
                        <div className="mt-1 text-lg font-semibold">{count}</div>
                      </button>
                    ))}
                  </div>
                  <div
                    className="rounded-xl border px-3 py-2 text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.18)", color: "var(--th-text-muted)", backgroundColor: "rgba(15,23,42,0.35)" }}
                  >
                    {focusedMobileSummary
                      ? tr(
                          `${focusedMobileSummary.column.labelKo} 컬럼만 표시 중입니다. 다른 lane은 위 미니맵에서 전환하세요.`,
                          `Showing the ${focusedMobileSummary.column.labelEn} column only. Switch lanes from the minimap above.`,
                        )
                      : tr(
                          "위 미니맵에서 lane을 선택하면 해당 컬럼만 단독으로 표시됩니다.",
                          "Pick a lane in the minimap above to render that column on its own.",
                        )}
                  </div>
                </div>
              )}

              {compactBoard ? (
                <div
                  className="pb-2"
                  data-testid="kanban-board-scroll"
                  style={{ overflowX: "hidden", overflowY: "visible" }}
                >
                  {(() => {
                    const focusedColumn = visibleColumns.find((column: any) => column.status === mobileColumnStatus)
                      ?? visibleColumns[0];
                    if (!focusedColumn) return null;
                    const columnCards = cardsByStatus.get(focusedColumn.status) ?? [];
                    const backlogCount = focusedColumn.status === "backlog"
                      ? columnCards.length + backlogIssues.length
                      : columnCards.length;
                    return (
                      <div id={`kanban-mobile-${focusedColumn.status}`} className="w-full">
                        <KanbanColumn
                          column={focusedColumn as KanbanColumnProps["column"]}
                          columnCards={columnCards}
                          backlogIssues={backlogIssues}
                          backlogCount={backlogCount}
                          tr={tr}
                          compactBoard
                          initialLoading={initialLoading}
                          loadingIssues={loadingIssues}
                          closingIssueNumber={closingIssueNumber}
                          assigningIssue={assigningIssue}
                          getAgentLabel={getAgentLabel}
                          getAgentProvider={getAgentProvider}
                          resolveAgentFromLabels={resolveAgentFromLabels}
                          onCardClick={handleCardOpen}
                          onBacklogIssueClick={handleBacklogIssueOpen}
                          onCloseIssue={handleCloseIssue}
                          onDirectAssignIssue={handleDirectAssignIssue}
                          onOpenAssignModal={handleOpenAssignModal}
                          onUpdateCardStatus={handleUpdateCardStatus}
                          onSetActionError={setActionError}
                        />
                      </div>
                    );
                  })()}
                </div>
              ) : (
                <div
                  className="pb-2"
                  data-testid="kanban-board-scroll"
                  style={{ overflowX: "hidden", overflowY: "visible" }}
                >
                  <div
                    className="grid items-start gap-3"
                    style={{
                      gridTemplateColumns: `repeat(${visibleColumns.length}, minmax(0, 1fr))`,
                    }}
                  >
                    {visibleColumns.map((column: any) => {
                      const columnCards = cardsByStatus.get(column.status) ?? [];
                      const backlogCount = column.status === "backlog" ? columnCards.length + backlogIssues.length : columnCards.length;
                      return (
                        <div key={column.status} id={`kanban-mobile-${column.status}`} className="min-w-0">
                          <KanbanColumn
                            column={column as KanbanColumnProps["column"]}
                            columnCards={columnCards}
                            backlogIssues={backlogIssues}
                            backlogCount={backlogCount}
                            tr={tr}
                            compactBoard={false}
                            initialLoading={initialLoading}
                            loadingIssues={loadingIssues}
                            closingIssueNumber={closingIssueNumber}
                            assigningIssue={assigningIssue}
                            getAgentLabel={getAgentLabel}
                            getAgentProvider={getAgentProvider}
                            resolveAgentFromLabels={resolveAgentFromLabels}
                            onCardClick={handleCardOpen}
                            onBacklogIssueClick={handleBacklogIssueOpen}
                            onCloseIssue={handleCloseIssue}
                            onDirectAssignIssue={handleDirectAssignIssue}
                            onOpenAssignModal={handleOpenAssignModal}
                            onUpdateCardStatus={handleUpdateCardStatus}
                            onSetActionError={setActionError}
                          />
                        </div>
                      );
                    })}
                  </div>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </>
  );
}
