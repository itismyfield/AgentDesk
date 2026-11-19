import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router";
import { describeRoutinePurpose, formatClock } from "../dashboard/routineModel";
import { CardNote, CardSkeleton, Chip, DONE_COLOR, HeaderLink, HomeCard, Muted, RatioBar, ROW_HOVER, StageTrack, toneText } from "./HomeCard";
import { autoPauseRule, buildCampaignRows, buildTodayRoutines, buildUsageRows, type Tr } from "./homeModel";
import { loadHomeRoutines, loadHomeUsage, loadRuntimeConfig, usePolled, type HomeCampaigns, type Loaded } from "./useHomeData";

// Home's lower row: campaigns, today's routines and account usage.

const CAMPAIGN_ROWS = 4;

/** Open campaigns with a progress track and one line of where each stands; a row opens the campaign. Campaigns
 * under way lead and the rest unfold on home. */
export function HomeCampaignsCard({ campaigns, tr, className }: { campaigns: Loaded<HomeCampaigns>; tr: Tr; className?: string }) {
  const [expanded, setExpanded] = useState(false);
  const { rows, shown } = buildCampaignRows({
    legacy: campaigns.data?.legacy ?? null,
    summaries: campaigns.data?.summaries ?? null,
    documents: campaigns.data?.documents ?? [],
    tr,
    limit: CAMPAIGN_ROWS,
  });
  // A failed latest poll or one failed list makes the shown rows incomplete, and an empty list is not "no campaigns".
  const unreadable = campaigns.failed || Boolean(campaigns.data && (!campaigns.data.legacy || !campaigns.data.summaries));
  const rest = rows.length - shown;
  return (
    <HomeCard
      title={tr("캠페인", "Campaigns")}
      count={campaigns.data && !(unreadable && rows.length === 0) ? rows.length : null}
      aside={<HeaderLink to="/campaigns">{tr("모두 보기", "See all")}</HeaderLink>}
      testId="home-campaigns"
      className={className}
    >
      {!campaigns.data && !campaigns.failed ? (
        <CardSkeleton label={tr("캠페인 불러오는 중", "Loading campaigns")} />
      ) : rows.length === 0 ? (
        <CardNote>{unreadable ? tr("캠페인 목록을 읽지 못해 확인 불가예요.", "Campaign lists did not load, so campaigns are unknown.") : tr("진행 중인 캠페인이 없어요.", "No open campaigns.")}</CardNote>
      ) : (
        <ul className="m-0 list-none p-0">
          {(expanded ? rows : rows.slice(0, shown)).map((row) => (
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
                  {row.segments ? <StageTrack segments={row.segments} /> : row.ratio !== null ? <RatioBar ratio={row.ratio} color={DONE_COLOR} /> : null}
                  <Muted className={`block truncate ${row.segments || row.ratio !== null ? "mt-1.5" : ""}`}>{row.line}</Muted>
                </span>
              </Link>
            </li>
          ))}
          {rest > 0 ? (
            <li className="flex justify-center border-t border-[var(--line-soft)] p-1">
              <button type="button" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)} className={`min-h-11 rounded-[var(--r-md)] px-4 text-sm font-medium text-[var(--fg)] ${ROW_HOVER}`}>
                {expanded ? tr("접기", "Show less") : tr(`${rest}개 더 보기`, `Show ${rest} more`)}
              </button>
            </li>
          ) : null}
        </ul>
      )}
      {unreadable && rows.length > 0 ? <CardNote>{tr("캠페인 목록을 다 읽지 못해 일부는 확인 불가예요.", "Not every campaign list loaded, so some campaigns are unknown.")}</CardNote> : null}
    </HomeCard>
  );
}

/** Today's routine runs: time, the routine's name and what it does, and the result or that it is still to come. */
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
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm" title={row.routine.name}>{row.routine.name}</span>
                  {purpose !== row.routine.name ? <span className="block truncate text-xs text-[var(--fg-muted)]" title={purpose}>{purpose}</span> : null}
                </span>
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
// The auto-run pause settings sit in the settings catalog's usage category.
const AUTO_PAUSE_SETTINGS = "/settings?category=usage";

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
                  {row.pct !== null ? `${row.pct}%` : row.unmeasured ? tr("측정 안 됨", "Not measured") : unknown}
                </span>
              </span>
              {row.pct === null ? null : <span className="mt-1.5 block"><RatioBar ratio={row.pct / 100} color={LEVEL_COLOR[row.level]} /></span>}
            </li>
          ))}
        </ul>
      )}
      {/* The settings link stays even when the rule could not be read. */}
      <p className="m-0 flex items-center gap-2 px-4 pb-1 text-xs text-[var(--fg-muted)]">
        <span className="min-w-0 flex-1">
          {rule
            ? rule.enabled && rule.pct !== null
              ? tr(`사용량이 ${rule.pct}% 이상이면 자동 진행을 멈춰요`, `Auto-run pauses at ${rule.pct}% usage`)
              : tr("사용량으로 자동 진행을 멈추지 않아요", "Auto-run does not pause for usage")
            : runtime.failed ? tr("자동 진행을 멈추는 기준은 확인 불가예요", "The auto-run pause rule is unknown") : null}
        </span>
        <Link to={AUTO_PAUSE_SETTINGS} className="-mr-1.5 inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center font-medium text-[var(--accent)] hover:underline">{tr("설정", "Settings")}</Link>
      </p>
    </HomeCard>
  );
}
