import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  formatElapsedCompact,
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
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import { STATUS_DOT } from "./constants";
import TurnTranscriptPanel from "./TurnTranscriptPanel";
import type { Translator } from "./types";
import * as api from "../../api";
import type {
  CronJob,
  AgentSkill,
  DiscordBinding,
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
  agentdesk: "#10b981",
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

function compactToken(value: string, head = 8, tail = 4): string {
  if (value.length <= head + tail + 3) return value;
  return `${value.slice(0, head)}...${value.slice(-tail)}`;
}

function DiscordSummaryLabel({
  summary,
}: {
  summary: {
    title: string;
    subtitle: string | null;
    webUrl: string | null;
    deepLink: string | null;
  };
}) {
  const href = summary.deepLink ?? summary.webUrl;
  const label = formatDiscordSummary(summary);

  if (!href) {
    return (
      <span
        className="block min-w-0 flex-1 truncate text-xs font-medium"
        style={{ color: "var(--th-text-primary)" }}
        title={label}
      >
        {label}
      </span>
    );
  }

  return (
    <a
      href={href}
      className="block min-w-0 flex-1 truncate text-xs font-medium hover:underline"
      style={{ color: "var(--th-text-primary)" }}
      title={summary.deepLink ?? summary.webUrl ?? label}
    >
      {label}
    </a>
  );
}

function DiscordDeepLinkChip({
  deepLink,
  label,
}: {
  deepLink: string | null;
  label: string;
}) {
  if (!deepLink) return null;
  return (
    <a
      href={deepLink}
      className="shrink-0 rounded px-1.5 py-0.5 text-xs"
      style={{ background: "rgba(88,101,242,0.15)", color: "#7289da" }}
      title={deepLink}
    >
      {label}
    </a>
  );
}

interface DetailAccordionProps {
  title: string;
  subtitle?: string | null;
  badge?: string | null;
  open: boolean;
  onToggle: () => void;
  children: ReactNode;
}

function DetailAccordion({
  title,
  subtitle,
  badge,
  open,
  onToggle,
  children,
}: DetailAccordionProps) {
  return (
    <div
      className="px-5 py-3"
      style={{ borderBottom: "1px solid var(--th-card-border)" }}
    >
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-start justify-between gap-3 text-left"
        aria-expanded={open}
      >
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <div
              className="text-xs font-semibold uppercase tracking-widest"
              style={{ color: "var(--th-text-muted)" }}
            >
              {title}
            </div>
            {badge && (
              <span
                className="rounded-full px-2 py-0.5 text-[11px] font-medium"
                style={{
                  background: "rgba(96,165,250,0.12)",
                  color: "#93c5fd",
                }}
              >
                {badge}
              </span>
            )}
          </div>
          {subtitle && (
            <div
              className="mt-1 text-xs leading-relaxed"
              style={{ color: "var(--th-text-muted)" }}
            >
              {subtitle}
            </div>
          )}
        </div>
        <span
          className="rounded-full px-2 py-1 text-xs font-medium"
          style={{
            background: "rgba(148,163,184,0.12)",
            color: "var(--th-text-muted)",
          }}
        >
          {open ? "▲" : "▼"}
        </span>
      </button>
      {open && <div className="mt-3">{children}</div>}
    </div>
  );
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
  const [transcriptOpen, setTranscriptOpen] = useState(true);
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
    setTranscriptOpen(true);
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
  const dbBindings = discordBindings.filter((binding) => inferBindingSource(binding) !== "role-map");
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
  const routingChips = [
    {
      label: "DB",
      value: compactToken(agent.id, 6, 4),
      fullValue: agent.id,
      fullLabel: tr("DB 레코드", "DB Record"),
      tone: "#60a5fa",
    },
    {
      label: "Role",
      value: agent.role_id
        ? compactToken(agent.role_id, 8, 4)
        : tr("없음", "None"),
      fullValue: agent.role_id || tr("없음", "None"),
      fullLabel: tr("Role ID", "Role ID"),
      tone: agent.role_id ? "#34d399" : "#94a3b8",
    },
    {
      label: "Launchd",
      value: cronJobs.length > 0 ? `${cronJobs.length}` : tr("없음", "None"),
      fullValue:
        cronJobs.length > 0 ? `${cronJobs.length} job` : tr("없음", "None"),
      fullLabel: tr("Launchd 귀속", "Launchd Ownership"),
      tone: cronJobs.length > 0 ? "#34d399" : "#94a3b8",
    },
    {
      label: "RoleMap",
      value:
        roleMapBindings.length > 0
          ? `${roleMapBindings.length}`
          : tr("없음", "None"),
      fullValue:
        roleMapBindings.length > 0
          ? `${roleMapBindings.length} route`
          : tr("없음", "None"),
      fullLabel: tr("RoleMap 경로", "RoleMap Route"),
      tone: roleMapBindings.length > 0 ? "#fbbf24" : "#94a3b8",
    },
    {
      label: "Discord",
      value: `${discordBindings.length}`,
      tone: discordBindings.length > 0 ? "#10b981" : "#94a3b8",
    },
    {
      label: "ADK",
      value: `${workingLinkedSessions.length}/${claudeSessions.length}`,
      fullValue: `${workingLinkedSessions.length}/${claudeSessions.length}`,
      fullLabel: tr("AgentDesk 링크", "AgentDesk Links"),
      tone: workingLinkedSessions.length > 0 ? "#38bdf8" : "#94a3b8",
    },
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
            <SurfaceSubsection title={tr("소속 부서", "Department")} className="min-w-0">
              <div className="min-w-0 flex flex-col items-stretch gap-2 sm:flex-row sm:items-center">
                <select
                  value={selectedDeptId}
                  onChange={(e) => void saveDepartment(e.target.value)}
                  disabled={savingDept}
                  className="min-w-0 w-full rounded-xl border px-3 py-2 text-sm outline-none sm:flex-1"
                  style={{
                    background: "var(--th-input-bg)",
                    borderColor: "var(--th-input-border)",
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
                <span className="self-start text-xs sm:shrink-0" style={{ color: "var(--th-text-muted)" }}>
                  {savingDept ? tr("저장 중...", "Saving...") : null}
                </span>
              </div>
            </SurfaceSubsection>

            <SurfaceSubsection title={tr("메인 Provider", "Main Provider")} className="min-w-0">
              <div className="min-w-0 flex flex-col items-stretch gap-2 sm:flex-row sm:items-center">
                <select
                  value={selectedProvider}
                  onChange={(e) => void saveProvider(e.target.value)}
                  disabled={savingProvider}
                  className="min-w-0 w-full rounded-xl border px-3 py-2 text-sm outline-none sm:flex-1"
                  style={{
                    background: "var(--th-input-bg)",
                    borderColor: "var(--th-input-border)",
                    color: "var(--th-text-primary)",
                  }}
                >
                  <option value="claude">Claude</option>
                  <option value="codex">Codex</option>
                  <option value="gemini">Gemini</option>
                  <option value="qwen">Qwen</option>
                </select>
                <span className="self-start text-xs sm:shrink-0" style={{ color: "var(--th-text-muted)" }}>
                  {savingProvider ? tr("저장 중...", "Saving...") : null}
                </span>
              </div>
            </SurfaceSubsection>

            <SurfaceSubsection title={tr("소속 오피스", "Offices")} className="min-w-0 md:col-span-2">
              {loadingOffices ? (
                <SurfaceNotice tone="neutral" compact>
                  {tr("불러오는 중...", "Loading...")}
                </SurfaceNotice>
              ) : officeMemberships.length === 0 ? (
                <SurfaceEmptyState className="text-xs">
                  {tr("등록된 오피스가 없습니다", "No offices")}
                </SurfaceEmptyState>
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
                        className="rounded-xl px-2.5 py-1.5 text-xs font-medium transition-all disabled:opacity-50"
                        style={assigned
                          ? { background: office.color, color: "#ffffff" }
                          : {
                            background: "var(--th-bg-surface)",
                            color: "var(--th-text-secondary)",
                            border: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
                          }}
                      >
                        {office.icon} {localeName(locale, office)}
                      </button>
                    );
                  })}
                </div>
              )}
            </SurfaceSubsection>

            <SurfaceSubsection title={tr("상태 요약", "Status Summary")} className="min-w-0 md:col-span-2">
              <div className="space-y-3">
                <SurfaceCard className="p-3">
                  <div className="mb-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr("현재 작업", "Current Work")}
                  </div>
                  <div className="text-xs leading-relaxed" style={{ color: "var(--th-text-primary)" }}>
                    {currentWorkSummary || tr("현재 작업 설명이 없습니다", "No current work detail")}
                  </div>
                </SurfaceCard>
                <div className="flex flex-wrap gap-2">
                  {currentWorkElapsedMs != null && (
                    <SurfaceMetricPill
                      label={tr("경과", "Elapsed")}
                      tone="info"
                      value={formatElapsedCompact(currentWorkElapsedMs, isKo)}
                    />
                  )}
                  <SurfaceMetricPill
                    label={tr("DB 경로", "DB routes")}
                    tone="accent"
                    value={`${dbBindings.length}`}
                  />
                </div>
                {currentWorkDetails.length > 0 && (
                  <div className="space-y-1">
                    {currentWorkDetails.map((line, idx) => (
                      <div key={`${line}:${idx}`} className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                        • {line}
                      </div>
                    ))}
                  </div>
                )}
              </div>
            </SurfaceSubsection>

            {warnings.length > 0 && (
              <SurfaceSubsection title={tr("이상 징후", "Warnings")} className="min-w-0 md:col-span-2">
                <div className="flex flex-wrap gap-2">
                  {warnings.map((warning) => (
                    <span
                      key={warning.code}
                      className="rounded-lg px-2 py-1 text-xs"
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
              </SurfaceSubsection>
            )}

            <SurfaceSubsection title={tr("정본 연결", "Source of Truth")} className="md:col-span-2">
              <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                {sourceOfTruthRows.map((row) => (
                  <SurfaceCard key={row.label} className="p-3" style={{ background: "var(--th-bg-surface)" }}>
                    <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {row.label}
                    </div>
                    <div className="mt-1 break-all text-xs font-medium" style={{ color: row.tone }}>
                      {row.value}
                    </div>
                  </SurfaceCard>
                ))}
              </div>
              {roleMapBindings.length > 0 && (
                <SurfaceNotice tone="warn" compact className="mt-3">
                  {tr("RoleMap 경로가 있으면 Discord source-of-truth는 role_map 우선으로 봅니다.", "When RoleMap exists, role_map is treated as the Discord source-of-truth.")}
                </SurfaceNotice>
              )}
            </SurfaceSubsection>

            {agent.personality && (
              <SurfaceSubsection title={tr("성격", "Personality")} className="md:col-span-2">
                <div className="whitespace-pre-wrap text-xs leading-relaxed" style={{ color: "var(--th-text-secondary)" }}>
                  {agent.personality}
                </div>
              </SurfaceSubsection>
            )}

            {agent.session_info && (
              <SurfaceSubsection title={tr("현재 작업", "Current Session")} className="md:col-span-2">
                <div className="text-xs leading-relaxed" style={{ color: "var(--th-text-secondary)" }}>
                  {agent.session_info}
                </div>
              </SurfaceSubsection>
            )}

            {discordBindings.length > 0 && (
              <SurfaceSubsection
                title={`${tr("Discord 라우팅", "Discord Routing")} (${discordBindings.length})`}
                description={tr("RoleMap/Primary/Alt/Codex는 이 agent에 연결된 Discord 경로의 source다.", "RoleMap/Primary/Alt/Codex indicate how this agent is wired to Discord.")}
                className="md:col-span-2"
              >
                <div className="space-y-1">
                  {discordBindings.map((b) => {
                    const source = inferBindingSource(b);
                    const sourceLabel = bindingSourceLabel(source);
                    const channelInfo = resolveDiscordChannelInfo(b.channelId);
                    const channelSummary = describeDiscordBinding(
                      b,
                      channelInfo,
                      resolveDiscordParentInfo(channelInfo),
                    );
                    const counterChannelInfo = resolveDiscordChannelInfo(
                      b.counterModelChannelId ?? null,
                    );
                    const counterSummary =
                      b.counterModelChannelId && b.counterModelChannelId !== b.channelId
                        ? describeDiscordTarget(
                            b.counterModelChannelId,
                            counterChannelInfo,
                            resolveDiscordParentInfo(counterChannelInfo),
                          )
                        : null;

                    return (
                      <SurfaceCard key={`${b.channelId}:${source}`} className="flex items-center gap-2 px-2.5 py-1.5" style={{ background: "var(--th-bg-surface)" }}>
                        <span className="text-sm">💬</span>
                        <div className="min-w-0 flex-1">
                          <div className="flex min-w-0 items-center gap-2">
                            <DiscordSummaryLabel summary={channelSummary} />
                            <DiscordDeepLinkChip
                              deepLink={channelSummary.deepLink}
                              label={tr("앱", "App")}
                            />
                          </div>
                          {counterSummary && (
                            <div className="mt-0.5 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                              {`counter: ${formatDiscordSummary(counterSummary)}`}
                            </div>
                          )}
                        </div>
                        <span className="rounded px-1.5 py-0.5 text-xs" style={{ background: "rgba(88,101,242,0.15)", color: "#7289da" }}>
                          {sourceLabel}
                        </span>
                      </SurfaceCard>
                    );
                  })}
                </div>
              </SurfaceSubsection>
            )}

            <SurfaceSubsection title={`${tr("연결된 AgentDesk 세션", "Linked AgentDesk Sessions")}${!loadingClaudeSessions ? ` (${claudeSessions.length})` : ""}`} className="md:col-span-2">
              {loadingClaudeSessions ? (
                <SurfaceNotice tone="neutral" compact>{tr("불러오는 중...", "Loading...")}</SurfaceNotice>
              ) : claudeSessions.length === 0 ? (
                <SurfaceEmptyState className="text-xs">
                  {tr("연결된 AgentDesk 세션 없음", "No linked AgentDesk sessions")}
                </SurfaceEmptyState>
              ) : (
                <div className="space-y-1.5">
                  {claudeSessions.map((s) => {
                    const sessionChannelInfo = resolveDiscordChannelInfo(
                      s.thread_channel_id ?? null,
                    );
                    const sessionSummary = describeDispatchedSession(
                      s,
                      sessionChannelInfo,
                      resolveDiscordParentInfo(sessionChannelInfo),
                    );

                    return (
                      <SurfaceCard key={s.id} className="flex items-start justify-between gap-2 px-2.5 py-2" style={{ background: "var(--th-bg-surface)" }}>
                        <div className="min-w-0">
                          <div className="flex min-w-0 items-center gap-2">
                            <DiscordSummaryLabel summary={sessionSummary} />
                            <DiscordDeepLinkChip
                              deepLink={sessionSummary.deepLink}
                              label={tr("앱", "App")}
                            />
                          </div>
                          <div className="mt-0.5 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                            {s.session_info || s.model || "AgentDesk session"}
                          </div>
                        </div>
                        <div className="flex shrink-0 items-center gap-1">
                          <span
                            className="rounded px-1.5 py-0.5 text-xs"
                            style={{
                              background:
                                s.provider === "codex"
                                  ? "rgba(56,189,248,0.18)"
                                  : s.provider === "gemini"
                                    ? "rgba(250,204,21,0.18)"
                                    : s.provider === "qwen"
                                      ? "rgba(34,197,94,0.18)"
                                      : "color-mix(in srgb, var(--th-accent-primary-soft) 80%, transparent)",
                              color:
                                s.provider === "codex"
                                  ? "#38bdf8"
                                  : s.provider === "gemini"
                                    ? "#facc15"
                                    : s.provider === "qwen"
                                      ? "#86efac"
                                      : "var(--th-accent-primary)",
                            }}
                          >
                            {s.provider === "codex" ? "Codex" : s.provider === "gemini" ? "Gemini" : s.provider === "qwen" ? "Qwen" : "Claude"}
                          </span>
                          <span
                            className="rounded px-1.5 py-0.5 text-xs"
                            style={{
                              background: s.status === "working" ? "rgba(16,185,129,0.15)" : "rgba(100,116,139,0.15)",
                              color: s.status === "working" ? "#34d399" : "#94a3b8",
                            }}
                          >
                            {s.status === "working" ? tr("작업중", "Working") : tr("대기", "Idle")}
                          </span>
                        </div>
                      </SurfaceCard>
                    );
                  })}
                </div>
              )}
            </SurfaceSubsection>

            <SurfaceSubsection title={`${tr("크론 작업", "Cron Jobs")} ${!loadingCron ? `(${cronJobs.length})` : ""}`} className="md:col-span-2">
              {loadingCron ? (
                <SurfaceNotice tone="neutral" compact>{tr("불러오는 중...", "Loading...")}</SurfaceNotice>
              ) : cronJobs.length === 0 ? (
                <SurfaceEmptyState className="text-xs">
                  {tr("등록된 크론 작업이 없습니다", "No cron jobs")}
                </SurfaceEmptyState>
              ) : (
                <div className="space-y-1.5">
                  {cronJobs.map((job) => (
                    <SurfaceCard key={job.id} className="flex items-start gap-2 px-2.5 py-2" style={{ background: "var(--th-bg-surface)" }}>
                      <span className={`mt-0.5 h-1.5 w-1.5 shrink-0 rounded-full ${
                        job.enabled
                          ? job.state?.lastStatus === "ok" ? "bg-emerald-400" : "bg-amber-400"
                          : "bg-slate-500"
                      }`} />
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-xs font-medium" style={{ color: "var(--th-text-primary)" }} title={job.name}>
                          {job.name}
                        </div>
                        <div className="mt-0.5 flex flex-wrap items-center gap-2">
                          <span className="text-xs font-mono" style={{ color: "var(--th-text-muted)" }}>
                            {formatSchedule(job.schedule, isKo)}
                          </span>
                          {job.state?.lastRunAtMs && (
                            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                              {tr("최근:", "Last:")} {timeAgo(job.state.lastRunAtMs, isKo)}
                              {job.state.lastDurationMs != null && ` (${formatDuration(job.state.lastDurationMs)})`}
                            </span>
                          )}
                        </div>
                      </div>
                      {!job.enabled && (
                        <span className="shrink-0 rounded px-1.5 py-0.5 text-xs" style={{ background: "rgba(100,116,139,0.2)", color: "#94a3b8" }}>
                          {tr("비활성", "Off")}
                        </span>
                      )}
                    </SurfaceCard>
                  ))}
                </div>
              )}
            </SurfaceSubsection>

            <SurfaceSubsection title={tr("최근 변경", "Recent Changes")} className="md:col-span-2">
              {loadingAudit ? (
                <SurfaceNotice tone="neutral" compact>{tr("불러오는 중...", "Loading...")}</SurfaceNotice>
              ) : auditLogs.length === 0 ? (
                <SurfaceEmptyState className="text-xs">
                  {tr("관련 변경 로그가 없습니다", "No related audit logs")}
                </SurfaceEmptyState>
              ) : (
                <div className="space-y-1.5">
                  {auditLogs.map((log) => (
                    <SurfaceCard key={log.id} className="px-3 py-2" style={{ background: "var(--th-bg-surface)" }}>
                      <div className="text-xs" style={{ color: "var(--th-text-primary)" }}>
                        {log.summary}
                      </div>
                      <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {log.action} • {timeAgo(log.created_at ?? log.ts, isKo)}
                      </div>
                    </SurfaceCard>
                  ))}
                </div>
              )}
            </SurfaceSubsection>

            <SurfaceSubsection title={tr("스킬", "Skills")} className="md:col-span-2">
              {loadingSkills ? (
                <SurfaceNotice tone="neutral" compact>{tr("불러오는 중...", "Loading...")}</SurfaceNotice>
              ) : agentSkills.length === 0 && sharedSkills.length === 0 ? (
                <SurfaceEmptyState className="text-xs">
                  {tr("등록된 스킬이 없습니다", "No skills")}
                </SurfaceEmptyState>
              ) : (
                <div className="space-y-2">
                  {agentSkills.length > 0 && (
                    <div>
                      <div className="mb-1 text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                        {tr("전용 스킬", "Agent-specific")}
                      </div>
                      <div className="flex flex-wrap gap-1">
                        {agentSkills.map((skill) => (
                          <span
                            key={skill.name}
                            className="rounded-full px-2 py-0.5 text-xs"
                            style={{
                              background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                              color: "var(--th-accent-primary)",
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
                      <SurfaceActionButton onClick={() => setShowSharedSkills(!showSharedSkills)} tone="neutral" compact>
                        {tr("공유 스킬", "Shared")} ({sharedSkills.length}) {showSharedSkills ? "▲" : "▼"}
                      </SurfaceActionButton>
                      {showSharedSkills && (
                        <div className="mt-1 flex flex-wrap gap-1">
                          {sharedSkills.map((skill) => (
                            <span
                              key={skill.name}
                              className="rounded-full px-2 py-0.5 text-xs"
                              style={{ background: "var(--th-bg-surface)", color: "var(--th-text-muted)" }}
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
            </SurfaceSubsection>

            <SurfaceSubsection title={tr("활동 레벨", "Activity Level")} className="md:col-span-2">
              <div className="space-y-3">
                <div className="flex items-center gap-2">
                  <span
                    className="shrink-0 rounded-full px-2 py-0.5 text-xs font-bold"
                    style={{
                      background: "color-mix(in srgb, var(--th-accent-primary-soft) 80%, transparent)",
                      color: "var(--th-accent-primary)",
                    }}
                  >
                    Lv.{levelInfo.level} {levelTitle}
                  </span>
                  <div className="h-1.5 flex-1 overflow-hidden rounded-full" style={{ background: "var(--th-bg-surface)" }}>
                    <div
                      className="h-full rounded-full transition-all"
                      style={{
                        width: `${Math.round(levelInfo.progress * 100)}%`,
                        background: "linear-gradient(90deg, var(--th-accent-primary), var(--th-accent-info))",
                      }}
                    />
                  </div>
                  <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {agent.stats_xp} / {levelInfo.nextThreshold === Infinity ? "MAX" : levelInfo.nextThreshold} XP
                  </span>
                </div>

                <SurfaceCard className="overflow-hidden px-0 py-0" style={{ background: "var(--th-bg-card)" }}>
                  <button
                    onClick={() => setTimelineOpen((v) => !v)}
                    className="flex w-full items-center justify-between px-4 py-3 text-xs font-semibold"
                    style={{ color: "var(--th-text-heading)" }}
                  >
                    <span>{tr("활동 타임라인", "Activity Timeline")}</span>
                    <span style={{ color: "var(--th-text-muted)" }}>{timelineOpen ? "▲" : "▼"}</span>
                  </button>
                  {timelineOpen && (
                    <div className="space-y-1.5 px-4 pb-3">
                      {loadingTimeline ? (
                        <SurfaceNotice tone="neutral" compact>…</SurfaceNotice>
                      ) : timeline.length === 0 ? (
                        <SurfaceEmptyState className="py-2 text-xs">{tr("활동 없음", "No activity")}</SurfaceEmptyState>
                      ) : (
                        <div className="max-h-64 space-y-1.5 overflow-y-auto">
                          {timeline.map((evt) => {
                            const sourceColor = evt.source === "dispatch" ? "#10b981" : evt.source === "session" ? "#38bdf8" : "#84cc16";
                            const sourceLabel = evt.source === "dispatch" ? "D" : evt.source === "session" ? "S" : "K";
                            const durationStr = evt.duration_ms != null
                              ? evt.duration_ms < 60_000
                                ? `${Math.round(evt.duration_ms / 1000)}s`
                                : `${Math.round(evt.duration_ms / 60_000)}m`
                              : null;
                            return (
                              <div key={`${evt.source}-${evt.id}`} className="flex items-start gap-2 text-xs">
                                <span
                                  className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full text-xs font-bold"
                                  style={{ backgroundColor: `${sourceColor}22`, color: sourceColor }}
                                >
                                  {sourceLabel}
                                </span>
                                <div className="min-w-0 flex-1">
                                  <div className="truncate" style={{ color: "var(--th-text-primary)" }}>{evt.title}</div>
                                  <div className="flex flex-wrap gap-2" style={{ color: "var(--th-text-muted)" }}>
                                    <span>{timeAgo(evt.timestamp, isKo)}</span>
                                    <span className="rounded px-1" style={{ backgroundColor: `${sourceColor}15`, color: sourceColor }}>
                                      {evt.status}
                                    </span>
                                    {durationStr && <span>{durationStr}</span>}
                                    {evt.detail && "issue" in evt.detail && <span>#{String(evt.detail.issue)}</span>}
                                  </div>
                                </div>
                              </div>
                            );
                          })}
                        </div>
                      )}
                    </div>
                  )}
                </SurfaceCard>

                <div className="flex flex-wrap items-center gap-2">
                  {agent.role_id && (
                    <span className="rounded px-1.5 py-0.5 font-mono text-xs" style={{ background: "var(--th-bg-surface)", color: "var(--th-text-muted)" }}>
                      {agent.role_id}
                    </span>
                  )}
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr("완료", "Done")} {agent.stats_tasks_done}
                  </span>
                </div>
              </div>
            </SurfaceSubsection>
          </div>
        </div>
      </div>
    </div>
  );
}
