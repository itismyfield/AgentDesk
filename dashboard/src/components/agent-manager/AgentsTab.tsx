import { useEffect, useId, useMemo, useState } from "react";
import { Grid2X2, List, Pencil, Plus, Search, UsersRound, X } from "lucide-react";
import { getSkillRanking } from "../../api";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { isTestAgent } from "../../lib/agentHelpers";
import { Button } from "../../kit/free/button-clean";
import { Checkbox } from "../../kit/free/checkbox-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Select } from "../../kit/free/select-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { ActionMenu } from "../common/ActionMenu";
import AgentInfoCard from "./AgentInfoCard";
import { AgentInfoCardContent } from "./AgentInfoCardContent";
import { filterAgents } from "./agent-list-controls";
import type { Translator } from "./types";
import type { AgentSortMode } from "./useAgentManagerController";
import { AgentsGridView } from "./AgentsGridView";
import { AgentManagerStyles } from "./AgentManagerStyles";

interface AgentsTabProps {
  tr: Translator;
  locale: string;
  isKo: boolean;
  agents: Agent[];
  departments: Department[];
  deptTab: string;
  setDeptTab: (deptId: string) => void;
  search: string;
  setSearch: (next: string) => void;
  statusFilter: string;
  setStatusFilter: (next: string) => void;
  sortMode: AgentSortMode;
  setSortMode: (next: AgentSortMode) => void;
  sortedAgents: Agent[];
  includeTests: boolean;
  setIncludeTests: (next: boolean) => void;
  onCreateAgent: () => void;
  onEditAgent: (agent: Agent) => void;
  onDuplicateAgent: (agent: Agent) => void;
  onEditDepartment: (department: Department) => void;
  onDeleteAgent: (agentId: string) => void;
  onAgentUpdated: () => void;
  saving: boolean;
}

type AgentViewMode = "grid" | "list";
const STATUS_KEYS = ["all", "working", "idle", "break", "offline"] as const;

function buildTopSkillMap(rows: Awaited<ReturnType<typeof getSkillRanking>>["byAgent"]) {
  const byAgent = new Map<string, string[]>();
  for (const row of rows) {
    const key = row.agent_role_id;
    const existing = byAgent.get(key) ?? [];
    if (!existing.includes(row.skill_desc_ko)) {
      existing.push(row.skill_desc_ko);
      byAgent.set(key, existing.slice(0, 3));
    }
  }
  return byAgent;
}

