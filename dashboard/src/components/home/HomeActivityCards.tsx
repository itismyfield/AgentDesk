import { useCallback, useState, type CSSProperties } from "react";
import { Link } from "react-router";
import type { RuntimeConfigResponse } from "../../api/settingsRuntime";
import { Button } from "../../kit/free/button-clean";
import { UsageMeter, type UsageMeterLabels } from "../../kit/pro/usage-meter";
import { useNow } from "../../lib/useNow";
import {
  Cap, CardNote, CardSkeleton, Chip, HeaderLink, InspSection, MeterBar, Row, RowLink, SectionCard, StageTrack, toneText,
} from "../common/KitPrimitives";
import { describeRoutinePurpose, formatClock } from "../dashboard/routineModel";
import { autoPauseRule, buildCampaignRows, buildTodayRoutines, buildUsageRows, pauseMeterScale, usageDetail, type RawUsageProvider, type Tr } from "./homeModel";
import { loadHomeRoutines, loadHomeUsage, loadRuntimeConfig, usePolled, type HomeCampaigns, type Loaded } from "./useHomeData";

const CAMPAIGN_ROWS = 4;
const FULL_TOUCH_BUTTON_STYLE = {
  "--aui-button-clean-h": "44px",
  "--aui-button-clean-muted": "var(--fg-dim)",
  width: "100%",
} as CSSProperties;

export function HomeCampaignsCard({ campaigns, tr, className }: { campaigns: Loaded<HomeCampaigns>; tr: Tr; className?: string }) {
  const [expanded, setExpanded] = useState(false);
  const { rows, shown } = buildCampaignRows({
    legacy: campaigns.data?.legacy ?? null,
    summaries: campaigns.data?.summaries ?? null,
    documents: campaigns.data?.documents ?? [],
    tr,
    limit: CAMPAIGN_ROWS,
  });
  const unreadable = campaigns.failed || Boolean(campaigns.data && (!campaigns.data.legacy || !campaigns.data.summaries));
  const rest = rows.length - shown;
  return (
    <SectionCard
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
        <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
          {(expanded ? rows : rows.slice(0, shown)).map((row) => (
            <li key={row.key} className="border-t border-[var(--line-soft)] first:border-t-0">
              <RowLink to={row.to} className="flex-col items-stretch gap-1.5 py-2.5 sm:min-h-16 sm:flex-row sm:items-center sm:gap-3.5 sm:py-3">
                <span className="flex min-w-0 items-baseline justify-between gap-2 sm:block sm:w-40 sm:shrink-0">
                  <span className="min-w-0 flex-1 truncate text-sm leading-5 font-medium sm:block" title={row.title}>{row.title}</span>
                  <span className="shrink-0 text-xs leading-4 sm:mt-0.5 sm:block sm:truncate" style={{ color: toneText(row.tone) ?? "var(--fg-muted)" }}>
                    {row.status}
                  </span>
                </span>
                <span className="min-w-0 flex-1">
                  {row.segments ? <StageTrack segments={row.segments} /> : row.ratio !== null ? <MeterBar ratio={row.ratio} tone="ok" /> : null}
                  <Cap className={`block truncate ${row.segments || row.ratio !== null ? "mt-1.5" : ""}`}>{row.line}</Cap>
                </span>
              </RowLink>
            </li>
          ))}
          {rest > 0 ? (
            <li className="flex justify-center border-t border-[var(--line-soft)] px-3 py-1.5 sm:px-4">
              <Button
                variant="ghost"
                aria-expanded={expanded}
                onClick={() => setExpanded((value) => !value)}
                style={FULL_TOUCH_BUTTON_STYLE}
              >
                {expanded ? tr("접기", "Show less") : tr(`${rest}개 더 보기`, `Show ${rest} more`)}
              </Button>
            </li>
          ) : null}
        </ul>
      )}
      {unreadable && rows.length > 0 ? <CardNote>{tr("캠페인 목록을 다 읽지 못해 일부는 확인 불가예요.", "Not every campaign list loaded, so some campaigns are unknown.")}</CardNote> : null}
    </SectionCard>
  );
}

