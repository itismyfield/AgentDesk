import { useEffect, useState } from "react";
import { Link } from "react-router";
import { ArrowUpRight, CheckCircle2 } from "lucide-react";
import * as api from "../api/client";
import type { DashboardStats, KanbanCard } from "../types";
import { HomeWidgetShell } from "./HomeOverviewWidgets";

const QUEUE_PATH = "/campaigns?tab=queue";
const LIST_LIMIT = 5;

function cardTitle(card: KanbanCard): string {
  return card.title.replace(/\\n|\n/g, " ").replace(/\s+/g, " ").trim();
}

/** Work that cannot move forward until a person looks at it. */
export function HomeAttention({ stats, isKo }: { stats: DashboardStats | null; isKo: boolean }) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const [cards, setCards] = useState<KanbanCard[] | null>(null);
  const kanban = stats?.kanban;
  const rows = [
    { key: "review", label: tr("검토 대기", "Waiting for review"), count: kanban?.review_queue ?? 0, tone: "warn" },
    { key: "blocked", label: tr("막힘", "Blocked"), count: kanban?.blocked ?? 0, tone: "danger" },
    { key: "failed", label: tr("실패", "Failed"), count: kanban?.failed ?? 0, tone: "danger" },
    { key: "acceptance", label: tr("수락 대기", "Waiting for acceptance"), count: kanban?.waiting_acceptance ?? 0, tone: "warn" },
    { key: "stale", label: tr("진행 멈춤", "Stalled"), count: kanban?.stale_in_progress ?? 0, tone: "warn" },
  ].filter((row) => row.count > 0);
  const total = rows.reduce((sum, row) => sum + row.count, 0);

  useEffect(() => {
    let active = true;
    Promise.all([
      api.getKanbanCards({ status: "review" }).catch(() => [] as KanbanCard[]),
      api.getKanbanCards({ status: "blocked" }).catch(() => [] as KanbanCard[]),
    ]).then(([review, blocked]) => {
      if (active) setCards([...blocked, ...review].slice(0, LIST_LIMIT));
    });
    return () => {
      active = false;
    };
  }, []);

  return (
    <HomeWidgetShell
      title={tr("확인이 필요해요", "Needs your attention")}
      subtitle={
        total > 0
          ? tr(`사람이 봐야 다음 단계로 넘어가는 작업 ${total}건`, `${total} items waiting on a person`)
          : tr("지금 확인할 일이 없어요.", "Nothing needs you right now.")
      }
      action={
        <Link
          to={QUEUE_PATH}
          className="inline-flex min-h-9 shrink-0 items-center gap-1 rounded-md px-2 text-xs font-medium text-th-text-secondary hover:bg-surface-subtle"
        >
          {tr("대기열", "Queue")}
          <ArrowUpRight size={13} aria-hidden />
        </Link>
      }
    >
      {total === 0 ? (
        <p className="flex items-center gap-2 py-2 text-sm text-th-text-muted">
          <CheckCircle2 size={16} className="text-[var(--th-accent-success)]" aria-hidden />
          {tr("검토·막힘·실패 모두 0건이에요.", "No reviews, blocks or failures.")}
        </p>
      ) : (
        <div data-testid="home-attention" className="space-y-3">
          <div className="flex flex-wrap gap-2">
            {rows.map((row) => (
              <Link
                key={row.key}
                to={QUEUE_PATH}
                className="inline-flex min-h-9 items-center gap-2 rounded-md border border-th-border-subtle px-3 text-sm hover:bg-surface-subtle"
              >
                <span
                  className="h-2 w-2 rounded-full"
                  style={{ background: row.tone === "danger" ? "var(--th-accent-danger)" : "var(--th-accent-warn)" }}
                  aria-hidden
                />
                <span className="text-th-text-secondary">{row.label}</span>
                <span className="font-semibold tabular-nums text-th-text-heading">{row.count.toLocaleString()}</span>
              </Link>
            ))}
          </div>
          {cards && cards.length > 0 ? (
            <ul className="divide-y divide-th-border-subtle">
              {cards.map((card) => (
                <li key={card.id} className="flex min-h-10 items-center gap-2 py-2 text-sm">
                  <span
                    className="shrink-0 rounded px-1.5 py-0.5 text-[11px] font-medium"
                    style={{
                      background: "var(--th-overlay-subtle)",
                      color: card.status === "blocked" ? "var(--th-accent-danger)" : "var(--th-accent-warn)",
                    }}
                  >
                    {card.status === "blocked" ? tr("막힘", "Blocked") : tr("검토", "Review")}
                  </span>
                  {card.github_issue_number ? (
                    <span className="shrink-0 text-xs tabular-nums text-th-text-muted">#{card.github_issue_number}</span>
                  ) : null}
                  <span className="min-w-0 flex-1 truncate text-th-text-primary" title={cardTitle(card)}>
                    {cardTitle(card)}
                  </span>
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      )}
    </HomeWidgetShell>
  );
}
