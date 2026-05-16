import { useEffect, useRef, useState } from "react";
import {
  getAgentWarnings,
  getAgentWorkElapsedMs,
  getAgentWorkSummary,
} from "../../agent-insights";
import type {
  Agent,
  Department,
  DispatchedSession,
} from "../../types";
import { localeName } from "../../i18n";
import AgentAvatar from "../AgentAvatar";
import {
  SurfaceActionButton,
  SurfaceMetricPill,
  SurfaceSection,
} from "../common/SurfacePrimitives";
import { STATUS_DOT } from "./constants";
import type { Translator } from "./types";
import * as api from "../../api";
import type {
  CronJob,
  AgentSkill,
  DiscordBinding,
  AgentOfficeMembership,
} from "../../api/client";
import { getAgentLevel, getAgentTitle } from "./agentProgress";
import { AgentInfoOperationsSections } from "./AgentInfoOperationsSections";
import { AgentInfoProfileSections } from "./AgentInfoProfileSections";
import { AgentInfoRoutingSections } from "./AgentInfoRoutingSections";
import { inferBindingSource } from "./AgentInfoCardModel";
import { isDiscordSnowflake } from "./discord-routing";

export { getAgentLevel, getAgentTitle } from "./agentProgress";

interface AgentInfoCardProps {
  agent: Agent;
  spriteMap: Map<string, number>;
  isKo: boolean;
  locale: string;
  tr: Translator;
  departments: Department[];
  onClose: () => void;
  onAgentUpdated?: () => void;
}

