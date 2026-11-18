import { useEffect, useMemo, useRef, useState } from "react";
import { CalendarDays, RefreshCw, Repeat2 } from "lucide-react";
import {
  getDiscordChannelInfo,
  getRoutineRuns,
  getRoutines,
  type RoutineRecord,
  type RoutineRunRecord,
} from "../../api";
import { Button } from "../../kit/free/button-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Loading, Skeleton } from "../../kit/free/skeleton-clean";
import { Drawer } from "../common/overlay/Drawer";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { formatDiscordWebUrl, isDiscordSnowflake } from "../agent-manager/discord-routing";
import type { TFunction } from "./model";
import {
  describeRoutinePurpose,
  describeRoutineSchedule,
  filterLabel,
  formatRelative,
  formatScheduleAbsolute,
  parseTime,
  ROUTINE_FILTERS,
  routineTimelineMarkers,
  runStatusLabel,
  runStatusTone,
  sortRoutinesChronologically,
  statusLabel,
  statusTone,
  type RoutineFilter,
  type RoutineMarker,
} from "./routineModel";
import { HomeRoutinesTimeline } from "./HomeRoutinesTimeline";
import { RoutineDetailBody, RoutineDetailPane, routineDetailTitle } from "./RoutineDetail";
import { RoutineOverlapList, RoutineRunsTimeline } from "./RoutineRunsTimeline";
import { RoutineScheduleList, type RoutineRowView } from "./RoutineScheduleRows";
import { pollWhileVisible } from "../../lib/pollWhileVisible";

export {
  describeRoutinePurpose,
  describeRoutineSchedule,
  sortRoutinesChronologically,
  summarizeRoutineRunResult,
} from "./routineModel";
export type { RoutineRunResultSummary } from "./routineModel";

// All routine threads share one guild, so a single channel lookup serves every row.
let cachedGuildId: string | null = null;

function useDiscordGuildId(sampleChannelId: string | null): string | null {
  const [guildId, setGuildId] = useState(cachedGuildId);
  useEffect(() => {
    if (guildId || !sampleChannelId) return undefined;
    let active = true;
    getDiscordChannelInfo(sampleChannelId)
      .then((info) => {
        if (!active || !info.guild_id) return;
        cachedGuildId = info.guild_id;
        setGuildId(info.guild_id);
      })
      .catch(() => {});
    return () => {
      active = false;
    };
  }, [guildId, sampleChannelId]);
  return guildId;
}

/** Wall clock for relative times and the timeline; ticks every 30s. */
function useNow(intervalMs = 30_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs]);
  return now;
}

interface RoutinesTimelineWidgetProps {
  t: TFunction;
  localeTag: string;
  language: "ko" | "en" | "ja" | "zh";
  /** Home card: no filters, failures first, at most five rows. */
  compact?: boolean;
}

const COMPACT_ROW_LIMIT = 5;

type ReturnTarget = { element: HTMLElement | null; routineId: string | null };

function RoutinesListSkeleton({ t }: { t: TFunction }) {
  return (
    <Loading
      label={t({ ko: "루틴 불러오는 중", en: "Loading routines", ja: "ルーチンを読み込み中", zh: "正在加载例程" })}
      data-testid="routines-loading"
      className="flex min-w-0 flex-col gap-7"
    >
      <div className="flex flex-col gap-2">
        <Skeleton width={64} height={8} line={16} />
        <div className="flex flex-col gap-3 rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-2)] px-4 py-3.5">
          <Skeleton width={96} height={8} line={16} />
          <Skeleton height={116} radius={6} />
          <Skeleton width={220} height={10} line={20} />
        </div>
      </div>
      <ul className="m-0 flex list-none flex-col gap-3 p-0">
        {[132, 104, 148, 116, 124].map((width) => (
          <li key={width} className="flex items-center gap-3 py-2">
            <Skeleton width={8} height={8} radius={2} />
            <span className="flex min-w-0 flex-1 flex-col gap-1">
              <Skeleton width={width} height={10} line={20} />
              <Skeleton width={width + 48} height={8} line={16} />
            </span>
            <Skeleton width={30} height={18} radius={9} />
          </li>
        ))}
      </ul>
    </Loading>
  );
}

