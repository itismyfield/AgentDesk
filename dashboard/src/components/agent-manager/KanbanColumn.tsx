import type { DragEvent } from "react";
import type { GitHubIssue } from "../../api";
import type {
  Agent,
  KanbanCard,
  KanbanCardStatus,
  KanbanRepoSource,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import MarkdownContent from "../common/MarkdownContent";
import {
  COLUMN_DEFS,
  STATUS_TRANSITIONS,
  TRANSITION_STYLE,
  formatIso,
  getCardDelayBadge,
  getCardMetadata,
  getChecklistSummary,
  isReviewCard,
  labelForStatus,
  parseIssueSections,
  priorityLabel,
} from "./kanban-utils";

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
  locale: UiLanguage;
  compactBoard: boolean;
  initialLoading: boolean;
  loadingIssues: boolean;
  draggingCardId: string | null;
  dragOverStatus: KanbanCardStatus | null;
  dragOverCardId: string | null;
  closingIssueNumber: number | null;
  assigningIssue: boolean;
  dispatchMap: Map<string, TaskDispatch>;
  dispatches: TaskDispatch[];
  repoSources: KanbanRepoSource[];
  selectedRepo: string;
  getAgentLabel: (agentId: string | null | undefined) => string;
  resolveAgentFromLabels: (labels: Array<{ name: string; color: string }>) => Agent | null;
  onCardClick: (cardId: string) => void;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
  onSetDraggingCardId: (id: string | null) => void;
  onSetDragOverStatus: (status: KanbanCardStatus | null) => void;
  onSetDragOverCardId: (id: string | null) => void;
  onDrop: (targetStatus: KanbanCardStatus, beforeCardId: string | null, event: DragEvent<HTMLElement>) => void;
  onCloseIssue: (issue: GitHubIssue) => void;
  onDirectAssignIssue: (issue: GitHubIssue, agentId: string) => void;
  onOpenAssignModal: (issue: GitHubIssue) => void;
  onUpdateCardStatus: (cardId: string, targetStatus: KanbanCardStatus) => void;
  onSetActionError: (error: string | null) => void;
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
  locale,
  compactBoard,
  initialLoading,
  loadingIssues,
  draggingCardId,
  dragOverStatus,
  dragOverCardId,
  closingIssueNumber,
  assigningIssue,
  dispatchMap,
  dispatches,
  repoSources,
  selectedRepo,
  getAgentLabel,
  resolveAgentFromLabels,
  onCardClick,
  onBacklogIssueClick,
  onSetDraggingCardId,
  onSetDragOverStatus,
  onSetDragOverCardId,
  onDrop,
  onCloseIssue,
  onDirectAssignIssue,
  onOpenAssignModal,
  onUpdateCardStatus,
  onSetActionError,
}: KanbanColumnProps) {
  return (
    <section
      className={`${compactBoard ? "w-full" : "w-[320px] shrink-0"} rounded-2xl border p-3 space-y-3`}
      style={{
        borderColor: dragOverStatus === column.status ? column.accent : "rgba(148,163,184,0.24)",
        backgroundColor: "var(--th-bg-surface)",
      }}
      onDragOver={(event) => {
        if (compactBoard) return;
        event.preventDefault();
        onSetDragOverStatus(column.status);
        onSetDragOverCardId(null);
      }}
      onDrop={(event) => {
        if (compactBoard) return;
        onDrop(column.status, null, event);
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
            column={column}
            tr={tr}
            locale={locale}
            compactBoard={compactBoard}
            closingIssueNumber={closingIssueNumber}
            assigningIssue={assigningIssue}
            repoSources={repoSources}
            selectedRepo={selectedRepo}
            getAgentLabel={getAgentLabel}
            resolveAgentFromLabels={resolveAgentFromLabels}
            onBacklogIssueClick={onBacklogIssueClick}
            onCloseIssue={onCloseIssue}
            onDirectAssignIssue={onDirectAssignIssue}
            onOpenAssignModal={onOpenAssignModal}
          />
        ))}

        {/* Empty state */}
        {backlogCount === 0 && !initialLoading && !(column.status === "backlog" && loadingIssues) && (
          <div className="rounded-xl border border-dashed px-3 py-4 text-xs text-center" style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-muted)" }}>
            {column.status === "backlog"
              ? tr("repo backlog가 비어 있습니다.", "This repo backlog is empty.")
              : tr("여기에 드롭", "Drop here")}
          </div>
        )}

        {/* Kanban cards */}
        {columnCards.map((card) => (
          <KanbanCardArticle
            key={card.id}
            card={card}
            column={column}
            tr={tr}
            locale={locale}
            compactBoard={compactBoard}
            draggingCardId={draggingCardId}
            dragOverCardId={dragOverCardId}
            dispatchMap={dispatchMap}
            dispatches={dispatches}
            getAgentLabel={getAgentLabel}
            onCardClick={onCardClick}
            onSetDraggingCardId={onSetDraggingCardId}
            onSetDragOverStatus={onSetDragOverStatus}
            onSetDragOverCardId={onSetDragOverCardId}
            onDrop={onDrop}
            onUpdateCardStatus={onUpdateCardStatus}
            onSetActionError={onSetActionError}
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
  column: ColumnDef;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  compactBoard: boolean;
  closingIssueNumber: number | null;
  assigningIssue: boolean;
  repoSources: KanbanRepoSource[];
  selectedRepo: string;
  getAgentLabel: (agentId: string | null | undefined) => string;
  resolveAgentFromLabels: (labels: Array<{ name: string; color: string }>) => Agent | null;
  onBacklogIssueClick: (issue: GitHubIssue) => void;
  onCloseIssue: (issue: GitHubIssue) => void;
  onDirectAssignIssue: (issue: GitHubIssue, agentId: string) => void;
  onOpenAssignModal: (issue: GitHubIssue) => void;
}

function BacklogIssueCard({
  issue,
  column,
  tr,
  locale,
  compactBoard,
  closingIssueNumber,
  assigningIssue,
  repoSources,
  selectedRepo,
  getAgentLabel,
  resolveAgentFromLabels,
  onBacklogIssueClick,
  onCloseIssue,
  onDirectAssignIssue,
  onOpenAssignModal,
}: BacklogIssueCardProps) {
  return (
    <article
      className="rounded-2xl border p-3 cursor-pointer transition-colors hover:border-[rgba(148,163,184,0.4)]"
      style={{ borderColor: "rgba(148,163,184,0.2)", backgroundColor: "var(--th-card-bg)" }}
      onClick={() => onBacklogIssueClick(issue)}
      draggable
      onDragStart={(event) => {
        event.dataTransfer.setData("application/x-backlog-issue", JSON.stringify(issue));
        event.dataTransfer.effectAllowed = "move";
      }}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
              #{issue.number}
            </span>
            {issue.labels.slice(0, 2).map((label) => (
              <span
                key={label.name}
                className="px-2 py-0.5 rounded-full text-xs"
                style={{ backgroundColor: `#${label.color}22`, color: `#${label.color}` }}
              >
                {label.name}
              </span>
            ))}
          </div>
          <h4 className="mt-2 text-sm font-semibold leading-snug" style={{ color: "var(--th-text-heading)" }}>
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
      <div className="mt-3 flex flex-col items-start gap-2 text-xs sm:flex-row sm:items-center sm:justify-between" style={{ color: "var(--th-text-muted)" }}>
        <span>{tr("업데이트", "Updated")}: {formatIso(issue.updatedAt, locale)}</span>
        <div className="flex gap-2">
          <button
            onClick={(event) => { event.stopPropagation(); onCloseIssue(issue); }}
            disabled={closingIssueNumber === issue.number}
            className="rounded-lg px-3 py-1.5 border disabled:opacity-50"
            style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-muted)" }}
          >
            {closingIssueNumber === issue.number ? tr("닫는 중", "Closing") : tr("닫기", "Close")}
          </button>
          <button
            onClick={(event) => {
              event.stopPropagation();
              const autoAgent = resolveAgentFromLabels(issue.labels);
              if (autoAgent) {
                onDirectAssignIssue(issue, autoAgent.id);
              } else {
                onOpenAssignModal(issue);
              }
            }}
            disabled={assigningIssue}
            className="rounded-lg px-3 py-1.5 text-white disabled:opacity-50"
            style={{ backgroundColor: column.accent }}
          >
            {(() => {
              const autoAgent = resolveAgentFromLabels(issue.labels);
              if (autoAgent) return `→ ${getAgentLabel(autoAgent.id)}`;
              return tr("할당", "Assign");
            })()}
          </button>
        </div>
      </div>
    </article>
  );
}

interface KanbanCardArticleProps {
  card: KanbanCard;
  column: ColumnDef;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  compactBoard: boolean;
  draggingCardId: string | null;
  dragOverCardId: string | null;
  dispatchMap: Map<string, TaskDispatch>;
  dispatches: TaskDispatch[];
  getAgentLabel: (agentId: string | null | undefined) => string;
  onCardClick: (cardId: string) => void;
  onSetDraggingCardId: (id: string | null) => void;
  onSetDragOverStatus: (status: KanbanCardStatus | null) => void;
  onSetDragOverCardId: (id: string | null) => void;
  onDrop: (targetStatus: KanbanCardStatus, beforeCardId: string | null, event: DragEvent<HTMLElement>) => void;
  onUpdateCardStatus: (cardId: string, targetStatus: KanbanCardStatus) => void;
  onSetActionError: (error: string | null) => void;
}

function KanbanCardArticle({
  card,
  column,
  tr,
  locale,
  compactBoard,
  draggingCardId,
  dragOverCardId,
  dispatchMap,
  dispatches,
  getAgentLabel,
  onCardClick,
  onSetDraggingCardId,
  onSetDragOverStatus,
  onSetDragOverCardId,
  onDrop,
  onUpdateCardStatus,
  onSetActionError,
}: KanbanCardArticleProps) {
  const latestDispatch = card.latest_dispatch_id ? dispatchMap.get(card.latest_dispatch_id) : undefined;
  const metadata = getCardMetadata(card);
  const checklistSummary = getChecklistSummary(card);
  const delayBadge = getCardDelayBadge(card, tr);

  return (
    <article
      draggable
      onDragStart={(event) => {
        if (compactBoard) return;
        onSetDraggingCardId(card.id);
        event.dataTransfer.effectAllowed = "move";
        event.dataTransfer.setData("text/plain", card.id);
      }}
      onDragEnd={() => {
        onSetDraggingCardId(null);
        onSetDragOverStatus(null);
        onSetDragOverCardId(null);
      }}
      onDragOver={(event) => {
        if (compactBoard) return;
        event.preventDefault();
        onSetDragOverStatus(column.status);
        onSetDragOverCardId(card.id);
      }}
      onDrop={(event) => {
        if (compactBoard) return;
        onDrop(column.status, card.id, event);
      }}
      onClick={() => onCardClick(card.id)}
      className="rounded-2xl border p-3 cursor-pointer transition-transform hover:-translate-y-0.5"
      style={{
        borderColor: dragOverCardId === card.id ? column.accent : "rgba(148,163,184,0.2)",
        backgroundColor: isReviewCard(card) ? "rgba(139,92,246,0.08)" : "var(--th-card-bg)",
        borderLeft: isReviewCard(card) ? "3px solid rgba(139,92,246,0.6)" : undefined,
        opacity: draggingCardId === card.id ? 0.45 : 1,
      }}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            {isReviewCard(card) && (
              <span className="px-1.5 py-0.5 rounded-full text-xs font-semibold" style={{ backgroundColor: "rgba(139,92,246,0.25)", color: "#a78bfa" }}>
                {card.latest_dispatch_type === "review-decision" ? "Decision" : "Review"}
              </span>
            )}
            <span className="px-2 py-0.5 rounded-full text-xs" style={{ color: "white", backgroundColor: column.accent }}>
              {priorityLabel(card.priority, tr)}
            </span>
            {card.github_issue_number && (
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
                #{card.github_issue_number}
              </span>
            )}
            {card.depth > 0 && (
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
                {tr("체인", "Chain")} {card.depth}
              </span>
            )}
            {metadata.retry_count ? (
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
                {tr("재시도", "Retry")} {metadata.retry_count}
              </span>
            ) : null}
            {metadata.failover_count ? (
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "#fca5a5" }}>
                {tr("Failover", "Failover")} {metadata.failover_count}
              </span>
            ) : null}
            {metadata.redispatch_count ? (
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "#fbbf24" }}>
                {tr("재디스패치", "Redispatch")} {metadata.redispatch_count}
              </span>
            ) : null}
            {checklistSummary && (
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "#99f6e4" }}>
                {tr("리뷰", "Review")} {checklistSummary}
              </span>
            )}
            {delayBadge && (
              <span className="px-2 py-0.5 rounded-full text-xs" style={{ color: "white", backgroundColor: delayBadge.tone }}>
                {delayBadge.label} {delayBadge.detail}
              </span>
            )}
          </div>
          <h4 className="mt-2 text-sm font-semibold leading-snug" style={{ color: "var(--th-text-heading)" }}>
            {card.title}
          </h4>
        </div>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {card.github_issue_number ? `#${card.github_issue_number}` : `#${card.id.slice(0, 6)}`}
        </span>
      </div>

      {card.description && (() => {
        const sections = parseIssueSections(card.description);
        const displayText = sections?.content ?? card.description;
        return (
          <div className="mt-2 text-xs" style={{ color: "var(--th-text-secondary)", display: "-webkit-box", WebkitLineClamp: 3, WebkitBoxOrient: "vertical", overflow: "hidden" }}>
            <MarkdownContent content={displayText} />
          </div>
        );
      })()}

      {card.status === "blocked" && card.blocked_reason && (
        <div className="mt-2 rounded-md px-2.5 py-2 text-xs" style={{ backgroundColor: "rgba(239,68,68,0.12)", border: "1px solid rgba(239,68,68,0.3)", color: "#fca5a5" }}>
          <span className="font-semibold">{tr("차단 사유", "Blocked reason")}:</span>{" "}
          {card.blocked_reason}
        </div>
      )}

      {card.status === "review" && card.review_status && (
        <div className="mt-2 rounded-md px-2.5 py-2 text-xs" style={{
          backgroundColor: (card.review_status === "dilemma_pending" || card.review_status === "suggestion_pending") ? "rgba(234,179,8,0.12)" : card.review_status === "improve_rework" ? "rgba(249,115,22,0.12)" : "rgba(20,184,166,0.12)",
          border: `1px solid ${(card.review_status === "dilemma_pending" || card.review_status === "suggestion_pending") ? "rgba(234,179,8,0.3)" : card.review_status === "improve_rework" ? "rgba(249,115,22,0.3)" : "rgba(20,184,166,0.3)"}`,
          color: (card.review_status === "dilemma_pending" || card.review_status === "suggestion_pending") ? "#fde047" : card.review_status === "improve_rework" ? "#fdba74" : "#5eead4",
        }}>
          {card.review_status === "reviewing" && (() => {
            const reviewDispatch = latestDispatch?.parent_dispatch_id
              ? dispatches.find((d) => d.parent_dispatch_id === latestDispatch?.id && d.dispatch_type === "review")
              : dispatches.find((d) => d.parent_dispatch_id === card.latest_dispatch_id && d.dispatch_type === "review");
            const verdictLabel = !reviewDispatch
              ? tr("verdict 대기중", "verdict pending")
              : reviewDispatch.status === "completed"
                ? tr("verdict 전달됨", "verdict delivered")
                : tr("verdict 미전달", "verdict not delivered");
            return <>{tr("카운터 모델 리뷰 중", "Counter-model reviewing")} · <span style={{ opacity: 0.7 }}>{verdictLabel}</span></>;
          })()}
          {card.review_status === "awaiting_dod" && tr("DoD 완료 대기", "Awaiting DoD completion")}
          {card.review_status === "improve_rework" && tr("개선 재작업 중", "Improvement rework")}
          {card.review_status === "suggestion_pending" && tr("리뷰 제안 결정 대기", "Review suggestions pending")}
          {card.review_status === "dilemma_pending" && tr("판단 대기 (딜레마)", "Dilemma pending")}
          {card.review_status === "decided" && tr("결정됨", "Decided")}
        </div>
      )}

      <div className="mt-3 space-y-1.5 text-xs" style={{ color: "var(--th-text-muted)" }}>
        <div>{tr("담당자", "Assignee")}: {getAgentLabel(card.assignee_agent_id)}</div>
        {latestDispatch && <div>{tr("디스패치", "Dispatch")}: {latestDispatch.status}</div>}
        {metadata.reward && (
          <div>{tr("완료 보상", "Completion reward")}: +{metadata.reward.xp} XP</div>
        )}
        {card.github_issue_url && (
          <a
            href={card.github_issue_url}
            target="_blank"
            rel="noreferrer"
            className="inline-flex hover:underline"
            onClick={(event) => event.stopPropagation()}
            style={{ color: "#93c5fd" }}
          >
            {tr("GitHub 이슈", "GitHub issue")}
          </a>
        )}
      </div>

      {/* Quick transition buttons */}
      {(STATUS_TRANSITIONS[card.status] ?? []).length > 0 && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {(STATUS_TRANSITIONS[card.status] ?? []).map((target) => {
            const style = TRANSITION_STYLE[target] ?? TRANSITION_STYLE.backlog;
            return (
              <button
                key={target}
                type="button"
                onClick={(event) => {
                  event.stopPropagation();
                  onSetActionError(null);
                  onUpdateCardStatus(card.id, target);
                }}
                className="rounded-lg px-2.5 py-1 text-xs font-medium border"
                style={{
                  backgroundColor: style.bg,
                  borderColor: style.text,
                  color: style.text,
                }}
              >
                → {labelForStatus(target, tr)}
              </button>
            );
          })}
        </div>
      )}
    </article>
  );
}