export default function AgentsTab({
  tr,
  locale,
  isKo,
  agents,
  departments,
  deptTab,
  setDeptTab,
  search,
  setSearch,
  statusFilter,
  setStatusFilter,
  sortMode,
  setSortMode,
  sortedAgents,
  includeTests,
  setIncludeTests,
  onCreateAgent,
  onEditAgent,
  onDuplicateAgent,
  onEditDepartment,
  onDeleteAgent,
  onAgentUpdated,
  saving,
}: AgentsTabProps) {
  const [preferredViewMode, setViewMode] = useState<AgentViewMode>("grid");
  // Phones always get the dense list; the detail opens as a full-screen page there.
  const isMobile = useIsMobile();
  const viewMode: AgentViewMode = isMobile ? "list" : preferredViewMode;
  const testAgentCount = agents.filter(isTestAgent).length;
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [topSkillsByAgent, setTopSkillsByAgent] = useState<Map<string, string[]>>(() => new Map());
  const panelId = useId();

  useEffect(() => {
    let cancelled = false;
    getSkillRanking("30d", 120)
      .then((ranking) => {
        if (!cancelled) setTopSkillsByAgent(buildTopSkillMap(ranking.byAgent));
      })
      .catch(() => {
        if (!cancelled) setTopSkillsByAgent(new Map());
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const countedAgents = includeTests ? agents : agents.filter((candidate) => !isTestAgent(candidate));
  const deptCounts = new Map<string, number>();
  for (const agent of countedAgents) {
    const key = agent.department_id || "__none";
    deptCounts.set(key, (deptCounts.get(key) ?? 0) + 1);
  }
  // Tab counts follow the other filters, so each number is what that tab would show.
  const statusCounts = useMemo(() => {
    const base = filterAgents(agents, { deptTab, statusFilter: "all", search, includeTests });
    const counts: Record<string, number> = { all: base.length };
    for (const agent of base) counts[agent.status] = (counts[agent.status] ?? 0) + 1;
    return counts;
  }, [agents, deptTab, includeTests, search]);

  useEffect(() => {
    if (selectedAgentId && !sortedAgents.some((agent) => agent.id === selectedAgentId)) {
      setSelectedAgentId(null);
    }
  }, [selectedAgentId, sortedAgents]);

  const selectedAgent = useMemo(
    () => sortedAgents.find((agent) => agent.id === selectedAgentId) ?? null,
    [selectedAgentId, sortedAgents],
  );
  const selectedDept = deptTab !== "all" ? departments.find((department) => department.id === deptTab) : undefined;
  const filtersActive = search.trim() !== "" || statusFilter !== "all" || deptTab !== "all";

  const statusLabels: Record<(typeof STATUS_KEYS)[number], string> = {
    all: tr("전체", "All"),
    working: tr("작업 중", "Working"),
    idle: tr("대기", "Idle"),
    break: tr("휴식", "Break"),
    offline: tr("오프라인", "Offline"),
  };

  const detailActions = (agent: Agent, closable: boolean) => {
    const name = localeName(locale, agent);
    return (
      <>
        <Button variant="secondary" size="sm" icon={<Pencil size={14} aria-hidden />} onClick={() => onEditAgent(agent)}>
          {tr("편집", "Edit")}
        </Button>
        <ActionMenu
          label={tr(`${name} 메뉴`, `${name} menu`)}
          items={[
            { key: "duplicate", label: tr("복제", "Duplicate"), onSelect: () => onDuplicateAgent(agent) },
            {
              key: "delete",
              label: tr("삭제", "Delete"),
              danger: true,
              disabled: saving || agent.status === "working",
              onSelect: () => onDeleteAgent(agent.id),
              confirm: {
                title: tr(`${name}을(를) 삭제할까요?`, `Delete ${name}?`),
                description: tr("에이전트 설정이 지워지고 되돌릴 수 없어요.", "The agent's settings are removed for good."),
                confirmLabel: tr("삭제", "Delete"),
                cancelLabel: tr("취소", "Cancel"),
              },
            },
          ]}
        />
        {closable ? (
          <IconButton icon={<X size={15} />} label={tr("상세 닫기", "Close details")} onClick={() => setSelectedAgentId(null)} />
        ) : null}
      </>
    );
  };

  const list =
    countedAgents.length === 0 ? (
      <EmptyState
        icon={<UsersRound size={18} />}
        title={tr("아직 에이전트가 없어요", "No agents yet")}
        text={tr("에이전트를 추가하면 여기에서 상태와 설정을 볼 수 있어요.", "Add an agent to see its status and settings here.")}
        action={
          <Button variant="secondary" size="sm" icon={<Plus size={14} aria-hidden />} onClick={onCreateAgent}>
            {tr("에이전트 추가", "Add agent")}
          </Button>
        }
      />
    ) : sortedAgents.length === 0 ? (
      <EmptyState
        icon={<Search size={18} />}
        title={tr("검색 결과 없음", "No agents found")}
        text={tr("검색어나 상태·부서 조건을 바꿔 보세요.", "Try another search, status or department.")}
        action={
          filtersActive ? (
            <Button
              variant="secondary"
              size="sm"
              onClick={() => {
                setSearch("");
                setStatusFilter("all");
                setDeptTab("all");
              }}
            >
              {tr("조건 지우기", "Clear filters")}
            </Button>
          ) : undefined
        }
      />
    ) : (
      <AgentsGridView
        agents={sortedAgents}
        locale={locale}
        tr={tr}
        viewMode={viewMode}
        selectedAgentId={selectedAgentId}
        onSelectAgent={(agent) => setSelectedAgentId(agent.id)}
        topSkillsByAgent={topSkillsByAgent}
      />
    );

  return (
    <div data-testid="agents-tab" className="agent-kit-scope min-w-0">
      <AgentManagerStyles />
      <div className="grid min-w-0 items-start gap-4 sm:grid-cols-[minmax(0,1fr)_minmax(18rem,38%)] xl:gap-5">
        <div className="@container flex min-w-0 flex-col gap-3">
          <div className="grid min-w-0 grid-cols-2 gap-2 @xl:grid-cols-[minmax(0,1fr)_9rem_12rem]">
            <TextField
              type="search"
              aria-label={tr("에이전트 검색", "Search agents")}
              placeholder={tr("이름·역할 검색", "Search name or role")}
              prefix={<Search size={14} aria-hidden />}
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              clearable
              onClear={() => setSearch("")}
              className="col-span-2 min-w-0 @xl:col-span-1"
            />
            <Select
              aria-label={tr("에이전트 정렬", "Sort agents")}
              value={sortMode}
              onChange={(value) => setSortMode(value as AgentSortMode)}
              className="min-w-0"
              options={[
                { value: "status", label: tr("상태순", "By status") },
                { value: "department", label: tr("부서순", "By department") },
                { value: "name", label: tr("이름순", "By name") },
                { value: "activity", label: tr("활동량순", "By activity") },
                { value: "created", label: tr("생성일순", "By created date") },
              ]}
            />
            <Select
              aria-label={tr("소속 부서", "Department")}
              value={deptTab}
              onChange={setDeptTab}
              className="min-w-0"
              options={[
                { value: "all", label: tr(`전체 부서 · ${countedAgents.length}명`, `All departments · ${countedAgents.length} agents`) },
                ...departments.map((department) => ({
                  value: department.id,
                  label: `${localeName(locale, department)} · ${deptCounts.get(department.id) ?? 0}`,
                })),
              ]}
            />
          </div>

          <div className="flex min-w-0 items-center gap-2">
            <SegmentedControl
              kind="tabs"
              aria-label={tr("에이전트 상태", "Agent status")}
              value={statusFilter}
              onChange={setStatusFilter}
              className="min-w-0 flex-1"
              options={STATUS_KEYS.map((key) => ({
                value: key,
                label: statusLabels[key],
                count: statusCounts[key] ?? 0,
                controls: panelId,
              }))}
            />
            {/* The kit control sizes itself to its slot (inline-size containment), so the slot gets a width. */}
            <div data-testid="agents-view-mode" className="hidden w-[4.5rem] shrink-0 sm:block pointer-coarse:w-[6.25rem]">
              <SegmentedControl
                aria-label={tr("보기 방식", "View mode")}
                value={viewMode}
                onChange={(value) => setViewMode(value as AgentViewMode)}
                variant="tray"
                size="sm"
                options={[
                  { value: "grid", icon: <Grid2X2 size={14} aria-hidden />, ariaLabel: tr("그리드", "Grid") },
                  { value: "list", icon: <List size={14} aria-hidden />, ariaLabel: tr("리스트", "List") },
                ]}
              />
            </div>
          </div>

          <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-3 gap-y-1">
            <div className="flex min-w-0 items-center gap-2">
              <p className="m-0 text-xs text-(--fg-muted)" aria-live="polite">
                {tr(`${sortedAgents.length}명 표시`, `${sortedAgents.length} agents shown`)}
              </p>
              {selectedDept ? (
                <Button variant="ghost" size="sm" onClick={() => onEditDepartment(selectedDept)}>
                  {tr("부서 편집", "Edit department")}
                </Button>
              ) : null}
            </div>
            {testAgentCount > 0 ? (
              <Checkbox
                data-testid="agents-include-tests"
                checked={includeTests}
                onChange={(event) => setIncludeTests(event.target.checked)}
                label={tr(`테스트 에이전트 포함 (${testAgentCount})`, `Include test agents (${testAgentCount})`)}
              />
            ) : null}
          </div>

          <div id={panelId} role="tabpanel" aria-label={statusLabels[(statusFilter as (typeof STATUS_KEYS)[number])] ?? statusLabels.all} className="min-w-0">
            {list}
          </div>
        </div>

        {!isMobile ? (
          <section
            data-testid="agents-detail-pane"
            aria-label={
              selectedAgent
                ? `${localeName(locale, selectedAgent)} — ${tr("에이전트 상세", "Agent details")}`
                : tr("에이전트 상세", "Agent details")
            }
            className="sticky top-0 max-h-[calc(100dvh-8rem)] min-w-0 overflow-y-auto overscroll-contain rounded-(--r-xl) bg-(--bg-1) p-4 shadow-[inset_0_0_0_1px_var(--line-soft)] xl:p-5"
          >
            {selectedAgent ? (
              <AgentInfoCardContent
                key={selectedAgent.id}
                agent={selectedAgent}
                isKo={isKo}
                locale={locale}
                tr={tr}
                departments={departments}
                onAgentUpdated={onAgentUpdated}
                actions={detailActions(selectedAgent, true)}
              />
            ) : (
              <EmptyState
                bare
                icon={<UsersRound size={18} />}
                title={tr("선택한 에이전트가 없어요", "No agent selected")}
                text={tr("에이전트를 고르면 상세 정보와 편집 메뉴가 보여요.", "Select an agent to see details and actions.")}
              />
            )}
          </section>
        ) : null}
      </div>

      {isMobile && selectedAgent ? (
        <AgentInfoCard
          agent={selectedAgent}
          isKo={isKo}
          locale={locale}
          tr={tr}
          departments={departments}
          onClose={() => setSelectedAgentId(null)}
          onAgentUpdated={onAgentUpdated}
          backLabel={tr("목록으로", "Back to list")}
          actions={detailActions(selectedAgent, false)}
        />
      ) : null}
    </div>
  );
}
