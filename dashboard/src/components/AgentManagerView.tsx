import { useEffect, useState } from "react";
import type { Agent, Department, DispatchedSession, KanbanCard } from "../types";
import type { UiLanguage } from "../i18n";
import AgentsTab from "./agent-manager/AgentsTab";
import BacklogTab from "./agent-manager/BacklogTab";
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

interface AgentManagerViewProps {
  agents: Agent[];
  departments: Department[];
  language: UiLanguage;
  officeId?: string | null;
  onAgentsChange: () => void;
  onDepartmentsChange: () => void;
  sessions?: DispatchedSession[];
  onAssign?: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
  activeTab?: AgentManagerTab;
  onTabChange?: (tab: AgentManagerTab) => void;
  kanbanCards?: KanbanCard[];
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
  officeId,
  onAgentsChange,
  onDepartmentsChange,
  sessions,
  onAssign,
  activeTab,
  onTabChange,
  kanbanCards = [],
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
    confirmArchiveId,
    confirmDeleteId,
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
    handleArchiveAgent,
    handleDeleteAgent,
    handleDragEnd,
    handleDragOver,
    handleDragStart,
    handleDrop,
    handleMoveDept,
    handleSaveAgent,
    handleSaveOrder,
    handleTabChange,
    handleUnarchiveAgent,
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
    setConfirmArchiveId,
    setConfirmDeleteId,
    setDeptModal,
    setDeptTab,
    setDispatchOpen,
    setSearch,
    setSortMode,
    setStatusFilter,
    setupWizard,
    setSetupWizard,
    sortMode,
    sortedAgents,
    spriteMap,
    statusFilter,
    tr,
  } = useAgentManagerController({
    agents,
    departments,
    language,
    officeId,
    onAgentsChange,
    onDepartmentsChange,
    sessions,
    onAssign,
    activeTab,
    onTabChange,
  });
  const [isDesktopViewport, setIsDesktopViewport] = useState(() =>
    typeof window === "undefined" ? true : window.matchMedia("(min-width: 640px)").matches,
  );

  useEffect(() => {
    if (typeof window === "undefined") {
      return undefined;
    }

    const mediaQuery = window.matchMedia("(min-width: 640px)");
    const updateViewport = () => setIsDesktopViewport(mediaQuery.matches);

    updateViewport();

    if (typeof mediaQuery.addEventListener === "function") {
      mediaQuery.addEventListener("change", updateViewport);
      return () => mediaQuery.removeEventListener("change", updateViewport);
    }

    mediaQuery.addListener(updateViewport);
    return () => mediaQuery.removeListener(updateViewport);
  }, []);

  // Auto-open the setup wizard when rendered via the /agents/new route.
  useEffect(() => {
    if (!autoOpenCreate) return;
    if (!setupWizard.open) {
      openCreateAgent();
    }
    onAutoOpenConsumed?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoOpenCreate]);

  const defaultTitle = tr("에이전트", "Agents");
  const resolvedTitle = title ?? defaultTitle;
  const resolvedSubtitle = subtitle
    ?? tr(
      "에이전트 / 부서 / 백로그 이슈를 한 곳에서 관리합니다.",
      "Manage agents, departments, and backlog issues in one place.",
    );
  const tabItems: Array<{
    key: AgentManagerTab;
    id: string;
    panelId: string;
    testId: string;
    label: string;
    title: string;
    description: string;
    tone: "neutral" | "accent" | "info" | "success" | "warn" | "danger";
    count: number;
  }> = [
    {
      key: "agents",
      id: "agents-tab-button-agents",
      panelId: "agents-tab-panel",
      testId: "agents-tab-button-agents",
      label: tr(`에이전트 ${agents.length}`, `Agents ${agents.length}`),
      title: tr("에이전트", "Agents"),
      description: tr("프로필, 스킬, 소속, provider를 관리합니다.", "Manage profiles, skills, memberships, and providers."),
      tone: "info",
      count: agents.length,
    },
    {
      key: "departments",
      id: "agents-tab-button-departments",
      panelId: "agents-departments-tab-panel",
      testId: "agents-tab-button-departments",
      label: tr(`부서 ${departments.length}`, `Departments ${departments.length}`),
      title: tr("부서", "Departments"),
      description: tr("순서, 역할, 테마를 운영 톤에 맞춰 정리합니다.", "Adjust order, roles, and themes in the design language."),
      tone: "accent",
      count: departments.length,
    },
    {
      key: "backlog",
      id: "agents-tab-button-backlog",
      panelId: "agents-backlog-tab-panel",
      testId: "agents-tab-button-backlog",
      label: tr(`백로그 ${kanbanCards.length}`, `Backlog ${kanbanCards.length}`),
      title: tr("백로그", "Backlog"),
      description: tr("핵심 backlog를 상태와 우선순위 중심으로 관리합니다.", "Review the backlog by state and priority."),
      tone: "warn",
      count: kanbanCards.length,
    },
  ];
  const headerActions = (
    <div className="flex flex-wrap items-center gap-2">
      {(showTabBar || resolvedTab === "departments") && resolvedTab !== "backlog" && (
        <SurfaceActionButton tone="neutral" compact onClick={openCreateDept}>
          + {tr("부서 추가", "Add Dept")}
        </SurfaceActionButton>
      )}
      {(showTabBar || resolvedTab === "agents") && resolvedTab !== "departments" && resolvedTab !== "backlog" && (
        <SurfaceActionButton compact onClick={openCreateAgent}>
          + {tr("에이전트 추가", "Add Agent")}
        </SurfaceActionButton>
      )}
      {canShowDispatch && (
        <SurfaceActionButton
          tone="success"
          compact
          onClick={() => setDispatchOpen((prev) => !prev)}
        >
          {dispatchOpen ? tr("파견 닫기", "Close Dispatch") : tr("파견 열기", "Open Dispatch")}
        </SurfaceActionButton>
      )}
    </div>
  );
  const tabBarDesktop = showTabBar ? (
    <div
      data-testid="agents-tab-bar"
      role="tablist"
      aria-label={tr("에이전트 관리 섹션", "Agent manager sections")}
      className="hidden flex-wrap gap-2 sm:flex"
    >
      {tabItems.map((item) => (
        <SurfaceSegmentButton
          key={item.key}
          id={isDesktopViewport ? item.id : undefined}
          data-testid={isDesktopViewport ? item.testId : undefined}
          role="tab"
          aria-selected={resolvedTab === item.key}
          aria-controls={item.panelId}
          active={resolvedTab === item.key}
          tone={item.tone}
          onClick={() => handleTabChange(item.key)}
        >
          {item.label}
        </SurfaceSegmentButton>
      ))}
    </div>
  ) : null;
  const tabBarMobile = showTabBar ? (
    <div
      data-testid="agents-tab-bar-mobile"
      role="tablist"
      aria-label={tr("에이전트 관리 섹션", "Agent manager sections")}
      className="mt-4 grid grid-cols-2 gap-2 sm:hidden"
    >
      {tabItems.map((item) => (
        <SurfaceSegmentButton
          key={item.key}
          id={!isDesktopViewport ? item.id : undefined}
          data-testid={!isDesktopViewport ? item.testId : undefined}
          role={!isDesktopViewport ? "tab" : undefined}
          aria-selected={!isDesktopViewport ? resolvedTab === item.key : undefined}
          aria-controls={!isDesktopViewport ? item.panelId : undefined}
          active={resolvedTab === item.key}
          tone={item.tone}
          onClick={() => handleTabChange(item.key)}
          className="min-w-0 justify-center whitespace-normal px-3 py-2 text-center leading-5"
        >
          {item.label}
        </SurfaceSegmentButton>
      ))}
    </div>
  ) : null;
  const dispatchToggle = canShowDispatch ? (
    <SurfaceActionButton
      tone="success"
      compact
      onClick={() => setDispatchOpen((prev) => !prev)}
    >
      {dispatchOpen ? tr("파견 닫기", "Close Dispatch") : tr("파견 열기", "Open Dispatch")}
    </SurfaceActionButton>
  ) : null;
  return (
    <div
      data-testid="agents-page"
      className={`mx-auto w-full max-w-5xl min-w-0 space-y-4 overflow-x-hidden p-4 pb-40 sm:p-6 ${
        scrollable ? "h-full overflow-y-auto" : ""
      }`}
      style={{
        paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))",
        WebkitOverflowScrolling: scrollable ? "touch" : undefined,
        touchAction: scrollable ? "pan-y" : undefined,
      }}
    >
      {showHeader && (
        <header className="space-y-3" data-testid="agents-page-header">
          <div className="flex flex-col gap-4 xl:flex-row xl:items-end xl:justify-between">
            <div className="min-w-0">
              <h1
                className="text-2xl font-black tracking-tight sm:text-3xl"
                style={{ color: "var(--th-text-heading)" }}
              >
                {resolvedTitle}
              </h1>
              <p
                className="mt-2 max-w-3xl text-sm leading-6"
                style={{ color: "var(--th-text-muted)" }}
              >
                {resolvedSubtitle}
              </p>
            </div>

            <div className="flex flex-col gap-3 xl:items-end">
              {tabBarDesktop}
              <div className="hidden sm:flex sm:flex-wrap sm:justify-end sm:gap-2">
                {headerActions}
              </div>
            </div>
          </div>

          {tabBarMobile}
          <div className="flex flex-wrap gap-2 sm:hidden">
            {headerActions}
          </div>
        </header>
      )}

      {!showHeader && (
        <>
          {tabBarDesktop}
          {tabBarMobile}
          {dispatchToggle && (
            <div className="flex justify-end">
              {dispatchToggle}
            </div>
          )}
        </>
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
            isKo={isKo}
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
            spriteMap={spriteMap}
            confirmDeleteId={confirmDeleteId}
            setConfirmDeleteId={setConfirmDeleteId}
            confirmArchiveId={confirmArchiveId}
            setConfirmArchiveId={setConfirmArchiveId}
            onOpenAgent={(agent) =>
              onSelectAgent ? onSelectAgent(agent) : openEditAgent(agent)
            }
            onEditAgent={openEditAgent}
            onDuplicateAgent={openDuplicateAgent}
            onArchiveAgent={handleArchiveAgent}
            onUnarchiveAgent={handleUnarchiveAgent}
            onEditDepartment={openEditDept}
            onDeleteAgent={handleDeleteAgent}
            saving={saving}
          />
        </div>
      ) : resolvedTab === "backlog" ? (
        <div
          id="agents-backlog-tab-panel"
          role="tabpanel"
          aria-labelledby="agents-tab-button-backlog"
        >
          <BacklogTab
            tr={tr}
            locale={locale}
            cards={kanbanCards}
            agents={agents}
          />
        </div>
      ) : (
        <div
          id="agents-departments-tab-panel"
          role="tabpanel"
          aria-labelledby="agents-tab-button-departments"
        >
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

      {canShowDispatch && dispatchOpen && sessions && onAssign && (
        <SurfaceSection
          eyebrow={tr("확장", "Extension")}
          title={tr("파견 세션", "Dispatch Sessions")}
          description={tr(
            "코어 탭 밖에서 감지된 세션을 부서와 에이전트에 연결합니다.",
            "Assign detected sessions outside the core tabs.",
          )}
          badge={`${sessions.length}`}
          className="rounded-[30px] p-4 sm:p-5"
          actions={(
            <SurfaceActionButton tone="neutral" compact onClick={() => setDispatchOpen(false)}>
              {tr("접기", "Collapse")}
            </SurfaceActionButton>
          )}
        >
          <div className="mt-4">
            <SessionPanel
              sessions={sessions}
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
          officeId={officeId}
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
