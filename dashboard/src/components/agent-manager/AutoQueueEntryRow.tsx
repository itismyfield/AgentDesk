import type { ReactNode } from "react";
import type { DispatchQueueEntry as DispatchQueueEntryType } from "../../api";
import type { UiLanguage } from "../../types";
import { QUEUE_ENTRY_STATUS_TONES, getBatchPhaseColor } from "../../theme/statusTokens";
import { buildDiscordThreadLinks } from "./discord-routing";
import { buildGitHubIssueUrl } from "./kanban-utils";
import { batchPhaseLabel, formatThreadLinkLabel, formatTs, threadGroupColor } from "./auto-queue-panel-utils";

export function EntryRow({
  entry,
  idx,
  tr,
  locale,
  onUpdateStatus,
  isDragging,
  isDropTarget,
  dragHandle,
  moveControls,
  showThreadGroup,
  showBatchPhase,
}: {
  entry: DispatchQueueEntryType;
  idx: number;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onUpdateStatus: (id: string, status: "pending" | "skipped") => void;
  isDragging?: boolean;
  isDropTarget?: boolean;
  dragHandle?: ReactNode;
  showThreadGroup?: boolean;
  showBatchPhase?: boolean;
  moveControls?: {
    canMoveUp: boolean;
    canMoveDown: boolean;
    onMoveUp: () => void;
    onMoveDown: () => void;
  };
}) {
  const effectiveDisplayStatus =
    entry.status === "dispatched" && (entry.card_status === "review" || entry.card_status === "rework")
      ? entry.card_status
      : entry.status;
  const sty =
    QUEUE_ENTRY_STATUS_TONES[effectiveDisplayStatus as keyof typeof QUEUE_ENTRY_STATUS_TONES]
    ?? QUEUE_ENTRY_STATUS_TONES.pending;
  const isPending = entry.status === "pending";
  const isFailed = entry.status === "failed";
  const retryCount = entry.retry_count ?? 0;
  const showReviewRound = (entry.card_status === "review" || entry.card_status === "rework") && (entry.review_round ?? 0) > 0;
  const githubIssueUrl = buildGitHubIssueUrl(entry.github_repo, entry.github_issue_number);
  const threadLinks = (entry.thread_links ?? []).filter(
    (link) => Boolean(link.url || link.thread_id),
  );

  return (
    <div
      className="flex flex-wrap items-start gap-2 rounded-xl border px-3 py-2 transition-all sm:flex-nowrap sm:items-center"
      style={{
        borderColor: isDropTarget
          ? "rgba(16,185,129,0.6)"
          : isFailed
            ? "rgba(239,68,68,0.35)"
          : entry.status === "dispatched"
            ? "rgba(245,158,11,0.3)"
            : "rgba(148,163,184,0.15)",
        backgroundColor: isDragging
          ? "rgba(16,185,129,0.12)"
          : isDropTarget
            ? "rgba(16,185,129,0.08)"
            : isFailed
              ? "rgba(239,68,68,0.08)"
            : entry.status === "dispatched"
              ? "rgba(245,158,11,0.06)"
              : "var(--th-overlay-medium)",
        opacity: isDragging ? 0.5 : 1,
      }}
    >
      <div className="flex min-w-0 flex-1 items-start gap-2">
        {isPending && dragHandle}
        <span
          className="w-5 shrink-0 text-center font-mono text-xs"
          style={{ color: "var(--th-text-muted)" }}
        >
          {idx + 1}
        </span>
        <div className="min-w-0 flex-1">
          <div
            className="text-sm font-medium leading-snug sm:text-xs"
            style={{
              color: "var(--th-text-primary)",
              display: "-webkit-box",
              WebkitLineClamp: 2,
              WebkitBoxOrient: "vertical",
              overflow: "hidden",
            }}
          >
            {showBatchPhase && (
              <span
                className="mr-1 rounded px-1 py-0.5 font-mono text-xs"
                style={{
                  backgroundColor: `${getBatchPhaseColor(entry.batch_phase ?? 0)}22`,
                  color: getBatchPhaseColor(entry.batch_phase ?? 0),
                }}
              >
                {batchPhaseLabel(entry.batch_phase ?? 0)}
              </span>
            )}
            {showThreadGroup && entry.thread_group != null && (
              <span
                className="mr-1 rounded px-1 py-0.5 font-mono text-xs"
                style={{
                  backgroundColor: `${threadGroupColor(entry.thread_group)}22`,
                  color: threadGroupColor(entry.thread_group),
                }}
              >
                G{entry.thread_group}
              </span>
            )}
            {entry.github_issue_number && (
              githubIssueUrl ? (
                <a
                  href={githubIssueUrl}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="mr-1 font-medium hover:underline"
                  style={{ color: "#60a5fa" }}
                  onClick={(e) => e.stopPropagation()}
                >
                  #{entry.github_issue_number}
                </a>
              ) : (
                <span
                  className="mr-1 font-medium"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  #{entry.github_issue_number}
                </span>
              )
            )}
            {entry.card_title ?? entry.card_id.slice(0, 8)}
          </div>
          {entry.reason && (
            <div
              className="mt-1 text-[11px] leading-snug sm:text-xs"
              style={{
                color: "var(--th-text-muted)",
                display: "-webkit-box",
                WebkitLineClamp: 2,
                WebkitBoxOrient: "vertical",
                overflow: "hidden",
              }}
            >
              {entry.reason}
            </div>
          )}
          {threadLinks.length > 0 && (
            <div className="mt-2 flex flex-wrap gap-1.5">
              {threadLinks.map((link) => {
                const label = formatThreadLinkLabel(link, tr);
                const key = `${entry.id}:${link.role}:${link.thread_id}`;
                const resolvedLink = link.url ? buildDiscordThreadLinks(link) : null;
                const href = resolvedLink
                  ? (resolvedLink.deepLink ?? resolvedLink.webUrl)
                  : null;
                const content = (
                  <>
                    <span>{label}</span>
                    {href ? (
                      <span aria-hidden="true">↗</span>
                    ) : (
                      <span className="font-mono opacity-70">
                        #{link.thread_id.slice(-4)}
                      </span>
                    )}
                  </>
                );

                if (href) {
                  return (
                    <a
                      key={key}
                      href={href}
                      target="_blank"
                      rel="noreferrer"
                      onClick={(event) => event.stopPropagation()}
                      className="inline-flex items-center gap-1 rounded-full px-2 py-1 text-[11px] font-medium transition-colors hover:brightness-110"
                      style={{
                        backgroundColor: "rgba(59,130,246,0.14)",
                        color: "#93c5fd",
                      }}
                    >
                      {content}
                    </a>
                  );
                }

                return (
                  <span
                    key={key}
                    className="inline-flex items-center gap-1 rounded-full px-2 py-1 text-[11px] font-medium"
                    style={{
                      backgroundColor: "rgba(148,163,184,0.12)",
                      color: "var(--th-text-muted)",
                    }}
                  >
                    {content}
                  </span>
                );
              })}
            </div>
          )}
        </div>
      </div>
      <div className="ml-auto flex shrink-0 items-center gap-1.5 self-start sm:self-center">
        <div
          className="shrink-0 rounded px-1.5 py-0.5 text-xs"
          style={{ backgroundColor: sty.bg, color: sty.text }}
        >
          {tr(sty.label, sty.labelEn)}
          {showReviewRound && ` R${entry.review_round}`}
        </div>
        {retryCount > 0 && (
          <span
            className="shrink-0 rounded px-1.5 py-0.5 text-[11px] font-mono"
            style={{
              backgroundColor: isFailed ? "rgba(239,68,68,0.12)" : "rgba(148,163,184,0.12)",
              color: isFailed ? "#f87171" : "var(--th-text-muted)",
            }}
            title={tr("누적 재시도 횟수", "Accumulated retry count")}
          >
            R{retryCount}
          </span>
        )}
        {isPending && moveControls && (
          <div
            className="inline-flex shrink-0 overflow-hidden rounded-md border"
            style={{ borderColor: "rgba(148,163,184,0.2)" }}
          >
            <button
              type="button"
              onClick={moveControls.onMoveUp}
              disabled={!moveControls.canMoveUp}
              aria-label={tr("위로 이동", "Move up")}
              title={tr("위로 이동", "Move up")}
              className="px-1.5 py-0.5 text-xs"
              style={{
                color: moveControls.canMoveUp
                  ? "var(--th-text-secondary)"
                  : "var(--th-text-muted)",
                backgroundColor: "var(--th-bg-surface)",
                opacity: moveControls.canMoveUp ? 1 : 0.45,
                touchAction: "manipulation",
              }}
            >
              ↑
            </button>
            <button
              type="button"
              onClick={moveControls.onMoveDown}
              disabled={!moveControls.canMoveDown}
              aria-label={tr("아래로 이동", "Move down")}
              title={tr("아래로 이동", "Move down")}
              className="border-l px-1.5 py-0.5 text-xs"
              style={{
                borderColor: "rgba(148,163,184,0.2)",
                color: moveControls.canMoveDown
                  ? "var(--th-text-secondary)"
                  : "var(--th-text-muted)",
                backgroundColor: "var(--th-bg-surface)",
                opacity: moveControls.canMoveDown ? 1 : 0.45,
                touchAction: "manipulation",
              }}
            >
              ↓
            </button>
          </div>
        )}
        {isPending && (
          <button
            onClick={() => onUpdateStatus(entry.id, "skipped")}
            className="shrink-0 rounded border px-1.5 py-0.5 text-xs"
            style={{
              borderColor: "rgba(148,163,184,0.2)",
              color: "var(--th-text-muted)",
            }}
          >
            {tr("건너뛰기", "Skip")}
          </button>
        )}
        {isFailed && (
          <button
            onClick={() => onUpdateStatus(entry.id, "pending")}
            className="shrink-0 rounded border px-1.5 py-0.5 text-xs"
            style={{
              borderColor: "rgba(239,68,68,0.35)",
              color: "#fca5a5",
              backgroundColor: "rgba(239,68,68,0.08)",
            }}
          >
            {tr("재시도", "Retry")}
          </button>
        )}
        {isFailed && (
          <button
            onClick={() => onUpdateStatus(entry.id, "skipped")}
            className="shrink-0 rounded border px-1.5 py-0.5 text-xs"
            style={{
              borderColor: "rgba(148,163,184,0.2)",
              color: "var(--th-text-muted)",
            }}
          >
            {tr("제외", "Dismiss")}
          </button>
        )}
        {entry.dispatched_at && (
          <span
            className="hidden shrink-0 text-xs sm:inline"
            style={{ color: "var(--th-text-muted)" }}
          >
            {formatTs(entry.dispatched_at, locale)}
          </span>
        )}
      </div>
    </div>
  );
}

// ── dnd-kit reorder controller for a list of pending entries ──
