import type * as api from "../../api";
import type { Agent } from "../../types";
import type { AgentSkill, CronJob } from "../../api/client";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type { Translator } from "./types";
import {
  formatDuration,
  formatSchedule,
  timeAgo,
} from "./AgentInfoCardModel";

interface AgentLevelInfo {
  level: number;
  progress: number;
  nextThreshold: number;
}

interface AuditLog {
  id: string;
  action: string;
  ts: number;
  detail?: string;
  summary?: string;
  created_at?: number;
}

interface AgentInfoOperationsSectionsProps {
  agent: Agent;
  isKo: boolean;
  tr: Translator;
  cronJobs: CronJob[];
  loadingCron: boolean;
  auditLogs: AuditLog[];
  loadingAudit: boolean;
  agentSkills: AgentSkill[];
  sharedSkills: AgentSkill[];
  loadingSkills: boolean;
  showSharedSkills: boolean;
  onToggleSharedSkills: () => void;
  levelInfo: AgentLevelInfo;
  levelTitle: string;
  timeline: api.TimelineEvent[];
  loadingTimeline: boolean;
  timelineOpen: boolean;
  onToggleTimeline: () => void;
}

export function AgentInfoOperationsSections({
  agent,
  isKo,
  tr,
  cronJobs,
  loadingCron,
  auditLogs,
  loadingAudit,
  agentSkills,
  sharedSkills,
  loadingSkills,
  showSharedSkills,
  onToggleSharedSkills,
  levelInfo,
  levelTitle,
  timeline,
  loadingTimeline,
  timelineOpen,
  onToggleTimeline,
}: AgentInfoOperationsSectionsProps) {
  return (
    <>
      <SurfaceSubsection
        title={`${tr("크론 작업", "Cron Jobs")} ${!loadingCron ? `(${cronJobs.length})` : ""}`}
        className="md:col-span-2"
      >
        {loadingCron ? (
          <SurfaceNotice tone="neutral" compact>
            {tr("불러오는 중...", "Loading...")}
          </SurfaceNotice>
        ) : cronJobs.length === 0 ? (
          <SurfaceEmptyState className="text-xs">
            {tr("등록된 크론 작업이 없습니다", "No cron jobs")}
          </SurfaceEmptyState>
        ) : (
          <div className="space-y-1.5">
            {cronJobs.map((job) => (
              <SurfaceCard
                key={job.id}
                className="flex items-start gap-2 px-2.5 py-2"
                style={{ background: "var(--th-bg-surface)" }}
              >
                <span
                  className={`mt-0.5 h-1.5 w-1.5 shrink-0 rounded-full ${
                    job.enabled
                      ? job.state?.lastStatus === "ok"
                        ? "bg-emerald-400"
                        : "bg-amber-400"
                      : "bg-slate-500"
                  }`}
                />
                <div className="min-w-0 flex-1">
                  <div
                    className="truncate text-xs font-medium"
                    style={{ color: "var(--th-text-primary)" }}
                    title={job.name}
                  >
                    {job.name}
                  </div>
                  <div className="mt-0.5 flex flex-wrap items-center gap-2">
                    <span className="text-xs font-mono" style={{ color: "var(--th-text-muted)" }}>
                      {formatSchedule(job.schedule, isKo)}
                    </span>
                    {job.state?.lastRunAtMs && (
                      <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {tr("최근:", "Last:")} {timeAgo(job.state.lastRunAtMs, isKo)}
                        {job.state.lastDurationMs != null &&
                          ` (${formatDuration(job.state.lastDurationMs)})`}
                      </span>
                    )}
                  </div>
                </div>
                {!job.enabled && (
                  <span
                    className="shrink-0 rounded px-1.5 py-0.5 text-xs"
                    style={{ background: "rgba(100,116,139,0.2)", color: "#94a3b8" }}
                  >
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
          <SurfaceNotice tone="neutral" compact>
            {tr("불러오는 중...", "Loading...")}
          </SurfaceNotice>
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
          <SurfaceNotice tone="neutral" compact>
            {tr("불러오는 중...", "Loading...")}
          </SurfaceNotice>
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
                        background:
                          "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
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
                <SurfaceActionButton onClick={onToggleSharedSkills} tone="neutral" compact>
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
            <div
              className="h-1.5 flex-1 overflow-hidden rounded-full"
              style={{ background: "var(--th-bg-surface)" }}
            >
              <div
                className="h-full rounded-full transition-all"
                style={{
                  width: `${Math.round(levelInfo.progress * 100)}%`,
                  background:
                    "linear-gradient(90deg, var(--th-accent-primary), var(--th-accent-info))",
                }}
              />
            </div>
            <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>
              {agent.stats_xp} / {levelInfo.nextThreshold === Infinity ? "MAX" : levelInfo.nextThreshold} XP
            </span>
          </div>

          <SurfaceCard className="overflow-hidden px-0 py-0" style={{ background: "var(--th-bg-card)" }}>
            <button
              onClick={onToggleTimeline}
              className="flex w-full items-center justify-between px-4 py-3 text-xs font-semibold"
              style={{ color: "var(--th-text-heading)" }}
            >
              <span>{tr("활동 타임라인", "Activity Timeline")}</span>
              <span style={{ color: "var(--th-text-muted)" }}>{timelineOpen ? "▲" : "▼"}</span>
            </button>
            {timelineOpen && (
              <div className="space-y-1.5 px-4 pb-3">
                {loadingTimeline ? (
                  <SurfaceNotice tone="neutral" compact>
                    …
                  </SurfaceNotice>
                ) : timeline.length === 0 ? (
                  <SurfaceEmptyState className="py-2 text-xs">
                    {tr("활동 없음", "No activity")}
                  </SurfaceEmptyState>
                ) : (
                  <div className="max-h-64 space-y-1.5 overflow-y-auto">
                    {timeline.map((event) => {
                      const sourceColor =
                        event.source === "dispatch"
                          ? "#10b981"
                          : event.source === "session"
                            ? "#38bdf8"
                            : "#84cc16";
                      const sourceLabel =
                        event.source === "dispatch" ? "D" : event.source === "session" ? "S" : "K";
                      const durationStr =
                        event.duration_ms != null
                          ? event.duration_ms < 60_000
                            ? `${Math.round(event.duration_ms / 1000)}s`
                            : `${Math.round(event.duration_ms / 60_000)}m`
                          : null;
                      return (
                        <div key={`${event.source}-${event.id}`} className="flex items-start gap-2 text-xs">
                          <span
                            className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full text-xs font-bold"
                            style={{ backgroundColor: `${sourceColor}22`, color: sourceColor }}
                          >
                            {sourceLabel}
                          </span>
                          <div className="min-w-0 flex-1">
                            <div className="truncate" style={{ color: "var(--th-text-primary)" }}>
                              {event.title}
                            </div>
                            <div className="flex flex-wrap gap-2" style={{ color: "var(--th-text-muted)" }}>
                              <span>{timeAgo(event.timestamp, isKo)}</span>
                              <span
                                className="rounded px-1"
                                style={{ backgroundColor: `${sourceColor}15`, color: sourceColor }}
                              >
                                {event.status}
                              </span>
                              {durationStr && <span>{durationStr}</span>}
                              {event.detail && "issue" in event.detail && <span>#{String(event.detail.issue)}</span>}
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
              <span
                className="rounded px-1.5 py-0.5 font-mono text-xs"
                style={{ background: "var(--th-bg-surface)", color: "var(--th-text-muted)" }}
              >
                {agent.role_id}
              </span>
            )}
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr("완료", "Done")} {agent.stats_tasks_done}
            </span>
          </div>
        </div>
      </SurfaceSubsection>
    </>
  );
}
