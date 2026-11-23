import { lazy, Suspense, useCallback, useEffect } from "react";
import { Plus } from "lucide-react";
import { useLocation, useNavigate, useSearchParams } from "react-router";
import type { Agent, Department, DispatchedSession } from "../types";
import type { UiLanguage } from "../i18n";
import { PageFrame } from "../app/PageFrame";
import { useWorkspace } from "../contexts/WorkspaceContext";
import { Button } from "../kit/free/button-clean";
import { IconButton } from "../kit/free/icon-button-clean";
import { SegmentedControl } from "../kit/free/segmented-control-clean";
import { useIsMobile } from "./common/overlay/useBreakpoint";
import AgentsTab from "./agent-manager/AgentsTab";
import DepartmentsTab from "./agent-manager/DepartmentsTab";
import AgentFormModal from "./agent-manager/AgentFormModal";
import AgentSetupWizard from "./agent-manager/AgentSetupWizard";
import DepartmentFormModal from "./agent-manager/DepartmentFormModal";
import { isListedAgent } from "./agent-manager/agent-list-controls";
import {
  type AgentManagerTab,
  useAgentManagerController,
} from "./agent-manager/useAgentManagerController";
import { SurfaceActionButton } from "./common/SurfacePrimitives";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));
const TAB_KEYS: AgentManagerTab[] = ["agents", "departments", "skills"];

interface AgentManagerViewProps {
  agents: Agent[];
  departments: Department[];
  language: UiLanguage;
  onAgentsChange: () => void;
  onDepartmentsChange: () => void;
  /** Sessions not linked to an agent, offered for attaching. */
  sessions?: DispatchedSession[];
  onAssign?: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
  activeTab?: AgentManagerTab;
  onTabChange?: (tab: AgentManagerTab) => void;
  autoOpenCreate?: boolean;
  onAutoOpenConsumed?: () => void;
}

