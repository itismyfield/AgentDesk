import { lazy, Suspense, useEffect } from "react";
import { useSearchParams } from "react-router";
import type { Agent, Department, DispatchedSession } from "../types";
import type { UiLanguage } from "../i18n";
import AgentsTab from "./agent-manager/AgentsTab";
import DepartmentsTab from "./agent-manager/DepartmentsTab";
import AgentFormModal from "./agent-manager/AgentFormModal";
import AgentSetupWizard from "./agent-manager/AgentSetupWizard";
import DepartmentFormModal from "./agent-manager/DepartmentFormModal";
import {
  type AgentManagerTab,
  useAgentManagerController,
} from "./agent-manager/useAgentManagerController";
import { SessionPanel } from "./session-panel/SessionPanel";
import {
  SurfaceActionButton,
  SurfaceSection,
  SurfaceSegmentButton,
} from "./common/SurfacePrimitives";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));
const TAB_KEYS: AgentManagerTab[] = ["agents", "departments", "skills"];

interface AgentManagerViewProps {
  agents: Agent[];
  departments: Department[];
  language: UiLanguage;
  onAgentsChange: () => void;
  onDepartmentsChange: () => void;
  sessions?: DispatchedSession[];
  onAssign?: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
  activeTab?: AgentManagerTab;
  onTabChange?: (tab: AgentManagerTab) => void;
  onSelectAgent?: (agent: Agent) => void;
  showHeader?: boolean;
  showTabBar?: boolean;
  title?: string;
  subtitle?: string;
  scrollable?: boolean;
  autoOpenCreate?: boolean;
  onAutoOpenConsumed?: () => void;
}

