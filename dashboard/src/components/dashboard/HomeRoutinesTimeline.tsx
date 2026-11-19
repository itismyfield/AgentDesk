import type { RefObject } from "react";
import { CalendarDays, RefreshCw } from "lucide-react";
import { Link } from "react-router";
import { Button } from "../../kit/free/button-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { Loading, Skeleton } from "../../kit/free/skeleton-clean";
import type { TFunction } from "./model";
import { RoutineScheduleList, type RoutineRowView } from "./RoutineScheduleRows";

/** Home card: same schedule rows as /routines (failures first, at most five), in the existing card frame. */
export function HomeRoutinesTimeline({
  t,
  loading,
  error,
  rows,
  listRef,
  onOpen,
  onRefresh,
}: {
  t: TFunction;
  loading: boolean;
  error: boolean;
  rows: RoutineRowView[];
  listRef: RefObject<HTMLUListElement | null>;
  onOpen: (id: string, trigger: HTMLElement) => void;
  onRefresh: () => void;
}) {
  const refreshLabel = t({ ko: "루틴 새로고침", en: "Refresh routines", ja: "ルーチンを再読み込み", zh: "刷新例程" });
  return (
    <section
      className="min-w-0 rounded-lg border border-[var(--line-soft)] bg-[var(--bg-1)] text-[var(--fg)]"
      data-testid="routines-timeline"
      aria-busy={loading}
    >
      <header className="flex items-start justify-between gap-3 border-b border-[var(--line-soft)] px-4 py-4 sm:px-5">
        <div className="min-w-0">
          <h2 className="m-0 text-base font-semibold text-[var(--fg)]">{t({ ko: "루틴", en: "Routines", ja: "ルーチン", zh: "例程" })}</h2>
          <p className="m-0 mt-1 text-xs leading-5 text-[var(--fg-muted)]">
            {t({ ko: "최근 실패를 먼저, 그다음 다음 실행 순", en: "Recent failures first, then next run", ja: "直近の失敗を先に、次に実行予定順", zh: "先显示最近失败，再按下次运行" })}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Link
            to="/routines"
            className="flex min-h-11 shrink-0 items-center rounded-[var(--r-sm)] px-2 text-xs font-medium text-[var(--accent)] focus-visible:outline-2 focus-visible:outline-[var(--accent)]"
          >
            {t({ ko: "전체 보기", en: "View all", ja: "すべて表示", zh: "查看全部" })}
          </Link>
          <Button
            variant="ghost"
            size="sm"
            aria-label={refreshLabel}
            title={refreshLabel}
            icon={<RefreshCw size={15} aria-hidden className={loading ? "motion-safe:animate-spin" : ""} />}
            onClick={onRefresh}
          />
        </div>
      </header>

      <div className="@container/routines px-4 py-2 sm:px-5">
        {error ? (
          <p role="alert" className="m-0 py-8 text-sm text-[var(--fg-muted)]">
            {t({ ko: "루틴 목록을 불러오지 못했어요. 새로고침해 주세요.", en: "Routines could not be loaded. Try refreshing.", ja: "ルーチン一覧を読み込めませんでした。再読み込みしてください。", zh: "无法加载例程列表。请刷新重试。" })}
          </p>
        ) : loading && rows.length === 0 ? (
          <Loading
            label={t({ ko: "루틴 불러오는 중", en: "Loading routines", ja: "ルーチンを読み込み中", zh: "正在加载例程" })}
            data-testid="routines-loading"
            className="flex flex-col gap-3 py-3"
          >
            {[132, 104, 148].map((width) => (
              <span key={width} className="flex items-center gap-3 py-1">
                <Skeleton width={8} height={8} radius={2} />
                <span className="flex min-w-0 flex-1 flex-col gap-1">
                  <Skeleton width={width} height={10} line={20} />
                  <Skeleton width={width + 40} height={8} line={16} />
                </span>
              </span>
            ))}
          </Loading>
        ) : rows.length === 0 ? (
          <EmptyState bare icon={<CalendarDays size={18} aria-hidden />} title={t({ ko: "표시할 루틴이 없어요.", en: "No routines to show.", ja: "表示するルーチンがありません。", zh: "没有可显示的例程。" })} />
        ) : (
          <RoutineScheduleList ref={listRef} rows={rows} compact onOpen={onOpen} />
        )}
      </div>
    </section>
  );
}
