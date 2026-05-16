import { closestCenter, DndContext } from "@dnd-kit/core";
import { SortableContext, verticalListSortingStrategy } from "@dnd-kit/sortable";
import type { DispatchQueueEntry as DispatchQueueEntryType } from "../../api";
import AutoQueuePanelHeader from "./AutoQueuePanelHeader";
import { AgentSubQueue, SortableEntryRow } from "./AutoQueueSortableRows";
import { createAutoQueuePhaseRenderers } from "./AutoQueuePhaseRenderers";
import { batchPhaseLabel, formatTs, sortEntriesForDisplay } from "./auto-queue-panel-utils";

type AgentQueueStats = {
  dispatched: number;
  done: number;
  failed: number;
  pending: number;
  skipped: number;
};

export default function AutoQueuePanelView({ ctx }: { ctx: any }) {
  const {
    agentStats,
    allDrag,
    allEntriesSorted,
    currentBatchPhase,
    entries,
    entriesByAgent,
    entriesByThreadGroup,
    expanded,
    getAgentLabel,
    handleEntryStatusUpdate,
    handleReorder,
    handleRunAction,
    hasBatchPhases,
    hasThreadGroups,
    locale,
    maxConcurrent,
    pendingCount,
    phaseSections,
    run,
    setViewMode,
    totalCount,
    tr,
    viewMode,
  } = ctx;
  const { renderPhaseBlock, renderPhaseGateIndicator, renderThreadGroupCard } = createAutoQueuePhaseRenderers(ctx);
  const agentStatsEntries = Object.entries(agentStats as Record<string, AgentQueueStats>);
  const allDisplayEntries = allEntriesSorted as DispatchQueueEntryType[];
  const phaseSectionEntries = phaseSections as Array<[number, DispatchQueueEntryType[]]>;
  const threadGroupEntries = Array.from(entriesByThreadGroup.entries()) as Array<[number, DispatchQueueEntryType[]]>;
  const agentEntriesList = Array.from(entriesByAgent.entries()) as Array<[string, DispatchQueueEntryType[]]>;

  return (
    <section
      className="rounded-2xl border px-3 py-2 sm:px-4 sm:py-2.5 space-y-2"
      style={{
        borderColor: run ? "rgba(16,185,129,0.35)" : "rgba(148,163,184,0.22)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      <AutoQueuePanelHeader ctx={ctx} />

      {/* Expanded: queue entries */}
      {expanded && (
        <div className="space-y-3">
          {/* View mode toggle + Agent summary chips */}
          {totalCount > 0 && (
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <div className="flex flex-wrap gap-1.5">
                {agentStatsEntries.map(([agentId, stats]) => (
                  <div
                    key={agentId}
                    className="inline-flex items-center gap-1.5 text-xs px-2 py-1 rounded-lg border"
                    style={{
                      borderColor: "rgba(148,163,184,0.18)",
                      backgroundColor: "var(--th-overlay-medium)",
                    }}
                  >
                    <span style={{ color: "var(--th-text-secondary)" }}>
                      {getAgentLabel(agentId)}
                    </span>
                    {stats.dispatched > 0 && (
                      <span style={{ color: "#fbbf24" }}>
                        {stats.dispatched}
                      </span>
                    )}
                    {stats.pending > 0 && (
                      <span style={{ color: "#94a3b8" }}>{stats.pending}</span>
                    )}
                    <span style={{ color: "#4ade80" }}>{stats.done}</span>
                    {stats.failed > 0 && (
                      <span style={{ color: "#f87171" }}>!{stats.failed}</span>
                    )}
                    {stats.skipped > 0 && (
                      <span style={{ color: "#6b7280" }}>-{stats.skipped}</span>
                    )}
                  </div>
                ))}
              </div>

              <div className="flex items-center gap-2 flex-wrap justify-end">
                {pendingCount > 1 && (
                  <span
                    className="text-xs"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {tr("순서 변경: 드래그 또는 ↑↓", "Reorder: drag or ↑↓")}
                  </span>
                )}

                {/* View mode toggle */}
                {(Object.keys(agentStats).length > 1 || hasThreadGroups) && (
                  <div
                    className="inline-flex rounded-lg border overflow-hidden"
                    style={{ borderColor: "rgba(148,163,184,0.22)" }}
                  >
                    {hasThreadGroups && (
                      <button
                        onClick={() => setViewMode("thread")}
                        className="text-xs px-2 py-1 transition-colors"
                        style={{
                          backgroundColor:
                            viewMode === "thread"
                              ? "rgba(16,185,129,0.2)"
                              : "transparent",
                          color:
                            viewMode === "thread"
                              ? "#10b981"
                              : "var(--th-text-muted)",
                        }}
                      >
                        {tr("스레드", "Thread")}
                      </button>
                    )}
                    <button
                      onClick={() => setViewMode("all")}
                      className="text-xs px-2 py-1 transition-colors"
                        style={{
                          backgroundColor:
                            viewMode === "all"
                            ? "rgba(16,185,129,0.2)"
                            : "transparent",
                          color:
                            viewMode === "all"
                            ? "#10b981"
                            : "var(--th-text-muted)",
                        }}
                    >
                      {tr("전체", "All")}
                    </button>
                    {Object.keys(agentStats).length > 1 && (
                      <button
                        onClick={() => setViewMode("agent")}
                        className="text-xs px-2 py-1 transition-colors"
                        style={{
                          backgroundColor:
                            viewMode === "agent"
                              ? "rgba(16,185,129,0.2)"
                              : "transparent",
                          color:
                            viewMode === "agent"
                              ? "#10b981"
                              : "var(--th-text-muted)",
                        }}
                      >
                        {tr("에이전트별", "By Agent")}
                      </button>
                    )}
                  </div>
                )}
              </div>
            </div>
          )}

          {/* ── All view: merged list with drag & drop ── */}
          {viewMode === "all" && (
            <DndContext
              sensors={allDrag.sensors}
              collisionDetection={closestCenter}
              onDragStart={allDrag.handleDragStart}
              onDragOver={allDrag.handleDragOver}
              onDragEnd={allDrag.handleDragEnd}
              onDragCancel={allDrag.handleDragCancel}
            >
              <SortableContext items={allDrag.pendingIds} strategy={verticalListSortingStrategy}>
                {hasBatchPhases ? (
                  <div className="space-y-3">
                    {phaseSectionEntries.map(([phase, phaseEntries]) => (
                      <div key={`phase-section-${phase}`}>
                        {renderPhaseBlock(
                          phase,
                          phaseEntries,
                          <div className="space-y-1">
                            {sortEntriesForDisplay(phaseEntries).map((entry, idx) => (
                              <div key={entry.id} className="flex items-center gap-1">
                                <span
                                  className="text-xs px-1.5 py-0.5 rounded shrink-0 max-w-[60px] truncate"
                                  style={{
                                    backgroundColor: "rgba(139,92,246,0.12)",
                                    color: "#a78bfa",
                                  }}
                                >
                                  {getAgentLabel(entry.agent_id)}
                                </span>
                                <div className="flex-1 min-w-0">
                                  <SortableEntryRow
                                    entry={entry}
                                    idx={idx}
                                    tr={tr}
                                    locale={locale}
                                    onUpdateStatus={handleEntryStatusUpdate}
                                    drag={allDrag}
                                    showThreadGroup={hasThreadGroups}
                                    showBatchPhase={hasBatchPhases}
                                  />
                                </div>
                              </div>
                            ))}
                          </div>,
                        )}
                        {renderPhaseGateIndicator(phase)}
                      </div>
                    ))}
                  </div>
                ) : (
                  <div className="space-y-1">
                    {allDisplayEntries.map((entry, idx) => (
                      <div key={entry.id} className="flex items-center gap-1">
                        <span
                          className="text-xs px-1.5 py-0.5 rounded shrink-0 max-w-[60px] truncate"
                          style={{
                            backgroundColor: "rgba(139,92,246,0.12)",
                            color: "#a78bfa",
                          }}
                        >
                          {getAgentLabel(entry.agent_id)}
                        </span>
                        <div className="flex-1 min-w-0">
                          <SortableEntryRow
                            entry={entry}
                            idx={idx}
                            tr={tr}
                            locale={locale}
                            onUpdateStatus={handleEntryStatusUpdate}
                            drag={allDrag}
                            showThreadGroup={hasThreadGroups}
                          />
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </SortableContext>
            </DndContext>
          )}

          {/* ── Thread group view ── */}
          {viewMode === "thread" && (
            <div className="space-y-3">
              {hasThreadGroups && (
                <div
                  className="flex items-center gap-2 text-xs px-1"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  <span>
                    {tr(
                      `동시 ${maxConcurrent}그룹 실행`,
                      `${maxConcurrent} concurrent groups`,
                    )}
                  </span>
                  <span style={{ color: "#4ade80" }}>
                    {entriesByThreadGroup.size}
                    {tr("그룹", " groups")}
                  </span>
                </div>
              )}
              {hasBatchPhases
                ? phaseSectionEntries.map(([phase, phaseEntries]) => {
                    const groupsInPhase = new Map<number, DispatchQueueEntryType[]>();
                    for (const entry of phaseEntries) {
                      const groupNum = entry.thread_group ?? 0;
                      const list = groupsInPhase.get(groupNum) ?? [];
                      list.push(entry);
                      groupsInPhase.set(groupNum, list);
                    }
                    return (
                      <div key={`phase-section-${phase}`}>
                        {renderPhaseBlock(
                          phase,
                          phaseEntries,
                          <div className="space-y-2">
                            {Array.from(groupsInPhase.entries())
                              .sort(([left], [right]) => left - right)
                              .map(([groupNum, groupEntries]) =>
                                renderThreadGroupCard(groupNum, groupEntries),
                              )}
                          </div>,
                        )}
                        {renderPhaseGateIndicator(phase)}
                      </div>
                    );
                  })
                : threadGroupEntries
                    .sort(([left], [right]) => left - right)
                    .map(([groupNum, groupEntries]) =>
                      renderThreadGroupCard(groupNum, groupEntries),
                    )}
            </div>
          )}

          {/* ── Agent view: grouped by agent with per-agent drag & drop ── */}
          {viewMode === "agent" &&
            agentEntriesList.map(
              ([agentId, agentEntries]) => (
                <AgentSubQueue
                  key={agentId}
                  agentId={agentId}
                  agentEntries={agentEntries}
                  getAgentLabel={getAgentLabel}
                  tr={tr}
                  locale={locale}
                  onUpdateStatus={handleEntryStatusUpdate}
                  onReorder={handleReorder}
                  showBatchPhase={hasBatchPhases}
                />
              ),
            )}

          {/* Run metadata */}
          {run && (
            <div
              className="flex flex-wrap gap-x-4 gap-y-1 text-xs px-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              <span>AI: {run.ai_model ?? "-"}</span>
              <span>
                {tr("생성", "Created")}: {formatTs(run.created_at, locale)}
              </span>
              {hasThreadGroups && (
                <span>
                  {tr("동시", "Concur")}: {maxConcurrent}
                  {tr("그룹", "grp")}
                </span>
              )}
              {hasBatchPhases && (
                <span>
                  {tr("활성 phase", "Active phase")}:{" "}
                  {batchPhaseLabel(currentBatchPhase ?? 0)}
                </span>
              )}
              <span>
                {tr("타임아웃", "Timeout")}: {run.timeout_minutes}
                {tr("분", "m")}
              </span>
              {run.status !== "completed" && (
                <button
                  onClick={() => void handleRunAction(run, "completed")}
                  className="underline"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("큐 종료", "End queue")}
                </button>
              )}
            </div>
          )}

          {entries.length === 0 && !run && (
            <div
              className="text-xs text-center py-3"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr(
                "활성 큐 없음. 준비됨 상태의 카드가 있으면 큐를 생성할 수 있습니다.",
                "No active queue. Generate one when there are ready cards.",
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
