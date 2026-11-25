import { useEffect, useMemo, useRef, useState } from "react";
import { CalendarDays, MessageSquare, Play, RefreshCw, Repeat2 } from "lucide-react";
import { getDiscordChannelInfo, getRoutineRuns, getRoutines, pauseRoutine, resumeRoutine, type RoutineRecord, type RoutineRunRecord } from "../../api";
import { request } from "../../api/httpClient";
import { Button } from "../../kit/free/button-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Switch } from "../../kit/free/switch-clean";
import { ScheduledRuns, type ScheduleDay, type ScheduledTask } from "../../kit/pro/scheduled-runs";
import { pollWhileVisible } from "../../lib/pollWhileVisible";
import { useNow } from "../../lib/useNow";
import { formatDiscordWebUrl, isDiscordSnowflake } from "../agent-manager/discord-routing";
import { Cap, CardSkeleton, Chip, InspSection, Num, ROW_HOVER_CLASS, SectionCard, SELECTED_ROW_STYLE, StatusDot, type ChipTone, type DotTone } from "../common/KitPrimitives";
import { BottomSheet } from "../common/overlay/BottomSheet";
import { useToast } from "../common/overlay/ToastProvider";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import type { TFunction } from "./model";
import { RoutineDetailBody, RoutineDetailPane, routineDetailTitle } from "./RoutineDetail";
import { describeRoutinePurpose, describeRoutineSchedule, filterLabel, formatRelative, formatScheduleAbsolute, parseTime, ROUTINE_FILTERS, runStatusLabel, runStatusTone, sortRoutinesChronologically, statusLabel, statusTone, type RoutineFilter, type RoutineTone } from "./routineModel";

export { describeRoutinePurpose, describeRoutineSchedule, sortRoutinesChronologically, summarizeRoutineRunResult } from "./routineModel";
export type { RoutineRunResultSummary } from "./routineModel";

let cachedGuildId: string | null = null;
function useDiscordGuildId(sampleChannelId: string | null): string | null {
  const [guildId, setGuildId] = useState(cachedGuildId);
  useEffect(() => {
    if (guildId || !sampleChannelId) return undefined;
    let active = true;
    getDiscordChannelInfo(sampleChannelId).then((info) => { if (active && info.guild_id) { cachedGuildId = info.guild_id; setGuildId(info.guild_id); } }).catch(() => {});
    return () => { active = false; };
  }, [guildId, sampleChannelId]);
  return guildId;
}

const DOT_TONE: Record<RoutineTone, DotTone> = { info: "info", success: "ok", warn: "warn", neutral: "neutral", danger: "err" };
const CHIP_TONE: Record<RoutineTone, ChipTone> = { info: "info", success: "ok", warn: "warn", neutral: "neutral", danger: "err" };
const DAY_LABELS_KO = ["일", "월", "화", "수", "목", "금", "토"];
const DAY_LABELS_EN = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const sessionBand = (h: number) => (h < 9 ? 0 : h < 15 ? 1 : h < 20 ? 2 : 3);

export function buildScheduledRunsModel(routines: RoutineRecord[], now: number, language: "ko" | "en" | "ja" | "zh", t: TFunction) {
  const start = new Date(now); start.setHours(0, 0, 0, 0);
  const baseMs = start.getTime(), weekEndMs = baseMs + 7 * 86_400_000;
  const labels = language === "en" ? DAY_LABELS_EN : DAY_LABELS_KO;
  const days: ScheduleDay[] = Array.from({ length: 7 }, (_, i) => { const d = new Date(baseMs + i * 86_400_000); return { label: labels[d.getDay()], date: `${d.getMonth() + 1}/${d.getDate()}` }; });
  const sessions = language === "en" ? ["00–09", "09–15", "15–20", "20–24"] : ["오전 00–09", "주간 09–15", "오후 15–20", "야간 20–24"];
  const current = { day: 0, session: sessionBand(new Date(now).getHours()) };
  const cellCounts = new Map<string, number>();
  const tasks: ScheduledTask[] = sortRoutinesChronologically(routines).filter((r) => r.status === "enabled").slice(0, 18).map((routine) => {
    const due = parseTime(routine.next_due_at);
    let reserved: { day: number; session: number } | undefined;
    if (due != null && due >= baseMs && due < weekEndMs) {
      const day = Math.floor((due - baseMs) / 86_400_000), session = sessionBand(new Date(due).getHours());
      const key = `${day}:${session}`, used = cellCounts.get(key) ?? 0;
      if (used < 2 && (day > 0 || session >= current.session)) { cellCounts.set(key, used + 1); reserved = { day, session }; }
    }
    return { id: routine.id, title: describeRoutinePurpose(routine, t), usage: Math.min(45, Math.max(12, Math.round((routine.timeout_secs ?? 300) / 20))), agent: routine.agent_id ?? undefined, description: `${routine.name} · ${describeRoutineSchedule(routine.schedule, language)}`, reserved };
  });
  return { days, sessions, current, tasks };
}

