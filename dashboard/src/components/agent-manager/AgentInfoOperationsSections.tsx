import { useId, useState } from "react";
import { ChevronDown } from "lucide-react";
import type * as api from "../../api";
import type { AgentSkill, CronJob } from "../../api/client";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { CompactBreakdown } from "../../kit/pro/compact-breakdown";
import type { AuditLogEntry } from "../../types";
import { Cap } from "../common/KitPrimitives";
import type { Translator } from "./types";
import { formatDuration, formatSchedule, timeAgo } from "./AgentInfoCardModel";
import { AgentDetailSection, SectionEmpty, SectionLoading, type DetailFold } from "./AgentInfoCardSections";
import type { Read } from "./useAgentDetailData";

const TIMELINE_SOURCE: Record<string, { letter: string; color: string }> = {
  dispatch: { letter: "D", color: "var(--ok)" },
  session: { letter: "S", color: "var(--info)" },
  kanban: { letter: "K", color: "var(--accent)" },
};

function cronDot(job: CronJob) {
  if (!job.enabled) return "var(--fg-faint)";
  return job.state?.lastStatus === "ok" ? "var(--ok)" : "var(--warn)";
}

const loadingText = (tr: Translator) => tr("불러오는 중...", "Loading...");

/** Skills: this agent's 30-day calls per skill (one series colour, clear of the state colours), then its own and shared
 * skills by name. */
export function AgentSkillsSection({ tr, fold, calls, skills }: {
  tr: Translator;
  fold: DetailFold;
  calls: Read<Array<{ id: string; label: string; value: number }>>;
  skills: Read<{ skills: AgentSkill[]; sharedSkills: AgentSkill[] }>;
}) {
  const [showShared, setShowShared] = useState(false);
  const sharedId = useId();
  const total = calls?.reduce((sum, row) => sum + row.value, 0);
  const own = skills?.skills ?? [];
  const shared = skills?.sharedSkills ?? [];
  return (
    <AgentDetailSection
      title={tr("스킬 · 30일 호출", "Skills · 30-day calls")}
      summary={fold.foldable && total !== undefined ? tr(`${total.toLocaleString()}회`, `${total.toLocaleString()} calls`) : undefined}
      fold={fold}
      testId="agent-detail-skills"
    >
      {calls === undefined ? (
        <SectionLoading label={loadingText(tr)} />
      ) : calls === null || calls.length === 0 ? (
        <SectionEmpty>{calls ? tr("30일 동안 부른 스킬이 없어요", "No skill calls in 30 days") : tr("호출 수를 불러오지 못했어요", "Could not load skill calls")}</SectionEmpty>
      ) : (
        <CompactBreakdown
          items={calls.map((row) => ({ ...row, color: "var(--accent)" }))}
          title={tr("스킬별 호출", "Calls by skill")}
          totalLabel={tr("전체 호출", "All calls")}
          format={(value) => value.toLocaleString()}
          hideFooter
          labels={{
            list: tr("스킬별 호출 수와 비중", "Calls and share by skill"),
            row: (label, value, share) => tr(`${label}, ${value}회, 전체의 ${share}`, `${label}, ${value} calls, ${share} of all`),
            summary: (name, amount) => `${name}: ${amount}`,
            empty: tr("30일 동안 부른 스킬이 없어요", "No skill calls in 30 days"),
            selectHint: "",
          }}
        />
      )}
      {skills === undefined ? null : own.length === 0 && shared.length === 0 ? (
        <SectionEmpty>{tr("등록된 스킬이 없습니다", "No skills")}</SectionEmpty>
      ) : (
        <div className="flex flex-col gap-2">
          {own.length > 0 ? (
            <div>
              <Cap className="mb-1.5 block">{tr("전용 스킬", "Agent-specific")}</Cap>
              <ul className="m-0 flex list-none flex-wrap gap-1.5 p-0">
                {own.map((skill) => (
                  <li key={skill.name} title={skill.description} className="max-w-full rounded-(--r-sm) bg-(--accent-soft) px-2 py-0.5 text-xs leading-5 text-(--fg) [overflow-wrap:anywhere]">
                    {skill.name}
                  </li>
                ))}
              </ul>
            </div>
          ) : null}
          {shared.length > 0 ? (
            <div className="flex flex-col items-start gap-1.5">
              <Button
                variant="ghost"
                size="sm"
                className="-ml-2"
                aria-expanded={showShared}
                aria-controls={showShared ? sharedId : undefined}
                iconRight={<ChevronDown aria-hidden size={14} className={`transition-transform motion-reduce:transition-none ${showShared ? "rotate-180" : ""}`} />}
                onClick={() => setShowShared((value) => !value)}
              >
                {`${tr("공유 스킬", "Shared")} (${shared.length})`}
              </Button>
              {showShared ? (
                <ul id={sharedId} className="m-0 flex list-none flex-wrap gap-1.5 p-0">
                  {shared.map((skill) => (
                    <li key={skill.name} title={skill.description} className="max-w-full rounded-(--r-sm) px-2 py-0.5 text-xs leading-5 text-(--fg-muted) shadow-[inset_0_0_0_1px_var(--line-soft)] [overflow-wrap:anywhere]">
                      {skill.name}
                    </li>
                  ))}
                </ul>
              ) : null}
            </div>
          ) : null}
        </div>
      )}
    </AgentDetailSection>
  );
}