/** `/agents`: agents, departments and skills under one frame. The tab (`?tab=`) and the open agent (`?agent=`) live in
 * the URL, so links land on them and the back button closes a detail opened here. */
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
  autoOpenCreate = false,
  onAutoOpenConsumed,
}: AgentManagerViewProps) {
  const { sessions: workspaceSessions, sessionsFailed } = useWorkspace();
  const {
    deptModal,
    deptOrder,
    deptOrderDirty,
    deptTab,
    draggingDeptId,
    dragOverDeptId,
    dragOverPosition,
    entries,
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
    setIncludeTests,
    setSearch,
    setSortMode,
    setStatusFilter,
    setupWizard,
    setSetupWizard,
    sortMode,
    sortedEntries,
    statusFilter,
    tr,
  } = useAgentManagerController({
    agents,
    departments,
    language,
    onAgentsChange,
    onDepartmentsChange,
    sessions: workspaceSessions,
    sessionsFailed,
    activeTab,
    onTabChange,
  });
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const location = useLocation();
  const [searchParams, setSearchParams] = useSearchParams();
  const urlTab = searchParams.get("tab") as AgentManagerTab | null;
  const tab = urlTab && TAB_KEYS.includes(urlTab) ? urlTab : resolvedTab;
  const selectedAgentId = searchParams.get("agent");
  const openedHere = (location.state as { agentOpened?: boolean } | null)?.agentOpened === true;

  const changeTab = (next: AgentManagerTab) => {
    handleTabChange(next);
    const params = new URLSearchParams(searchParams);
    if (next === "agents") params.delete("tab");
    else params.set("tab", next);
    setSearchParams(params, { replace: true, state: location.state });
  };

  // The first open adds a history entry, so back (or closing) returns to the list; switching agents replaces it.
  const selectAgent = useCallback((agentId: string | null) => {
    if (!agentId && openedHere) {
      navigate(-1);
      return;
    }
    const params = new URLSearchParams(searchParams);
    if (agentId) params.set("agent", agentId);
    else params.delete("agent");
    setSearchParams(params, agentId && !selectedAgentId ? { state: { agentOpened: true } } : { replace: true, state: location.state });
  }, [location.state, navigate, openedHere, searchParams, selectedAgentId, setSearchParams]);

  // Auto-open the setup wizard when rendered via the /agents/new route.
  useEffect(() => {
    if (!autoOpenCreate) return;
    if (!setupWizard.open) {
      openCreateAgent();
    }
    onAutoOpenConsumed?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoOpenCreate]);

  const listedCount = entries.filter((entry) => isListedAgent(entry.agent, includeTests)).length;
  // Aborted sessions are finished work, not something to attach to an agent.
  const unlinkedSessions = sessions?.filter((session) => session.status !== "aborted");
  const header = (
    <div data-testid="agents-tab-bar" className="flex min-w-0 items-center gap-2">
      {/* The kit control is an inline-size container, so its slot needs a definite width. */}
      <div className="min-w-0 flex-1 sm:w-80 sm:flex-none">
        <SegmentedControl<AgentManagerTab>
          kind="tabs"
          variant="tray"
          fullWidth
          aria-label={tr("에이전트 관리 화면", "Agent manager views")}
          value={tab}
          onChange={changeTab}
          options={[
            { value: "agents", label: tr("에이전트", "Agents"), count: listedCount, controls: "agents-tab-panel" },
            { value: "departments", label: tr("부서", "Departments"), count: departments.length, controls: "agents-departments-tab-panel" },
            { value: "skills", label: tr("스킬", "Skills"), controls: "agents-skills-tab-panel" },
          ]}
        />
      </div>
      {isMobile ? (
        <IconButton variant="primary" icon={<Plus size={18} />} label={tr("에이전트 추가", "Add agent")} onClick={openCreateAgent} />
      ) : (
        <Button size="lg" icon={<Plus size={15} aria-hidden />} onClick={openCreateAgent}>{tr("에이전트 추가", "Add agent")}</Button>
      )}
    </div>
  );

  return (
    <PageFrame
      testId="agents-page"
      title={tr("에이전트", "Agents")}
      description={tr(
        "막히거나 실패한 에이전트가 먼저 보여요. 설정은 오른쪽 상세에서 바꿔요.",
        "Stuck or failing agents come first. Change settings in the detail on the right.",
      )}
      actions={header}
    >
      {tab === "agents" ? (
        <div id="agents-tab-panel" role="tabpanel" aria-label={tr("에이전트", "Agents")}>
          <AgentsTab
            tr={tr}
            locale={locale}
            isKo={isKo}
            entries={entries}
            sortedEntries={sortedEntries}
            departments={departments}
            deptTab={deptTab}
            setDeptTab={setDeptTab}
            search={search}
            setSearch={setSearch}
            statusFilter={statusFilter}
            setStatusFilter={setStatusFilter}
            sortMode={sortMode}
            setSortMode={setSortMode}
            includeTests={includeTests}
            setIncludeTests={setIncludeTests}
            selectedAgentId={selectedAgentId}
            onSelectAgent={selectAgent}
            unlinkedSessions={unlinkedSessions}
            onAssign={onAssign}
            onCreateAgent={openCreateAgent}
            onEditAgent={openEditAgent}
            onDuplicateAgent={openDuplicateAgent}
            onEditDepartment={openEditDept}
            onDeleteAgent={handleDeleteAgent}
            onAgentUpdated={onAgentsChange}
            saving={saving}
          />
        </div>
      ) : tab === "skills" ? (
        <div id="agents-skills-tab-panel" role="tabpanel" aria-label={tr("스킬", "Skills")}>
          <Suspense fallback={null}>
            <SkillCatalogView embedded />
          </Suspense>
        </div>
      ) : (
        <div id="agents-departments-tab-panel" role="tabpanel" aria-label={tr("부서", "Departments")} className="space-y-3">
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

      {/* Agent create/edit modal */}
      {agentModal.open && (
        <AgentFormModal
          isKo={isKo}
          locale={locale}
          tr={tr}
          form={form}
          departments={departments}
          isEdit={!!agentModal.editAgent}
          agentId={agentModal.editAgent?.id}
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
    </PageFrame>
  );
}
