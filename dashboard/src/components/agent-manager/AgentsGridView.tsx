import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import { getCurrentTaskSummary } from "../../lib/agentHelpers";
import AgentAvatar from "../AgentAvatar";
import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";
import { getAgentLevel, getAgentTitle } from "./agentProgress";
import type { Translator } from "./types";

interface AgentsGridViewProps {
  agents: Agent[];
  confirmDeleteId: string | null;
  departments: Department[];
  isKo: boolean;
  locale: string;
  onDeleteAgent: (agentId: string) => void;
  onDuplicateAgent: (agent: Agent) => void;
  onEditAgent: (agent: Agent) => void;
  onEditDepartment: (department: Department) => void;
  onOpenAgent: (agent: Agent) => void;
  saving: boolean;
  selectedAgent: Agent | null;
  selectedAgentId: string | null;
  setConfirmDeleteId: (id: string | null) => void;
  setSelectedAgentId: (id: string | null) => void;
  spriteMap: Map<string, number>;
  topSkillsByAgent: Map<string, string[]>;
  tr: Translator;
}

function agentSecondaryLine(agent: Agent, locale: string) {
  if (agent.alias?.trim()) return `aka ${agent.alias.trim()}`;
  return locale === "en" ? agent.name_ko || agent.name : agent.name;
}

