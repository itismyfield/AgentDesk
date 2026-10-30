import { ChevronRight, RefreshCw } from "lucide-react";
import type { TFunction } from "./model";

type RoutineTone = "info" | "success" | "warn" | "neutral" | "danger";

export interface HomeRoutineRow {
  id: string;
  name: string;
  purpose: string;
  status: string;
  tone: RoutineTone;
  latestStatus: string | null;
  latestTone: RoutineTone;
  nextRun: string;
  relative: string;
  schedule: string;
  agent: string | null;
  lastRun: string;
  openLabel: string;
  onOpen: () => void;
}

const toneColors: Record<RoutineTone, string> = {
  info: "bg-[var(--th-accent-info)]",
  success: "bg-[var(--th-accent-success)]",
  warn: "bg-[var(--th-accent-warn)]",
  neutral: "bg-th-text-muted",
  danger: "bg-[var(--th-accent-danger)]",
};

export function HomeRoutinesTimeline({
  t,
  loading,
  error,
  count,
  activeCount,
  nextRun,
  filters,
  rows,
  onRefresh,
}: {
  t: TFunction;
  loading: boolean;
  error: boolean;
  count: number;
  activeCount: number;
  nextRun: string;
  filters: { id: string; label: string; active: boolean; onSelect: () => void }[];
  rows: HomeRoutineRow[];
  onRefresh: () => void;
}) {
  return (
    <section
      className="min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface text-th-text-primary"
      data-testid="routines-timeline"
      aria-busy={loading}
    >
      <header className="flex items-start justify-between gap-3 border-b border-th-border-subtle px-4 py-4 sm:px-5">
        <div className="min-w-0">
          <h2 className="text-base font-semibold text-th-text-heading">
            {t({ ko: "루틴 시간표", en: "Routines Timeline", ja: "ルーチン時系列", zh: "例程时间线" })}
          </h2>
          <p className="mt-1 text-xs leading-5 text-th-text-muted">
            {t({ ko: "다음 실행 시간순", en: "Ordered by next run", ja: "次回実行時刻順", zh: "按下次运行时间排序" })}
          </p>
        </div>
        <button
          type="button"
          onClick={onRefresh}
          className="flex h-11 w-11 shrink-0 items-center justify-center rounded-md border border-th-border-subtle text-th-text-secondary transition-colors hover:bg-surface-hover focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)]"
          aria-label={t({ ko: "루틴 새로고침", en: "Refresh routines", ja: "ルーチンを再読み込み", zh: "刷新例程" })}
        >
          <RefreshCw size={16} className={loading ? "animate-spin" : ""} />
        </button>
      </header>

      <div className="px-4 sm:px-5">
        <div className="flex flex-wrap gap-x-5 gap-y-2 py-3 text-xs text-th-text-muted">
          <span>{t({ ko: "등록", en: "Registered", ja: "登録", zh: "已注册" })} <strong className="ml-1 font-medium tabular-nums text-th-text-primary">{count}</strong></span>
          <span>{t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" })} <strong className="ml-1 font-medium tabular-nums text-th-text-primary">{activeCount}</strong></span>
          <span className="min-w-0 [overflow-wrap:anywhere]">{t({ ko: "다음", en: "Next", ja: "次回", zh: "下次" })} <strong className="ml-1 font-medium tabular-nums text-th-text-primary">{nextRun}</strong></span>
        </div>

        <div className="flex flex-wrap gap-1 border-b border-th-border-subtle" data-testid="routines-filter-controls">
          {filters.map((filter) => (
            <button
              key={filter.id}
              type="button"
              onClick={filter.onSelect}
              aria-pressed={filter.active}
              className={`min-h-11 min-w-11 border-b-2 px-3 text-xs font-medium transition-colors hover:text-th-text-primary focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] ${filter.active ? "border-th-text-primary text-th-text-primary" : "border-transparent text-th-text-muted"}`}
            >
              {filter.label}
            </button>
          ))}
        </div>

        {error ? (
          <p role="alert" className="py-8 text-sm text-th-text-muted">
            {t({ ko: "루틴 목록을 불러오지 못했습니다. 새로고침해 주세요.", en: "Routines could not be loaded. Try refreshing.", ja: "ルーチン一覧を読み込めませんでした。再読み込みしてください。", zh: "无法加载例程列表。请刷新重试。" })}
          </p>
        ) : loading && rows.length === 0 ? (
          <div data-testid="routines-loading" role="status" className="py-8 text-sm text-th-text-muted">
            {t({ ko: "루틴 불러오는 중…", en: "Loading routines…", ja: "ルーチンを読み込み中…", zh: "正在加载例程…" })}
          </div>
        ) : rows.length === 0 ? (
          <p className="py-8 text-sm text-th-text-muted">
            {t({ ko: "표시할 루틴이 없습니다.", en: "No routines to show.", ja: "表示するルーチンがありません。", zh: "没有可显示的例程。" })}
          </p>
        ) : (
          <div className="divide-y divide-th-border-subtle" data-testid="routines-timeline-list">
            {rows.map((row) => (
              <button
                key={row.id}
                type="button"
                data-testid={`routine-row-${row.id}`}
                onClick={row.onOpen}
                aria-label={row.openLabel}
                className="grid min-h-11 w-full min-w-0 gap-3 py-4 text-left transition-colors hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] sm:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_10rem] sm:gap-5"
              >
                <div className="min-w-0">
                  <div className="text-sm font-medium leading-6 text-th-text-heading [overflow-wrap:anywhere]">{row.name}</div>
                  <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs leading-5 text-th-text-muted">
                    <span className="inline-flex items-center gap-1.5"><span className={`h-1.5 w-1.5 shrink-0 rounded-full ${toneColors[row.tone]}`} />{row.status}</span>
                    {row.latestStatus ? <span className="inline-flex items-center gap-1.5"><span className={`h-1.5 w-1.5 shrink-0 rounded-full ${toneColors[row.latestTone]}`} />{row.latestStatus}</span> : null}
                  </div>
                </div>
                <div className="min-w-0">
                  <p className="text-xs leading-5 text-th-text-secondary [overflow-wrap:anywhere]">{row.purpose}</p>
                  <div className="mt-1 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                    {row.schedule}{row.agent ? ` · ${row.agent}` : ""}
                  </div>
                </div>
                <div className="flex min-w-0 items-center justify-between gap-3 sm:items-start">
                  <div className="min-w-0 text-xs leading-5 sm:text-right">
                    <div className="font-medium tabular-nums text-th-text-primary [overflow-wrap:anywhere]">{row.nextRun}</div>
                    <div className="text-th-text-muted [overflow-wrap:anywhere]">{row.relative}</div>
                    <div className="text-th-text-muted [overflow-wrap:anywhere]">{t({ ko: "최근", en: "Last", ja: "前回", zh: "上次" })} {row.lastRun}</div>
                  </div>
                  <ChevronRight size={14} className="mt-1 shrink-0 text-th-text-muted" />
                </div>
              </button>
            ))}
          </div>
        )}
      </div>
    </section>
  );
}
