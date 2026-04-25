import { useEffect, useMemo, useState } from "react";
import { getSkillRanking } from "../../api";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import AgentAvatar from "../AgentAvatar";
import { getProviderMeta } from "../../app/providerTheme";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
  SurfaceSegmentButton,
} from "../common/SurfacePrimitives";
import AgentCard from "./AgentCard";
import { getAgentLevel, getAgentTitle } from "./AgentInfoCard";
import type { Translator } from "./types";
import type { AgentSortMode } from "./useAgentManagerController";
import {
  archiveBlockedByActiveTurn,
  resolveArchiveChannelImpact,
} from "./archive-impact";

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

function agentSecondaryLine(agent: Agent, locale: string) {
  if (agent.alias?.trim()) return `aka ${agent.alias.trim()}`;
  return locale === "en" ? agent.name_ko || agent.name : agent.name;
}

function currentTaskSummary(
  agent: Agent,
  tr: Translator,
): { label: string; value: string } {
  if (agent.current_task_id) {
    return {
      label: tr("현재 작업", "Current Task"),
      value: agent.current_task_id,
    };
  }
  if (agent.workflow_pack_key) {
    return {
      label: tr("워크플로우", "Workflow"),
      value: agent.workflow_pack_key,
    };
  }
  if (agent.session_info) {
    return {
      label: tr("세션", "Session"),
      value: agent.session_info,
    };
  }
  if (agent.personality) {
    return {
      label: tr("메모", "Notes"),
      value: agent.personality,
    };
  }
  return {
    label: tr("상태", "Status"),
    value: tr("대기 중", "Standing by"),
  };
}

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
        <div
          data-testid="agents-view-grid"
          className="grid gap-4 xl:grid-cols-[minmax(0,1.45fr)_minmax(320px,0.95fr)]"
        >
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2 2xl:grid-cols-3">
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
                  className="rounded-[28px] border p-4 text-left transition-transform hover:-translate-y-0.5"
                  style={{
                    background: isSelected
                      ? "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 98%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)"
                      : "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 88%, transparent) 100%)",
                    borderColor: isSelected
                      ? "color-mix(in srgb, var(--th-accent) 56%, var(--th-border))"
                      : "color-mix(in srgb, var(--th-border) 68%, transparent)",
                    boxShadow: isSelected
                      ? "0 0 0 1px color-mix(in srgb, var(--th-accent) 22%, transparent), 0 22px 50px rgba(15, 23, 42, 0.24)"
                      : undefined,
                  }}
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
                              : agent.status === "archived"
                                ? "bg-zinc-400"
                              : "bg-sky-400"
                        }`}
                        style={{ borderColor: "var(--th-card-bg)" }}
                      />
                    </div>

                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {localeName(locale, agent)}
                      </div>
                      <div className="truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {agentSecondaryLine(agent, locale)}
                      </div>
                    </div>

                    <span
                      className="rounded-full border px-2 py-0.5 text-[10px] font-medium"
                      style={{
                        borderColor: providerMeta.border,
                        background: providerMeta.bg,
                        color: providerMeta.color,
                      }}
                    >
                      {providerMeta.label}
                    </span>
                  </div>

                  <div className="mt-4 flex flex-wrap items-center gap-2">
                    {department ? (
                      <span
                        className="rounded-full border px-2 py-1 text-[11px]"
                        style={{
                          color: department.color,
                          borderColor: `color-mix(in srgb, ${department.color} 30%, var(--th-border))`,
                          background: `color-mix(in srgb, ${department.color} 12%, transparent)`,
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
                      className="rounded-full border px-2 py-1 text-[11px]"
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
            <SurfaceCard className="rounded-[30px] p-5 sm:p-6 xl:sticky xl:top-0">
              {(() => {
                const department = departments.find((candidate) => candidate.id === selectedAgent.department_id);
                const providerMeta = getProviderMeta(selectedAgent.cli_provider);
                const levelInfo = getAgentLevel(selectedAgent.stats_xp);
                const levelTitle = getAgentTitle(selectedAgent.stats_xp, isKo);
                const task = currentTaskSummary(selectedAgent, tr);
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
                        <div className="truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {agentSecondaryLine(selectedAgent, locale)} · {providerMeta.label}
                        </div>
                      </div>
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
                                  : selectedAgent.status === "archived"
                                    ? tr("보관됨", "Archived")
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
                          className="flex items-center justify-between gap-3 rounded-2xl border px-3 py-2"
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
                      className="rounded-[24px] border px-4 py-4"
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
                      <div className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
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
                              className="rounded-full border px-2 py-1 text-[11px]"
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

                    {selectedAgent.archive_state || selectedAgent.archive_reason ? (
                      <SurfaceNotice tone="neutral" compact>
                        <div className="space-y-1">
                          <div>
                            {tr("보관 상태", "Archive State")}: {selectedAgent.archive_state ?? selectedAgent.status}
                          </div>
                          {selectedAgent.archive_reason && (
                            <div>{selectedAgent.archive_reason}</div>
                          )}
                        </div>
                      </SurfaceNotice>
                    ) : null}

                    {confirmArchiveId === selectedAgent.id && (() => {
                      const channelImpact = resolveArchiveChannelImpact(selectedAgent);
                      const blocked = archiveBlockedByActiveTurn(selectedAgent);
                      const roleLabel = (role: "primary" | "alt" | "codex") =>
                        role === "primary"
                          ? tr("기본", "Primary")
                          : role === "alt"
                            ? tr("대체", "Alt")
                            : tr("Codex", "Codex");
                      return (
                        <SurfaceNotice tone={blocked ? "danger" : "warn"} data-testid="archive-confirm-impact">
                          <div className="space-y-2">
                            <div className="font-medium">
                              {tr(
                                "보관하면 role_map에서 비활성화되고 아래 Discord 채널이 readonly 처리됩니다.",
                                "Archiving disables the role map entry and makes the following Discord channels readonly.",
                              )}
                            </div>
                            {channelImpact.length > 0 ? (
                              <ul
                                className="space-y-1 rounded-xl border px-3 py-2 text-xs"
                                style={{
                                  borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                                  background: "color-mix(in srgb, var(--th-bg-surface) 86%, transparent)",
                                  color: "var(--th-text-secondary)",
                                }}
                                data-testid="archive-confirm-channels"
                              >
                                {channelImpact.map((channel) => (
                                  <li
                                    key={`${channel.role}-${channel.id}`}
                                    className="flex items-center justify-between gap-3"
                                  >
                                    <span className="font-mono">#{channel.id}</span>
                                    <span
                                      className="rounded-full border px-2 py-0.5 text-[10px]"
                                      style={{
                                        borderColor:
                                          "color-mix(in srgb, var(--th-border) 60%, transparent)",
                                        color: "var(--th-text-muted)",
                                      }}
                                    >
                                      {roleLabel(channel.role)}
                                    </span>
                                  </li>
                                ))}
                              </ul>
                            ) : (
                              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                                {tr(
                                  "연결된 Discord 채널이 없습니다.",
                                  "No Discord channels are bound to this agent.",
                                )}
                              </div>
                            )}
                            {blocked ? (
                              <div className="text-xs font-medium" style={{ color: "var(--th-accent-danger)" }}>
                                {tr(
                                  "현재 진행 중인 턴이 있어 API가 보관 요청을 거부합니다. 턴이 끝난 뒤 다시 시도하세요.",
                                  "An active turn will cause the API to reject the request. Wait for the turn to finish.",
                                )}
                              </div>
                            ) : (
                              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                                {tr(
                                  "보관 해제하면 동일한 채널로 role_map 이 복원됩니다.",
                                  "Unarchiving will restore the role_map binding to the same channels.",
                                )}
                              </div>
                            )}
                          </div>
                        </SurfaceNotice>
                      );
                    })()}

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
                      {selectedAgent.status === "archived" ? (
                        <SurfaceActionButton
                          onClick={() => onUnarchiveAgent(selectedAgent.id)}
                          disabled={saving}
                          tone="success"
                        >
                          {tr("보관 해제", "Unarchive")}
                        </SurfaceActionButton>
                      ) : confirmArchiveId === selectedAgent.id ? (
                        <>
                          <SurfaceActionButton
                            onClick={() => onArchiveAgent(selectedAgent.id)}
                            disabled={saving || selectedAgent.status === "working"}
                            tone="warn"
                          >
                            {tr("보관", "Archive")}
                          </SurfaceActionButton>
                          <SurfaceActionButton
                            onClick={() => setConfirmArchiveId(null)}
                            tone="neutral"
                          >
                            {tr("취소", "Cancel")}
                          </SurfaceActionButton>
                        </>
                      ) : (
                        <SurfaceActionButton
                          onClick={() => {
                            setConfirmDeleteId(null);
                            setConfirmArchiveId(selectedAgent.id);
                          }}
                          tone="neutral"
                        >
                          {tr("보관", "Archive")}
                        </SurfaceActionButton>
                      )}
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
                          onClick={() => {
                            setConfirmArchiveId(null);
                            setConfirmDeleteId(selectedAgent.id);
                          }}
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
            <SurfaceCard className="rounded-[30px] p-8 text-center">
              <div className="text-3xl">🧑‍💻</div>
              <div className="mt-3 text-sm font-medium" style={{ color: "var(--th-text-secondary)" }}>
                {tr("에이전트를 선택하세요", "Select an agent")}
              </div>
              <div className="mt-2 text-xs leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "상세 정보와 운영 액션이 오른쪽 패널에 열립니다.",
                  "Details and operational actions appear in the side panel.",
                )}
              </div>
            </SurfaceCard>
          )}
        </div>
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
