import { useEffect, useMemo, useState } from "react";
import { getSkillRanking } from "../../api";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
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
  spriteMap: Map<string, number>;
  confirmDeleteId: string | null;
  setConfirmDeleteId: (id: string | null) => void;
  confirmArchiveId: string | null;
  setConfirmArchiveId: (id: string | null) => void;
  onOpenAgent: (agent: Agent) => void;
  onEditAgent: (agent: Agent) => void;
  onDuplicateAgent: (agent: Agent) => void;
  onArchiveAgent: (agentId: string) => void;
  onUnarchiveAgent: (agentId: string) => void;
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
  spriteMap,
  confirmDeleteId,
  setConfirmDeleteId,
  confirmArchiveId,
  setConfirmArchiveId,
  onOpenAgent,
  onEditAgent,
  onDuplicateAgent,
  onArchiveAgent,
  onUnarchiveAgent,
  onEditDepartment,
  onDeleteAgent,
  saving,
}: AgentsTabProps) {
  const [viewMode, setViewMode] = useState<AgentViewMode>("grid");
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

  const deptCounts = new Map<string, { total: number; working: number }>();
  for (const agent of agents) {
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
      <SurfaceCard className="space-y-3 rounded-[28px] p-4 sm:p-5">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap gap-2">
            <SurfaceSegmentButton
              active={deptTab === "all"}
              onClick={() => setDeptTab("all")}
              tone="accent"
            >
              {tr("전체", "All")} <span className="opacity-60">{agents.length}</span>
            </SurfaceSegmentButton>
            {departments.map((department) => {
              const count = deptCounts.get(department.id);
              return (
                <SurfaceSegmentButton
                  key={department.id}
                  active={deptTab === department.id}
                  onClick={() => setDeptTab(department.id)}
                  tone="info"
                  className="flex items-center gap-1"
                  style={{ maxWidth: "100%" }}
                >
                  <span
                    onDoubleClick={(event) => {
                      event.preventDefault();
                      event.stopPropagation();
                      onEditDepartment(department);
                    }}
                    title={tr("더블클릭: 부서 편집", "Double-click: edit dept")}
                    className="inline-flex items-center gap-1"
                  >
                    <span>{department.icon}</span>
                    <span className="hidden sm:inline">
                      {localeName(locale, department)}
                    </span>
                    <span className="opacity-60">{count?.total ?? 0}</span>
                  </span>
                </SurfaceSegmentButton>
              );
            })}
          </div>

          <div data-testid="agents-view-mode" className="flex items-center gap-2">
            <SurfaceSegmentButton
              active={viewMode === "grid"}
              onClick={() => setViewMode("grid")}
              tone="warn"
            >
              {tr("그리드", "Grid")}
            </SurfaceSegmentButton>
            <SurfaceSegmentButton
              active={viewMode === "list"}
              onClick={() => setViewMode("list")}
              tone="warn"
            >
              {tr("리스트", "List")}
            </SurfaceSegmentButton>
          </div>
        </div>

        <div className="flex flex-col gap-2 lg:flex-row lg:items-center lg:justify-between">
          <div className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "기본 그리드는 시안 원형을 유지하고, 리스트는 운영용 확장 보기로 제공합니다.",
              "Grid keeps the reference shell; list mode remains as the operational extension.",
            )}
          </div>
          <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
            <select
              value={statusFilter}
              onChange={(event) => setStatusFilter(event.target.value)}
              className="rounded-xl px-3 py-2 text-xs outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="all">{tr("상태: 전체", "Status: All")}</option>
              <option value="working">{tr("근무 중", "Working")}</option>
              <option value="idle">{tr("대기", "Idle")}</option>
              <option value="break">{tr("휴식", "Break")}</option>
              <option value="offline">{tr("오프라인", "Offline")}</option>
              <option value="archived">{tr("보관됨", "Archived")}</option>
            </select>
            <select
              value={sortMode}
              onChange={(event) => setSortMode(event.target.value as AgentSortMode)}
              className="rounded-xl px-3 py-2 text-xs outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="status">{tr("정렬: 상태", "Sort: Status")}</option>
              <option value="department">{tr("정렬: 부서", "Sort: Department")}</option>
              <option value="name">{tr("정렬: 이름", "Sort: Name")}</option>
              <option value="xp">{tr("정렬: XP", "Sort: XP")}</option>
              <option value="activity">{tr("정렬: 활동량", "Sort: Activity")}</option>
              <option value="created">{tr("정렬: 생성일", "Sort: Created")}</option>
              <option value="archived">{tr("정렬: 보관일", "Sort: Archived")}</option>
            </select>
            <input
              type="text"
              placeholder={`${tr("검색", "Search")}...`}
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              className="w-full rounded-xl px-3 py-2 text-xs outline-none transition-shadow sm:w-48"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            />
          </div>
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
          confirmArchiveId={confirmArchiveId}
          confirmDeleteId={confirmDeleteId}
          departments={departments}
          isKo={isKo}
          locale={locale}
          onArchiveAgent={onArchiveAgent}
          onDeleteAgent={onDeleteAgent}
          onDuplicateAgent={onDuplicateAgent}
          onEditAgent={onEditAgent}
          onEditDepartment={onEditDepartment}
          onOpenAgent={onOpenAgent}
          onUnarchiveAgent={onUnarchiveAgent}
          saving={saving}
          selectedAgent={selectedAgent}
          selectedAgentId={selectedAgentId}
          setConfirmArchiveId={setConfirmArchiveId}
          setConfirmDeleteId={setConfirmDeleteId}
          setSelectedAgentId={setSelectedAgentId}
          spriteMap={spriteMap}
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
                spriteMap={spriteMap}
                isKo={isKo}
                locale={locale}
                tr={tr}
                departments={departments}
                onOpen={() => onOpenAgent(agent)}
                onEdit={() => onEditAgent(agent)}
                confirmDeleteId={confirmDeleteId}
                onDeleteClick={() => setConfirmDeleteId(agent.id)}
                onDeleteConfirm={() => onDeleteAgent(agent.id)}
                onDeleteCancel={() => setConfirmDeleteId(null)}
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