export function HomeRoutinesCard({ tr, isKo, className }: { tr: Tr; isKo: boolean; className?: string }) {
  const routines = usePolled(loadHomeRoutines, 60_000);
  const now = useNow(60_000);
  const t = useCallback((messages: { ko: string; en?: string }) => (isKo ? messages.ko : messages.en ?? messages.ko), [isKo]);
  const rows = buildTodayRoutines(routines.data ?? [], now, tr);
  const finished = rows.filter((row) => row.finished).length;

  return (
    <SectionCard
      title={tr("오늘 루틴", "Today's routines")}
      aside={routines.data ? (
        rows.length ? (
          <Cap className="font-display tabular-nums">{tr(`${rows.length}개 중 ${finished}개 끝남`, `${finished} of ${rows.length} done`)}</Cap>
        ) : (
          <HeaderLink to="/routines">{tr("루틴 보기", "Routines")}</HeaderLink>
        )
      ) : null}
      testId="home-routines"
      className={className}
    >
      {!routines.data ? (
        routines.failed ? <CardNote>{tr("루틴을 읽지 못했어요.", "Could not read routines.")}</CardNote> : <CardSkeleton label={tr("루틴 불러오는 중", "Loading routines")} />
      ) : rows.length === 0 ? (
        <CardNote>{tr("오늘 도는 루틴이 없어요.", "No routines run today.")}</CardNote>
      ) : (
        <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
          {rows.map((row) => {
            const purpose = describeRoutinePurpose(row.routine, t);
            return (
              <Row as="li" key={row.key} density="sm" data-testid={`home-routine-${row.key}`}>
                <Cap className="w-11 shrink-0 font-display tabular-nums">{formatClock(row.at)}</Cap>
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm leading-5" title={row.routine.name}>{row.routine.name}</span>
                  {purpose !== row.routine.name ? <Cap className="mt-0.5 block truncate" title={purpose}>{purpose}</Cap> : null}
                </span>
                <Chip tone={row.tone}>{row.chip}</Chip>
              </Row>
            );
          })}
        </ul>
      )}
    </SectionCard>
  );
}

const AUTO_PAUSE_SETTINGS = "/settings?category=usage";
const USAGE_METER_STYLE = { "--aui-usage-meter-meter-fill": "var(--accent)" } as CSSProperties;

function usageLabels(tr: Tr, rule: ReturnType<typeof autoPauseRule>): Partial<UsageMeterLabels> {
  const hasPauseBelowLimit = Boolean(rule?.enabled && rule.pct !== null && rule.pct < 100);
  return {
    limits: tr("공급자별 한도", "Limits by provider"),
    limitReached: tr("한도 도달", "Limit reached"),
    nearLimit: tr("한도에 가까움", "Close to the limit"),
    danger: hasPauseBelowLimit ? tr("자동 진행 멈춤 기준 이상", "At or past the auto-run pause") : tr("한도 소진 임박", "Critical limit"),
    notMeasured: tr("측정 안 됨", "Not measured"),
    unlimited: tr("무제한", "Unlimited"),
    announce: (state, title, actual, percent) => state === "limit"
      ? tr(`${title}, 한도 도달. ${actual}.`, `${title}, limit reached. ${actual}.`)
      : state === "warn" ? tr(`${title}, ${percent}% 썼어요.`, `${title}, ${percent}% used.`) : tr(`${title}, ${actual} 썼어요.`, `${title}, ${actual} used.`),
    announceDanger: (title, _actual, percent) => hasPauseBelowLimit
      ? tr(`${title}, 자동 진행 멈춤 기준 이상이에요. ${percent}%.`, `${title}, at or past the auto-run pause. ${percent}%.`)
      : tr(`${title}, 한도 소진이 임박했어요. ${percent}%.`, `${title}, approaching the limit. ${percent}%.`),
  };
}

