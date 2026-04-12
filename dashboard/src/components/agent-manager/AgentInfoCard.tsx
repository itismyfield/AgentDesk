import { useEffect, useRef, useState } from "react";
import {
  formatElapsedCompact,
  getAgentWarnings,
  getAgentWorkElapsedMs,
  getAgentWorkSummary,
} from "../../agent-insights";
import type {
  Agent,
  AuditLogEntry,
  Department,
  DispatchedSession,
} from "../../types";
import { localeName } from "../../i18n";
import AgentAvatar from "../AgentAvatar";
import { STATUS_DOT } from "./constants";
import TurnTranscriptPanel from "./TurnTranscriptPanel";
import type { Translator } from "./types";
import * as api from "../../api";
import type {
  CronJob,
  AgentSkill,
  DiscordBinding,
  DiscordChannelInfo,
  AgentOfficeMembership,
} from "../../api/client";
import {
  describeDiscordBinding,
  describeDiscordTarget,
  describeDispatchedSession,
  formatDiscordSummary,
  isDiscordSnowflake,
} from "./discord-routing";

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

function formatSchedule(schedule: CronJob["schedule"], isKo: boolean): string {
  if (schedule.kind === "every" && schedule.everyMs) {
    const mins = Math.round(schedule.everyMs / 60000);
    if (mins >= 60) {
      const hrs = Math.round(mins / 60);
      return isKo ? `${hrs}시간마다` : `Every ${hrs}h`;
    }
    return isKo ? `${mins}분마다` : `Every ${mins}m`;
  }
  if (schedule.kind === "cron" && schedule.cron) {
    return schedule.cron;
  }
  if (schedule.kind === "at" && schedule.atMs) {
    return new Date(schedule.atMs).toLocaleString();
  }
  return schedule.kind;
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function timeAgo(ms: number, isKo: boolean): string {
  const diff = Date.now() - ms;
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return isKo ? "방금" : "just now";
  if (mins < 60) return isKo ? `${mins}분 전` : `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return isKo ? `${hrs}시간 전` : `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  return isKo ? `${days}일 전` : `${days}d ago`;
}

// Gamification: XP-based level system
const LEVEL_THRESHOLDS = [
  0, 100, 300, 600, 1000, 1600, 2500, 4000, 6000, 10000,
];
const LEVEL_TITLES_KO = [
  "신입",
  "수습",
  "사원",
  "주임",
  "대리",
  "과장",
  "차장",
  "부장",
  "이사",
  "사장",
];
const LEVEL_TITLES_EN = [
  "Newbie",
  "Trainee",
  "Staff",
  "Associate",
  "Sr. Associate",
  "Manager",
  "Asst. Dir.",
  "Director",
  "VP",
  "President",
];

export function getAgentLevel(xp: number) {
  let level = 1;
  for (let i = LEVEL_THRESHOLDS.length - 1; i >= 0; i--) {
    if (xp >= LEVEL_THRESHOLDS[i]) {
      level = i + 1;
      break;
    }
  }
  const nextThreshold =
    LEVEL_THRESHOLDS[Math.min(level, LEVEL_THRESHOLDS.length - 1)] ?? Infinity;
  const currentThreshold = LEVEL_THRESHOLDS[level - 1] ?? 0;
  const progress =
    nextThreshold === Infinity
      ? 1
      : (xp - currentThreshold) / (nextThreshold - currentThreshold);
  return {
    level,
    progress: Math.min(1, progress),
    nextThreshold,
    currentThreshold,
  };
}

export function getAgentTitle(xp: number, isKo: boolean) {
  const { level } = getAgentLevel(xp);
  const idx = Math.min(level - 1, LEVEL_TITLES_KO.length - 1);
  return isKo ? LEVEL_TITLES_KO[idx] : LEVEL_TITLES_EN[idx];
}

const ACTIVITY_SOURCE_COLORS: Record<string, string> = {
  agentdesk: "#a78bfa",
  idle: "#64748b",
};

function inferBindingSource(binding: DiscordBinding): string {
  if (binding.channelId.startsWith("dm:")) return "dm";
  if (binding.source) return binding.source;
  return "channel";
}

function bindingSourceLabel(source: string): string {
  switch (source) {
    case "role-map":
      return "RoleMap";
    case "primary":
      return "Primary";
    case "alt":
      return "Alt";
    case "codex":
      return "Codex";
    case "dm":
      return "DM";
    default:
      return "Channel";
  }
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
  const [discordChannels, setDiscordChannels] = useState<Record<string, DiscordChannelInfo>>({});
  const [loadingBindings, setLoadingBindings] = useState(true);
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
  const [auditLogs, setAuditLogs] = useState<AuditLogEntry[]>([]);
  const [loadingAudit, setLoadingAudit] = useState(true);
  const [timeline, setTimeline] = useState<api.TimelineEvent[]>([]);
  const [loadingTimeline, setLoadingTimeline] = useState(true);
  const [timelineOpen, setTimelineOpen] = useState(false);

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

    setLoadingAudit(true);
    api
      .getAuditLogs(8, { entityType: "agent", entityId: agent.id })
      .then((rows) => {
        setAuditLogs(rows);
        setLoadingAudit(false);
      })
      .catch(() => {
        setAuditLogs([]);
        setLoadingAudit(false);
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
    const queue: string[] = [];
    const queued = new Set<string>();
    const enqueue = (value: string | null | undefined) => {
      if (!isDiscordSnowflake(value) || queued.has(value) || discordChannels[value]) {
        return;
      }
      queued.add(value);
      queue.push(value);
    };

    discordBindings.forEach((binding) => {
      enqueue(binding.channelId);
      enqueue(binding.counterModelChannelId);
    });
    claudeSessions.forEach((session) => enqueue(session.thread_channel_id));

    if (queue.length === 0) return;

    let stale = false;
    void (async () => {
      const fetched: Record<string, DiscordChannelInfo> = {};
      while (queue.length > 0) {
        const channelId = queue.shift();
        if (!channelId) continue;
        try {
          const info = await api.getDiscordChannelInfo(channelId);
          if (!info?.id) continue;
          fetched[info.id] = info;
          if (
            isDiscordSnowflake(info.parent_id)
            && !queued.has(info.parent_id)
            && !discordChannels[info.parent_id]
            && !fetched[info.parent_id]
          ) {
            queued.add(info.parent_id);
            queue.push(info.parent_id);
          }
        } catch {
          // Ignore per-channel lookup failures and keep the raw fallback text.
        }
      }

      if (!stale && Object.keys(fetched).length > 0) {
        setDiscordChannels((prev) => ({ ...prev, ...fetched }));
      }
    })();

    return () => {
      stale = true;
    };
  }, [agent.id, claudeSessions, discordBindings, discordChannels]);

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
  const getDiscordChannel = (channelId: string | null | undefined) =>
    channelId ? discordChannels[channelId] : undefined;

  const workingLinkedSessions = claudeSessions.filter(
    (session) => session.status === "working",
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
          formatDiscordSummary(
            describeDispatchedSession(
              session,
              getDiscordChannel(session.thread_channel_id),
              getDiscordChannel(
                getDiscordChannel(session.thread_channel_id)?.parent_id,
              ),
            ),
          ),
        ]),
      ].filter((value): value is string => Boolean(value && value.trim())),
    ),
  ).slice(0, 3);
  const roleMapBindings = discordBindings.filter(
    (binding) => inferBindingSource(binding) === "role-map",
  );
  const dbBindings = discordBindings.filter(
    (binding) => inferBindingSource(binding) !== "role-map",
  );
  const sourceOfTruthRows = [
    {
      label: tr("DB 레코드", "DB Record"),
      value: agent.id,
      tone: "#60a5fa",
    },
    {
      label: tr("Role ID", "Role ID"),
      value: agent.role_id || tr("없음", "None"),
      tone: agent.role_id ? "#34d399" : "#94a3b8",
    },
    {
      label: tr("Launchd 귀속", "Launchd Ownership"),
      value:
        cronJobs.length > 0 ? `${cronJobs.length} job` : tr("없음", "None"),
      tone: cronJobs.length > 0 ? "#34d399" : "#94a3b8",
    },
    {
      label: tr("RoleMap 경로", "RoleMap Route"),
      value:
        roleMapBindings.length > 0
          ? `${roleMapBindings.length} route`
          : tr("없음", "None"),
      tone: roleMapBindings.length > 0 ? "#fbbf24" : "#94a3b8",
    },
    {
      label: tr("Discord 경로", "Discord Routes"),
      value: `${discordBindings.length}`,
      tone: discordBindings.length > 0 ? "#a78bfa" : "#94a3b8",
    },
    {
      label: tr("AgentDesk 링크", "AgentDesk Links"),
      value: `${workingLinkedSessions.length}/${claudeSessions.length}`,
      tone: workingLinkedSessions.length > 0 ? "#38bdf8" : "#94a3b8",
    },
  ];

  return (
    <div
      ref={overlayRef}
      className="fixed inset-0 z-50 flex items-end justify-center p-0 sm:items-center sm:p-4"
      style={{
        background: "var(--th-modal-overlay)",
        paddingTop: "calc(1rem + env(safe-area-inset-top))",
      }}
      onClick={(e) => {
        if (e.target === overlayRef.current) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={`${localeName(locale, agent)} — ${tr("직원 상세", "Agent Details")}`}
        className="w-full max-w-lg max-h-full overflow-y-auto overscroll-contain rounded-t-3xl shadow-2xl animate-in fade-in zoom-in-95 duration-200 sm:max-h-[90vh] sm:rounded-2xl"
        style={{
          background: "var(--th-card-bg)",
          border: "1px solid var(--th-card-border)",
          backdropFilter: "blur(20px)",
          paddingBottom: "max(env(safe-area-inset-bottom), 0px)",
        }}
      >
        {/* Header */}
        <div
          className="flex items-center gap-4 p-5"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div className="relative shrink-0">
            <AgentAvatar
              agent={agent}
              spriteMap={spriteMap}
              size={56}
              rounded="xl"
            />
            <div
              className={`absolute -bottom-0.5 -right-0.5 w-3.5 h-3.5 rounded-full border-2 ${STATUS_DOT[agent.status] ?? STATUS_DOT.idle}`}
              style={{ borderColor: "var(--th-card-bg)" }}
            />
          </div>
          <div className="flex-1 min-w-0">
            <div
              className="font-bold text-base"
              style={{ color: "var(--th-text-heading)" }}
            >
              {localeName(locale, agent)}
            </div>
            {(() => {
              const primary = localeName(locale, agent);
              const sub = locale === "en" ? agent.name_ko || "" : agent.name;
              return primary !== sub && sub ? (
                <div
                  className="text-xs mt-0.5"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {sub}
                </div>
              ) : null;
            })()}
            <div className="flex items-center gap-1 mt-1">
              {editingAlias ? (
                <input
                  autoFocus
                  value={aliasValue}
                  onChange={(e) => setAliasValue(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") saveAlias();
                    if (e.key === "Escape") {
                      setEditingAlias(false);
                      setAliasValue(agent.alias ?? "");
                    }
                  }}
                  onBlur={saveAlias}
                  disabled={savingAlias}
                  placeholder={tr("별명 입력", "Enter alias")}
                  className="text-xs px-1.5 py-0.5 rounded border outline-none"
                  style={{
                    background: "var(--th-bg-surface)",
                    borderColor: "var(--th-input-border)",
                    color: "var(--th-text-primary)",
                    width: "120px",
                  }}
                />
              ) : (
                <button
                  onClick={() => {
                    setAliasValue(agent.alias ?? "");
                    setEditingAlias(true);
                  }}
                  className="text-xs px-1.5 py-0.5 rounded hover:bg-[var(--th-bg-surface-hover)] transition-colors"
                  style={{
                    color: agent.alias
                      ? "var(--th-text-secondary)"
                      : "var(--th-text-muted)",
                  }}
                  title={tr("별명 편집", "Edit alias")}
                >
                  {agent.alias
                    ? `aka ${agent.alias}`
                    : `+ ${tr("별명", "alias")}`}
                </button>
              )}
            </div>
            <div className="flex items-center gap-2 mt-1.5">
              <span
                className="text-xs px-2 py-0.5 rounded-full font-medium"
                style={{
                  background:
                    agent.status === "working"
                      ? "rgba(16,185,129,0.15)"
                      : agent.status === "break"
                        ? "rgba(245,158,11,0.15)"
                        : agent.status === "offline"
                          ? "rgba(239,68,68,0.15)"
                          : "rgba(100,116,139,0.15)",
                  color:
                    agent.status === "working"
                      ? "#34d399"
                      : agent.status === "break"
                        ? "#fbbf24"
                        : agent.status === "offline"
                          ? "#f87171"
                          : "#94a3b8",
                }}
              >
                {isKo
                  ? statusLabel[agent.status]?.ko
                  : statusLabel[agent.status]?.en}
              </span>
              {agent.status === "working" && sourceLabel && (
                <span
                  className="text-xs px-2 py-0.5 rounded-full"
                  style={{
                    background: "rgba(99,102,241,0.18)",
                    color: "#a5b4fc",
                  }}
                >
                  {sourceLabel}
                </span>
              )}
              {dept && (
                <span
                  className="text-xs px-2 py-0.5 rounded-full"
                  style={{
                    background: "var(--th-bg-surface)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {dept.icon} {localeName(locale, dept)}
                </span>
              )}
              {!dept && (
                <span
                  className="text-xs px-2 py-0.5 rounded-full"
                  style={{
                    background: "var(--th-bg-surface)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {tr("미배정", "Unassigned")}
                </span>
              )}
            </div>
          </div>
          <button
            onClick={onClose}
            className="w-11 h-11 rounded-lg flex items-center justify-center hover:bg-[var(--th-bg-surface-hover)] transition-colors self-start"
            style={{ color: "var(--th-text-muted)" }}
            aria-label="Close"
          >
            ✕
          </button>
        </div>

        <TurnTranscriptPanel
          source={{
            type: "agent",
            id: agent.id,
            refreshSeed: `${agent.status}:${agent.session_info ?? ""}:${agent.current_task_id ?? ""}`,
          }}
          isKo={isKo}
          tr={tr}
        />

        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("소속 부서", "Department")}
          </div>
          <div className="flex items-center gap-2">
            <select
              value={selectedDeptId}
              onChange={(e) => void saveDepartment(e.target.value)}
              disabled={savingDept}
              className="flex-1 px-3 py-2 rounded-lg text-sm outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="">{tr("— 미배정 —", "— Unassigned —")}</option>
              {departments.map((d) => (
                <option key={d.id} value={d.id}>
                  {d.icon} {localeName(locale, d)}
                </option>
              ))}
            </select>
            <span
              className="text-xs shrink-0"
              style={{ color: "var(--th-text-muted)" }}
            >
              {savingDept ? tr("저장 중...", "Saving...") : null}
            </span>
          </div>
        </div>

        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("메인 Provider", "Main Provider")}
          </div>
          <div className="flex items-center gap-2">
            <select
              value={selectedProvider}
              onChange={(e) => void saveProvider(e.target.value)}
              disabled={savingProvider}
              className="flex-1 px-3 py-2 rounded-lg text-sm outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="claude">Claude</option>
              <option value="codex">Codex</option>
              <option value="gemini">Gemini</option>
              <option value="qwen">Qwen</option>
            </select>
            <span
              className="text-xs shrink-0"
              style={{ color: "var(--th-text-muted)" }}
            >
              {savingProvider ? tr("저장 중...", "Saving...") : null}
            </span>
          </div>
        </div>

        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("소속 오피스", "Offices")}
          </div>
          {loadingOffices ? (
            <div
              className="text-xs py-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("불러오는 중...", "Loading...")}
            </div>
          ) : officeMemberships.length === 0 ? (
            <div
              className="text-xs py-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("등록된 오피스가 없습니다", "No offices")}
            </div>
          ) : (
            <div className="flex flex-wrap gap-2">
              {officeMemberships.map((office) => {
                const assigned = office.assigned;
                const savingOffice = !!savingOfficeIds[office.id];

                return (
                  <button
                    key={office.id}
                    onClick={() => void toggleOfficeMembership(office)}
                    disabled={savingOffice}
                    className="px-2.5 py-1.5 rounded-lg text-xs font-medium transition-all disabled:opacity-50"
                    style={
                      assigned
                        ? { background: office.color, color: "#ffffff" }
                        : {
                            background: "var(--th-bg-surface)",
                            color: "var(--th-text-secondary)",
                            border: "1px solid var(--th-card-border)",
                          }
                    }
                  >
                    <span>
                      {office.icon} {localeName(locale, office)}
                    </span>
                  </button>
                );
              })}
            </div>
          )}
        </div>

        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("상태 요약", "Status Summary")}
          </div>
          <div className="space-y-2">
            <div
              className="rounded-xl px-3 py-2"
              style={{ background: "var(--th-bg-surface)" }}
            >
              <div
                className="text-xs mb-1"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("현재 작업", "Current Work")}
              </div>
              <div
                className="text-xs leading-relaxed"
                style={{ color: "var(--th-text-primary)" }}
              >
                {currentWorkSummary ||
                  tr("현재 작업 설명이 없습니다", "No current work detail")}
              </div>
            </div>
            <div className="flex flex-wrap gap-2">
              {currentWorkElapsedMs != null && (
                <span
                  className="text-xs px-2 py-1 rounded-lg"
                  style={{
                    background: "rgba(59,130,246,0.14)",
                    color: "#93c5fd",
                  }}
                >
                  {tr("경과", "Elapsed")}:{" "}
                  {formatElapsedCompact(currentWorkElapsedMs, isKo)}
                </span>
              )}
              <span
                className="text-xs px-2 py-1 rounded-lg"
                style={{
                  background: "rgba(56,189,248,0.14)",
                  color: "#67e8f9",
                }}
              >
                AgentDesk {workingLinkedSessions.length}/{claudeSessions.length}
              </span>
              <span
                className="text-xs px-2 py-1 rounded-lg"
                style={{
                  background: "rgba(168,85,247,0.14)",
                  color: "#d8b4fe",
                }}
              >
                {tr("DB 경로", "DB routes")}: {dbBindings.length}
              </span>
            </div>
            {currentWorkDetails.length > 0 && (
              <div className="space-y-1">
                {currentWorkDetails.map((line, idx) => (
                  <div
                    key={`${line}:${idx}`}
                    className="text-xs"
                    style={{ color: "var(--th-text-secondary)" }}
                  >
                    • {line}
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>

        {warnings.length > 0 && (
          <div
            className="px-5 py-3"
            style={{ borderBottom: "1px solid var(--th-card-border)" }}
          >
            <div
              className="text-xs font-semibold uppercase tracking-widest mb-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("이상 징후", "Warnings")}
            </div>
            <div className="flex flex-wrap gap-2">
              {warnings.map((warning) => (
                <span
                  key={warning.code}
                  className="text-xs px-2 py-1 rounded-lg"
                  style={{
                    background:
                      warning.severity === "error"
                        ? "rgba(239,68,68,0.14)"
                        : warning.severity === "warning"
                          ? "rgba(245,158,11,0.14)"
                          : "rgba(96,165,250,0.14)",
                    color:
                      warning.severity === "error"
                        ? "#fca5a5"
                        : warning.severity === "warning"
                          ? "#fcd34d"
                          : "#93c5fd",
                  }}
                >
                  {isKo ? warning.ko : warning.en}
                </span>
              ))}
            </div>
          </div>
        )}

        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("정본 연결", "Source of Truth")}
          </div>
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
            {sourceOfTruthRows.map((row) => (
              <div
                key={row.label}
                className="rounded-xl px-3 py-2"
                style={{ background: "var(--th-bg-surface)" }}
              >
                <div
                  className="text-xs"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {row.label}
                </div>
                <div
                  className="mt-1 text-xs font-medium break-all"
                  style={{ color: row.tone }}
                >
                  {row.value}
                </div>
              </div>
            ))}
          </div>
          {roleMapBindings.length > 0 && (
            <div
              className="mt-2 text-xs"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr(
                "RoleMap 경로가 있으면 Discord source-of-truth는 role_map 우선으로 봅니다.",
                "When RoleMap exists, role_map is treated as the Discord source-of-truth.",
              )}
            </div>
          )}
        </div>

        {/* Personality */}
        {agent.personality && (
          <div
            className="px-5 py-3"
            style={{ borderBottom: "1px solid var(--th-card-border)" }}
          >
            <div
              className="text-xs font-semibold uppercase tracking-widest mb-1.5"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("성격", "Personality")}
            </div>
            <div
              className="text-xs leading-relaxed whitespace-pre-wrap"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {agent.personality}
            </div>
          </div>
        )}

        {/* Session info */}
        {agent.session_info && (
          <div
            className="px-5 py-3"
            style={{ borderBottom: "1px solid var(--th-card-border)" }}
          >
            <div
              className="text-xs font-semibold uppercase tracking-widest mb-1.5"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("현재 작업", "Current Session")}
            </div>
            <div
              className="text-xs leading-relaxed"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {agent.session_info}
            </div>
          </div>
        )}

        {/* Discord Bindings */}
        {discordBindings.length > 0 && (
          <div
            className="px-5 py-3"
            style={{ borderBottom: "1px solid var(--th-card-border)" }}
          >
            <div
              className="text-xs font-semibold uppercase tracking-widest mb-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("Discord 라우팅", "Discord Routing")} (
              {discordBindings.length})
            </div>
            <div
              className="text-xs mb-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr(
                "RoleMap/Primary/Alt/Codex는 이 agent에 연결된 Discord 경로의 source다.",
                "RoleMap/Primary/Alt/Codex indicate how this agent is wired to Discord.",
              )}
            </div>
            <div className="space-y-1">
              {discordBindings.map((b) => {
                const source = inferBindingSource(b);
                const sourceLabel = bindingSourceLabel(source);
                const bindingChannel = getDiscordChannel(b.channelId);
                const bindingParent = getDiscordChannel(bindingChannel?.parent_id);
                const bindingSummary = describeDiscordBinding(
                  b,
                  bindingChannel,
                  bindingParent,
                );
                const counterChannel =
                  b.counterModelChannelId && b.counterModelChannelId !== b.channelId
                    ? getDiscordChannel(b.counterModelChannelId)
                    : undefined;
                const counterParent = getDiscordChannel(counterChannel?.parent_id);
                const counterSummary =
                  b.counterModelChannelId && b.counterModelChannelId !== b.channelId
                    ? describeDiscordTarget(
                        b.counterModelChannelId,
                        counterChannel,
                        counterParent,
                      )
                    : null;
                const subtitleParts = [
                  bindingSummary.subtitle,
                  counterSummary
                    ? `counter: ${formatDiscordSummary(counterSummary)}`
                    : null,
                ].filter((value): value is string => Boolean(value));

                return (
                  <div
                    key={`${b.channelId}:${source}`}
                    className="flex items-center gap-2 px-2.5 py-1.5 rounded-lg"
                    style={{ background: "var(--th-bg-surface)" }}
                  >
                    <span className="text-sm">💬</span>
                    <div className="min-w-0 flex-1">
                      <div
                        className="text-xs font-medium truncate"
                        style={{ color: "var(--th-text-primary)" }}
                      >
                        {bindingSummary.title}
                      </div>
                      {subtitleParts.length > 0 && (
                        <div
                          className="text-xs truncate mt-0.5"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {subtitleParts.join(" · ")}
                        </div>
                      )}
                    </div>
                    <div className="flex items-center gap-1 shrink-0">
                      {bindingSummary.webUrl && (
                        <a
                          href={bindingSummary.webUrl}
                          target="_blank"
                          rel="noreferrer"
                          className="text-[11px] px-1.5 py-0.5 rounded hover:brightness-110"
                          style={{
                            background: "rgba(59,130,246,0.15)",
                            color: "#93c5fd",
                          }}
                        >
                          Web
                        </a>
                      )}
                      {bindingSummary.deepLink && (
                        <a
                          href={bindingSummary.deepLink}
                          className="text-[11px] px-1.5 py-0.5 rounded hover:brightness-110"
                          style={{
                            background: "rgba(88,101,242,0.16)",
                            color: "#a5b4fc",
                          }}
                        >
                          App
                        </a>
                      )}
                      <span
                        className="text-xs px-1.5 py-0.5 rounded"
                        style={{
                          background: "rgba(88,101,242,0.15)",
                          color: "#7289da",
                        }}
                      >
                        {sourceLabel}
                      </span>
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}

        {/* Linked AgentDesk Sessions */}
        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("연결된 AgentDesk 세션", "Linked AgentDesk Sessions")}
            {!loadingClaudeSessions && ` (${claudeSessions.length})`}
          </div>
          {loadingClaudeSessions ? (
            <div
              className="text-xs py-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("불러오는 중...", "Loading...")}
            </div>
          ) : claudeSessions.length === 0 ? (
            <div
              className="text-xs py-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("연결된 AgentDesk 세션 없음", "No linked AgentDesk sessions")}
            </div>
          ) : (
            <div className="space-y-1.5">
              {claudeSessions.map((s) => {
                const sessionChannel = getDiscordChannel(s.thread_channel_id);
                const sessionParent = getDiscordChannel(sessionChannel?.parent_id);
                const sessionSummary = describeDispatchedSession(
                  s,
                  sessionChannel,
                  sessionParent,
                );
                const sessionSubtitle = [
                  sessionSummary.subtitle,
                  s.session_info || s.model || "AgentDesk session",
                ]
                  .filter((value): value is string => Boolean(value))
                  .join(" · ");

                return (
                  <div
                    key={s.id}
                    className="flex items-start justify-between gap-2 px-2.5 py-2 rounded-lg"
                    style={{ background: "var(--th-bg-surface)" }}
                  >
                    <div className="min-w-0">
                      <div
                        className="text-xs font-medium truncate"
                        style={{ color: "var(--th-text-primary)" }}
                      >
                        {sessionSummary.title}
                      </div>
                      <div
                        className="text-xs truncate mt-0.5"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {sessionSubtitle}
                      </div>
                    </div>
                    <div className="flex items-center gap-1 shrink-0 flex-wrap justify-end">
                      {sessionSummary.webUrl && (
                        <a
                          href={sessionSummary.webUrl}
                          target="_blank"
                          rel="noreferrer"
                          className="text-[11px] px-1.5 py-0.5 rounded hover:brightness-110"
                          style={{
                            background: "rgba(59,130,246,0.15)",
                            color: "#93c5fd",
                          }}
                        >
                          Web
                        </a>
                      )}
                      {sessionSummary.deepLink && (
                        <a
                          href={sessionSummary.deepLink}
                          className="text-[11px] px-1.5 py-0.5 rounded hover:brightness-110"
                          style={{
                            background: "rgba(88,101,242,0.16)",
                            color: "#a5b4fc",
                          }}
                        >
                          App
                        </a>
                      )}
                      <span
                        className="text-xs px-1.5 py-0.5 rounded"
                        style={{
                          background:
                            s.provider === "codex"
                              ? "rgba(56,189,248,0.18)"
                              : s.provider === "gemini"
                                ? "rgba(250,204,21,0.18)"
                                : s.provider === "qwen"
                                  ? "rgba(34,197,94,0.18)"
                                  : "rgba(167,139,250,0.18)",
                          color:
                            s.provider === "codex"
                              ? "#38bdf8"
                              : s.provider === "gemini"
                                ? "#facc15"
                                : s.provider === "qwen"
                                  ? "#86efac"
                                  : "#c4b5fd",
                        }}
                      >
                        {s.provider === "codex"
                          ? "Codex"
                          : s.provider === "gemini"
                            ? "Gemini"
                            : s.provider === "qwen"
                              ? "Qwen"
                              : "Claude"}
                      </span>
                      <span
                        className="text-xs px-1.5 py-0.5 rounded"
                        style={{
                          background:
                            s.status === "working"
                              ? "rgba(16,185,129,0.15)"
                              : "rgba(100,116,139,0.15)",
                          color: s.status === "working" ? "#34d399" : "#94a3b8",
                        }}
                      >
                        {s.status === "working"
                          ? tr("작업중", "Working")
                          : tr("대기", "Idle")}
                      </span>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {/* Cron Jobs */}
        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("크론 작업", "Cron Jobs")}{" "}
            {!loadingCron && `(${cronJobs.length})`}
          </div>
          {loadingCron ? (
            <div
              className="text-xs py-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("불러오는 중...", "Loading...")}
            </div>
          ) : cronJobs.length === 0 ? (
            <div
              className="text-xs py-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("등록된 크론 작업이 없습니다", "No cron jobs")}
            </div>
          ) : (
            <div className="space-y-1.5">
              {cronJobs.map((job) => (
                <div
                  key={job.id}
                  className="flex items-start gap-2 px-2.5 py-2 rounded-lg"
                  style={{ background: "var(--th-bg-surface)" }}
                >
                  <span
                    className={`mt-0.5 w-1.5 h-1.5 rounded-full shrink-0 ${
                      job.enabled
                        ? job.state?.lastStatus === "ok"
                          ? "bg-emerald-400"
                          : "bg-amber-400"
                        : "bg-slate-500"
                    }`}
                  />
                  <div className="flex-1 min-w-0">
                    <div
                      className="text-xs font-medium truncate"
                      style={{ color: "var(--th-text-primary)" }}
                      title={job.name}
                    >
                      {job.name}
                    </div>
                    <div className="flex items-center gap-2 mt-0.5 flex-wrap">
                      <span
                        className="text-xs font-mono"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {formatSchedule(job.schedule, isKo)}
                      </span>
                      {job.state?.lastRunAtMs && (
                        <span
                          className="text-xs"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {tr("최근:", "Last:")}{" "}
                          {timeAgo(job.state.lastRunAtMs, isKo)}
                          {job.state.lastDurationMs != null &&
                            ` (${formatDuration(job.state.lastDurationMs)})`}
                        </span>
                      )}
                    </div>
                  </div>
                  {!job.enabled && (
                    <span
                      className="text-xs px-1.5 py-0.5 rounded shrink-0"
                      style={{
                        background: "rgba(100,116,139,0.2)",
                        color: "#94a3b8",
                      }}
                    >
                      {tr("비활성", "Off")}
                    </span>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>

        <div
          className="px-5 py-3"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
        >
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("최근 변경", "Recent Changes")}
          </div>
          {loadingAudit ? (
            <div
              className="text-xs py-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("불러오는 중...", "Loading...")}
            </div>
          ) : auditLogs.length === 0 ? (
            <div
              className="text-xs py-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("관련 변경 로그가 없습니다", "No related audit logs")}
            </div>
          ) : (
            <div className="space-y-1.5">
              {auditLogs.map((log) => (
                <div
                  key={log.id}
                  className="rounded-xl px-3 py-2"
                  style={{ background: "var(--th-bg-surface)" }}
                >
                  <div
                    className="text-xs"
                    style={{ color: "var(--th-text-primary)" }}
                  >
                    {log.summary}
                  </div>
                  <div
                    className="mt-1 text-xs"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {log.action} • {timeAgo(log.created_at, isKo)}
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* Skills */}
        <div className="px-5 py-3">
          <div
            className="text-xs font-semibold uppercase tracking-widest mb-2"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("스킬", "Skills")}
          </div>
          {loadingSkills ? (
            <div
              className="text-xs py-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("불러오는 중...", "Loading...")}
            </div>
          ) : agentSkills.length === 0 && sharedSkills.length === 0 ? (
            <div
              className="text-xs py-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("등록된 스킬이 없습니다", "No skills")}
            </div>
          ) : (
            <div className="space-y-2">
              {agentSkills.length > 0 && (
                <div>
                  <div
                    className="text-xs mb-1 font-medium"
                    style={{ color: "var(--th-text-secondary)" }}
                  >
                    {tr("전용 스킬", "Agent-specific")}
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {agentSkills.map((skill) => (
                      <span
                        key={skill.name}
                        className="text-xs px-2 py-0.5 rounded-full"
                        style={{
                          background: "rgba(99,102,241,0.15)",
                          color: "#a5b4fc",
                        }}
                        title={skill.description}
                      >
                        {skill.name}
                      </span>
                    ))}
                  </div>
                </div>
              )}
              {sharedSkills.length > 0 && (
                <div>
                  <button
                    onClick={() => setShowSharedSkills(!showSharedSkills)}
                    className="text-xs font-medium flex items-center gap-1 hover:underline"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {tr("공유 스킬", "Shared")} ({sharedSkills.length})
                    <span className="text-xs">
                      {showSharedSkills ? "▲" : "▼"}
                    </span>
                  </button>
                  {showSharedSkills && (
                    <div className="flex flex-wrap gap-1 mt-1">
                      {sharedSkills.map((skill) => (
                        <span
                          key={skill.name}
                          className="text-xs px-2 py-0.5 rounded-full"
                          style={{
                            background: "var(--th-bg-surface)",
                            color: "var(--th-text-muted)",
                          }}
                          title={skill.description}
                        >
                          {skill.name}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
              )}
            </div>
          )}
        </div>

        {/* Footer with Level */}
        <div
          className="px-5 py-3 space-y-2"
          style={{ borderTop: "1px solid var(--th-card-border)" }}
        >
          {/* Level progress bar */}
          {(() => {
            const lv = getAgentLevel(agent.stats_xp);
            const title = getAgentTitle(agent.stats_xp, isKo);
            return (
              <div className="flex items-center gap-2">
                <span
                  className="text-xs font-bold px-2 py-0.5 rounded-full shrink-0"
                  style={{
                    background: "rgba(99,102,241,0.18)",
                    color: "#a5b4fc",
                  }}
                >
                  Lv.{lv.level} {title}
                </span>
                <div
                  className="flex-1 h-1.5 rounded-full overflow-hidden"
                  style={{ background: "var(--th-bg-surface)" }}
                >
                  <div
                    className="h-full rounded-full transition-all"
                    style={{
                      width: `${Math.round(lv.progress * 100)}%`,
                      background: "linear-gradient(90deg, #6366f1, #a78bfa)",
                    }}
                  />
                </div>
                <span
                  className="text-xs shrink-0"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {agent.stats_xp} /{" "}
                  {lv.nextThreshold === Infinity ? "MAX" : lv.nextThreshold} XP
                </span>
              </div>
            );
          })()}
          {/* Activity Timeline */}
          <div
            className="rounded-2xl border overflow-hidden"
            style={{
              borderColor: "var(--th-border-subtle)",
              background: "var(--th-bg-card)",
            }}
          >
            <button
              onClick={() => setTimelineOpen((v) => !v)}
              className="w-full flex items-center justify-between px-4 py-3 text-xs font-semibold"
              style={{ color: "var(--th-text-heading)" }}
            >
              <span>{tr("활동 타임라인", "Activity Timeline")}</span>
              <span style={{ color: "var(--th-text-muted)" }}>
                {timelineOpen ? "▲" : "▼"}
              </span>
            </button>
            {timelineOpen && (
              <div className="px-4 pb-3 space-y-1.5 max-h-64 overflow-y-auto">
                {loadingTimeline ? (
                  <div
                    className="text-xs py-2"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    …
                  </div>
                ) : timeline.length === 0 ? (
                  <div
                    className="text-xs py-2"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {tr("활동 없음", "No activity")}
                  </div>
                ) : (
                  timeline.map((evt) => {
                    const sourceColor =
                      evt.source === "dispatch"
                        ? "#a78bfa"
                        : evt.source === "session"
                          ? "#38bdf8"
                          : "#4ade80";
                    const sourceLabel =
                      evt.source === "dispatch"
                        ? "D"
                        : evt.source === "session"
                          ? "S"
                          : "K";
                    const durationStr =
                      evt.duration_ms != null
                        ? evt.duration_ms < 60_000
                          ? `${Math.round(evt.duration_ms / 1000)}s`
                          : `${Math.round(evt.duration_ms / 60_000)}m`
                        : null;
                    return (
                      <div
                        key={`${evt.source}-${evt.id}`}
                        className="flex items-start gap-2 text-xs"
                      >
                        <span
                          className="shrink-0 w-4 h-4 rounded-full flex items-center justify-center text-xs font-bold mt-0.5"
                          style={{
                            backgroundColor: `${sourceColor}22`,
                            color: sourceColor,
                          }}
                        >
                          {sourceLabel}
                        </span>
                        <div className="min-w-0 flex-1">
                          <div
                            className="truncate"
                            style={{ color: "var(--th-text-primary)" }}
                          >
                            {evt.title}
                          </div>
                          <div
                            className="flex gap-2"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            <span>{timeAgo(evt.timestamp, isKo)}</span>
                            <span
                              className="px-1 rounded"
                              style={{
                                backgroundColor: `${sourceColor}15`,
                                color: sourceColor,
                              }}
                            >
                              {evt.status}
                            </span>
                            {durationStr && <span>{durationStr}</span>}
                            {evt.detail && "issue" in evt.detail && (
                              <span>#{String(evt.detail.issue)}</span>
                            )}
                          </div>
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            )}
          </div>

          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              {agent.role_id && (
                <span
                  className="text-xs font-mono px-1.5 py-0.5 rounded"
                  style={{
                    background: "var(--th-bg-surface)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {agent.role_id}
                </span>
              )}
              <span
                className="text-xs"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("완료", "Done")} {agent.stats_tasks_done}
              </span>
            </div>
            <button
              onClick={onClose}
              className="px-3 py-1.5 rounded-lg text-xs font-medium transition-all hover:bg-[var(--th-bg-surface-hover)]"
              style={{
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-secondary)",
              }}
            >
              {tr("닫기", "Close")}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
