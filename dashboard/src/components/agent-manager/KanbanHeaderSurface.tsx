import { localeName } from "../../i18n";
import {
  SurfaceActionButton,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type { KanbanCardTypeFilter, KanbanSignalStatusFilter } from "./kanban-filter-state";
import KanbanHeaderAlerts from "./KanbanHeaderAlerts";

interface KanbanHeaderSurfaceProps {
  ctx: any;
}

export default function KanbanHeaderSurface({ ctx }: KanbanHeaderSurfaceProps) {
  const {
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
    settingsOpen,
    showClosed,
    signalFilterLabel,
    signalStatusFilter,
    stalledCards,
    SURFACE_CHIP_STYLE,
    SURFACE_FIELD_STYLE,
    SURFACE_PANEL_STYLE,
    totalVisible,
    tr,
    updateRepoDefaultAgent,
  } = ctx;

  return (
    <>
      {!headerOpen && (
        <div
          data-testid="kanban-header-collapsed"
          className="flex items-center gap-2 rounded-2xl border px-3 py-2 min-w-0"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-info) 16%, var(--th-border) 84%)",
            background: "color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-info) 4%)",
          }}
        >
          <div className="-mx-1 flex min-w-0 flex-1 items-center gap-1.5 overflow-x-auto px-1">
            {repoSources.length === 0 ? (
              <span
                className="rounded-full border border-dashed px-3 py-1 text-xs shrink-0"
                style={{ color: "var(--th-text-muted)", borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)" }}
              >
                {tr("repo 없음", "No repo")}
              </span>
            ) : (
              repoSources.map((source: any) => {
                const active = selectedRepo === source.repo;
                return (
                  <button
                    key={source.id}
                    type="button"
                    onClick={() => setSelectedRepo(source.repo)}
                    className="max-w-[160px] truncate rounded-full border px-2.5 py-1 text-xs shrink-0"
                    style={{
                      borderColor: active
                        ? "color-mix(in srgb, var(--th-accent-info) 32%, var(--th-border) 68%)"
                        : "color-mix(in srgb, var(--th-border) 72%, transparent)",
                      background: active
                        ? "color-mix(in srgb, var(--th-badge-sky-bg) 72%, var(--th-card-bg) 28%)"
                        : "transparent",
                      color: active ? "var(--th-accent-info)" : "var(--th-text-primary)",
                    }}
                    title={source.repo}
                  >
                    {source.repo.split("/")[1] ?? source.repo}
                  </button>
                );
              })
            )}
          </div>
          <span className="text-[11px] shrink-0" style={{ color: "var(--th-text-muted)" }}>
            {tr(`${openCount}건`, `${openCount}`)}
          </span>
          <button
            type="button"
            data-testid="kanban-header-expand"
            onClick={() => setHeaderOpen(true)}
            className="rounded-lg border px-2 py-1 text-xs shrink-0"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
              color: "var(--th-text-secondary)",
            }}
            aria-label={tr("칸반 헤더 펼치기", "Expand kanban header")}
          >
            ▼
          </button>
        </div>
      )}
      {headerOpen && (
      <SurfaceSection
        eyebrow={tr("워크 오케스트레이션", "Work orchestration")}
        title={tr("칸반", "Kanban")}
        description={tr(
          "Repo intake, backlog triage, dispatch, review 흐름을 한 표면에서 다룹니다.",
          "Handle repo intake, backlog triage, dispatch, and review flow from one surface.",
        )}
        badge={tr(`${openCount}건 진행`, `${openCount} active`)}
        className="rounded-[30px] p-4 sm:p-5"
        style={{
          borderColor: "color-mix(in srgb, var(--th-accent-info) 16%, var(--th-border) 84%)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-info) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
        }}
      >
        <div className="mt-4 grid gap-2 sm:grid-cols-3">
          <SurfaceMetricPill
            tone="info"
            label={tr("가시 범위", "Visible scope")}
            value={initialLoading ? "…" : `${totalVisible}${tr("건", " items")}`}
            className="w-full"
          />
          <SurfaceMetricPill
            tone="accent"
            label={tr("Repo 초점", "Repo focus")}
            value={selectedRepoLabel}
            className="w-full"
          />
          <SurfaceMetricPill
            tone="neutral"
            label={tr("담당 범위", "Agent scope")}
            value={selectedAgentScopeLabel}
            className="w-full"
          />
        </div>

        {/* #1253: Controls toolbar — search + agent filter chips inline,
            active-filter badge + advanced overflow menu + settings toggle on
            the right. Replaces the Controls SurfaceSubsection so the kanban
            scan flow isn't pushed below a 2-column scope/controls grid. */}
        <div
          data-testid="kanban-toolbar"
          className="mt-4 flex flex-col gap-2 lg:flex-row lg:items-center lg:flex-wrap"
        >
          <input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder={tr("제목 / 설명 / 담당자 검색", "Search title / description / assignee")}
            className="min-w-0 rounded-xl border px-3 py-2 text-sm lg:w-[260px] lg:flex-none"
            style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
          />

          <div
            data-testid="kanban-toolbar-agent-chips"
            className="-mx-1 flex min-w-0 flex-1 items-center gap-2 overflow-x-auto px-1 pb-1 lg:pb-0"
          >
            <SurfaceSegmentButton
              onClick={() => setAgentFilter("all")}
              active={agentFilter === "all"}
              tone="accent"
              className="shrink-0"
            >
              {tr("전체 에이전트", "All agents")}
            </SurfaceSegmentButton>
            {agents.map((agent: any) => (
              <SurfaceSegmentButton
                key={agent.id}
                onClick={() => setAgentFilter(agent.id)}
                active={agentFilter === agent.id}
                tone="accent"
                className="max-w-[180px] shrink-0 truncate"
              >
                {getAgentLabel(agent.id)}
              </SurfaceSegmentButton>
            ))}
          </div>

          <div className="flex shrink-0 flex-wrap items-center gap-2">
            {stalledCards.length > 0 && (
              <SurfaceActionButton
                tone="danger"
                compact
                onClick={() => { setStalledPopup(true); setStalledSelected(new Set()); }}
                className="animate-pulse"
              >
                {tr(`정체 ${stalledCards.length}건`, `${stalledCards.length} stalled`)}
              </SurfaceActionButton>
            )}
            {deferredDodCount > 0 && (
              <SurfaceActionButton tone="warn" compact onClick={() => setDeferredDodPopup(true)}>
                {tr(`미검증 DoD ${deferredDodCount}건`, `${deferredDodCount} deferred DoD`)}
              </SurfaceActionButton>
            )}

            <span
              data-testid="kanban-toolbar-active-filter-badge"
              className="inline-flex items-center gap-1.5 rounded-full border px-3 py-1 text-xs"
              style={{
                ...SURFACE_CHIP_STYLE,
                color: activeFilterCount > 0 ? "var(--th-accent-primary)" : "var(--th-text-secondary)",
                borderColor: activeFilterCount > 0
                  ? "color-mix(in srgb, var(--th-accent-primary) 36%, var(--th-border) 64%)"
                  : SURFACE_CHIP_STYLE.borderColor,
              }}
            >
              <span className="text-[11px] uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("활성 필터", "Active")}
              </span>
              <span className="font-semibold">{activeFilterCount}</span>
            </span>

            <div className="relative" ref={advancedFiltersRef}>
              <SurfaceActionButton
                data-testid="kanban-toolbar-advanced-toggle"
                tone={advancedFilterDirty ? "info" : "neutral"}
                compact
                onClick={() => setAdvancedFiltersOpen((prev: boolean) => !prev)}
              >
                {tr("고급 필터", "Advanced")}
                {advancedFilterDirty && (
                  <span className="ml-1.5 inline-block h-1.5 w-1.5 rounded-full" style={{ background: "var(--th-accent-info)" }} />
                )}
              </SurfaceActionButton>
              {advancedFiltersOpen && (
                <div
                  data-testid="kanban-toolbar-advanced-menu"
                  className="absolute right-0 top-full z-30 mt-2 w-72 space-y-2 rounded-2xl border p-3 shadow-xl"
                  style={SURFACE_PANEL_STYLE}
                  onMouseDown={(event) => event.stopPropagation()}
                >
                  <label className="block space-y-1">
                    <span className="text-[11px] uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                      {tr("부서", "Department")}
                    </span>
                    <select
                      value={deptFilter}
                      onChange={(event) => setDeptFilter(event.target.value)}
                      className="w-full rounded-xl border px-3 py-2 text-sm"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    >
                      <option value="all">{tr("전체 부서", "All departments")}</option>
                      {departments.map((department: any) => (
                        <option key={department.id} value={department.id}>{localeName(locale, department)}</option>
                      ))}
                    </select>
                  </label>
                  <label className="block space-y-1">
                    <span className="text-[11px] uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                      {tr("카드 유형", "Card type")}
                    </span>
                    <select
                      value={cardTypeFilter}
                      onChange={(event) => setCardTypeFilter(event.target.value as KanbanCardTypeFilter)}
                      className="w-full rounded-xl border px-3 py-2 text-sm"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    >
                      <option value="all">{tr("전체 카드", "All cards")}</option>
                      <option value="issue">{tr("이슈만", "Issues only")}</option>
                      <option value="review">{tr("리뷰만", "Reviews only")}</option>
                    </select>
                  </label>
                  <label className="block space-y-1">
                    <span className="text-[11px] uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                      {tr("대시보드 신호", "Dashboard signal")}
                    </span>
                    <select
                      value={signalStatusFilter}
                      onChange={(event) => setSignalStatusFilter(event.target.value as KanbanSignalStatusFilter)}
                      className="w-full rounded-xl border px-3 py-2 text-sm"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    >
                      <option value="all">{tr("대시보드 신호 전체", "All dashboard signals")}</option>
                      <option value="review">{tr("리뷰 대기", "Review queue")}</option>
                      <option value="blocked">{tr("수동 개입", "Manual intervention")}</option>
                      <option value="requested">{tr("준비됨", "Ready")}</option>
                      <option value="stalled">{tr("진행 정체", "Stale in progress")}</option>
                    </select>
                  </label>
                  <label
                    className="flex items-center gap-2 rounded-xl border px-3 py-2 text-sm"
                    style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-secondary)" }}
                  >
                    <input
                      type="checkbox"
                      checked={showClosed}
                      onChange={(event) => setShowClosed(event.target.checked)}
                    />
                    {tr("닫힌 컬럼 표시", "Show closed columns")}
                  </label>
                  {advancedFilterDirty && (
                    <button
                      type="button"
                      onClick={resetAdvancedFilters}
                      className="w-full rounded-xl border px-3 py-2 text-xs"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-secondary)" }}
                    >
                      {tr("고급 필터 모두 해제", "Reset advanced filters")}
                    </button>
                  )}
                </div>
              )}
            </div>

            <SurfaceActionButton
              tone={settingsOpen ? "info" : "neutral"}
              compact
              onClick={() => setSettingsOpen((prev: boolean) => !prev)}
            >
              {settingsOpen ? tr("설정 접기", "Close settings") : tr("설정 열기", "Open settings")}
            </SurfaceActionButton>

            <SurfaceActionButton
              data-testid="kanban-header-collapse"
              tone="neutral"
              compact
              onClick={() => setHeaderOpen(false)}
            >
              {tr("헤더 접기 ▲", "Collapse ▲")}
            </SurfaceActionButton>
          </div>
        </div>

        {signalFilterLabel && (
          <SurfaceNotice
            tone="warn"
            className="mt-3"
            compact
            action={(
              <SurfaceActionButton
                type="button"
                tone="warn"
                compact
                onClick={() => setSignalStatusFilter("all")}
              >
                {tr("해제", "Clear")}
              </SurfaceActionButton>
            )}
          >
            <div className="text-xs leading-5">
              {tr("대시보드 포커스", "Dashboard focus")}: {signalFilterLabel}
            </div>
          </SurfaceNotice>
        )}

        {/* #1253: Scope is now a single full-width collapsible. Repo
            add/remove lives inline so users don't have to open the settings
            panel just to manage backlog repos. */}
        <SurfaceSubsection
          className="mt-3"
          title={tr("Scope", "Scope")}
          description={tr(
            "Repo 초점과 담당 범위, repo 추가/삭제를 한 화면에서 다룹니다.",
            "Switch repo focus, assignee scope, and add/remove repos from one place.",
          )}
          actions={(
            <SurfaceActionButton
              data-testid="kanban-scope-toggle"
              tone="neutral"
              compact
              onClick={() => setScopeOpen((prev: boolean) => !prev)}
            >
              {scopeOpen ? tr("접기", "Collapse") : tr("펼치기", "Expand")}
            </SurfaceActionButton>
          )}
        >
          {scopeOpen && (
            <div data-testid="kanban-scope-body" className="space-y-4">
              <div className="-mx-1 overflow-x-auto px-1 pb-1">
                <div className="flex min-w-max gap-2 sm:min-w-0 sm:flex-wrap">
                  {repoSources.length === 0 ? (
                    <span
                      className="rounded-full border border-dashed px-3 py-1.5 text-xs"
                      style={{ color: "var(--th-text-muted)", borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)" }}
                    >
                      {tr("선택된 backlog repo 없음", "No backlog repo selected")}
                    </span>
                  ) : (
                    repoSources.map((source: any) => {
                      const active = selectedRepo === source.repo;
                      return (
                        <span
                          key={source.id}
                          className="inline-flex items-center rounded-full border"
                          style={{
                            borderColor: active
                              ? "color-mix(in srgb, var(--th-accent-info) 32%, var(--th-border) 68%)"
                              : "color-mix(in srgb, var(--th-border) 72%, transparent)",
                            background: active
                              ? "color-mix(in srgb, var(--th-badge-sky-bg) 72%, var(--th-card-bg) 28%)"
                              : SURFACE_CHIP_STYLE.background,
                          }}
                        >
                          <button
                            type="button"
                            onClick={() => setSelectedRepo(source.repo)}
                            className="max-w-[180px] truncate px-3 py-1.5 text-xs"
                            style={{ color: active ? "var(--th-accent-info)" : "var(--th-text-primary)" }}
                            title={source.repo}
                          >
                            {source.repo.split("/")[1] ?? source.repo}
                          </button>
                          <button
                            type="button"
                            onClick={() => void handleRemoveRepo(source)}
                            disabled={repoBusy}
                            className="px-2 py-1.5 text-xs leading-none disabled:opacity-40"
                            style={{ color: "var(--th-text-muted)" }}
                            title={tr(`Repo 삭제: ${source.repo}`, `Remove repo: ${source.repo}`)}
                            aria-label={tr(`Repo 삭제: ${source.repo}`, `Remove repo: ${source.repo}`)}
                          >
                            ×
                          </button>
                        </span>
                      );
                    })
                  )}
                </div>
              </div>

              <div className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_auto]">
                <input
                  list="kanban-repo-options"
                  value={repoInput}
                  onChange={(event) => setRepoInput(event.target.value)}
                  placeholder={tr("owner/repo 입력 또는 선택", "Type or pick owner/repo")}
                  className="min-w-0 rounded-xl border px-3 py-2 text-sm"
                  style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                />
                <datalist id="kanban-repo-options">
                  {availableRepos.map((repo: any) => (
                    <option key={repo.nameWithOwner} value={repo.nameWithOwner} />
                  ))}
                </datalist>
                <SurfaceActionButton
                  data-testid="kanban-scope-add-repo"
                  onClick={() => void handleAddRepo()}
                  disabled={repoBusy || !repoInput.trim()}
                  tone="info"
                  className="w-full sm:w-auto"
                >
                  {repoBusy ? tr("처리 중", "Working") : tr("Repo 추가", "Add repo")}
                </SurfaceActionButton>
              </div>

              {selectedRepo && repoAgentEntries.length > 1 && (
                <div>
                  <div className="mb-2 text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                    {tr("Agent scope", "Agent scope")}
                  </div>
                  {repoAgentEntries.length <= 4 ? (
                    <div className="flex flex-wrap gap-2">
                      <SurfaceSegmentButton
                        onClick={() => setSelectedAgentId(null)}
                        active={!selectedAgentId}
                        tone="accent"
                      >
                        {tr(`전체 (${repoCards.length})`, `All (${repoCards.length})`)}
                      </SurfaceSegmentButton>
                      {repoAgentEntries.map(([aid, count]: any) => (
                        <SurfaceSegmentButton
                          key={aid}
                          onClick={() => setSelectedAgentId(aid)}
                          active={selectedAgentId === aid}
                          tone="accent"
                          className="max-w-[180px] truncate"
                        >
                          {getAgentLabel(aid)} ({count})
                        </SurfaceSegmentButton>
                      ))}
                    </div>
                  ) : (
                    <select
                      value={selectedAgentId ?? ""}
                      onChange={(event) => setSelectedAgentId(event.target.value || null)}
                      className="w-full rounded-xl border px-3 py-2 text-sm sm:max-w-[260px]"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    >
                      <option value="">{tr(`전체 (${repoCards.length})`, `All (${repoCards.length})`)}</option>
                      {repoAgentEntries.map(([aid, count]: any) => (
                        <option key={aid} value={aid}>
                          {getAgentLabel(aid)} ({count})
                        </option>
                      ))}
                    </select>
                  )}
                </div>
              )}

              {selectedAgentId && agentPipelineStages.length > 0 && (
                <SurfaceNotice tone="info" compact>
                  <div className="text-xs leading-5">
                    {tr("선택 에이전트 pipeline", "Selected agent pipeline")}: {agentPipelineStages.map((stage: any) => stage.stage_name).join(" / ")}
                  </div>
                </SurfaceNotice>
              )}
            </div>
          )}
        </SurfaceSubsection>

        {settingsOpen && (
          <SurfaceSubsection
            className="mt-3"
            title={tr("Repo Settings", "Repo Settings")}
            description={tr(
              "선택된 repo의 기본 담당자처럼 칸반 외부에서 거의 바뀌지 않는 설정만 모았습니다. Repo 추가/삭제는 위 Scope 섹션에서 직접 다룹니다.",
              "Settings that rarely change at the kanban surface — repo add/remove now lives in the Scope section above.",
            )}
          >
            {repoSources.length === 0 ? (
              <SurfaceNotice tone="neutral" compact>
                <div className="text-xs leading-5">
                  {tr("먼저 Scope에서 backlog repo를 추가하세요.", "Add a backlog repo from the Scope section first.")}
                </div>
              </SurfaceNotice>
            ) : selectedRepoSource ? (
              <label
                className="flex items-center gap-2 rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-secondary)" }}
              >
                <span className="shrink-0">{tr("기본 담당자", "Default agent")}</span>
                <select
                  value={selectedRepoSource.default_agent_id ?? ""}
                  onChange={(event) => updateRepoDefaultAgent(selectedRepoSource, event.target.value || null)}
                  className="min-w-0 flex-1 rounded-lg border px-2 py-1 text-xs"
                  style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                >
                  <option value="">{tr("없음", "None")}</option>
                  {agents.map((agent: any) => (
                    <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                  ))}
                </select>
              </label>
            ) : (
              <SurfaceNotice tone="neutral" compact>
                <div className="text-xs leading-5">
                  {tr("Scope에서 repo를 선택하면 기본 담당자를 바꿀 수 있습니다.", "Select a repo in the Scope section to change its default agent.")}
                </div>
              </SurfaceNotice>
            )}
          </SurfaceSubsection>
        )}

        {actionError && (
          <SurfaceNotice tone="danger" className="mt-4">
            {actionError}
          </SurfaceNotice>
        )}
        <KanbanHeaderAlerts ctx={ctx} />
      </SurfaceSection>
      )}
    </>
  );
}
