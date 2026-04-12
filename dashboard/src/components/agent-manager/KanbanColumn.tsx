import type { GitHubIssue } from "../../api";
import type {
  KanbanCard,
  KanbanCardStatus,
} from "../../types";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface ColumnDef {
  status: KanbanCardStatus;
  labelKo: string;
  labelEn: string;
  accent: string;
}

export interface KanbanColumnProps {
  column: ColumnDef;
  columnCards: KanbanCard[];
  backlogIssues: GitHubIssue[];
  backlogCount: number;
  tr: (ko: string, en: string) => string;
  compactBoard: boolean;
  initialLoading: boolean;
  loadingIssues: boolean;
  onCardClick: (cardId: string) => void;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export default function KanbanColumn({
  column,
  columnCards,
  backlogIssues,
  backlogCount,
  tr,
  compactBoard,
  initialLoading,
  loadingIssues,
  onCardClick,
  onBacklogIssueClick,
}: KanbanColumnProps) {
  return (
    <section
      className={`${compactBoard ? "w-full" : "w-full"} min-w-0 rounded-2xl border p-3 space-y-3`}
      style={{
        borderColor: "rgba(148,163,184,0.24)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      {/* Column header */}
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <span className="w-2.5 h-2.5 rounded-full" style={{ backgroundColor: column.accent }} />
          <h3 className="font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {tr(column.labelKo, column.labelEn)}
          </h3>
        </div>
        <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
          {(initialLoading || (column.status === "backlog" && loadingIssues)) ? "…" : backlogCount}
        </span>
      </div>

      {/* Card list */}
      <div className="space-y-2 min-h-12">
        {column.status === "backlog" && loadingIssues && (
          <div className="rounded-xl border border-dashed px-3 py-4 text-xs text-center" style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-muted)" }}>
            {tr("GitHub backlog 로딩 중...", "Loading GitHub backlog...")}
          </div>
        )}

        {/* Backlog issues */}
        {column.status === "backlog" && backlogIssues.map((issue) => (
          <BacklogIssueCard
            key={`issue-${issue.number}`}
            issue={issue}
            onBacklogIssueClick={onBacklogIssueClick}
          />
        ))}

        {/* Empty state */}
        {backlogCount === 0 && !initialLoading && !(column.status === "backlog" && loadingIssues) && (
          <div className="rounded-xl border border-dashed px-3 py-4 text-xs text-center" style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-muted)" }}>
            {column.status === "backlog"
              ? tr("repo backlog가 비어 있습니다.", "This repo backlog is empty.")
              : tr("카드가 없습니다.", "No cards in this column.")}
          </div>
        )}

        {/* Kanban cards */}
        {columnCards.map((card) => (
          <KanbanCardArticle
            key={card.id}
            card={card}
            onCardClick={onCardClick}
          />
        ))}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

interface BacklogIssueCardProps {
  issue: GitHubIssue;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
  metaBadge?: string;
}

export function BacklogIssueCard({
  issue,
  onBacklogIssueClick,
  metaBadge,
}: BacklogIssueCardProps) {
  return (
    <article
      className="rounded-2xl border p-3 cursor-pointer transition-colors hover:border-[rgba(148,163,184,0.4)]"
      style={{ borderColor: "rgba(148,163,184,0.2)", backgroundColor: "var(--th-card-bg)" }}
      onClick={() => onBacklogIssueClick(issue)}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5 text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
            <span>#{issue.number}</span>
            {metaBadge && (
              <span
                className="rounded-full px-1.5 py-0.5"
                style={{ backgroundColor: "rgba(96,165,250,0.16)", color: "#93c5fd" }}
              >
                {metaBadge}
              </span>
            )}
          </div>
          <h4 className="mt-1 text-sm font-semibold leading-snug" style={{ color: "var(--th-text-heading)" }}>
            {issue.title}
          </h4>
        </div>
        <a
          href={issue.url}
          target="_blank"
          rel="noreferrer"
          className="text-xs hover:underline"
          style={{ color: "#93c5fd" }}
          onClick={(event) => event.stopPropagation()}
        >
          GH
        </a>
      </div>
    </article>
  );
}

interface KanbanCardArticleProps {
  card: KanbanCard;
  onCardClick: (cardId: string) => void;
  metaBadge?: string;
}

export function KanbanCardArticle({
  card,
  onCardClick,
  metaBadge,
}: KanbanCardArticleProps) {
  const cardNumber = card.github_issue_number ? `#${card.github_issue_number}` : `#${card.id.slice(0, 6)}`;

  return (
    <article
      onClick={() => onCardClick(card.id)}
      className="rounded-2xl border p-3 cursor-pointer transition-colors hover:border-[rgba(148,163,184,0.4)]"
      style={{
        borderColor: "rgba(148,163,184,0.2)",
        backgroundColor: "var(--th-card-bg)",
      }}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5 text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
            <span>{cardNumber}</span>
            {metaBadge && (
              <span
                className="rounded-full px-1.5 py-0.5"
                style={{ backgroundColor: "rgba(14,165,233,0.16)", color: "#7dd3fc" }}
              >
                {metaBadge}
              </span>
            )}
          </div>
          <h4 className="mt-1 text-sm font-semibold leading-snug" style={{ color: "var(--th-text-heading)" }}>
            {card.title}
          </h4>
        </div>
        {card.github_issue_url && (
          <a
            href={card.github_issue_url}
            target="_blank"
            rel="noreferrer"
            className="text-xs hover:underline"
            onClick={(event) => event.stopPropagation()}
            style={{ color: "#93c5fd" }}
          >
            GH
          </a>
        )}
      </div>
    </article>
  );
}