/** Activity, read when opened: the card, dispatch and session timeline, then recent audited changes. */
export function AgentActivitySection({ tr, isKo, fold, timeline, audit }: {
  tr: Translator;
  isKo: boolean;
  fold: DetailFold;
  timeline: Read<api.TimelineEvent[]>;
  audit: Read<AuditLogEntry[]>;
}) {
  return (
    <AgentDetailSection title={tr("활동", "Activity")} summary={fold.foldable && !fold.open ? tr("카드·세션 기록, 최근 변경", "Card and session log, changes") : undefined} fold={fold} testId="agent-detail-activity">
      <Cap>{tr("카드·세션 기록", "Card and session log")}</Cap>
      {timeline === undefined ? (
        <SectionLoading label={loadingText(tr)} />
      ) : !timeline?.length ? (
        <SectionEmpty>{tr("활동 없음", "No activity")}</SectionEmpty>
      ) : (
        <ul className="m-0 flex max-h-64 list-none flex-col gap-2 overflow-y-auto overscroll-contain p-0">
          {timeline.map((event) => {
            const source = TIMELINE_SOURCE[event.source] ?? TIMELINE_SOURCE.kanban;
            const duration = event.duration_ms == null ? null : event.duration_ms < 60_000 ? `${Math.round(event.duration_ms / 1000)}s` : `${Math.round(event.duration_ms / 60_000)}m`;
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
                    {duration ? <span className="tabular-nums">{duration}</span> : null}
                    {event.detail && "issue" in event.detail ? <span>#{String(event.detail.issue)}</span> : null}
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}
      <Cap className="mt-2">{tr("최근 변경", "Recent changes")}</Cap>
      {audit === undefined ? (
        <SectionLoading label={loadingText(tr)} />
      ) : audit === null ? (
        <p className="m-0 rounded-(--r-md) bg-[color-mix(in_oklch,var(--warn)_12%,var(--bg-1))] px-3 py-2 text-xs leading-5 text-(--fg-dim)">
          {tr("변경 로그를 불러오지 못했습니다. 상세 화면을 다시 열어 확인해 주세요.", "Could not load changes. Reopen the details to retry.")}
        </p>
      ) : audit.length === 0 ? (
        <SectionEmpty>{tr("관련 변경 로그가 없습니다", "No related audit logs")}</SectionEmpty>
      ) : (
        <ul className="m-0 flex list-none flex-col gap-2 p-0">
          {audit.map((log) => (
            <li key={log.id} className="min-w-0 text-[13px] leading-5">
              <div className="text-(--fg)">{log.summary}</div>
              <div className="text-xs text-(--fg-muted)">
                {log.action} · <span className="tabular-nums">{timeAgo(log.created_at, isKo)}</span>
              </div>
            </li>
          ))}
        </ul>
      )}
    </AgentDetailSection>
  );
}

/** Cron jobs, read when opened; the count shows once read. */
export function AgentCronSection({ tr, isKo, fold, jobs }: { tr: Translator; isKo: boolean; fold: DetailFold; jobs: Read<CronJob[]> }) {
  return (
    <AgentDetailSection title={tr("크론", "Cron")} summary={jobs ? jobs.length : undefined} fold={fold} testId="agent-detail-cron">
      {jobs === undefined ? (
        <SectionLoading label={loadingText(tr)} />
      ) : !jobs?.length ? (
        <SectionEmpty>{jobs ? tr("등록된 크론 작업이 없습니다", "No cron jobs") : tr("크론을 불러오지 못했어요", "Could not load cron jobs")}</SectionEmpty>
      ) : (
        <ul className="m-0 flex list-none flex-col gap-2 p-0">
          {jobs.map((job) => (
            <li key={job.id} className="flex min-w-0 items-start gap-2.5 text-[13px] leading-5">
              <span aria-hidden className="mt-1.5 size-2 shrink-0 rounded-full" style={{ background: cronDot(job) }} />
              <div className="min-w-0 flex-1">
                <div className="truncate font-medium text-(--fg)" title={job.name}>{job.name}</div>
                <div className="flex flex-wrap items-center gap-x-2 text-xs text-(--fg-muted)">
                  <span className="font-mono">{formatSchedule(job.schedule, isKo)}</span>
                  {job.state?.lastRunAtMs ? (
                    <span className="tabular-nums">
                      {tr("최근:", "Last:")} {timeAgo(job.state.lastRunAtMs, isKo)}
                      {job.state.lastDurationMs != null && ` (${formatDuration(job.state.lastDurationMs)})`}
                    </span>
                  ) : null}
                </div>
              </div>
              {!job.enabled ? <Badge tone="neutral">{tr("비활성", "Off")}</Badge> : null}
            </li>
          ))}
        </ul>
      )}
    </AgentDetailSection>
  );
}
