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

function compactToken(value: string, head = 8, tail = 4): string {
  if (value.length <= head + tail + 3) return value;
  return `${value.slice(0, head)}...${value.slice(-tail)}`;
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
      fullValue: `${discordBindings.length}`,
      fullLabel: tr("Discord 경로", "Discord Routes"),
      tone: discordBindings.length > 0 ? "#a78bfa" : "#94a3b8",
    },
    {
      label: "ADK",
      value: `${workingLinkedSessions.length}/${claudeSessions.length}`,
      fullValue: `${workingLinkedSessions.length}/${claudeSessions.length}`,
      fullLabel: tr("AgentDesk 링크", "AgentDesk Links"),
      tone: workingLinkedSessions.length > 0 ? "#38bdf8" : "#94a3b8",
    },
  ];
  const renderLinkedSessionCard = (
    session: DispatchedSession,
    emphasized: boolean,
  ) => {
    const providerTone =
      session.provider === "codex"
        ? { bg: "rgba(56,189,248,0.18)", text: "#38bdf8", label: "Codex" }
        : session.provider === "gemini"
          ? { bg: "rgba(250,204,21,0.18)", text: "#facc15", label: "Gemini" }
          : session.provider === "qwen"
            ? { bg: "rgba(34,197,94,0.18)", text: "#86efac", label: "Qwen" }
            : { bg: "rgba(167,139,250,0.18)", text: "#c4b5fd", label: "Claude" };

    return (
      <div
        key={session.id}
        className="flex items-start justify-between gap-2 rounded-xl border px-3 py-2"
        style={{
          borderColor: emphasized
            ? "rgba(16,185,129,0.26)"
            : "rgba(148,163,184,0.12)",
          background: emphasized
            ? "rgba(16,185,129,0.08)"
            : "var(--th-bg-surface)",
        }}
      >
        <div className="min-w-0">
          <div
            className="text-xs font-medium truncate"
            style={{ color: "var(--th-text-primary)" }}
          >
            {session.name || session.session_key}
          </div>
          <div
            className="mt-0.5 text-xs truncate"
            style={{ color: "var(--th-text-muted)" }}
          >
            {session.session_info || session.model || "AgentDesk session"}
          </div>
        </div>
        <div className="flex shrink-0 flex-wrap items-center justify-end gap-1">
          <span
            className="whitespace-nowrap rounded px-1.5 py-0.5 text-xs"
            style={{
              background: providerTone.bg,
              color: providerTone.text,
            }}
          >
            {providerTone.label}
          </span>
          <span
            className="whitespace-nowrap rounded px-1.5 py-0.5 text-xs"
            style={{
              background: emphasized
                ? "rgba(16,185,129,0.15)"
                : "rgba(100,116,139,0.15)",
              color: emphasized ? "#34d399" : "#94a3b8",
            }}
          >
            {emphasized ? tr("작업중", "Working") : tr("대기", "Idle")}
          </span>
        </div>
      </div>
    );
  };

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
            <div className="mt-1.5 flex flex-wrap items-center gap-2">
              <span
                className="whitespace-nowrap rounded-full px-2 py-0.5 text-xs font-medium"
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
                  className="whitespace-nowrap rounded-full px-2 py-0.5 text-xs"
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
                  className="whitespace-nowrap rounded-full px-2 py-0.5 text-xs"
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
                  className="whitespace-nowrap rounded-full px-2 py-0.5 text-xs"
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

        <DetailAccordion
          title={tr("트랜스크립트", "Transcript")}
          subtitle={tr(
            "완료된 턴을 다시 살펴보고 필요한 로그만 펼쳐 봅니다.",
            "Replay completed turns and expand only the logs you need.",
          )}
          badge={tr("최근 8개", "Recent 8")}
          open={transcriptOpen}
          onToggle={() => setTranscriptOpen((prev) => !prev)}
        >
          <TurnTranscriptPanel
            source={{
              type: "agent",
              id: agent.id,
              refreshSeed: `${agent.status}:${agent.session_info ?? ""}:${agent.current_task_id ?? ""}`,
            }}
            isKo={isKo}
            tr={tr}
            embedded
          />
        </DetailAccordion>

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

        <DetailAccordion
          title={tr("세션", "Sessions")}
          subtitle={tr(
            "working 세션은 바로 보이고 idle 세션은 접어 둡니다.",
            "Working sessions stay visible while idle sessions are tucked away.",
          )}
          badge={
            loadingClaudeSessions
              ? tr("불러오는 중", "Loading")
              : `${workingLinkedSessions.length}/${claudeSessions.length}`
          }
          open={sessionsOpen}
          onToggle={() => setSessionsOpen((prev) => !prev)}
        >
          <div className="space-y-3">
            <div
              className="rounded-xl border px-3 py-3"
              style={{
                borderColor: "rgba(148,163,184,0.12)",
                background: "var(--th-bg-surface)",
              }}
            >
              <div
                className="text-xs font-semibold uppercase tracking-widest"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("현재 작업", "Current Work")}
              </div>
              <div
                className="mt-2 text-xs leading-relaxed"
                style={{ color: "var(--th-text-primary)" }}
              >
                {currentWorkSummary ||
                  tr("현재 작업 설명이 없습니다", "No current work detail")}
              </div>
              <div className="mt-3 flex flex-wrap gap-2">
                {currentWorkElapsedMs != null && (
                  <span
                    className="rounded-lg px-2 py-1 text-xs"
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
                  className="rounded-lg px-2 py-1 text-xs"
                  style={{
                    background: "rgba(56,189,248,0.14)",
                    color: "#67e8f9",
                  }}
                >
                  AgentDesk {workingLinkedSessions.length}/{claudeSessions.length}
                </span>
              </div>
              {currentWorkDetails.length > 0 && (
                <div className="mt-3 space-y-1">
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

            {agent.session_info && (
              <div
                className="rounded-xl border px-3 py-3"
                style={{
                  borderColor: "rgba(148,163,184,0.12)",
                  background: "rgba(255,255,255,0.03)",
                }}
              >
                <div
                  className="text-xs font-semibold uppercase tracking-widest"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("세션 메모", "Session Note")}
                </div>
                <div
                  className="mt-2 text-xs leading-relaxed"
                  style={{ color: "var(--th-text-secondary)" }}
                >
                  {agent.session_info}
                </div>
              </div>
            )}

            <div className="space-y-2">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div
                  className="text-xs font-semibold uppercase tracking-widest"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("연결된 AgentDesk 세션", "Linked AgentDesk Sessions")}
                  {!loadingClaudeSessions && ` (${claudeSessions.length})`}
                </div>
                {!loadingClaudeSessions && idleLinkedSessions.length > 0 && (
                  <button
                    type="button"
                    onClick={() => setIdleSessionsOpen((prev) => !prev)}
                    className="rounded-full px-2 py-1 text-xs"
                    style={{
                      background: "rgba(148,163,184,0.12)",
                      color: "var(--th-text-muted)",
                    }}
                  >
                    {idleSessionsOpen
                      ? tr("idle 접기", "Hide idle")
                      : tr(
                          `idle ${idleLinkedSessions.length}개`,
                          `Idle ${idleLinkedSessions.length}`,
                        )}
                  </button>
                )}
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
                  {tr(
                    "연결된 AgentDesk 세션 없음",
                    "No linked AgentDesk sessions",
                  )}
                </div>
              ) : (
                <div className="space-y-2">
                  {workingLinkedSessions.length > 0 ? (
                    <div className="space-y-1.5">
                      {workingLinkedSessions.map((session) =>
                        renderLinkedSessionCard(session, true),
                      )}
                    </div>
                  ) : (
                    <div
                      className="rounded-xl px-3 py-2 text-xs"
                      style={{
                        background: "rgba(148,163,184,0.08)",
                        color: "var(--th-text-muted)",
                      }}
                    >
                      {tr(
                        "현재 working 세션은 없습니다.",
                        "There are no working sessions right now.",
                      )}
                    </div>
                  )}

                  {idleLinkedSessions.length > 0 && !idleSessionsOpen && (
                    <div
                      className="rounded-xl px-3 py-2 text-xs"
                      style={{
                        background: "rgba(148,163,184,0.08)",
                        color: "var(--th-text-muted)",
                      }}
                    >
                      {tr(
                        `idle 세션 ${idleLinkedSessions.length}개는 접혀 있습니다.`,
                        `${idleLinkedSessions.length} idle sessions are tucked away.`,
                      )}
                    </div>
                  )}

                  {idleLinkedSessions.length > 0 && idleSessionsOpen && (
                    <div className="space-y-1.5">
                      {idleLinkedSessions.map((session) =>
                        renderLinkedSessionCard(session, false),
                      )}
                    </div>
                  )}
                </div>
              )}
            </div>
          </div>
        </DetailAccordion>

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

        <DetailAccordion
          title={tr("라우팅", "Routing")}
          subtitle={tr(
            "RoleMap/Primary/Alt/Codex가 이 agent의 Discord 연결 경로를 설명합니다.",
            "RoleMap/Primary/Alt/Codex explain how this agent is wired to Discord.",
          )}
          badge={`${discordBindings.length}`}
          open={routingOpen}
          onToggle={() => setRoutingOpen((prev) => !prev)}
        >
          <div className="space-y-3">
            <div className="overflow-x-auto pb-1">
              <div className="flex min-w-max items-center gap-2">
                {routingChips.map((chip) => (
                  <div
                    key={chip.label}
                    className="flex shrink-0 items-center gap-2 rounded-full border px-2.5 py-1 text-xs"
                    style={{
                      borderColor: `${chip.tone}44`,
                      background: `${chip.tone}14`,
                    }}
                    title={`${chip.fullLabel}: ${chip.fullValue}`}
                  >
                    <span style={{ color: "var(--th-text-muted)" }}>
                      {chip.label}
                    </span>
                    <span className="font-medium" style={{ color: chip.tone }}>
                      {chip.value}
                    </span>
                  </div>
                ))}
              </div>
            </div>

            {roleMapBindings.length > 0 && (
              <div
                className="text-xs"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr(
                  "RoleMap 경로가 있으면 Discord source-of-truth는 role_map 우선으로 봅니다.",
                  "When RoleMap exists, role_map is treated as the Discord source-of-truth.",
                )}
              </div>
            )}

            {discordBindings.length === 0 ? (
              <div
                className="text-xs py-1"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("연결된 Discord 경로 없음", "No Discord routes")}
              </div>
            ) : (
              <div className="space-y-1">
                {discordBindings.map((binding) => {
                  const source = inferBindingSource(binding);
                  const sourcePill = bindingSourceLabel(source);
                  const subtitle =
                    binding.counterModelChannelId &&
                    binding.counterModelChannelId !== binding.channelId
                      ? `counter: ${binding.counterModelChannelId}`
                      : null;

                  return (
                    <div
                      key={`${binding.channelId}:${source}`}
                      className="flex items-center gap-2 rounded-lg px-2.5 py-1.5"
                      style={{ background: "var(--th-bg-surface)" }}
                    >
                      <span className="text-sm">💬</span>
                      <div className="min-w-0 flex-1">
                        <div
                          className="text-xs font-medium truncate"
                          style={{ color: "var(--th-text-primary)" }}
                        >
                          {binding.channelId}
                        </div>
                        {subtitle && (
                          <div
                            className="mt-0.5 text-xs truncate"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            {subtitle}
                          </div>
                        )}
                      </div>
                      <span
                        className="rounded px-1.5 py-0.5 text-xs"
                        style={{
                          background: "rgba(88,101,242,0.15)",
                          color: "#7289da",
                        }}
                      >
                        {sourcePill}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </DetailAccordion>

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

          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              {agent.role_id && (
                <span
                  className="whitespace-nowrap rounded px-1.5 py-0.5 font-mono text-xs"
                  style={{
                    background: "var(--th-bg-surface)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {agent.role_id}
                </span>
              )}
              <span
                className="whitespace-nowrap text-xs"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("완료", "Done")} {agent.stats_tasks_done}
              </span>
            </div>
            <button
              onClick={onClose}
              className="shrink-0 whitespace-nowrap rounded-lg px-3 py-1.5 text-xs font-medium transition-all hover:bg-[var(--th-bg-surface-hover)]"
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