export function HomeUsageCard({
  tr,
  isKo,
  usageState,
  runtimeState,
  className,
}: {
  tr: Tr;
  isKo: boolean;
  usageState?: Loaded<RawUsageProvider[]>;
  runtimeState?: Loaded<RuntimeConfigResponse>;
  className?: string;
}) {
  const internalUsage = usePolled(loadHomeUsage, 60_000, usageState === undefined);
  const internalRuntime = usePolled(loadRuntimeConfig, 300_000, runtimeState === undefined);
  const usage = usageState ?? internalUsage;
  const runtime = runtimeState ?? internalRuntime;
  const rows = buildUsageRows(usage.data ?? [], isKo);
  const measured = rows.map((row) => row.measuredAt).filter((time): time is number => time !== null);
  const unknown = tr("확인 불가", "Unknown");
  const measuredLabel = (() => {
    if (measured.length === 0) return unknown;
    const latestClock = formatClock(Math.max(...measured));
    const oldestClock = formatClock(Math.min(...measured));
    if (rows.some((row) => row.stale && row.measuredAt !== null) && oldestClock !== latestClock) {
      return tr(`${oldestClock}–${latestClock} 측정`, `measured ${oldestClock}–${latestClock}`);
    }
    return tr(`${latestClock} 측정`, `measured ${latestClock}`);
  })();
  const rule = autoPauseRule(runtime.data);
  const hasRows = Boolean(usage.data && rows.length > 0);
  const ruleText = rule
    ? rule.enabled && rule.pct !== null
      ? tr(`사용량이 ${rule.pct}% 이상이면 자동 진행을 멈춰요`, `Auto-run pauses at ${rule.pct}% usage`)
      : tr("사용량으로 자동 진행을 멈추지 않아요", "Auto-run does not pause for usage")
    : runtime.failed ? tr("자동 진행을 멈추는 기준은 확인 불가예요", "The auto-run pause rule is unknown") : null;

  const pauseFooter = (
    <p className={`m-0 flex flex-wrap items-center gap-x-1.5 text-xs leading-4 text-[var(--fg-muted)] ${hasRows ? "" : "px-3 pb-3.5 sm:px-4"}`}>
      {ruleText ? <span>{ruleText} ·</span> : null}
      <Link
        to={AUTO_PAUSE_SETTINGS}
        className="-my-3.5 -mx-1.5 inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-[var(--r-sm)] px-1.5 font-medium text-[var(--accent-fg)] hover:underline focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]"
      >
        {tr("설정", "Settings")}
      </Link>
    </p>
  );

  return (
    <SectionCard
      title={tr("사용량", "Usage")}
      aside={usage.data || usage.failed ? (
        <Cap className="font-display tabular-nums">
          {tr("한도 추적기", "Limit tracker")} · {measuredLabel}
        </Cap>
      ) : null}
      testId="home-usage"
      className={className}
    >
      {!usage.data ? (
        <>
          {usage.failed ? <CardNote>{tr("사용량은 지금 확인 불가예요.", "Usage is unknown right now.")}</CardNote> : <CardSkeleton label={tr("사용량 불러오는 중", "Loading usage")} />}
          {pauseFooter}
        </>
      ) : rows.length === 0 ? (
        <>
          <CardNote>{tr("측정값이 없어 확인 불가예요.", "No measurements, so usage is unknown.")}</CardNote>
          {pauseFooter}
        </>
      ) : (
        <InspSection className="gap-3 [&_.aui-usage-meter]:max-w-none!" style={USAGE_METER_STYLE}>
          <UsageMeter
            layout="limits"
            title={tr("공급자별 한도", "Limits by provider")}
            locale={isKo ? "ko-KR" : "en-US"}
            limits={rows.map((row) => ({ group: row.group ?? undefined, label: row.label, used: row.pct, total: 100, unit: "%", detail: usageDetail(row, rule, tr) }))}
            {...pauseMeterScale(rule)}
            labels={usageLabels(tr, rule)}
          />
          {pauseFooter}
        </InspSection>
      )}
    </SectionCard>
  );
}
