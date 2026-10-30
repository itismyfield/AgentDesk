import { useState } from "react";
import { Link } from "react-router";
import { ArrowUpRight, ListFilter } from "lucide-react";
import type { KanbanCard } from "../types";
import { BOARD_COLUMN_DEFS, getBoardColumnStatus } from "../components/agent-manager/kanban-utils";

const STATUS_ORDER = ["failed", "review", "qa_in_progress", "qa_pending", "in_progress", "requested", "backlog", "done"];

export function HomeWorkSnapshot({
  cards,
  recentDoneCards,
  isKo,
}: {
  cards: KanbanCard[];
  recentDoneCards: KanbanCard[];
  isKo: boolean;
}) {
  const [filter, setFilter] = useState("all");
  const tr = (ko: string, en: string) => isKo ? ko : en;
  const labels: Record<string, string> = Object.fromEntries(
    BOARD_COLUMN_DEFS.map(column => [column.status, isKo ? column.labelKo : column.labelEn]),
  );
  const current = [
    ...cards.filter(card => card.status !== "done" && card.status !== "cancelled"),
    ...recentDoneCards,
  ].sort((a, b) => {
    const rank = (card: KanbanCard) => {
      const index = STATUS_ORDER.indexOf(getBoardColumnStatus(card.status));
      return index === -1 ? 2 : index;
    };
    return rank(a) - rank(b);
  });
  const filtered = filter === "all"
    ? current
    : current.filter(card => getBoardColumnStatus(card.status) === filter);
  const preview = filtered.slice(0, 6);
  const attentionCount = current.filter(card => getBoardColumnStatus(card.status) === "failed").length;

  return (
    <section className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface text-th-text-primary" aria-labelledby="home-work-title">
      <header className="flex flex-wrap items-start justify-between gap-3 border-b border-th-border-subtle px-4 py-4 sm:px-5">
        <div>
          <h2 id="home-work-title" className="text-base font-semibold">{tr("작업 현황", "Work overview")}</h2>
          <p className="mt-1 text-xs leading-5 text-th-text-muted">
            {tr("현재 작업과 최근 24시간 완료", "Current work and completions in the last 24 hours")}
            {attentionCount > 0 ? <span className="ml-2 font-medium text-[var(--th-badge-amber-text)]">{tr(`확인 필요 ${attentionCount}건`, `${attentionCount} need attention`)}</span> : null}
          </p>
        </div>
        <Link to="/kanban" className="inline-flex min-h-11 shrink-0 items-center gap-2 rounded-md border border-th-border-subtle px-3 text-xs font-medium transition-colors hover:bg-surface-hover focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)]">
          {tr("칸반 열기", "Open kanban")}<ArrowUpRight size={14} />
        </Link>
      </header>
      <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-3 sm:px-5">
        <span className="text-xs text-th-text-muted" aria-live="polite">{tr(`전체 ${current.length}건`, `${current.length} total`)}{filter !== "all" ? tr(` · 선택 ${filtered.length}건`, ` · ${filtered.length} matching`) : ""}</span>
        <label className="flex items-center gap-2 text-xs text-th-text-secondary">
          <ListFilter size={14} aria-hidden="true" />
          <select aria-label={tr("작업 상태", "Work status")} value={filter} onChange={event => setFilter(event.target.value)} className="h-11 max-w-full rounded-md border border-th-border-subtle bg-th-bg-surface px-2 text-xs text-th-text-primary focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)]">
            <option value="all">{tr("전체 상태", "All statuses")}</option>
            {STATUS_ORDER.map(status => <option key={status} value={status}>{labels[status]}</option>)}
          </select>
        </label>
      </div>
      <div className="px-4 sm:px-5">
        <div className="hidden grid-cols-[6rem_minmax(0,1fr)_10rem_4rem] gap-x-4 border-b border-th-border-subtle pb-2 text-xs text-th-text-muted sm:grid" aria-hidden="true">
          <span>{tr("상태", "Status")}</span><span>{tr("작업", "Task")}</span><span>{tr("저장소", "Repository")}</span><span className="text-right">{tr("이슈", "Issue")}</span>
        </div>
        {preview.length === 0 ? (
          <div className="border-t border-th-border-subtle py-10 text-center sm:border-t-0" role="status">
            <p className="text-sm font-medium">{tr("표시할 작업이 없습니다", "No work to show")}</p>
            <p className="mt-2 text-xs leading-5 text-th-text-muted">{tr("다른 상태를 선택하거나 칸반에서 전체 작업을 확인하세요.", "Choose another status or open kanban to see all work.")}</p>
          </div>
        ) : (
          <ul className="divide-y divide-th-border-subtle border-t border-th-border-subtle sm:border-t-0">
            {preview.map(card => {
              const status = getBoardColumnStatus(card.status);
              return (
                <li key={card.id} className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] items-start gap-x-4 gap-y-1.5 py-3 sm:grid-cols-[6rem_minmax(0,1fr)_10rem_4rem] sm:py-3.5" data-testid="home-work-row">
                  <span className="order-1 inline-flex items-center gap-2 text-xs leading-6 text-th-text-secondary">
                    <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${status === "failed" ? "bg-[var(--th-accent-warn)]" : status === "in_progress" ? "bg-[var(--th-accent-info)]" : "bg-th-text-muted"}`} aria-hidden="true" />
                    {labels[status] ?? card.status}
                  </span>
                  <span className="order-3 col-span-2 min-w-0 text-sm font-medium leading-6 [overflow-wrap:anywhere] sm:order-2 sm:col-span-1">{card.title}</span>
                  <span className="order-4 col-span-2 min-w-0 text-xs leading-6 text-th-text-muted [overflow-wrap:anywhere] sm:order-3 sm:col-span-1">{card.github_repo ?? tr("저장소 미지정", "No repository")}</span>
                  <span className="order-2 text-right font-mono text-xs leading-6 tabular-nums text-th-text-muted sm:order-4">{card.github_issue_number != null ? `#${card.github_issue_number}` : "—"}</span>
                </li>
              );
            })}
          </ul>
        )}
      </div>
      <footer className="border-t border-th-border-subtle px-4 py-3 text-xs text-th-text-muted sm:px-5">
        {tr(`${filtered.length}건 중 ${preview.length}건 표시 · 완료는 최근 24시간 기준`, `Showing ${preview.length} of ${filtered.length} · Completions from the last 24 hours`)}
      </footer>
    </section>
  );
}
