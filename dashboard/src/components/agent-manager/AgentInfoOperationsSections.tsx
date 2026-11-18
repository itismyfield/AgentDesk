import { useId } from "react";
import { ChevronDown } from "lucide-react";
import type * as api from "../../api";
import type { AgentSkill, CronJob } from "../../api/client";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import type { Translator } from "./types";
import {
  formatDuration,
  formatSchedule,
  timeAgo,
} from "./AgentInfoCardModel";
import { AgentDetailSection, SectionEmpty, SectionLoading } from "./AgentInfoCardSections";

interface AuditLog {
  id: string;
  action: string;
  ts: number;
  detail?: string;
  summary?: string;
  created_at?: number;
}

interface AgentInfoOperationsSectionsProps {
  isKo: boolean;
  tr: Translator;
  cronJobs: CronJob[];
  loadingCron: boolean;
  auditLogs: AuditLog[];
  loadingAudit: boolean;
  auditError: boolean;
  agentSkills: AgentSkill[];
  sharedSkills: AgentSkill[];
  loadingSkills: boolean;
  showSharedSkills: boolean;
  onToggleSharedSkills: () => void;
  timeline: api.TimelineEvent[];
  loadingTimeline: boolean;
  timelineOpen: boolean;
  onToggleTimeline: () => void;
}

const TIMELINE_SOURCE: Record<string, { letter: string; color: string }> = {
  dispatch: { letter: "D", color: "var(--ok)" },
  session: { letter: "S", color: "var(--info)" },
  kanban: { letter: "K", color: "var(--accent)" },
};

function cronDot(job: CronJob) {
  if (!job.enabled) return "var(--fg-faint)";
  return job.state?.lastStatus === "ok" ? "var(--ok)" : "var(--warn)";
}

const chevron = (open: boolean) => (
  <ChevronDown aria-hidden size={14} className={`transition-transform motion-reduce:transition-none ${open ? "rotate-180" : ""}`} />
);