type ReturnTarget = { element: HTMLElement | null; routineId: string | null };

export function RoutinesTimelineWidget({ t, localeTag, language }: { t: TFunction; localeTag: string; language: "ko" | "en" | "ja" | "zh" }) {
  const m = (ko: string, en: string, ja = en, zh = en) => t({ ko, en, ja, zh });
  const [filter, setFilter] = useState<RoutineFilter>("all");
  const [routines, setRoutines] = useState<RoutineRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  const [refreshFailed, setRefreshFailed] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const [selectedRoutineId, setSelectedRoutineId] = useState<string | null>(null);
  const [selectedRuns, setSelectedRuns] = useState<RoutineRunRecord[]>([]);
  const [runsLoading, setRunsLoading] = useState(false);
  const [runsError, setRunsError] = useState(false);
  const [pendingIds, setPendingIds] = useState<ReadonlySet<string>>(() => new Set());
  const [runningIds, setRunningIds] = useState<ReadonlySet<string>>(() => new Set());
  const now = useNow(30_000);
  const optimistic = useRef(new Map<string, string>());
  const listRef = useRef<HTMLUListElement>(null);
  const routinesRef = useRef<RoutineRecord[]>([]);
  routinesRef.current = routines;
  const filterRef = useRef<RoutineFilter | null>(null);
  const returnTarget = useRef<ReturnTarget>({ element: null, routineId: null });
  const pendingFocus = useRef<ReturnTarget | null>(null);
  const isMobile = useIsMobile(), split = !isMobile, toast = useToast();

  useEffect(() => {
    let mounted = true;
    let generation = 0;
    const replace = filterRef.current !== filter;
    filterRef.current = filter;
    const load = async (quiet: boolean) => {
      const requestId = ++generation;
      if (!quiet) {
        setError(false);
        setRefreshFailed(false);
        setLoading(true);
        if (replace) {
          routinesRef.current = [];
          setRoutines([]);
        }
      }
      const keepRows = routinesRef.current.length > 0;
      try {
        const next = await getRoutines(filter === "all" ? undefined : { status: filter });
        if (!mounted || requestId !== generation) return;
        const p = optimistic.current;
        setRoutines(p.size ? next.map((item) => (p.has(item.id) ? { ...item, status: p.get(item.id)! } : item)) : next);
        setError(false);
        setRefreshFailed(false);
      } catch {
        if (!mounted || requestId !== generation) return;
        if (keepRows) setRefreshFailed(true);
        else setError(true);
      } finally {
        if (mounted && requestId === generation) setLoading(false);
      }
    };
    void load(false);
    const stop = pollWhileVisible(() => void load(true), 60_000);
    return () => { mounted = false; stop(); };
  }, [filter, reloadKey]);

  const sortedRoutines = useMemo(() => sortRoutinesChronologically(routines), [routines]);
  const activeCount = routines.filter((r) => r.status === "enabled").length;
  const selectedRoutine = selectedRoutineId ? routines.find((r) => r.id === selectedRoutineId) ?? null : null;
  const scheduleBoard = useMemo(() => buildScheduledRunsModel(routines, now, language, t), [language, now, routines, t]);
  const guildId = useDiscordGuildId(routines.find((r) => isDiscordSnowflake(r.discord_thread_id))?.discord_thread_id ?? null);
  const threadUrlFor = (r: RoutineRecord | null) => guildId && r && isDiscordSnowflake(r.discord_thread_id) ? formatDiscordWebUrl(guildId, r.discord_thread_id) : null;

  useEffect(() => {
    if (!selectedRoutineId) { setSelectedRuns([]); setRunsError(false); setRunsLoading(false); return; }
    let mounted = true; setRunsLoading(true); setRunsError(false);
    void getRoutineRuns(selectedRoutineId, 8).then((runs) => { if (mounted) setSelectedRuns(runs); }).catch(() => { if (mounted) setRunsError(true); }).finally(() => { if (mounted) setRunsLoading(false); });
    return () => { mounted = false; };
  }, [selectedRoutineId, reloadKey]);

  const detailOpen = Boolean(selectedRoutine);
  useEffect(() => {
    if (detailOpen || !pendingFocus.current) return undefined;
    const target = pendingFocus.current;
    pendingFocus.current = null;
    const timer = window.setTimeout(() => {
      const el = target.element?.isConnected && target.element.getClientRects().length ? target.element : null;
      const row = target.routineId ? listRef.current?.querySelector<HTMLElement>(`[data-row="${CSS.escape(target.routineId)}"]`) : null;
      (el ?? row)?.focus();
    }, 0);
    return () => window.clearTimeout(timer);
  }, [detailOpen]);

  useEffect(() => {
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      const key = event.key.toLowerCase();
      if ((key !== "j" && key !== "k") || event.altKey || event.ctrlKey || event.metaKey || event.defaultPrevented) return;
      if (document.activeElement && document.activeElement !== document.body) return;
      const list = listRef.current, first = list?.querySelector<HTMLElement>("[data-row]");
      if (!list || !first) return;
      event.preventDefault();
      const selected = selectedRoutineId ? list.querySelector<HTMLElement>(`[data-row="${CSS.escape(selectedRoutineId)}"]`) : null;
      (selected ?? first).focus();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selectedRoutineId]);

  const openRoutine = (routineId: string, trigger: HTMLElement) => {
    returnTarget.current = { element: trigger, routineId };
    setSelectedRoutineId(routineId);
  };
  const closeDetail = () => {
    pendingFocus.current = returnTarget.current;
    setSelectedRoutineId(null);
  };

  const toggleRoutine = async (routineId: string) => {
    const routine = routines.find((item) => item.id === routineId);
    if (!routine || pendingIds.has(routineId) || (routine.status !== "enabled" && routine.status !== "paused")) return;
    const previous = routine.status, pausing = previous === "enabled", next = pausing ? "paused" : "enabled";
    optimistic.current.set(routineId, next);
    setRoutines((list) => list.map((item) => (item.id === routineId ? { ...item, status: next } : item)));
    setPendingIds((cur) => new Set(cur).add(routineId));
    try {
      await (pausing ? pauseRoutine(routineId) : resumeRoutine(routineId));
      optimistic.current.delete(routineId);
      setReloadKey((v) => v + 1);
    } catch (err) {
      optimistic.current.delete(routineId);
      setRoutines((list) => list.map((item) => (item.id === routineId ? { ...item, status: previous } : item)));
      const reason = err instanceof Error ? err.message : String(err);
      toast.show({ variant: "error", message: pausing ? m(`${routine.name} 일시정지하지 못했어요: ${reason}`, `Could not pause ${routine.name}: ${reason}`) : m(`${routine.name} 다시 켜지 못했어요: ${reason}`, `Could not resume ${routine.name}: ${reason}`) });
    } finally { setPendingIds((cur) => { const c = new Set(cur); c.delete(routineId); return c; }); }
  };

  const runNow = async (routine: RoutineRecord) => {
    if (runningIds.has(routine.id) || routine.status !== "enabled" || Boolean(routine.in_flight_run_id)) return;
    setRunningIds((cur) => new Set(cur).add(routine.id));
    try {
      const res = await request<{ outcome?: { status?: string; error?: string | null } }>(`/api/routines/${encodeURIComponent(routine.id)}/run-now`, { method: "POST", body: "{}", timeoutMs: 60_000, maxRetries: 0, suppressErrorToast: true });
      if (res?.outcome?.status === "failed") {
        const detail = res.outcome.error ? `: ${res.outcome.error}` : "";
        toast.show({ variant: "warning", message: m(`${routine.name} 실행이 실패로 끝났어요${detail}`, `${routine.name} finished with failure${detail}`) });
      } else {
        toast.show({ variant: "success", message: m(`${routine.name} 루틴을 바로 실행했어요.`, `Triggered ${routine.name} now.`) });
      }
      setReloadKey((v) => v + 1);
    } catch (err) {
      toast.show({ variant: "error", message: m(`${routine.name} 즉시 실행 실패: ${err instanceof Error ? err.message : String(err)}`, `Could not run ${routine.name}: ${String(err)}`) });
    } finally { setRunningIds((cur) => { const c = new Set(cur); c.delete(routine.id); return c; }); }
  };

  const detailContent = selectedRoutine ? (
    <div className="flex min-w-0 flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-2 rounded-[var(--r-sm)] border border-[var(--line-soft)] bg-[var(--bg-1)] px-3 py-2.5">
        <Cap className="[word-break:keep-all]">{selectedRoutine.status === "enabled" ? m("예약 시각 전이라도 지금 바로 1회 실행할 수 있어요.", "Run this routine immediately outside its schedule.") : m("활성 상태인 루틴만 즉시 실행할 수 있어요.", "Enable this routine before running it now.")}</Cap>
        <Button size="sm" variant="primary" icon={<Play size={14} />} loading={runningIds.has(selectedRoutine.id)} disabled={selectedRoutine.status !== "enabled" || Boolean(selectedRoutine.in_flight_run_id) || runningIds.has(selectedRoutine.id)} data-testid="routine-detail-run-now" onClick={() => void runNow(selectedRoutine)}>
          {m("지금 실행", "Run now", "今すぐ実行", "立即运行")}
        </Button>
      </div>
      <RoutineDetailBody routine={selectedRoutine} threadUrl={threadUrlFor(selectedRoutine)} runs={selectedRuns} runsLoading={runsLoading} runsError={runsError} t={t} localeTag={localeTag} language={language} now={now} />
    </div>
  ) : null;

  const refreshLabel = m("루틴 새로고침", "Refresh routines", "ルーチンを再読み込み", "刷新例程");

  return (
    <div className="flex min-w-0 flex-col gap-4" data-testid="routines-timeline" aria-busy={loading}>
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="min-w-0 max-w-full flex-auto" data-testid="routines-filter-controls">
          <SegmentedControl variant="tray" size="sm" aria-label={m("상태로 거르기", "Filter by status", "状態で絞り込む", "按状态筛选")} value={filter} onChange={(v) => setFilter(v)} options={ROUTINE_FILTERS.map((item) => ({ value: item, label: filterLabel(item, t) }))} />
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {loading && routines.length === 0 ? null : <Cap className="font-display tabular-nums" data-testid="routines-active-count">{m("활성", "Active", "有効", "活跃")} {activeCount}/{routines.length}</Cap>}
          <Button variant="secondary" size="sm" aria-label={refreshLabel} title={refreshLabel} icon={<RefreshCw size={14} aria-hidden className={loading ? "motion-safe:animate-spin" : ""} />} onClick={() => setReloadKey((v) => v + 1)}>
            {m("새로고침", "Refresh", "更新", "刷新")}
          </Button>
        </div>
      </div>

      {refreshFailed && !error ? (
        <p role="alert" data-testid="routines-refresh-error" className="m-0 text-sm text-[var(--fg-muted)]">
          {m("목록을 새로 읽지 못했어요. 마지막 목록을 보여 드려요.", "Could not refresh the list. Showing the last one.", "一覧を更新できませんでした。最後の一覧を表示しています。", "无法刷新列表，仍显示上次的列表。")}
        </p>
      ) : null}
      {error ? (
        <SectionCard title={m("루틴", "Routines")}>
          <InspSection className="items-start gap-3">
            <p role="alert" className="m-0 text-sm text-[var(--fg-muted)]">{m("루틴 목록을 불러오지 못했어요. 새로고침해 주세요.", "Routines could not be loaded. Try refreshing.")}</p>
            <Button variant="secondary" size="sm" icon={<RefreshCw size={14} aria-hidden />} onClick={() => setReloadKey((v) => v + 1)}>{m("다시 불러오기", "Try again")}</Button>
          </InspSection>
        </SectionCard>
      ) : loading && routines.length === 0 ? (
        <SectionCard title={m("루틴 불러오는 중", "Loading routines")} testId="routines-loading"><CardSkeleton label={m("루틴 불러오는 중", "Loading routines")} rows={5} /></SectionCard>
      ) : routines.length === 0 ? (
        <EmptyState data-testid="routines-empty" icon={<CalendarDays size={18} aria-hidden />} title={m("표시할 루틴이 없어요.", "No routines to show.")} text={filter === "all" ? m("에이전트에게 정기 작업을 맡기면 이곳에 반복 예약이 나타나요.", "Routines appear here once an agent takes on recurring work.") : m("이 상태의 루틴이 없어요.", "No routines have this status.")} action={filter === "all" ? undefined : <Button variant="secondary" size="sm" onClick={() => setFilter("all")}>{filterLabel("all", t)}</Button>} />
      ) : (
        <>
          <SectionCard title={m("주간·시간대별 예약 배치", "Weekly schedule slots")} count={scheduleBoard.tasks.length} testId="routines-scheduled-runs">
            <InspSection
              className="[&_.aui-scheduled-runs]:max-w-none! [&_.aui-scheduled-runs-modes]:hidden! [&_.aui-scheduled-runs-tapmove]:hidden! [&_.aui-scheduled-runs-detail-hint]:hidden! [&_.aui-scheduled-runs-card]:cursor-default!"
              onPointerDownCapture={(e) => { if ((e.target as HTMLElement).closest(".aui-scheduled-runs-card")) e.stopPropagation(); }}
              onKeyDownCapture={(e) => { if ((e.key === " " || e.key === "Enter") && (e.target as HTMLElement).closest(".aui-scheduled-runs-card")) e.stopPropagation(); }}
            >
              <ScheduledRuns title={m("7일 실행 배치표", "7-day routine board")} subtitle={m("읽기 전용 · 시간대별 활성 루틴 밀도와 다음 예정 슬롯을 보여 줘요", "Read-only · active routines mapped onto upcoming 7-day time bands")} tasks={scheduleBoard.tasks} days={scheduleBoard.days} sessions={scheduleBoard.sessions} current={scheduleBoard.current} defaultMode="manual" autoStart={false} />
            </InspSection>
          </SectionCard>

          <div className="@container/routines grid min-w-0 grid-cols-1 items-start gap-4 min-[900px]:grid-cols-[minmax(0,1fr)_clamp(320px,36%,430px)]">
            <SectionCard title={m("반복 예약 목록", "Recurring schedules")} count={sortedRoutines.length} aside={<Repeat2 size={14} aria-hidden className="ml-auto text-[var(--fg-muted)]" />}>
              <ul ref={listRef} className="m-0 list-none divide-y divide-[var(--line-soft)] border-t border-[var(--line-soft)] p-0" data-testid="routines-timeline-list" onKeyDown={(event) => {
                const lower = event.key.toLowerCase(), down = event.key === "ArrowDown" || lower === "j", up = event.key === "ArrowUp" || lower === "k";
                if ((!down && !up) || event.altKey || event.ctrlKey || event.metaKey) return;
                const buttons = Array.from(event.currentTarget.querySelectorAll<HTMLElement>("[data-row]")), idx = buttons.indexOf(event.target as HTMLElement);
                if (idx >= 0) { event.preventDefault(); buttons[Math.max(0, Math.min(buttons.length - 1, idx + (down ? 1 : -1)))]?.focus(); }
              }}>
                {sortedRoutines.map((routine) => {
                  const enabled = routine.status === "enabled", selected = split && selectedRoutineId === routine.id, due = parseTime(routine.next_due_at);
                  const title = describeRoutinePurpose(routine, t), schedule = describeRoutineSchedule(routine.schedule, language);
                  const nextText = !enabled ? statusLabel(routine, t) : due != null ? formatScheduleAbsolute(due, now, localeTag, t) : m("예정 없음", "Not scheduled");
                  const relative = enabled && due != null ? formatRelative(due, localeTag, now) : null, latest = runStatusLabel(routine.last_run?.status, t), threadUrl = threadUrlFor(routine);
                  const nextPhrase = enabled && due != null ? `${m("다음", "next", "次回", "下次")} ${nextText}` : nextText;
                  const rowLabel = `${title} (${routine.name}), ${schedule}, ${nextPhrase}${latest ? `, ${latest}` : ""}`;
                  const runningNow = runningIds.has(routine.id) || Boolean(routine.in_flight_run_id), switchLocked = routine.status !== "enabled" && routine.status !== "paused", switchPending = pendingIds.has(routine.id);
                  const switchLabel = routine.status === "detached" ? m(`${routine.name} 자동 실행 · 분리된 루틴이라 켜고 끌 수 없어요`, `${routine.name} automatic runs · detached routines cannot be switched`) : m(`${routine.name} 자동 실행`, `${routine.name} automatic runs`);
                  return (
                    <li key={routine.id} data-selected={selected || undefined} className={`flex min-w-0 flex-wrap items-center gap-2 px-3 py-2.5 sm:px-4 ${ROW_HOVER_CLASS}`} style={selected ? SELECTED_ROW_STYLE : undefined}>
                      <button type="button" data-row={routine.id} data-testid={`routine-row-${routine.id}`} aria-current={selected ? "true" : undefined} aria-label={rowLabel} onClick={(e) => openRoutine(routine.id, e.currentTarget)} className="flex min-h-11 min-w-[12rem] flex-1 items-center gap-3 border-0 bg-transparent p-0 text-left focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--accent)]">
                        <StatusDot tone={DOT_TONE[statusTone(routine)]} />
                        <span className="flex min-w-0 flex-1 flex-col">
                          <strong className={`truncate text-[13px] leading-5 font-medium ${enabled ? "text-[var(--fg)]" : "text-[var(--fg-muted)]"}`}>{title}</strong>
                          <Cap className="truncate"><span className="font-mono text-[11px]">{routine.name}</span> · {schedule}{routine.agent_id ? ` · ${routine.agent_id}` : ""}</Cap>
                          <span className="hidden text-xs leading-4 tabular-nums text-[var(--fg-dim)] max-sm:block">{nextText}</span>
                        </span>
                        <span className="flex shrink-0 flex-col items-end text-xs max-sm:hidden">
                          <Num className={enabled ? "text-[var(--fg)]" : "text-[var(--fg-muted)]"}>{nextText}</Num>
                          {relative ? <Cap>{relative}</Cap> : null}
                        </span>
                        {latest ? <Chip tone={CHIP_TONE[runStatusTone(routine.last_run?.status)]} className="inline-flex max-sm:hidden">{latest}</Chip> : null}
                      </button>
                      <div className="ml-auto flex shrink-0 items-center gap-1.5">
                        <Button size="sm" variant="secondary" icon={<Play size={13} />} loading={runningIds.has(routine.id)} disabled={!enabled || runningNow} data-testid={`routine-run-now-${routine.id}`} aria-label={m(`${routine.name} 지금 실행`, `Run ${routine.name} now`)} onClick={() => void runNow(routine)}>
                          {m("지금 실행", "Run now")}
                        </Button>
                        {threadUrl ? <a href={threadUrl} target="_blank" rel="noreferrer" data-testid={`routine-thread-${routine.id}`} aria-label={m(`${routine.name} 디스코드 스레드 열기`, `Open ${routine.name} Discord thread`)} className="inline-flex size-11 shrink-0 items-center justify-center rounded-[var(--r-sm)] text-[var(--fg-muted)] hover:text-[var(--fg)]"><MessageSquare size={16} aria-hidden /></a> : null}
                        <span className="inline-flex size-11 shrink-0 items-center justify-center">
                          <Switch size="sm" checked={enabled} disabled={switchLocked || switchPending} aria-busy={switchPending || undefined} aria-label={switchLabel} data-testid={`routine-switch-${routine.id}`} onChange={() => void toggleRoutine(routine.id)} />
                        </span>
                      </div>
                    </li>
                  );
                })}
              </ul>
            </SectionCard>
            {split ? <div className="sticky top-0 flex max-h-[calc(100dvh-7rem)] min-h-[18rem] min-w-0 flex-col"><RoutineDetailPane routine={selectedRoutine} t={t} onClose={closeDetail}>{detailContent}</RoutineDetailPane></div> : null}
          </div>
        </>
      )}
      {!split ? <BottomSheet open={Boolean(selectedRoutine)} onClose={closeDetail} title={routineDetailTitle(selectedRoutine, t)}>{detailContent}</BottomSheet> : null}
    </div>
  );
}
