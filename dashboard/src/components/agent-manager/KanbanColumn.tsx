import type { GitHubIssue } from "../../api";
import type {
  Agent,
  KanbanCard,
  KanbanCardStatus,
} from "../../types";
import { getProviderMeta } from "../../app/providerTheme";
import {
  STATUS_TRANSITIONS,
  buildGitHubIssueUrl,
  isReviewCard,
  labelForStatus,
} from "./kanban-utils";
import {
  SurfaceActionButton,
  SurfaceEmptyState,
} from "../common/SurfacePrimitives";

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
  closingIssueNumber: number | null;
  assigningIssue: boolean;
  getAgentLabel: (agentId: string | null | undefined) => string;
  getAgentProvider: (
    agentId: string | null | undefined,
  ) => Agent["cli_provider"] | null | undefined;
  resolveAgentFromLabels: (labels: Array<{ name: string; color: string }>) => Agent | null;
  onCardClick: (cardId: string) => void;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
  onCloseIssue: (issue: GitHubIssue) => void;
  onDirectAssignIssue: (issue: GitHubIssue, agentId: string) => void;
  onOpenAssignModal: (issue: GitHubIssue) => void;
  onUpdateCardStatus: (cardId: string, targetStatus: KanbanCardStatus) => void;
  onSetActionError: (error: string | null) => void;
}

export default function KanbanColumn({
  column,
  columnCards,
  backlogIssues,
  backlogCount,
  tr,
  compactBoard,
  initialLoading,
  loadingIssues,
  closingIssueNumber,
  assigningIssue,
  getAgentLabel,
  getAgentProvider,
  resolveAgentFromLabels,
  onCardClick,
  onBacklogIssueClick,
  onCloseIssue,
  onDirectAssignIssue,
  onOpenAssignModal,
  onUpdateCardStatus,
  onSetActionError,
}: KanbanColumnProps) {
  return (
    <section
      data-testid={`kanban-column-${column.status}`}
      className={`${compactBoard ? "w-full" : "w-full min-w-0"} space-y-3 rounded-lg border p-3`}
      style={{
        borderColor: "var(--th-border-subtle)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <span className="h-2.5 w-2.5 rounded-full" style={{ backgroundColor: column.accent }} />
          <h3 className="font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {tr(column.labelKo, column.labelEn)}
          </h3>
        </div>
        <span
          className="rounded-full px-2 py-0.5 text-xs bg-surface-medium"
          style={{ color: "var(--th-text-secondary)" }}
        >
          {initialLoading || (column.status === "backlog" && loadingIssues) ? "…" : backlogCount}
        </span>
      </div>

      {/* #1253: cap each column's body and let it scroll internally so a long
          backlog (often 50+ open issues) doesn't stretch the whole page. */}
      <div className="space-y-2 min-h-12 overflow-y-auto pr-1" style={{ maxHeight: "calc(100vh - 22rem)" }}>
        {column.status === "backlog" && loadingIssues && (
          <SurfaceEmptyState
            className="rounded-2xl px-3 py-4 text-center text-xs"
            style={{ borderColor: "var(--th-border-subtle)" }}
          >
            {tr("GitHub backlog 로딩 중...", "Loading GitHub backlog...")}
          </SurfaceEmptyState>
        )}

        {column.status === "backlog" && backlogIssues.map((issue) => (
          <BacklogIssueCard
            key={`issue-${issue.number}`}
            issue={issue}
            columnAccent={column.accent}
            tr={tr}
            closingIssueNumber={closingIssueNumber}
            assigningIssue={assigningIssue}
            getAgentLabel={getAgentLabel}
            resolveAgentFromLabels={resolveAgentFromLabels}
            onBacklogIssueClick={onBacklogIssueClick}
            onCloseIssue={onCloseIssue}
            onDirectAssignIssue={onDirectAssignIssue}
            onOpenAssignModal={onOpenAssignModal}
          />
        ))}

        {backlogCount === 0 && !initialLoading && !(column.status === "backlog" && loadingIssues) && (
          <SurfaceEmptyState
            className="rounded-2xl px-3 py-4 text-center text-xs"
            style={{ borderColor: "var(--th-border-subtle)" }}
          >
            {column.status === "backlog"
              ? tr("repo backlog가 비어 있습니다.", "This repo backlog is empty.")
              : tr("현재 카드가 없습니다.", "No cards in this lane.")}
          </SurfaceEmptyState>
        )}

        {columnCards.map((card) => (
          <KanbanCardArticle
            key={card.id}
            card={card}
            tr={tr}
            getAgentProvider={getAgentProvider}
            onCardClick={onCardClick}
            onUpdateCardStatus={onUpdateCardStatus}
            onSetActionError={onSetActionError}
          />
        ))}
      </div>
    </section>
  );
}

interface BacklogIssueCardProps {
  issue: GitHubIssue;
  columnAccent: string;
  tr: (ko: string, en: string) => string;
  closingIssueNumber: number | null;
  assigningIssue: boolean;
  getAgentLabel: (agentId: string | null | undefined) => string;
  resolveAgentFromLabels: (labels: Array<{ name: string; color: string }>) => Agent | null;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
  onCloseIssue: (issue: GitHubIssue) => void;
  onDirectAssignIssue: (issue: GitHubIssue, agentId: string) => void;
  onOpenAssignModal: (issue: GitHubIssue) => void;
}

function BacklogIssueCard({
  issue,
  tr,
  closingIssueNumber,
  assigningIssue,
  getAgentLabel,
  resolveAgentFromLabels,
  onBacklogIssueClick,
  onCloseIssue,
  onDirectAssignIssue,
  onOpenAssignModal,
}: BacklogIssueCardProps) {
  const autoAgent = resolveAgentFromLabels(issue.labels);

  return (
    <article
      data-testid={`kanban-backlog-issue-${issue.number}`}
      className="min-w-0 rounded-md border p-3 transition-colors hover:bg-surface-subtle"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-bg-primary)",
      }}
      onClick={() => onBacklogIssueClick(issue)}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <span
            className="rounded-full px-2 py-0.5 text-xs bg-surface-medium"
            style={{ color: "var(--th-text-secondary)" }}
          >
            #{issue.number}
          </span>
          <h4 className="mt-1 text-sm font-medium leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-heading)" }}>
            <button type="button" className="min-h-11 w-full text-left [overflow-wrap:anywhere]" onClick={event => { event.stopPropagation(); onBacklogIssueClick(issue); }}>{issue.title}</button>
          </h4>
        </div>
        <a
          href={issue.url}
          target="_blank"
          rel="noopener noreferrer"
          className="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-xs text-th-text-secondary hover:bg-surface-hover"
          style={{ color: "var(--th-text-secondary)" }}
          onClick={(event) => event.stopPropagation()}
        >
          GH
        </a>
      </div>

      <div className="mt-3 flex flex-col items-start gap-2 text-xs sm:flex-row sm:items-center sm:justify-between">
        <div className="flex gap-2" onClick={(event) => event.stopPropagation()}>
          <SurfaceActionButton
            onClick={() => onCloseIssue(issue)}
            disabled={closingIssueNumber === issue.number}
            tone="neutral"
            compact
          >
            {closingIssueNumber === issue.number ? tr("닫는 중", "Closing") : tr("닫기", "Close")}
          </SurfaceActionButton>
          <button
            onClick={(event) => {
              event.stopPropagation();
              if (autoAgent) {
                onDirectAssignIssue(issue, autoAgent.id);
                return;
              }
              onOpenAssignModal(issue);
            }}
            disabled={assigningIssue}
            className="min-h-11 rounded-md border border-th-border-subtle px-3 text-th-text-primary disabled:opacity-50"
            style={{ backgroundColor: "var(--th-overlay-subtle)" }}
          >
            {autoAgent ? `→ ${getAgentLabel(autoAgent.id)}` : tr("할당", "Assign")}
          </button>
        </div>
      </div>
    </article>
  );
}

