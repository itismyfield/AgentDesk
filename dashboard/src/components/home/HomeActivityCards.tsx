import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router";
import { describeRoutinePurpose, formatClock } from "../dashboard/routineModel";
import { CardNote, CardSkeleton, Chip, DONE_COLOR, HeaderLink, HomeCard, Muted, RatioBar, ROW_HOVER, StageTrack, toneText } from "./HomeCard";
import { autoPauseRule, buildCampaignRows, buildTodayRoutines, buildUsageRows, type Tr } from "./homeModel";
import { loadHomeRoutines, loadHomeUsage, loadRuntimeConfig, usePolled, type HomeCampaigns, type Loaded } from "./useHomeData";

// Home's lower row: campaigns, today's routines and account usage.

const CAMPAIGN_ROWS = 4;

/** Open campaigns with a progress track and one line of where each stands; a row opens the campaign. */
export function HomeCampaignsCard({ campaigns, tr, className }: { campaigns: Loaded<HomeCampaigns>; tr: Tr; className?: string }) {
  const { rows, total } = buildCampaignRows({
    legacy: campaigns.data?.legacy ?? null,
    summaries: campaigns.data?.summaries ?? null,
    documents: campaigns.data?.documents ?? [],
    tr,
    limit: CAMPAIGN_ROWS,
  });
  return (
    <HomeCard
      title={tr("캠페인", "Campaigns")}
      count={campaigns.data ? total : null}
      aside={<HeaderLink to="/campaigns">{tr("모두 보기", "See all")}</HeaderLink>}
      testId="home-campaigns"
      className={className}
    >
      {!campaigns.data ? (
        campaigns.failed ? <CardNote>{tr("캠페인을 읽지 못했어요.", "Could not read campaigns.")}</CardNote> : <CardSkeleton label={tr("캠페인 불러오는 중", "Loading campaigns")} />
      ) : rows.length === 0 ? (
        <CardNote>{tr("진행 중인 캠페인이 없어요.", "No open campaigns.")}</CardNote>
      ) : (
        <ul className="m-0 list-none p-0">
          {rows.map((row) => (
            <li key={row.key} className="border-t border-[var(--line-soft)]">
              <Link
                to={row.to}
                className={`flex min-h-16 flex-col gap-2 px-4 py-3 ${ROW_HOVER} focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)] sm:flex-row sm:items-center sm:gap-3.5`}
              >
                <span className="min-w-0 sm:w-40 sm:shrink-0">
                  <span className="block truncate text-sm font-medium" title={row.title}>{row.title}</span>
                  <span className="block truncate text-xs" style={{ color: toneText(row.tone) ?? "var(--fg-muted)" }}>{row.status}</span>
                </span>
                <span className="min-w-0 flex-1">
                  {row.segments ? <StageTrack segments={row.segments} /> : <RatioBar ratio={row.ratio ?? 0} color={DONE_COLOR} />}
                  <Muted className="mt-1.5 block truncate">{row.line}</Muted>
                </span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </HomeCard>
  );
}

/** Today's routine runs: time, what it does, and the result or that it is still to come. */
export function HomeRoutinesCard({ tr, isKo, className }: { tr: Tr; isKo: boolean; className?: string }) {
  const routines = usePolled(loadHomeRoutines, 60_000);
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 60_000);
    return () => window.clearInterval(timer);
  }, []);
  const t = useCallback((messages: { ko: string; en?: string }) => (isKo ? messages.ko : messages.en ?? messages.ko), [isKo]);
  const rows = buildTodayRoutines(routines.data ?? [], now, tr);
  const finished = rows.filter((row) => row.finished).length;

  return (
    <HomeCard
      title={tr("오늘 루틴", "Today's routines")}
      aside={routines.data ? (
        rows.length ? <Muted className="tabular-nums">{tr(`${rows.length}개 중 ${finished}개 끝남`, `${finished} of ${rows.length} done`)}</Muted> : <HeaderLink to="/routines">{tr("루틴 보기", "Routines")}</HeaderLink>
      ) : null}
      testId="home-routines"
      className={className}
    >
      {!routines.data ? (
        routines.failed ? <CardNote>{tr("루틴을 읽지 못했어요.", "Could not read routines.")}</CardNote> : <CardSkeleton label={tr("루틴 불러오는 중", "Loading routines")} />
      ) : rows.length === 0 ? (
        <CardNote>{tr("오늘 도는 루틴이 없어요.", "No routines run today.")}</CardNote>
      ) : (
        <ul className="m-0 list-none p-0">
          {rows.map((row) => {
            const purpose = describeRoutinePurpose(row.routine, t);
            return (
              <li key={row.key} data-testid={`home-routine-${row.key}`} className="flex min-h-11 items-center gap-3 border-t border-[var(--line-soft)] px-4 py-2.5">
                <Muted className="w-11 shrink-0 tabular-nums">{formatClock(row.at)}</Muted>
                <span className="min-w-0 flex-1 truncate text-sm" title={`${purpose} (${row.routine.name})`}>{purpose}</span>
                <Chip tone={row.tone}>{row.chip}</Chip>
              </li>
            );
          })}
        </ul>
      )}
    </HomeCard>
  );
}

const LEVEL_COLOR = { normal: "var(--accent)", warning: "var(--warn)", danger: "var(--err)" } as const;
const AUTO_PAUSE_SETTINGS = "/settings?settingsPanel=workflow";

/** Account limits from the limit tracker as bars, when they were measured, and the auto-run pause rule. */
export function HomeUsageCard({ tr, isKo, className }: { tr: Tr; isKo: boolean; className?: string }) {
  const usage = usePolled(loadHomeUsage, 60_000);
  const runtime = usePolled(loadRuntimeConfig, 300_000);
  const rows = buildUsageRows(usage.data ?? [], isKo);
  const measured = rows.map((row) => row.measuredAt).filter((time): time is number => time !== null);
  const unknown = tr("확인 불가", "Unknown");
  const rule = autoPauseRule(runtime.data);

  return (
    <HomeCard
      title={tr("사용량", "Usage")}
      aside={usage.data || usage.failed ? (
        <Muted className="tabular-nums">{tr("한도 추적기", "Limit tracker")} · {measured.length ? formatClock(Math.max(...measured)) : unknown}</Muted>
      ) : null}
      testId="home-usage"
      className={className}
    >
      {!usage.data ? (
        usage.failed ? <CardNote>{tr("사용량은 지금 확인 불가예요.", "Usage is unknown right now.")}</CardNote> : <CardSkeleton label={tr("사용량 불러오는 중", "Loading usage")} />
      ) : rows.length === 0 ? (
        <CardNote>{tr("측정값이 없어 확인 불가예요.", "No measurements, so usage is unknown.")}</CardNote>
      ) : (
        <ul className="m-0 flex list-none flex-col gap-3 border-t border-[var(--line-soft)] px-4 py-3.5">
          {rows.map((row) => (
            <li key={row.key} className="min-w-0">
              <span className="flex min-w-0 items-baseline gap-2 text-[13px] leading-5">
                <span className="min-w-0 flex-1 truncate text-[var(--fg-muted)]" title={row.note ?? row.label}>{row.label}</span>
                {row.stale && row.measuredAt !== null ? <Muted>{tr(`${formatClock(row.measuredAt)} 값`, `as of ${formatClock(row.measuredAt)}`)}</Muted> : null}
                <span className="shrink-0 tabular-nums" style={{ color: row.pct === null ? "var(--fg-muted)" : row.level === "normal" ? undefined : LEVEL_COLOR[row.level] }}>
                  {row.pct === null ? unknown : `${row.pct}%`}
                </span>
              </span>
              <span className="mt-1.5 block"><RatioBar ratio={(row.pct ?? 0) / 100} color={LEVEL_COLOR[row.level]} /></span>
            </li>
          ))}
        </ul>
      )}
      {rule ? (
        <p className="m-0 flex items-center gap-2 px-4 pb-1 text-xs text-[var(--fg-muted)]">
          <span className="min-w-0 flex-1">
            {rule.enabled && rule.pct !== null
              ? tr(`사용량이 ${rule.pct}% 이상이면 자동 진행을 멈춰요`, `Auto-run pauses at ${rule.pct}% usage`)
              : tr("사용량으로 자동 진행을 멈추지 않아요", "Auto-run does not pause for usage")}
          </span>
          <Link to={AUTO_PAUSE_SETTINGS} className="-mr-1.5 inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center font-medium text-[var(--accent)] hover:underline">{tr("설정", "Settings")}</Link>
        </p>
      ) : null}
    </HomeCard>
  );
}