export function RoutinesTimelineWidget({ t, localeTag, language, compact = false }: RoutinesTimelineWidgetProps) {
  const [filter, setFilter] = useState<RoutineFilter>("all");
  const [routines, setRoutines] = useState<RoutineRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const [selectedRoutineId, setSelectedRoutineId] = useState<string | null>(null);
  const [overlap, setOverlap] = useState<RoutineMarker[] | null>(null);
  const [selectedRuns, setSelectedRuns] = useState<RoutineRunRecord[]>([]);
  const [runsLoading, setRunsLoading] = useState(false);
  const [runsError, setRunsError] = useState(false);
  const now = useNow();
  const isMobile = useIsMobile();
  const split = !compact && !isMobile;
  const listRef = useRef<HTMLUListElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const returnTarget = useRef<ReturnTarget>({ element: null, routineId: null });
  const pendingFocus = useRef<ReturnTarget | null>(null);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      setError(false);
      setLoading(true);
      try {
        const next = await getRoutines(filter === "all" ? undefined : { status: filter });
        if (mounted) setRoutines(next);
      } catch {
        if (mounted) setError(true);
      } finally {
        if (mounted) setLoading(false);
      }
    };

    void load();
    const stopPolling = pollWhileVisible(() => void load(), 60_000);
    return () => {
      mounted = false;
      stopPolling();
    };
  }, [filter, reloadKey]);

  const sortedRoutines = useMemo(() => {
    const chronological = sortRoutinesChronologically(routines);
    if (!compact) return chronological;
    const failed = (routine: RoutineRecord) => runStatusTone(routine.last_run?.status) === "danger";
    return [...chronological.filter(failed), ...chronological.filter((routine) => !failed(routine))].slice(0, COMPACT_ROW_LIMIT);
  }, [compact, routines]);
  const activeCount = routines.filter((routine) => routine.status === "enabled").length;
  const markers = useMemo(() => routineTimelineMarkers(routines, now), [routines, now]);
  const selectedRoutine = selectedRoutineId ? routines.find((routine) => routine.id === selectedRoutineId) ?? null : null;
  const guildId = useDiscordGuildId(
    routines.find((routine) => isDiscordSnowflake(routine.discord_thread_id))?.discord_thread_id ?? null,
  );
  const threadUrlFor = (routine: RoutineRecord | null): string | null =>
    guildId && routine && isDiscordSnowflake(routine.discord_thread_id)
      ? formatDiscordWebUrl(guildId, routine.discord_thread_id)
      : null;

  useEffect(() => {
    if (!selectedRoutineId) {
      setSelectedRuns([]);
      setRunsError(false);
      setRunsLoading(false);
      return;
    }
    let mounted = true;
    setRunsLoading(true);
    setRunsError(false);
    void getRoutineRuns(selectedRoutineId, 8)
      .then((runs) => {
        if (mounted) setSelectedRuns(runs);
      })
      .catch(() => {
        if (mounted) setRunsError(true);
      })
      .finally(() => {
        if (mounted) setRunsLoading(false);
      });
    return () => {
      mounted = false;
    };
  }, [selectedRoutineId]);

  // After the detail closes, focus goes back to what opened it: the row or marker, else that routine's row,
  // else the list heading. Runs after the sheet's own focus restore so it has the last word.
  const detailOpen = Boolean(selectedRoutine) || Boolean(overlap);
  useEffect(() => {
    if (detailOpen || !pendingFocus.current) return undefined;
    const target = pendingFocus.current;
    pendingFocus.current = null;
    const timer = window.setTimeout(() => {
      const element = target.element?.isConnected && target.element.getClientRects().length ? target.element : null;
      const row = target.routineId
        ? listRef.current?.querySelector<HTMLElement>(`[data-row="${CSS.escape(target.routineId)}"]`)
        : null;
      (element ?? row ?? headingRef.current)?.focus();
    }, 0);
    return () => window.clearTimeout(timer);
  }, [detailOpen]);

  // J/K from the page background (nothing focused) enter the list at the selected row, else the first one.
  useEffect(() => {
    if (compact) return undefined;
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      const key = event.key.toLowerCase();
      if ((key !== "j" && key !== "k") || event.altKey || event.ctrlKey || event.metaKey || event.defaultPrevented) return;
      if (document.activeElement && document.activeElement !== document.body) return;
      const list = listRef.current;
      const first = list?.querySelector<HTMLElement>("[data-row]");
      if (!list || !first) return;
      event.preventDefault();
      const selected = selectedRoutineId ? list.querySelector<HTMLElement>(`[data-row="${CSS.escape(selectedRoutineId)}"]`) : null;
      (selected ?? first).focus();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [compact, selectedRoutineId]);

  const openRoutine = (routineId: string, trigger: HTMLElement) => {
    returnTarget.current = { element: trigger, routineId };
    setOverlap(null);
    setSelectedRoutineId(routineId);
  };
  const openCluster = (group: RoutineMarker[], trigger: HTMLElement) => {
    returnTarget.current = { element: trigger, routineId: null };
    setSelectedRoutineId(null);
    setOverlap(group);
  };
  const closeDetail = () => {
    pendingFocus.current = returnTarget.current;
    setSelectedRoutineId(null);
    setOverlap(null);
  };

  const rows: RoutineRowView[] = sortedRoutines.map((routine) => {
    const enabled = routine.status === "enabled";
    const due = parseTime(routine.next_due_at);
    const title = describeRoutinePurpose(routine, t);
    const schedule = describeRoutineSchedule(routine.schedule, language);
    const nextText = !enabled
      ? statusLabel(routine, t)
      : due != null
        ? formatScheduleAbsolute(due, now, localeTag, t)
        : t({ ko: "예정 없음", en: "Not scheduled", ja: "予定なし", zh: "无计划" });
    const latestStatus = runStatusLabel(routine.last_run?.status, t);
    const nextPhrase = enabled && due != null ? `${t({ ko: "다음", en: "next", ja: "次回", zh: "下次" })} ${nextText}` : nextText;
    return {
      id: routine.id,
      title,
      slug: routine.name,
      schedule,
      agent: routine.agent_id ?? null,
      tone: statusTone(routine),
      paused: !enabled,
      nextText,
      relative: enabled && due != null ? formatRelative(due, localeTag, now) : null,
      latestStatus,
      latestTone: runStatusTone(routine.last_run?.status),
      threadUrl: threadUrlFor(routine),
      enabled,
      label: `${title} (${routine.name}), ${schedule}, ${nextPhrase}${latestStatus ? `, ${latestStatus}` : ""}`,
      threadLabel: t({
        ko: `${routine.name} 디스코드 스레드 열기`,
        en: `Open ${routine.name} Discord thread`,
        ja: `${routine.name} の Discord スレッドを開く`,
        zh: `打开 ${routine.name} 的 Discord 帖子`,
      }),
      switchLabel: t({
        ko: `${routine.name} 자동 실행 ${enabled ? "켜짐" : "꺼짐"}`,
        en: `${routine.name} automatic runs ${enabled ? "on" : "off"}`,
        ja: `${routine.name} 自動実行 ${enabled ? "オン" : "オフ"}`,
        zh: `${routine.name} 自动运行${enabled ? "开启" : "关闭"}`,
      }),
      switchHint: t({
        ko: "대시보드에서는 상태만 보여요",
        en: "The dashboard only shows this state",
        ja: "ダッシュボードでは状態のみ表示します",
        zh: "仪表板仅显示此状态",
      }),
    };
  });

  const detailBody = selectedRoutine ? (
    <RoutineDetailBody
      routine={selectedRoutine}
      threadUrl={threadUrlFor(selectedRoutine)}
      runs={selectedRuns}
      runsLoading={runsLoading}
      runsError={runsError}
      t={t}
      localeTag={localeTag}
      language={language}
      now={now}
    />
  ) : null;

  const drawer = split ? null : (
    <Drawer
      open={detailOpen}
      onClose={closeDetail}
      width="min(560px, 100vw)"
      closeButtonClassName="min-h-11 min-w-11"
      title={
        selectedRoutine
          ? routineDetailTitle(selectedRoutine, t)
          : t({
              ko: `가까운 실행 ${overlap?.length ?? 0}개`,
              en: `${overlap?.length ?? 0} nearby runs`,
              ja: `近い実行 ${overlap?.length ?? 0} 件`,
              zh: `${overlap?.length ?? 0} 次相近运行`,
            })
      }
    >
      {detailBody ??
        (overlap ? (
          <RoutineOverlapList
            markers={overlap}
            now={now}
            localeTag={localeTag}
            t={t}
            onPick={(marker) => {
              setOverlap(null);
              setSelectedRoutineId(marker.routine.id);
              returnTarget.current = { ...returnTarget.current, routineId: marker.routine.id };
            }}
          />
        ) : null)}
    </Drawer>
  );

  if (compact) {
    return (
      <>
        <HomeRoutinesTimeline
          t={t}
          loading={loading}
          error={error}
          rows={rows}
          listRef={listRef}
          onOpen={openRoutine}
          onRefresh={() => setReloadKey((value) => value + 1)}
        />
        {drawer}
      </>
    );
  }

  const refreshLabel = t({ ko: "루틴 새로고침", en: "Refresh routines", ja: "ルーチンを再読み込み", zh: "刷新例程" });

  return (
    <div className="flex min-w-0 flex-col gap-5" data-testid="routines-timeline" aria-busy={loading}>
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="min-w-0 max-w-full flex-auto" data-testid="routines-filter-controls">
          <SegmentedControl
            variant="tray"
            size="sm"
            aria-label={t({ ko: "상태로 거르기", en: "Filter by status", ja: "状態で絞り込む", zh: "按状态筛选" })}
            value={filter}
            onChange={(value) => setFilter(value)}
            options={ROUTINE_FILTERS.map((item) => ({ value: item, label: filterLabel(item, t) }))}
          />
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {loading && routines.length === 0 ? null : (
            <span className="text-xs tabular-nums text-[var(--fg-muted)]" data-testid="routines-active-count">
              {t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" })} {activeCount}/{routines.length}
            </span>
          )}
          <Button
            variant="ghost"
            size="sm"
            aria-label={refreshLabel}
            title={refreshLabel}
            icon={<RefreshCw size={15} aria-hidden className={loading ? "motion-safe:animate-spin" : ""} />}
            onClick={() => setReloadKey((value) => value + 1)}
          />
        </div>
      </div>

      <div
        className={
          split
            ? "grid min-w-0 grid-cols-[minmax(0,1fr)_clamp(300px,36%,420px)] items-start gap-6"
            : "flex min-w-0 flex-col"
        }
      >
        <div className="@container/routines flex min-w-0 flex-col gap-7">
          {error ? (
            <div role="alert" className="flex flex-col items-start gap-2 py-8 text-sm text-[var(--fg-muted)]">
              {t({
                ko: "루틴 목록을 불러오지 못했어요. 새로고침해 주세요.",
                en: "Routines could not be loaded. Try refreshing.",
                ja: "ルーチン一覧を読み込めませんでした。再読み込みしてください。",
                zh: "无法加载例程列表。请刷新重试。",
              })}
              <Button variant="secondary" size="sm" icon={<RefreshCw size={14} aria-hidden />} onClick={() => setReloadKey((value) => value + 1)}>
                {t({ ko: "다시 불러오기", en: "Try again", ja: "再読み込み", zh: "重试" })}
              </Button>
            </div>
          ) : loading && routines.length === 0 ? (
            <RoutinesListSkeleton t={t} />
          ) : routines.length === 0 ? (
            <EmptyState
              data-testid="routines-empty"
              icon={<CalendarDays size={18} aria-hidden />}
              title={t({ ko: "표시할 루틴이 없어요.", en: "No routines to show.", ja: "表示するルーチンがありません。", zh: "没有可显示的例程。" })}
              text={
                filter === "all"
                  ? t({
                      ko: "에이전트에게 정기 작업을 맡기면 이곳에 반복 예약이 나타나요.",
                      en: "Routines appear here once an agent takes on recurring work.",
                      ja: "エージェントに定期作業を任せるとここに表示されます。",
                      zh: "让智能体承担定期工作后，例程会显示在这里。",
                    })
                  : t({ ko: "이 상태의 루틴이 없어요.", en: "No routines have this status.", ja: "この状態のルーチンはありません。", zh: "没有此状态的例程。" })
              }
              action={
                filter === "all" ? undefined : (
                  <Button variant="secondary" size="sm" onClick={() => setFilter("all")}>
                    {filterLabel("all", t)}
                  </Button>
                )
              }
            />
          ) : (
            <>
              <RoutineRunsTimeline
                markers={markers}
                now={now}
                t={t}
                localeTag={localeTag}
                isMobile={isMobile}
                onOpen={openRoutine}
                onOpenCluster={openCluster}
              />
              <section className="flex min-w-0 flex-col gap-2" aria-labelledby="routines-list-heading">
                <h2
                  id="routines-list-heading"
                  ref={headingRef}
                  tabIndex={-1}
                  className="m-0 flex items-center gap-1.5 text-xs font-medium leading-4 text-[var(--fg-muted)] outline-none"
                >
                  <Repeat2 size={13} aria-hidden />
                  {t({ ko: "반복 예약", en: "Recurring schedules", ja: "繰り返し予約", zh: "重复计划" })}
                </h2>
                <RoutineScheduleList ref={listRef} rows={rows} selectedId={split ? selectedRoutineId : null} onOpen={openRoutine} />
              </section>
            </>
          )}
        </div>
        {split ? (
          <div className="sticky top-0 flex max-h-[calc(100dvh-9rem)] min-h-[18rem] min-w-0 flex-col">
            <RoutineDetailPane routine={selectedRoutine} t={t} onClose={closeDetail}>
              {detailBody}
            </RoutineDetailPane>
          </div>
        ) : null}
      </div>
      {drawer}
    </div>
  );
}