export default function AgentInfoCard({
  agent,
  spriteMap,
  isKo,
  locale,
  tr,
  departments,
  onClose,
  onAgentUpdated,
}: AgentInfoCardProps) {
  const overlayRef = useRef<HTMLDivElement>(null);
  const [cronJobs, setCronJobs] = useState<CronJob[]>([]);
  const [agentSkills, setAgentSkills] = useState<AgentSkill[]>([]);
  const [sharedSkills, setSharedSkills] = useState<AgentSkill[]>([]);
  const [loadingCron, setLoadingCron] = useState(true);
  const [loadingSkills, setLoadingSkills] = useState(true);
  const [loadingClaudeSessions, setLoadingClaudeSessions] = useState(true);
  const [claudeSessions, setClaudeSessions] = useState<DispatchedSession[]>([]);
  const [showSharedSkills, setShowSharedSkills] = useState(false);
  const [discordBindings, setDiscordBindings] = useState<DiscordBinding[]>([]);
  const [discordChannelInfoById, setDiscordChannelInfoById] = useState<
    Record<string, api.DiscordChannelInfo>
  >({});
  const [loadingBindings, setLoadingBindings] = useState(true);
  const [auditLogs, setAuditLogs] = useState<Array<{ id: string; action: string; ts: number; detail?: string; summary?: string; created_at?: number }>>([]);
  const [loadingAudit, setLoadingAudit] = useState(true);
  const [editingAlias, setEditingAlias] = useState(false);
  const [aliasValue, setAliasValue] = useState(agent.alias ?? "");
  const [savingAlias, setSavingAlias] = useState(false);
  const [selectedDeptId, setSelectedDeptId] = useState(
    agent.department_id ?? "",
  );
  const [savingDept, setSavingDept] = useState(false);
  const [selectedProvider, setSelectedProvider] = useState<string>(
    agent.cli_provider ?? "claude",
  );
  const [savingProvider, setSavingProvider] = useState(false);
  const [officeMemberships, setOfficeMemberships] = useState<
    AgentOfficeMembership[]
  >([]);
  const [loadingOffices, setLoadingOffices] = useState(true);
  const [savingOfficeIds, setSavingOfficeIds] = useState<
    Record<string, boolean>
  >({});
  const [timeline, setTimeline] = useState<api.TimelineEvent[]>([]);
  const [loadingTimeline, setLoadingTimeline] = useState(true);
  const [timelineOpen, setTimelineOpen] = useState(false);
  const [sessionsOpen, setSessionsOpen] = useState(true);
  const [routingOpen, setRoutingOpen] = useState(false);
  const [idleSessionsOpen, setIdleSessionsOpen] = useState(false);

  const saveAlias = async () => {
    const trimmed = aliasValue.trim();
    const newAlias = trimmed || null;
    if (newAlias === (agent.alias ?? null)) {
      setEditingAlias(false);
      return;
    }
    setSavingAlias(true);
    try {
      await api.updateAgent(agent.id, { alias: newAlias });
      setEditingAlias(false);
      onAgentUpdated?.();
    } catch (e) {
      console.error("Alias save failed:", e);
    } finally {
      setSavingAlias(false);
    }
  };

  useEffect(() => {
    setAliasValue(agent.alias ?? "");
    setSelectedDeptId(agent.department_id ?? "");
    setSelectedProvider(agent.cli_provider ?? "claude");
    setSessionsOpen(true);
    setRoutingOpen(false);
    setIdleSessionsOpen(false);
  }, [agent.alias, agent.department_id, agent.cli_provider, agent.id]);

  const saveDepartment = async (nextDeptId: string) => {
    const previousDeptId = selectedDeptId;
    if ((nextDeptId || "") === previousDeptId) return;

    setSelectedDeptId(nextDeptId);
    setSavingDept(true);
    try {
      await api.updateAgent(agent.id, { department_id: nextDeptId || null });
      onAgentUpdated?.();
    } catch (e) {
      setSelectedDeptId(previousDeptId);
      console.error("Department save failed:", e);
    } finally {
      setSavingDept(false);
    }
  };

  const saveProvider = async (nextProvider: string) => {
    if (nextProvider === selectedProvider) return;
    const previousProvider = selectedProvider;
    setSelectedProvider(nextProvider);
    setSavingProvider(true);
    try {
      await api.updateAgent(agent.id, {
        cli_provider: nextProvider as Agent["cli_provider"],
      });
      onAgentUpdated?.();
    } catch (e) {
      setSelectedProvider(previousProvider);
      console.error("Provider save failed:", e);
    } finally {
      setSavingProvider(false);
    }
  };

  const toggleOfficeMembership = async (office: AgentOfficeMembership) => {
    const nextAssigned = !office.assigned;

    setSavingOfficeIds((prev) => ({ ...prev, [office.id]: true }));
    setOfficeMemberships((prev) =>
      prev.map((item) =>
        item.id === office.id ? { ...item, assigned: nextAssigned } : item,
      ),
    );

    try {
      if (nextAssigned) {
        await api.addAgentToOffice(office.id, agent.id);
      } else {
        await api.removeAgentFromOffice(office.id, agent.id);
      }
      onAgentUpdated?.();
    } catch (e) {
      setOfficeMemberships((prev) =>
        prev.map((item) =>
          item.id === office.id ? { ...item, assigned: office.assigned } : item,
        ),
      );
      console.error("Office membership toggle failed:", e);
    } finally {
      setSavingOfficeIds((prev) => {
        const next = { ...prev };
        delete next[office.id];
        return next;
      });
    }
  };

  const dept = departments.find((d) => d.id === selectedDeptId);
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  useEffect(() => {
    setLoadingCron(true);
    api
      .getAgentCron(agent.id)
      .then((jobs) => {
        setCronJobs(jobs);
        setLoadingCron(false);
      })
      .catch(() => setLoadingCron(false));

    setLoadingSkills(true);
    api
      .getAgentSkills(agent.id)
      .then((data) => {
        setAgentSkills(data.skills);
        setSharedSkills(data.sharedSkills);
        setLoadingSkills(false);
      })
      .catch(() => setLoadingSkills(false));

    setLoadingClaudeSessions(true);
    api
      .getAgentDispatchedSessions(agent.id)
      .then((rows) => {
        setClaudeSessions(rows);
        setLoadingClaudeSessions(false);
      })
      .catch(() => setLoadingClaudeSessions(false));

    setLoadingBindings(true);
    api
      .getDiscordBindings()
      .then((bindings) => {
        setDiscordBindings(bindings.filter((b) => b.agentId === agent.id));
        setLoadingBindings(false);
      })
      .catch(() => {
        setDiscordBindings([]);
        setLoadingBindings(false);
      });

    setLoadingOffices(true);
    api
      .getAgentOffices(agent.id)
      .then((offices) => {
        setOfficeMemberships(offices);
        setLoadingOffices(false);
      })
      .catch(() => {
        setOfficeMemberships([]);
        setLoadingOffices(false);
      });

    setLoadingTimeline(true);
    api
      .getAgentTimeline(agent.id, 30)
      .then((events) => {
        setTimeline(events);
        setLoadingTimeline(false);
      })
      .catch(() => {
        setTimeline([]);
        setLoadingTimeline(false);
      });
  }, [agent.id]);

  useEffect(() => {
    const seedIds = Array.from(
      new Set(
        [
          ...discordBindings.flatMap((binding) => [
            binding.channelId,
            binding.counterModelChannelId ?? null,
          ]),
          ...claudeSessions.map((session) => session.thread_channel_id ?? null),
        ].filter((value): value is string => isDiscordSnowflake(value)),
      ),
    );

    if (seedIds.length === 0) {
      setDiscordChannelInfoById({});
      return;
    }

    let cancelled = false;

    const loadChannelInfo = async (ids: string[]) => {
      const entries = await Promise.all(
        ids.map(async (id) => {
          try {
            const info = await api.getDiscordChannelInfo(id);
            return info?.id ? ([id, info] as const) : null;
          } catch {
            return null;
          }
        }),
      );
      return Object.fromEntries(
        entries.filter(
          (entry): entry is readonly [string, api.DiscordChannelInfo] =>
            entry !== null,
        ),
      );
    };

    void (async () => {
      const initialInfo = await loadChannelInfo(seedIds);
      const parentIds = Array.from(
        new Set(
          Object.values(initialInfo)
            .map((info) => info.parent_id ?? null)
            .filter(
              (value): value is string =>
                isDiscordSnowflake(value) && !(value in initialInfo),
            ),
        ),
      );
      const parentInfo =
        parentIds.length > 0 ? await loadChannelInfo(parentIds) : {};

      if (!cancelled) {
        setDiscordChannelInfoById({ ...initialInfo, ...parentInfo });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [claudeSessions, discordBindings]);

  const statusLabel: Record<string, { ko: string; en: string }> = {
    working: { ko: "근무 중", en: "Working" },
    idle: { ko: "대기", en: "Idle" },
    break: { ko: "휴식", en: "Break" },
    offline: { ko: "오프라인", en: "Offline" },
  };

  const sourceLabel =
    agent.activity_source === "agentdesk"
      ? tr("AgentDesk 작업", "AgentDesk")
      : null;

  const workingLinkedSessions = claudeSessions.filter(
    (session) => session.status === "working",
  );
  const idleLinkedSessions = claudeSessions.filter(
    (session) => session.status !== "working",
  );
  const currentWorkSummary = getAgentWorkSummary(agent, {
    linkedSessions: workingLinkedSessions,
  });
  const currentWorkElapsedMs = getAgentWorkElapsedMs(
    agent,
    workingLinkedSessions,
  );
  const warnings = getAgentWarnings(agent, {
    hasDiscordBindings: loadingBindings
      ? undefined
      : discordBindings.length > 0,
    linkedSessions: workingLinkedSessions,
  });
  const currentWorkDetails = Array.from(
    new Set(
      [
        agent.session_info,
        ...workingLinkedSessions.flatMap((session) => [
          session.session_info,
          session.name,
        ]),
      ].filter((value): value is string => Boolean(value && value.trim())),
    ),
  ).slice(0, 3);
  const roleMapBindings = discordBindings.filter(
    (binding) => inferBindingSource(binding) === "role-map",
  );
  const resolveDiscordChannelInfo = (
    channelId: string | null | undefined,
  ): api.DiscordChannelInfo | null =>
    channelId && isDiscordSnowflake(channelId)
      ? discordChannelInfoById[channelId] ?? null
      : null;
  const resolveDiscordParentInfo = (
    channelInfo: api.DiscordChannelInfo | null | undefined,
  ): api.DiscordChannelInfo | null =>
    channelInfo?.parent_id && isDiscordSnowflake(channelInfo.parent_id)
      ? discordChannelInfoById[channelInfo.parent_id] ?? null
      : null;
  const sourceOfTruthRows: Array<{ label: string; value: string; tone?: string }> = [
    { label: tr("Agent ID", "Agent ID"), value: agent.id },
    { label: tr("이름", "Name"), value: agent.name },
  ];
  const primaryName = localeName(locale, agent);
  const secondaryName = locale === "en" ? agent.name_ko || "" : agent.name;
  const levelInfo = getAgentLevel(agent.stats_xp);
  const levelTitle = getAgentTitle(agent.stats_xp, isKo);

  return (
    <div
      ref={overlayRef}
      className="fixed inset-0 z-50 flex items-start justify-center overflow-hidden px-3 py-4 sm:items-center sm:p-4"
      style={{
        background: "var(--th-modal-overlay)",
        paddingTop: "max(1rem, calc(env(safe-area-inset-top) + 0.75rem))",
        paddingBottom: "max(1rem, calc(env(safe-area-inset-bottom) + 0.75rem))",
      }}
      onClick={(e) => {
        if (e.target === overlayRef.current) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={`${primaryName} — ${tr("직원 상세", "Agent Details")}`}
        className="w-full self-start max-h-[calc(100dvh-env(safe-area-inset-top)-env(safe-area-inset-bottom)-1.5rem)] max-w-[calc(100vw-1.5rem)] overflow-x-hidden overflow-y-auto overscroll-contain rounded-[32px] border p-4 shadow-2xl animate-in fade-in zoom-in-95 duration-200 sm:my-auto sm:max-h-[90vh] sm:max-w-4xl sm:p-5"
        style={{
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          WebkitOverflowScrolling: "touch",
          touchAction: "pan-y",
        }}
      >
        <div className="space-y-4">
          <SurfaceSection
            eyebrow={tr("직원 상세", "Agent Details")}
            title={primaryName}
            description={primaryName !== secondaryName && secondaryName ? secondaryName : undefined}
            actions={(
              <SurfaceActionButton onClick={onClose} tone="neutral" aria-label="Close">
                {tr("닫기", "Close")}
              </SurfaceActionButton>
            )}
          >
            <div className="mt-4 flex flex-col gap-4 md:flex-row md:items-start">
              <div className="relative shrink-0">
                <AgentAvatar agent={agent} spriteMap={spriteMap} size={64} rounded="xl" />
                <div
                  className={`absolute -bottom-0.5 -right-0.5 h-4 w-4 rounded-full border-2 ${STATUS_DOT[agent.status] ?? STATUS_DOT.idle}`}
                  style={{ borderColor: "var(--th-card-bg)" }}
                />
              </div>
              <div className="min-w-0 flex-1 space-y-3">
                <div className="flex flex-wrap items-center gap-2">
                  {editingAlias ? (
                    <input
                      autoFocus
                      value={aliasValue}
                      onChange={(e) => setAliasValue(e.target.value)}
                      onKeyDown={(e) => { if (e.key === "Enter") saveAlias(); if (e.key === "Escape") { setEditingAlias(false); setAliasValue(agent.alias ?? ""); } }}
                      onBlur={saveAlias}
                      disabled={savingAlias}
                      placeholder={tr("별명 입력", "Enter alias")}
                      className="rounded-xl border px-2 py-1 text-xs outline-none"
                      style={{
                        background: "var(--th-bg-surface)",
                        borderColor: "var(--th-input-border)",
                        color: "var(--th-text-primary)",
                        width: "140px",
                      }}
                    />
                  ) : (
                    <SurfaceActionButton
                      onClick={() => { setAliasValue(agent.alias ?? ""); setEditingAlias(true); }}
                      tone="neutral"
                      compact
                      title={tr("별명 편집", "Edit alias")}
                    >
                      {agent.alias ? `aka ${agent.alias}` : `+ ${tr("별명", "alias")}`}
                    </SurfaceActionButton>
                  )}
                  <span
                    className="rounded-full px-2 py-0.5 text-xs font-medium"
                    style={{
                      background: agent.status === "working" ? "rgba(16,185,129,0.15)" :
                        agent.status === "break" ? "rgba(245,158,11,0.15)" :
                        agent.status === "offline" ? "rgba(239,68,68,0.15)" :
                        "rgba(100,116,139,0.15)",
                      color: agent.status === "working" ? "#34d399" :
                        agent.status === "break" ? "#fbbf24" :
                        agent.status === "offline" ? "#f87171" :
                        "#94a3b8",
                    }}
                  >
                    {isKo ? statusLabel[agent.status]?.ko : statusLabel[agent.status]?.en}
                  </span>
                  {agent.status === "working" && sourceLabel && (
                    <span
                      className="rounded-full px-2 py-0.5 text-xs"
                      style={{
                        background: "color-mix(in srgb, var(--th-accent-primary-soft) 80%, transparent)",
                        color: "var(--th-accent-primary)",
                      }}
                    >
                      {sourceLabel}
                    </span>
                  )}
                  <span className="rounded-full px-2 py-0.5 text-xs" style={{ background: "var(--th-bg-surface)", color: "var(--th-text-muted)" }}>
                    {dept ? `${dept.icon} ${localeName(locale, dept)}` : tr("미배정", "Unassigned")}
                  </span>
                </div>
                <div className="flex flex-wrap gap-2">
                  <SurfaceMetricPill
                    label={tr("레벨", "Level")}
                    tone="accent"
                    value={`Lv.${levelInfo.level} ${levelTitle}`}
                  />
                  <SurfaceMetricPill
                    label={tr("완료", "Done")}
                    tone="success"
                    value={`${agent.stats_tasks_done} ${tr("건", "tasks")}`}
                  />
                  <SurfaceMetricPill
                    label="AgentDesk"
                    tone="info"
                    value={`${workingLinkedSessions.length}/${claudeSessions.length}`}
                  />
                </div>
              </div>
            </div>
          </SurfaceSection>

          <div className="grid min-w-0 gap-4 md:grid-cols-2">
            <AgentInfoProfileSections
              agent={agent}
              departments={departments}
              locale={locale}
              isKo={isKo}
              tr={tr}
              selectedDeptId={selectedDeptId}
              savingDept={savingDept}
              onSaveDepartment={(nextDeptId) => void saveDepartment(nextDeptId)}
              selectedProvider={selectedProvider}
              savingProvider={savingProvider}
              onSaveProvider={(nextProvider) => void saveProvider(nextProvider)}
              loadingOffices={loadingOffices}
              officeMemberships={officeMemberships}
              savingOfficeIds={savingOfficeIds}
              onToggleOfficeMembership={(office) => void toggleOfficeMembership(office)}
              currentWorkSummary={currentWorkSummary}
              currentWorkElapsedMs={currentWorkElapsedMs}
              currentWorkDetails={currentWorkDetails}
              discordBindings={discordBindings}
              warnings={warnings}
            />

            <AgentInfoRoutingSections
              tr={tr}
              discordBindings={discordBindings}
              roleMapBindings={roleMapBindings}
              claudeSessions={claudeSessions}
              loadingClaudeSessions={loadingClaudeSessions}
              sourceOfTruthRows={sourceOfTruthRows}
              resolveDiscordChannelInfo={resolveDiscordChannelInfo}
              resolveDiscordParentInfo={resolveDiscordParentInfo}
            />

            <AgentInfoOperationsSections
              agent={agent}
              isKo={isKo}
              tr={tr}
              cronJobs={cronJobs}
              loadingCron={loadingCron}
              auditLogs={auditLogs}
              loadingAudit={loadingAudit}
              agentSkills={agentSkills}
              sharedSkills={sharedSkills}
              loadingSkills={loadingSkills}
              showSharedSkills={showSharedSkills}
              onToggleSharedSkills={() => setShowSharedSkills((value) => !value)}
              levelInfo={levelInfo}
              levelTitle={levelTitle}
              timeline={timeline}
              loadingTimeline={loadingTimeline}
              timelineOpen={timelineOpen}
              onToggleTimeline={() => setTimelineOpen((value) => !value)}
            />
          </div>
        </div>
      </div>
    </div>
  );
}