export default function AgentManagerView({
  agents,
  departments,
  language,
  onAgentsChange,
  onDepartmentsChange,
  sessions,
  onAssign,
  activeTab,
  onTabChange,
  onSelectAgent,
  showHeader = true,
  showTabBar = true,
  title,
  subtitle,
  scrollable = true,
  autoOpenCreate = false,
  onAutoOpenConsumed,
}: AgentManagerViewProps) {
  const {
    canShowDispatch,
    deptModal,
    deptOrder,
    deptOrderDirty,
    deptTab,
    draggingDeptId,
    dragOverDeptId,
    dragOverPosition,
    dispatchOpen,
    form,
    agentModal,
    handleCancelOrder,
    handleDeleteAgent,
    handleDragEnd,
    handleDragOver,
    handleDragStart,
    handleDrop,
    handleMoveDept,
    handleSaveAgent,
    handleSaveOrder,
    handleTabChange,
    includeTests,
    isKo,
    locale,
    openCreateAgent,
    openCreateDept,
    openDuplicateAgent,
    openEditAgent,
    openEditDept,
    reorderSaving,
    resolvedTab,
    saving,
    search,
    setAgentModal,
    setDeptModal,
    setDeptTab,
    setDispatchOpen,
    setIncludeTests,
    setSearch,
    setSortMode,
    setStatusFilter,
    setupWizard,
    setSetupWizard,
    sortMode,
    sortedAgents,
    statusFilter,
    tr,
  } = useAgentManagerController({
    agents,
    departments,
    language,
    onAgentsChange,
    onDepartmentsChange,
    sessions,
    onAssign,
    activeTab,
    onTabChange,
  });
  const [searchParams, setSearchParams] = useSearchParams();

  // `/agents?tab=skills` deep links (old /meetings URL) pick the tab once, then drop the param.
  useEffect(() => {
    const requested = searchParams.get("tab") as AgentManagerTab | null;
    if (!requested || !TAB_KEYS.includes(requested)) return;
    handleTabChange(requested);
    const next = new URLSearchParams(searchParams);
    next.delete("tab");
    setSearchParams(next, { replace: true });
  }, [handleTabChange, searchParams, setSearchParams]);

  // Auto-open the setup wizard when rendered via the /agents/new route.
  useEffect(() => {
    if (!autoOpenCreate) return;
    if (!setupWizard.open) {
      openCreateAgent();
    }
    onAutoOpenConsumed?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoOpenCreate]);

  const resolvedTitle = title ?? tr("에이전트", "Agents");
  const resolvedSubtitle = subtitle ?? tr(
    "에이전트와 부서, 쓸 수 있는 스킬을 관리해요.",
    "Manage agents, departments and available skills.",
  );
  const tabItems: Array<{ key: AgentManagerTab; panelId: string; label: string }> = [
    { key: "agents", panelId: "agents-tab-panel", label: tr(`에이전트 ${sortedAgents.length}`, `Agents ${sortedAgents.length}`) },
    { key: "departments", panelId: "agents-departments-tab-panel", label: tr(`부서 ${departments.length}`, `Departments ${departments.length}`) },
    { key: "skills", panelId: "agents-skills-tab-panel", label: tr("스킬", "Skills") },
  ];
  // Aborted sessions are finished work, not something to attach to an agent.
  const liveSessions = sessions?.filter((session) => session.status !== "aborted");
  // Header actions stay the same on every tab so the header never shifts.
  const headerActions = (
    <div className="flex shrink-0 flex-wrap items-center gap-2">
      {canShowDispatch && (
        <SurfaceActionButton tone="neutral" compact onClick={() => setDispatchOpen((prev) => !prev)} aria-expanded={dispatchOpen}>
          {dispatchOpen ? tr("실행 중 세션 닫기", "Hide live sessions") : tr(`실행 중 세션 ${liveSessions?.length ?? 0}`, `Live sessions ${liveSessions?.length ?? 0}`)}
        </SurfaceActionButton>
      )}
      <SurfaceActionButton compact onClick={openCreateAgent}>
        + {tr("에이전트 추가", "Add agent")}
      </SurfaceActionButton>
    </div>
  );
  const tabBar = showTabBar ? (
    <div
      data-testid="agents-tab-bar"
      role="tablist"
      aria-label={tr("에이전트 관리 섹션", "Agent manager sections")}
      className="-mx-1 flex gap-1 overflow-x-auto px-1 [scrollbar-width:none]"
    >
      {tabItems.map((item) => (
        <SurfaceSegmentButton
          key={item.key}
          id={`agents-tab-button-${item.key}`}
          data-testid={`agents-tab-button-${item.key}`}
          role="tab"
          aria-selected={resolvedTab === item.key}
          aria-controls={item.panelId}
          active={resolvedTab === item.key}
          tone="neutral"
          onClick={() => handleTabChange(item.key)}
          className="shrink-0 whitespace-nowrap [word-break:keep-all]"
        >
          {item.label}
        </SurfaceSegmentButton>
      ))}
    </div>
  ) : null;
  return (
    <div
      data-testid="agents-page"
      className={`mx-auto w-full max-w-[88rem] min-w-0 space-y-5 overflow-x-hidden p-4 pb-8 sm:p-8 ${
        scrollable ? "h-full overflow-y-auto" : ""
      }`}
      style={{
        paddingBottom: "max(2rem, env(safe-area-inset-bottom))",
        WebkitOverflowScrolling: scrollable ? "touch" : undefined,
        touchAction: scrollable ? "pan-y" : undefined,
      }}
    >
      {showHeader ? (
        <header className="space-y-4 border-b border-th-border-subtle pb-4" data-testid="agents-page-header">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0">
              <h1 className="text-2xl font-semibold tracking-tight" style={{ color: "var(--th-text-heading)" }}>
                {resolvedTitle}
              </h1>
              <p className="mt-1 max-w-3xl text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {resolvedSubtitle}
              </p>
            </div>
            {headerActions}
          </div>
          {tabBar}
        </header>
      ) : (
        <div className="flex flex-wrap items-center justify-between gap-3">
          {tabBar}
          {headerActions}
        </div>
      )}

      {/* Tab content */}
      {resolvedTab === "agents" ? (
        <div
          id="agents-tab-panel"
          role="tabpanel"
          aria-labelledby="agents-tab-button-agents"
          className="space-y-4"
        >
          <AgentsTab
            tr={tr}
            locale={locale}
            agents={agents}
            departments={departments}
            deptTab={deptTab}
            setDeptTab={setDeptTab}
            search={search}
            setSearch={setSearch}
            statusFilter={statusFilter}
            setStatusFilter={setStatusFilter}
            sortMode={sortMode}
            setSortMode={setSortMode}
            sortedAgents={sortedAgents}
            includeTests={includeTests}
            setIncludeTests={setIncludeTests}
            onOpenAgent={(agent) =>
              onSelectAgent ? onSelectAgent(agent) : openEditAgent(agent)
            }
            onEditAgent={openEditAgent}
            onDuplicateAgent={openDuplicateAgent}
            onEditDepartment={openEditDept}
            onDeleteAgent={handleDeleteAgent}
            saving={saving}
          />
        </div>
      ) : resolvedTab === "skills" ? (
        <div id="agents-skills-tab-panel" role="tabpanel" aria-labelledby="agents-tab-button-skills">
          <Suspense fallback={null}>
            <SkillCatalogView embedded />
          </Suspense>
        </div>
      ) : (
        <div
          id="agents-departments-tab-panel"
          role="tabpanel"
          aria-labelledby="agents-tab-button-departments"
          className="space-y-3"
        >
          <div className="flex justify-end">
            <SurfaceActionButton tone="neutral" compact onClick={openCreateDept}>
              + {tr("부서 추가", "Add department")}
            </SurfaceActionButton>
          </div>
          <DepartmentsTab
            tr={tr}
            locale={locale}
            agents={agents}
            departments={departments}
            deptOrder={deptOrder}
            deptOrderDirty={deptOrderDirty}
            reorderSaving={reorderSaving}
            draggingDeptId={draggingDeptId}
            dragOverDeptId={dragOverDeptId}
            dragOverPosition={dragOverPosition}
            onSaveOrder={handleSaveOrder}
            onCancelOrder={handleCancelOrder}
            onMoveDept={handleMoveDept}
            onEditDept={openEditDept}
            onDragStart={handleDragStart}
            onDragOver={handleDragOver}
            onDrop={handleDrop}
            onDragEnd={handleDragEnd}
          />
        </div>
      )}

      {canShowDispatch && dispatchOpen && liveSessions && onAssign && (
        <SurfaceSection
          title={tr("실행 중 세션", "Live sessions")}
          description={tr(
            "에이전트에 연결되지 않은 세션이에요. 부서나 에이전트에 붙일 수 있어요.",
            "Sessions not linked to an agent yet. Attach them to a department or agent.",
          )}
          badge={`${liveSessions.length}`}
          className="rounded-[30px] p-4 sm:p-5"
          actions={(
            <SurfaceActionButton tone="neutral" compact onClick={() => setDispatchOpen(false)}>
              {tr("닫기", "Close")}
            </SurfaceActionButton>
          )}
        >
          <div className="mt-4">
            <SessionPanel
              sessions={liveSessions}
              departments={departments}
              agents={agents}
              onAssign={onAssign}
            />
          </div>
        </SurfaceSection>
      )}

      {/* Agent create/edit modal */}
      {agentModal.open && (
        <AgentFormModal
          isKo={isKo}
          locale={locale}
          tr={tr}
          form={form}
          departments={departments}
          isEdit={!!agentModal.editAgent}
          saving={saving}
          onSave={handleSaveAgent}
          onClose={() => setAgentModal({ open: false, editAgent: null })}
        />
      )}

      <AgentSetupWizard
        open={setupWizard.open}
        mode={setupWizard.mode}
        sourceAgent={setupWizard.sourceAgent}
        departments={departments}
        locale={locale}
        tr={tr}
        onClose={() => setSetupWizard({ open: false, mode: "create", sourceAgent: null })}
        onDone={() => {
          setSetupWizard({ open: false, mode: "create", sourceAgent: null });
          onAgentsChange();
        }}
      />

      {/* Department modal */}
      {deptModal.open && (
        <DepartmentFormModal
          locale={locale}
          tr={tr}
          department={deptModal.editDept}
          departments={departments}
          onSave={() => {
            setDeptModal({ open: false, editDept: null });
            onDepartmentsChange();
          }}
          onClose={() => setDeptModal({ open: false, editDept: null })}
        />
      )}
    </div>
  );
}
