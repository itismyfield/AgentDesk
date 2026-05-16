import BacklogIssueDetail from "./BacklogIssueDetail";
import KanbanAssignIssueModal from "./KanbanAssignIssueModal";
import KanbanBoardSurface from "./KanbanBoardSurface";
import KanbanCardDetail from "./KanbanCardDetail";
import KanbanHeaderSurface from "./KanbanHeaderSurface";
import KanbanPipelineHooksCard from "./KanbanPipelineHooksCard";
import KanbanStatusModals from "./KanbanStatusModals";

interface KanbanTabViewProps {
  ctx: any;
}

export default function KanbanTabView({ ctx }: KanbanTabViewProps) {
  const {
    activeFilterCount, actionError, advancedFilterDirty, advancedFiltersOpen, advancedFiltersRef, agentFilter, agentPipelineStages, agents, assignAssigneeId, assignBeforeReady, assignIssue, assigningIssue, auditLog, availableRepos, backlogIssues, boardColumns, bulkBusy, cancelBusy, cancelConfirm, cardTypeFilter, cards, cardsById, cardsByStatus, clearCardReviews, closingIssueNumber, compactBoard, deferredDodCount, departments, deptFilter, dispatches, editor, executeBulkCancel, filteredCards, focusMobileColumn, focusedMobileSummary, getAgentLabel, getAgentProvider, ghComments, handleAddRepo, handleAssignIssue, handleBacklogIssueOpen, handleBulkAction, handleCardOpen, handleCloseIssue, handleDirectAssignIssue, handleOpenAssignModal, handleRemoveRepo, handleUpdateCardStatus, headerOpen, initialLoading, invalidateCardActivity, loadingIssues, locale, mobileColumnStatus, mobileColumnSummaries, onDeleteCard, onPatchDeferDod, onRedispatchCard, onRetryCard, onUpdateCard, openCount, pipelineHookEntries, pipelineHookNames, readyCount, recentDoneCards, recentDoneOpen, recentDonePage, repoAgentEntries, repoBusy, repoCards, repoInput, repoSources, resetAdvancedFilters, resolveAgentFromLabels, reviewData, reviewDecisions, reviewQueueCount, scopeOpen, search, selectedAgentId, selectedAgentScopeLabel, selectedBacklogIssue, selectedCard, selectedRepo, selectedRepoLabel, selectedRepoSource, setActionError, setAdvancedFiltersOpen, setAgentFilter, setAssignAssigneeId, setAssignBeforeReady, setAssignIssue, setCancelConfirm, setCardTypeFilter, setDeferredDodPopup, setDeptFilter, setEditor, setHeaderOpen, setRecentDoneOpen, setRecentDonePage, setRedispatchReason, setRepoInput, setRetryAssigneeId, setRetryingCard, setReviewDecisions, setReviewData, setScopeOpen, setSearch, setSelectedAgentId, setSelectedBacklogIssue, setSelectedCardId, setSelectedRepo, setSettingsOpen, setShowClosed, setSignalStatusFilter, setStalledPopup, setStalledSelected, setTimelineFilter, setVerifyingDeferredDodIds, settingsOpen, showClosed, signalFilterLabel, signalStatusFilter, stalledCards, stalledPopup, stalledSelected, SURFACE_CHIP_STYLE, SURFACE_FIELD_STYLE, SURFACE_GHOST_BUTTON_STYLE, SURFACE_MODAL_CARD_STYLE, SURFACE_PANEL_STYLE, timelineFilter, totalVisible, tr, updateRepoDefaultAgent, verifyingDeferredDodIds, visibleColumns, redispatching, redispatchReason, retryAssigneeId, retryingCard, savingCard, setRedispatching, setSavingCard,
  } = ctx;

  return (
    <div
      data-testid="kanban-page"
      className="mx-auto w-full max-w-6xl min-w-0 space-y-4 overflow-x-hidden pb-24 md:pb-0"
      style={{ paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))" }}
    >
      <KanbanHeaderSurface
        ctx={{
          activeFilterCount,
          actionError,
          advancedFilterDirty,
          advancedFiltersOpen,
          advancedFiltersRef,
          agentFilter,
          agentPipelineStages,
          agents,
          availableRepos,
          bulkBusy,
          cards,
          cardTypeFilter,
          deferredDodCount,
          departments,
          deptFilter,
          getAgentLabel,
          handleAddRepo,
          handleBulkAction,
          handleRemoveRepo,
          headerOpen,
          initialLoading,
          locale,
          onPatchDeferDod,
          openCount,
          repoAgentEntries,
          repoBusy,
          repoCards,
          repoInput,
          repoSources,
          resetAdvancedFilters,
          scopeOpen,
          search,
          selectedAgentId,
          selectedAgentScopeLabel,
          selectedRepo,
          selectedRepoLabel,
          selectedRepoSource,
          setActionError,
          setAdvancedFiltersOpen,
          setAgentFilter,
          setCardTypeFilter,
          setDeferredDodPopup,
          setDeptFilter,
          setHeaderOpen,
          setRepoInput,
          setScopeOpen,
          setSearch,
          setSelectedAgentId,
          setSelectedRepo,
          setSettingsOpen,
          setShowClosed,
          setSignalStatusFilter,
          setStalledPopup,
          setStalledSelected,
          setVerifyingDeferredDodIds,
          settingsOpen,
          showClosed,
          signalFilterLabel,
          signalStatusFilter,
          stalledCards,
          stalledPopup,
          stalledSelected,
          SURFACE_CHIP_STYLE,
          SURFACE_FIELD_STYLE,
          SURFACE_PANEL_STYLE,
          totalVisible,
          tr,
          updateRepoDefaultAgent,
          verifyingDeferredDodIds,
        }}
      />

      <KanbanStatusModals
        ctx={{
          agents,
          assignBeforeReady,
          cancelBusy,
          cancelConfirm,
          cardsById,
          executeBulkCancel,
          invalidateCardActivity,
          onUpdateCard,
          setActionError,
          setAssignBeforeReady,
          setCancelConfirm,
          SURFACE_FIELD_STYLE,
          tr,
        }}
      />

      <KanbanBoardSurface
        ctx={{
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
        }}
      />

        {selectedCard && (
          <KanbanCardDetail
            card={selectedCard}
            tr={tr}
            locale={locale}
            agents={agents}
            dispatches={dispatches}
            editor={editor}
            setEditor={setEditor}
            savingCard={savingCard}
            setSavingCard={setSavingCard}
            retryingCard={retryingCard}
            setRetryingCard={setRetryingCard}
            redispatching={redispatching}
            setRedispatching={setRedispatching}
            redispatchReason={redispatchReason}
            setRedispatchReason={setRedispatchReason}
            retryAssigneeId={retryAssigneeId}
            setRetryAssigneeId={setRetryAssigneeId}
            actionError={actionError}
            setActionError={setActionError}
            auditLog={auditLog}
            ghComments={ghComments}
            reviewData={reviewData}
            setReviewData={() => {
              clearCardReviews(selectedCard.id);
            }}
            reviewDecisions={reviewDecisions}
            setReviewDecisions={setReviewDecisions}
            timelineFilter={timelineFilter}
            setTimelineFilter={setTimelineFilter}
            setCancelConfirm={setCancelConfirm}
            onClose={() => setSelectedCardId(null)}
            onUpdateCard={onUpdateCard}
            onRetryCard={onRetryCard}
            onRedispatchCard={onRedispatchCard}
            onDeleteCard={onDeleteCard}
            invalidateCardActivity={invalidateCardActivity}
          />
        )}

      <KanbanPipelineHooksCard
        ctx={{
          pipelineHookEntries,
          pipelineHookNames,
          selectedRepo,
          SURFACE_CHIP_STYLE,
          tr,
        }}
      />

      {/* #1253 (revised): "최근 완료" is now rendered right above the kanban
          columns inside the board container, so completion history sits
          next to the active board rather than below it. */}

      <KanbanAssignIssueModal
        ctx={{
          agents,
          assignAssigneeId,
          assignIssue,
          assigningIssue,
          getAgentLabel,
          handleAssignIssue,
          selectedRepo,
          setAssignAssigneeId,
          setAssignIssue,
          SURFACE_CHIP_STYLE,
          SURFACE_FIELD_STYLE,
          SURFACE_GHOST_BUTTON_STYLE,
          SURFACE_MODAL_CARD_STYLE,
          SURFACE_PANEL_STYLE,
          tr,
        }}
      />

      {selectedBacklogIssue && (
        <BacklogIssueDetail
          issue={selectedBacklogIssue}
          tr={tr}
          locale={locale}
          closingIssueNumber={closingIssueNumber}
          onClose={() => setSelectedBacklogIssue(null)}
          onCloseIssue={handleCloseIssue}
          onAssign={(issue) => {
            setAssignIssue(issue);
            const repoSource = repoSources.find((source: any) => source.repo === selectedRepo);
            setAssignAssigneeId(repoSource?.default_agent_id ?? "");
          }}
        />
      )}
    </div>
  );
}