export function AgentInfoOperationsSections({
  isKo,
  tr,
  cronJobs,
  loadingCron,
  auditLogs,
  loadingAudit,
  auditError,
  agentSkills,
  sharedSkills,
  loadingSkills,
  showSharedSkills,
  onToggleSharedSkills,
  timeline,
  loadingTimeline,
  timelineOpen,
  onToggleTimeline,
}: AgentInfoOperationsSectionsProps) {
  const sharedId = useId();
  const timelineId = useId();
  const loadingLabel = tr("불러오는 중...", "Loading...");

  return (
    <>
      <AgentDetailSection title={`${tr("크론 작업", "Cron Jobs")} ${!loadingCron ? `(${cronJobs.length})` : ""}`}>
        {loadingCron ? (
          <SectionLoading label={loadingLabel} />
        ) : cronJobs.length === 0 ? (
          <SectionEmpty>{tr("등록된 크론 작업이 없습니다", "No cron jobs")}</SectionEmpty>
        ) : (
          <ul className="m-0 flex list-none flex-col gap-2 p-0">
            {cronJobs.map((job) => (
              <li key={job.id} className="flex min-w-0 items-start gap-2.5 text-[13px] leading-5">
                <span aria-hidden className="mt-1.5 size-2 shrink-0 rounded-full" style={{ background: cronDot(job) }} />
                <div className="min-w-0 flex-1">
                  <div className="truncate font-medium text-(--fg)" title={job.name}>
                    {job.name}
                  </div>
                  <div className="flex flex-wrap items-center gap-x-2 text-xs text-(--fg-muted)">
                    <span className="font-mono">{formatSchedule(job.schedule, isKo)}</span>
                    {job.state?.lastRunAtMs && (
                      <span className="tabular-nums">
                        {tr("최근:", "Last:")} {timeAgo(job.state.lastRunAtMs, isKo)}
                        {job.state.lastDurationMs != null && ` (${formatDuration(job.state.lastDurationMs)})`}
                      </span>
                    )}
                  </div>
                </div>
                {!job.enabled && <Badge tone="neutral">{tr("비활성", "Off")}</Badge>}
              </li>
            ))}
          </ul>
        )}
      </AgentDetailSection>

      <AgentDetailSection title={tr("최근 변경", "Recent Changes")}>
        {loadingAudit ? (
          <SectionLoading label={loadingLabel} />
        ) : auditError ? (
          <p className="m-0 rounded-(--r-md) bg-[color-mix(in_oklch,var(--warn)_12%,var(--bg-1))] px-3 py-2 text-xs leading-5 text-(--fg-dim)">
            {tr("변경 로그를 불러오지 못했습니다. 상세 화면을 다시 열어 확인해 주세요.", "Could not load changes. Reopen the details to retry.")}
          </p>
        ) : auditLogs.length === 0 ? (
          <SectionEmpty>{tr("관련 변경 로그가 없습니다", "No related audit logs")}</SectionEmpty>
        ) : (
          <ul className="m-0 flex list-none flex-col gap-2 p-0">
            {auditLogs.map((log) => (
              <li key={log.id} className="min-w-0 text-[13px] leading-5">
                <div className="text-(--fg)">{log.summary}</div>
                <div className="text-xs text-(--fg-muted)">
                  {log.action} · <span className="tabular-nums">{timeAgo(log.created_at ?? log.ts, isKo)}</span>
                </div>
              </li>
            ))}
          </ul>
        )}
      </AgentDetailSection>

      <AgentDetailSection title={tr("스킬", "Skills")}>
        {loadingSkills ? (
          <SectionLoading label={loadingLabel} />
        ) : agentSkills.length === 0 && sharedSkills.length === 0 ? (
          <SectionEmpty>{tr("등록된 스킬이 없습니다", "No skills")}</SectionEmpty>
        ) : (
          <div className="flex flex-col gap-2">
            {agentSkills.length > 0 && (
              <div>
                <div className="mb-1.5 text-xs text-(--fg-muted)">{tr("전용 스킬", "Agent-specific")}</div>
                <ul className="m-0 flex list-none flex-wrap gap-1.5 p-0">
                  {agentSkills.map((skill) => (
                    <li
                      key={skill.name}
                      title={skill.description}
                      className="max-w-full rounded-(--r-sm) bg-(--accent-soft) px-2 py-0.5 text-xs leading-5 text-(--fg) [overflow-wrap:anywhere]"
                    >
                      {skill.name}
                    </li>
                  ))}
                </ul>
              </div>
            )}
            {sharedSkills.length > 0 && (
              <div className="flex flex-col items-start gap-1.5">
                <Button
                  variant="ghost"
                  size="sm"
                  aria-expanded={showSharedSkills}
                  aria-controls={sharedId}
                  iconRight={chevron(showSharedSkills)}
                  onClick={onToggleSharedSkills}
                >
                  {`${tr("공유 스킬", "Shared")} (${sharedSkills.length})`}
                </Button>
                {showSharedSkills && (
                  <ul id={sharedId} className="m-0 flex list-none flex-wrap gap-1.5 p-0">
                    {sharedSkills.map((skill) => (
                      <li
                        key={skill.name}
                        title={skill.description}
                        className="max-w-full rounded-(--r-sm) px-2 py-0.5 text-xs leading-5 text-(--fg-muted) shadow-[inset_0_0_0_1px_var(--line-soft)] [overflow-wrap:anywhere]"
                      >
                        {skill.name}
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}
          </div>
        )}
      </AgentDetailSection>

      <AgentDetailSection
        title={tr("활동", "Activity")}
        aside={
          <Button
            variant="ghost"
            size="sm"
            aria-expanded={timelineOpen}
            aria-controls={timelineId}
            iconRight={chevron(timelineOpen)}
            onClick={onToggleTimeline}
          >
            {tr("활동 타임라인", "Activity Timeline")}
          </Button>
        }
      >
        {timelineOpen ? (
          <div id={timelineId}>
            {loadingTimeline ? (
              <SectionLoading label={loadingLabel} />
            ) : timeline.length === 0 ? (
              <SectionEmpty>{tr("활동 없음", "No activity")}</SectionEmpty>
            ) : (
              <ul className="m-0 flex max-h-64 list-none flex-col gap-2 overflow-y-auto overscroll-contain p-0">
                {timeline.map((event) => {
                  const source = TIMELINE_SOURCE[event.source] ?? TIMELINE_SOURCE.kanban;
                  const durationStr =
                    event.duration_ms != null
                      ? event.duration_ms < 60_000
                        ? `${Math.round(event.duration_ms / 1000)}s`
                        : `${Math.round(event.duration_ms / 60_000)}m`
                      : null;
                  return (
                    <li key={`${event.source}-${event.id}`} className="flex items-start gap-2 text-xs">
                      <span
                        aria-hidden
                        className="mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-full text-[10px] font-bold"
                        style={{ background: `color-mix(in oklch, ${source.color} 18%, transparent)`, color: source.color }}
                      >
                        {source.letter}
                      </span>
                      <div className="min-w-0 flex-1">
                        <div className="text-[13px] leading-5 text-(--fg) [overflow-wrap:anywhere]">{event.title}</div>
                        <div className="flex flex-wrap gap-x-2 text-(--fg-muted)">
                          <span className="tabular-nums">{timeAgo(event.timestamp, isKo)}</span>
                          <span>{event.status}</span>
                          {durationStr && <span className="tabular-nums">{durationStr}</span>}
                          {event.detail && "issue" in event.detail && <span>#{String(event.detail.issue)}</span>}
                        </div>
                      </div>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>
        ) : null}
      </AgentDetailSection>
    </>
  );
}