interface KanbanCardArticleProps {
  card: KanbanCard;
  tr: (ko: string, en: string) => string;
  getAgentProvider: (
    agentId: string | null | undefined,
  ) => Agent["cli_provider"] | null | undefined;
  onCardClick: (cardId: string) => void;
  onUpdateCardStatus: (cardId: string, targetStatus: KanbanCardStatus) => void;
  onSetActionError: (error: string | null) => void;
}

function KanbanCardArticle({
  card,
  tr,
  getAgentProvider,
  onCardClick,
  onUpdateCardStatus,
  onSetActionError,
}: KanbanCardArticleProps) {
  const provider = getAgentProvider(card.assignee_agent_id);
  const providerMeta = provider ? getProviderMeta(provider) : null;
  const githubIssueUrl = buildGitHubIssueUrl(
    card.github_repo,
    card.github_issue_number,
    card.github_issue_url,
  );

  return (
    <article
      data-testid={`kanban-card-${card.id}`}
      onClick={() => onCardClick(card.id)}
      className="min-w-0 rounded-md border p-3 transition-colors hover:bg-surface-subtle"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-bg-primary)",
        borderLeft: isReviewCard(card)
          ? "3px solid color-mix(in oklch, var(--ok) 62%, transparent)"
          : undefined,
      }}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            <span
              className="rounded-full px-2 py-0.5 text-xs bg-surface-medium"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {card.github_issue_number ? `#${card.github_issue_number}` : `#${card.id.slice(0, 6)}`}
            </span>
            {providerMeta ? (
              <span
                className="rounded-full border px-2 py-0.5 text-[11px] font-semibold"
                style={{
                  color: "var(--th-text-muted)",
                  background: "transparent",
                  borderColor: "var(--th-border-subtle)",
                }}
              >
                {providerMeta.label}
              </span>
            ) : null}
          </div>
          <h4 className="mt-1 text-sm font-medium leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-heading)" }}>
            <button type="button" className="min-h-11 w-full text-left [overflow-wrap:anywhere]" onClick={event => { event.stopPropagation(); onCardClick(card.id); }}>{card.title}</button>
          </h4>
        </div>
        {githubIssueUrl && (
          <a
            href={githubIssueUrl}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-xs text-th-text-secondary hover:bg-surface-hover"
            style={{ color: "var(--info)" }}
            onClick={(event) => event.stopPropagation()}
          >
            GH
          </a>
        )}
      </div>

      {(STATUS_TRANSITIONS[card.status] ?? []).length > 0 && (
        <div className="mt-2 flex flex-wrap gap-1.5" onClick={(event) => event.stopPropagation()}>
          {(STATUS_TRANSITIONS[card.status] ?? []).map((target) => {
            return (
              <SurfaceActionButton
                key={target}
                onClick={() => {
                  onSetActionError(null);
                  onUpdateCardStatus(card.id, target);
                }}
                tone="neutral"
                compact
                className="min-h-11"
                style={{ color: "var(--th-text-secondary)" }}
              >
                → {labelForStatus(target, tr)}
              </SurfaceActionButton>
            );
          })}
        </div>
      )}
    </article>
  );
}
