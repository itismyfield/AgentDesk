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
  onEditAgent: (agent: Agent) => void;
  onEditDepartment: (department: Department) => void;
  onDeleteAgent: (agentId: string) => void;
  saving: boolean;
  randomIconSprites: {
    total: [number, number];
  };
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
  onEditAgent,
  onEditDepartment,
  onDeleteAgent,
  saving,
  randomIconSprites,
}: AgentsTabProps) {
  const workingCount = agents.filter((agent) => agent.status === "working").length;
  const deptCounts = new Map<string, { total: number; working: number }>();
  for (const agent of agents) {
    const key = agent.department_id || "__none";
    const count = deptCounts.get(key) ?? { total: 0, working: 0 };
    count.total += 1;
    if (agent.status === "working") count.working += 1;
    deptCounts.set(key, count);
  }

  return (
    <>
      <div className="flex flex-wrap gap-3">
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
        ].map((summary) => (
          <SurfaceMetricPill
            key={summary.label}
            label={`${typeof summary.icon === "string" ? summary.icon : "🧩"} ${summary.label}`}
            value={
              <span className="text-2xl font-bold tabular-nums" style={{ color: "var(--th-text-heading)" }}>
                {summary.value}
              </span>
            }
            tone={summary.tone}
            className="min-w-[132px] flex-1 sm:flex-none"
            style={{ minHeight: 76 }}
          />
        ))}
      </div>

      <SurfaceCard className="space-y-3 rounded-3xl p-4 sm:p-5">
        <div className="flex flex-wrap items-center gap-2">
          <SurfaceSegmentButton active={deptTab === "all"} onClick={() => setDeptTab("all")} tone="accent">
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
                  onDoubleClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    onEditDepartment(department);
                  }}
                  title={tr("더블클릭: 부서 편집", "Double-click: edit dept")}
                  className="inline-flex items-center gap-1"
                >
                  <span>{department.icon}</span>
                  <span className="hidden sm:inline">{localeName(locale, department)}</span>
                  <span className="opacity-60">{count?.total ?? 0}</span>
                </span>
              </SurfaceSegmentButton>
            );
          })}
        </div>

        <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
          <div className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr("부서 chip 더블클릭으로 편집할 수 있습니다.", "Double-click a department chip to edit it.")}
          </div>
          <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
            <select
              value={statusFilter}
              onChange={(e) => setStatusFilter(e.target.value)}
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
              onChange={(e) => setSearch(e.target.value)}
              className="w-full rounded-xl px-3 py-2 text-xs outline-none transition-shadow sm:w-40"
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
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
          {sortedAgents.map((agent) => (
            <AgentCard
              key={agent.id}
              agent={agent}
              spriteMap={spriteMap}
              isKo={isKo}
              locale={locale}
              tr={tr}
              departments={departments}
              onEdit={() => onEditAgent(agent)}
              confirmDeleteId={confirmDeleteId}
              onDeleteClick={() => setConfirmDeleteId(agent.id)}
              onDeleteConfirm={() => onDeleteAgent(agent.id)}
              onDeleteCancel={() => setConfirmDeleteId(null)}
              saving={saving}
            />
          ))}
        </div>
      )}
    </>
  );
}
