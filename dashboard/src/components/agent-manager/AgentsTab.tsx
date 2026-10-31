import { useEffect, useMemo, useState } from "react";
import { getSkillRanking } from "../../api";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { isTestAgent } from "../../lib/agentHelpers";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceSegmentButton,
} from "../common/SurfacePrimitives";
import AgentCard from "./AgentCard";
import type { Translator } from "./types";
import type { AgentSortMode } from "./useAgentManagerController";
import { AgentsGridView } from "./AgentsGridView";

interface AgentsTabProps {
  tr: Translator;
  locale: string;
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
  onOpenAgent: (agent: Agent) => void;
  onEditAgent: (agent: Agent) => void;
  onDuplicateAgent: (agent: Agent) => void;
  onEditDepartment: (department: Department) => void;
  onDeleteAgent: (agentId: string) => void;
  saving: boolean;
}

type AgentViewMode = "grid" | "list";

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
  onOpenAgent,
  onEditAgent,
  onDuplicateAgent,
  onEditDepartment,
  onDeleteAgent,
  saving,
}: AgentsTabProps) {
  const [preferredViewMode, setViewMode] = useState<AgentViewMode>("grid");
  // On phones a tap opens the full-screen detail directly, so the list mode always wins.
  const isMobile = useIsMobile();
  const viewMode: AgentViewMode = isMobile ? "list" : preferredViewMode;
  const testAgentCount = agents.filter(isTestAgent).length;
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [topSkillsByAgent, setTopSkillsByAgent] = useState<Map<string, string[]>>(
    () => new Map(),
  );

  useEffect(() => {
    let cancelled = false;

    getSkillRanking("30d", 120)
      .then((ranking) => {
        if (cancelled) return;
        setTopSkillsByAgent(buildTopSkillMap(ranking.byAgent));
      })
      .catch(() => {
        if (!cancelled) setTopSkillsByAgent(new Map());
      });

    return () => {
      cancelled = true;
    };
  }, []);

  const countedAgents = includeTests ? agents : agents.filter((candidate) => !isTestAgent(candidate));
  const deptCounts = new Map<string, { total: number; working: number }>();
  for (const agent of countedAgents) {
    const key = agent.department_id || "__none";
    const count = deptCounts.get(key) ?? { total: 0, working: 0 };
    count.total += 1;
    if (agent.status === "working") count.working += 1;
    deptCounts.set(key, count);
  }

  useEffect(() => {
    if (sortedAgents.length === 0) {
      setSelectedAgentId(null);
      return;
    }

    if (selectedAgentId && !sortedAgents.some((agent) => agent.id === selectedAgentId)) {
      setSelectedAgentId(null);
    }
  }, [selectedAgentId, sortedAgents]);

  const selectedAgent = useMemo(
    () => sortedAgents.find((agent) => agent.id === selectedAgentId) ?? null,
    [selectedAgentId, sortedAgents],
  );

  return (
    <div data-testid="agents-tab" className="space-y-4">
      <SurfaceCard className="min-w-0 space-y-3 p-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex min-w-0 flex-1 flex-wrap items-center gap-2">
            <label className="min-w-0 flex-1 sm:flex-none">
              <span className="sr-only">{tr("소속 부서", "Department")}</span>
              <select value={deptTab} onChange={event => setDeptTab(event.target.value)} className="min-h-11 w-full max-w-full rounded-md border border-th-input-border bg-th-input-bg px-3 text-xs text-th-text-primary sm:w-56">
                <option value="all">{tr(`전체 부서 · ${countedAgents.length}명`, `All departments · ${countedAgents.length} agents`)}</option>
                {departments.map(department => <option key={department.id} value={department.id}>{localeName(locale, department)} · {deptCounts.get(department.id)?.total ?? 0}</option>)}
              </select>
            </label>
            {deptTab !== "all" && departments.find(department => department.id === deptTab) ? <SurfaceActionButton tone="neutral" compact onClick={() => onEditDepartment(departments.find(department => department.id === deptTab)!)}>{tr("부서 편집", "Edit department")}</SurfaceActionButton> : null}
          </div>
          <div data-testid="agents-view-mode" className="hidden items-center gap-1 sm:flex" aria-label={tr("보기 방식", "View mode")}>
            <SurfaceSegmentButton active={viewMode === "grid"} onClick={() => setViewMode("grid")} tone="neutral">{tr("그리드", "Grid")}</SurfaceSegmentButton>
            <SurfaceSegmentButton active={viewMode === "list"} onClick={() => setViewMode("list")} tone="neutral">{tr("리스트", "List")}</SurfaceSegmentButton>
          </div>
        </div>
        <div className="grid min-w-0 grid-cols-2 gap-2 sm:grid-cols-[minmax(0,1fr)_auto_auto]">
          <input aria-label={tr("에이전트 검색", "Search agents")} type="search" placeholder={tr("이름·역할 검색", "Search name or role")} value={search} onChange={event => setSearch(event.target.value)} className="col-span-2 min-h-11 min-w-0 w-full rounded-md border border-th-input-border bg-th-input-bg px-3 text-sm text-th-text-primary sm:col-span-1" />
          <select aria-label={tr("에이전트 상태", "Agent status")} value={statusFilter} onChange={event => setStatusFilter(event.target.value)} className="min-h-11 min-w-0 rounded-md border border-th-input-border bg-th-input-bg px-3 text-xs text-th-text-primary">
            <option value="all">{tr("전체 상태", "All statuses")}</option>
            <option value="working">{tr("작업 중", "Working")}</option>
            <option value="idle">{tr("대기", "Idle")}</option>
            <option value="break">{tr("휴식", "Break")}</option>
            <option value="offline">{tr("오프라인", "Offline")}</option>
          </select>
          <select aria-label={tr("에이전트 정렬", "Sort agents")} value={sortMode} onChange={event => setSortMode(event.target.value as AgentSortMode)} className="min-h-11 min-w-0 rounded-md border border-th-input-border bg-th-input-bg px-3 text-xs text-th-text-primary">
            <option value="status">{tr("상태순", "By status")}</option>
            <option value="department">{tr("부서순", "By department")}</option>
            <option value="name">{tr("이름순", "By name")}</option>
            <option value="activity">{tr("활동량순", "By activity")}</option>
            <option value="created">{tr("생성일순", "By created date")}</option>
          </select>
        </div>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-xs text-th-text-muted" aria-live="polite">{tr(`${sortedAgents.length}명 표시`, `${sortedAgents.length} agents shown`)}</p>
          {testAgentCount > 0 ? (
            <label className="inline-flex min-h-11 cursor-pointer items-center gap-2 text-xs text-th-text-secondary">
              <input
                type="checkbox"
                data-testid="agents-include-tests"
                checked={includeTests}
                onChange={(event) => setIncludeTests(event.target.checked)}
                className="h-4 w-4 accent-[var(--th-accent-primary)]"
              />
              {tr(`테스트 에이전트 포함 (${testAgentCount})`, `Include test agents (${testAgentCount})`)}
            </label>
          ) : null}
        </div>
      </SurfaceCard>

      {sortedAgents.length === 0 ? (
        <SurfaceEmptyState className="py-12 text-center">
          <div className="text-3xl">🔍</div>
          <div className="mt-2 text-sm">{tr("검색 결과 없음", "No agents found")}</div>
        </SurfaceEmptyState>
      ) : viewMode === "grid" ? (
        <AgentsGridView
          agents={sortedAgents}
          departments={departments}
          locale={locale}
          onDeleteAgent={onDeleteAgent}
          onDuplicateAgent={onDuplicateAgent}
          onEditAgent={onEditAgent}
          onEditDepartment={onEditDepartment}
          onOpenAgent={onOpenAgent}
          saving={saving}
          selectedAgent={selectedAgent}
          selectedAgentId={selectedAgentId}
          setSelectedAgentId={setSelectedAgentId}
          topSkillsByAgent={topSkillsByAgent}
          tr={tr}
        />
      ) : (
        <div
          data-testid={`agents-view-${viewMode}`}
          className="space-y-3"
        >
          {sortedAgents.map((agent) => {
            const roleKey = agent.role_id || agent.id;
            const topSkills = topSkillsByAgent.get(roleKey) ?? [];

            return (
              <AgentCard
                key={agent.id}
                agent={agent}
                locale={locale}
                tr={tr}
                departments={departments}
                onOpen={() => onOpenAgent(agent)}
                onEdit={() => onEditAgent(agent)}
                onDelete={() => onDeleteAgent(agent.id)}
                saving={saving}
                topSkills={topSkills}
                viewMode={viewMode}
              />
            );
          })}
        </div>
      )}
    </div>
  );
}
