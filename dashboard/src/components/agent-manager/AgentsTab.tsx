import { useEffect, useMemo, useState } from "react";
import { getSkillRanking } from "../../api";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import {
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceSegmentButton,
} from "../common/SurfacePrimitives";
import AgentCard from "./AgentCard";
import { StackedSpriteIcon } from "./EmojiPicker";
import type { Translator } from "./types";

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
  sortedAgents: Agent[];
  spriteMap: Map<string, number>;
  confirmDeleteId: string | null;
  setConfirmDeleteId: (id: string | null) => void;
  onOpenAgent: (agent: Agent) => void;
  onEditAgent: (agent: Agent) => void;
  onEditDepartment: (department: Department) => void;
  onDeleteAgent: (agentId: string) => void;
  saving: boolean;
  randomIconSprites: {
    total: [number, number];
  };
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
  sortedAgents,
  spriteMap,
  confirmDeleteId,
  setConfirmDeleteId,
  onOpenAgent,
  onEditAgent,
  onEditDepartment,
  onDeleteAgent,
  saving,
  randomIconSprites,
}: AgentsTabProps) {
  const [viewMode, setViewMode] = useState<AgentViewMode>("grid");
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

  const workingCount = agents.filter((agent) => agent.status === "working").length;
  const deptCounts = new Map<string, { total: number; working: number }>();
  for (const agent of agents) {
    const key = agent.department_id || "__none";
    const count = deptCounts.get(key) ?? { total: 0, working: 0 };
    count.total += 1;
    if (agent.status === "working") count.working += 1;
    deptCounts.set(key, count);
  }

  const skillCoverage = useMemo(() => {
    const uniqueAgents = new Set<string>();
    for (const agent of agents) {
      const roleKey = agent.role_id || agent.id;
      if (topSkillsByAgent.has(roleKey)) uniqueAgents.add(agent.id);
    }
    return uniqueAgents.size;
  }, [agents, topSkillsByAgent]);

  return (
    <div data-testid="agents-tab" className="space-y-4">
      <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4">
        {[
          {
            label: tr("전체 인원", "Total"),
            value: agents.length,
            tone: "accent" as const,
            icon: <StackedSpriteIcon sprites={randomIconSprites.total} />,
          },
          {
            label: tr("근무 중", "Working"),
            value: workingCount,
            tone: "success" as const,
            icon: "💼",
          },
          {
            label: tr("부서", "Departments"),
            value: departments.length,
            tone: "info" as const,
            icon: "🏢",
          },
          {
            label: tr("스킬 신호", "Skill Signals"),
            value: skillCoverage,
            tone: "warn" as const,
            icon: "🧠",
          },
        ].map((summary) => (
          <SurfaceMetricPill
            key={summary.label}
            label={`${typeof summary.icon === "string" ? summary.icon : "🧩"} ${summary.label}`}
            value={
              <span
                className="text-2xl font-bold tabular-nums"
                style={{ color: "var(--th-text-heading)" }}
              >
                {summary.value}
              </span>
            }
            tone={summary.tone}
            className="min-w-[132px]"
            style={{ minHeight: 88 }}
          />
        ))}
      </div>

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

        <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
          <div className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "카드를 누르면 상세 drawer가 열리고, 부서 chip 더블클릭으로 부서를 편집합니다.",
              "Tap a card to open the detail drawer. Double-click a department chip to edit it.",
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
      ) : (
        <div
          data-testid={`agents-view-${viewMode}`}
          className={
            viewMode === "grid"
              ? "grid grid-cols-1 gap-3 lg:grid-cols-2"
              : "space-y-3"
          }
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