export function AgentsGridView({
  agents: sortedAgents,
  confirmDeleteId,
  departments,
  isKo,
  locale,
  onDeleteAgent,
  onDuplicateAgent,
  onEditAgent,
  onEditDepartment,
  onOpenAgent,
  saving,
  selectedAgent,
  selectedAgentId,
  setConfirmDeleteId,
  setSelectedAgentId,
  spriteMap,
  topSkillsByAgent,
  tr,
}: AgentsGridViewProps) {
  return (
<div
  data-testid="agents-view-grid"
  className={selectedAgent ? "grid min-w-0 items-start gap-4 xl:grid-cols-[minmax(0,1.3fr)_minmax(320px,0.9fr)]" : "grid min-w-0 gap-3"}
>
  <div className={`grid min-w-0 grid-cols-1 gap-3 ${selectedAgent ? "" : "sm:grid-cols-2 2xl:grid-cols-3"}`}>
    {sortedAgents.map((agent) => {
      const department = departments.find((candidate) => candidate.id === agent.department_id);
      const providerMeta = getProviderMeta(agent.cli_provider);
      const levelInfo = getAgentLevel(agent.stats_xp);
      const isSelected = agent.id === selectedAgentId;

      return (
        <button
          key={agent.id}
          type="button"
          data-testid={`agents-grid-card-${agent.id}`}
          onClick={() => setSelectedAgentId(agent.id)}
          aria-pressed={isSelected}
          className={`min-w-0 rounded-lg border p-4 text-left transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] ${isSelected ? "border-[var(--th-accent-primary)] bg-surface-subtle" : "border-th-border-subtle bg-th-bg-surface hover:bg-surface-subtle"}`}
        >
          <div className="flex items-start gap-3">
            <div className="relative shrink-0">
              <AgentAvatar agent={agent} spriteMap={spriteMap} size={40} rounded="xl" />
              <span
                className={`absolute -bottom-0.5 -right-0.5 h-3 w-3 rounded-full border-2 ${
                  agent.status === "working"
                    ? "bg-emerald-400"
                    : agent.status === "break"
                      ? "bg-amber-300"
                    : agent.status === "offline"
                      ? "bg-slate-500"
                      : "bg-sky-400"
                }`}
                style={{ borderColor: "var(--th-card-bg)" }}
              />
            </div>

            <div className="min-w-0 flex-1">
              <div className="text-sm font-semibold leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-heading)" }}>
                {localeName(locale, agent)}
              </div>
              <div className="text-xs leading-5 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-muted)" }}>
                {agentSecondaryLine(agent, locale)}
              </div>
            </div>

            <span
              className="shrink-0 text-xs leading-6"
              style={{
                borderColor: "transparent",
                background: "transparent",
                color: "var(--th-text-secondary)",
              }}
            >
              {providerMeta.label}
            </span>
          </div>

          <div className="mt-3 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-th-text-muted">
            <span>{agent.status === "working" ? tr("근무 중", "Working") : agent.status === "offline" ? tr("오프라인", "Offline") : agent.status === "break" ? tr("휴식", "Break") : tr("대기", "Idle")}</span>
            {department ? (
              <span
                className="text-xs leading-5"
                style={{
                  color: "var(--th-text-muted)",
                  borderColor: "transparent",
                  background: "transparent",
                }}
                onDoubleClick={(event) => {
                  event.preventDefault();
                  event.stopPropagation();
                  onEditDepartment(department);
                }}
                title={tr("더블클릭: 부서 편집", "Double-click: edit dept")}
              >
                {department.icon} {localeName(locale, department)}
              </span>
            ) : null}
            <span
              className="text-xs leading-5"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                color: "var(--th-text-secondary)",
              }}
            >
              lv.{levelInfo.level}
            </span>
          </div>
        </button>
      );
    })}
  </div>

  {selectedAgent ? (
    <SurfaceCard data-testid="agents-selection-panel" className="order-first min-w-0 p-4 sm:p-5 xl:order-none xl:sticky xl:top-0">
      {(() => {
        const department = departments.find((candidate) => candidate.id === selectedAgent.department_id);
        const providerMeta = getProviderMeta(selectedAgent.cli_provider);
        const levelInfo = getAgentLevel(selectedAgent.stats_xp);
        const levelTitle = getAgentTitle(selectedAgent.stats_xp, isKo);
        const task = getCurrentTaskSummary(selectedAgent, tr);
        const roleKey = selectedAgent.role_id || selectedAgent.id;
        const topSkills = topSkillsByAgent.get(roleKey) ?? [];

        return (
          <div className="space-y-5">
            <div className="flex items-start gap-3">
              <AgentAvatar agent={selectedAgent} spriteMap={spriteMap} size={44} rounded="xl" />
              <div className="min-w-0 flex-1">
                <div className="truncate text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {localeName(locale, selectedAgent)}
                </div>
                <div className="text-xs leading-5 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-muted)" }}>
                  {agentSecondaryLine(selectedAgent, locale)} · {providerMeta.label}
                </div>
              </div>
              <SurfaceActionButton tone="neutral" compact onClick={() => setSelectedAgentId(null)} aria-label={tr("선택 닫기", "Close selection")}>×</SurfaceActionButton>
              <SurfaceActionButton tone="neutral" compact onClick={() => onOpenAgent(selectedAgent)}>
                {tr("상세", "Open")}
              </SurfaceActionButton>
            </div>

            <div className="grid gap-2">
              {[
                {
                  label: tr("상태", "Status"),
                  value:
                    selectedAgent.status === "working"
                      ? tr("근무 중", "Working")
                      : selectedAgent.status === "break"
                        ? tr("휴식", "Break")
                        : selectedAgent.status === "offline"
                          ? tr("오프라인", "Offline")
                          : tr("대기", "Idle"),
                },
                {
                  label: tr("부서", "Department"),
                  value: department ? `${department.icon} ${localeName(locale, department)}` : tr("미배정", "Unassigned"),
                },
                {
                  label: tr("레벨", "Level"),
                  value: `lv.${levelInfo.level} · ${levelTitle}`,
                },
                {
                  label: tr("누적 XP", "Total XP"),
                  value: selectedAgent.stats_xp.toLocaleString(),
                },
              ].map((item) => (
                <div
                  key={item.label}
                  className="flex items-center justify-between gap-3 border-b border-th-border-subtle py-2"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                    background: "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)",
                  }}
                >
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {item.label}
                  </span>
                  <span className="text-xs text-right" style={{ color: "var(--th-text-primary)" }}>
                    {item.value}
                  </span>
                </div>
              ))}
            </div>

            <div
              className="border-t border-th-border-subtle py-4"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                background: "color-mix(in srgb, var(--th-bg-surface) 84%, transparent)",
              }}
            >
              <div
                className="text-[11px] font-semibold uppercase tracking-[0.16em]"
                style={{ color: "var(--th-text-muted)" }}
              >
                {task.label}
              </div>
              <div className="mt-2 text-sm leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-primary)" }}>
                {task.value}
              </div>
            </div>

            <div className="space-y-2">
              <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("최근 스킬", "Recent Skills")}
              </div>
              <div className="flex flex-wrap gap-2">
                {topSkills.length > 0 ? (
                  topSkills.map((skill) => (
                    <span
                      key={`${selectedAgent.id}-${skill}`}
                      className="text-xs leading-5"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                        color: "var(--th-text-secondary)",
                      }}
                    >
                      {skill}
                    </span>
                  ))
                ) : (
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr("최근 스킬 데이터 없음", "No recent skill data")}
                  </span>
                )}
              </div>
            </div>

            <div className="flex flex-wrap gap-2">
              <SurfaceActionButton onClick={() => onEditAgent(selectedAgent)} tone="neutral">
                {tr("편집", "Edit")}
              </SurfaceActionButton>
              <SurfaceActionButton onClick={() => onDuplicateAgent(selectedAgent)} tone="info">
                {tr("복제", "Duplicate")}
              </SurfaceActionButton>
              <SurfaceActionButton onClick={() => onOpenAgent(selectedAgent)}>
                {tr("상세 보기", "Open Detail")}
              </SurfaceActionButton>
              {confirmDeleteId === selectedAgent.id ? (
                <>
                  <SurfaceActionButton
                    onClick={() => onDeleteAgent(selectedAgent.id)}
                    disabled={saving || selectedAgent.status === "working"}
                    tone="danger"
                  >
                    {tr("해고", "Fire")}
                  </SurfaceActionButton>
                  <SurfaceActionButton
                    onClick={() => setConfirmDeleteId(null)}
                    tone="neutral"
                  >
                    {tr("취소", "Cancel")}
                  </SurfaceActionButton>
                </>
              ) : (
                <SurfaceActionButton
                  onClick={() => setConfirmDeleteId(selectedAgent.id)}
                  tone="neutral"
                >
                  {tr("삭제", "Delete")}
                </SurfaceActionButton>
              )}
            </div>
          </div>
        );
      })()}
    </SurfaceCard>
  ) : (
    <p className="text-xs leading-5 text-th-text-muted">{tr("에이전트를 선택하면 상세 정보와 편집·복제·삭제 동작을 확인할 수 있습니다.", "Select an agent to view details, edit, duplicate, or delete.")}</p>
  )}
</div>
  );
}
