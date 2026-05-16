import type {
  CardAuditLogEntry,
  GitHubComment,
  KanbanReview,
} from "../../api";
import CardIssueContent from "./CardIssueContent";
import CardTimeline from "./CardTimeline";
import TurnTranscriptPanel from "./TurnTranscriptPanel";
import KanbanCardActivitySections from "./KanbanCardActivitySections";
import KanbanCardReviewPanel from "./KanbanCardReviewPanel";
import { localeName } from "../../i18n";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceMetricPill,
  SurfaceNotice,
} from "../common/SurfacePrimitives";
import type {
  Agent,
  KanbanCard,
  KanbanCardMetadata,
  KanbanCardPriority,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import {
  PRIORITY_OPTIONS,
  STATUS_TRANSITIONS,
  TRANSITION_STYLE,
  buildGitHubIssueUrl,
  formatIso,
  labelForStatus,
  parseCardMetadata,
  priorityLabel,
  stringifyCardMetadata,
  type EditorState,
} from "./kanban-utils";

export function canRetryCard(card: KanbanCard | null) {
  return Boolean(card && ["blocked", "requested", "in_progress"].includes(card.status));
}

export function canRedispatchCard(card: KanbanCard | null) {
  return Boolean(card && ["requested", "in_progress"].includes(card.status));
}

// ---------------------------------------------------------------------------
// Props
// ---------------------------------------------------------------------------

export interface KanbanCardDetailProps {
  card: KanbanCard;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  agents: Agent[];
  dispatches: TaskDispatch[];

  // Editor state (lifted)
  editor: EditorState;
  setEditor: React.Dispatch<React.SetStateAction<EditorState>>;

  // Loading states
  savingCard: boolean;
  setSavingCard: React.Dispatch<React.SetStateAction<boolean>>;
  retryingCard: boolean;
  setRetryingCard: React.Dispatch<React.SetStateAction<boolean>>;
  redispatching: boolean;
  setRedispatching: React.Dispatch<React.SetStateAction<boolean>>;
  redispatchReason: string;
  setRedispatchReason: React.Dispatch<React.SetStateAction<string>>;
  retryAssigneeId: string;
  setRetryAssigneeId: React.Dispatch<React.SetStateAction<string>>;

  // Error
  actionError: string | null;
  setActionError: React.Dispatch<React.SetStateAction<string | null>>;

  // Activity data
  auditLog: CardAuditLogEntry[];
  ghComments: GitHubComment[];
  reviewData: KanbanReview | null;
  setReviewData: React.Dispatch<React.SetStateAction<KanbanReview | null>>;
  reviewDecisions: Record<string, "accept" | "reject">;
  setReviewDecisions: React.Dispatch<React.SetStateAction<Record<string, "accept" | "reject">>>;
  timelineFilter: "review" | "pm" | "work" | "general" | null;
  setTimelineFilter: React.Dispatch<React.SetStateAction<"review" | "pm" | "work" | "general" | null>>;

  // Cancel confirm
  setCancelConfirm: React.Dispatch<React.SetStateAction<{ cardIds: string[]; source: "bulk" | "single" } | null>>;

  // Callbacks
  onClose: () => void;
  onUpdateCard: (
    id: string,
    patch: Partial<KanbanCard> & { before_card_id?: string | null },
  ) => Promise<void>;
  onRetryCard: (
    id: string,
    payload?: { assignee_agent_id?: string | null; request_now?: boolean },
  ) => Promise<void>;
  onRedispatchCard: (
    id: string,
    payload?: { reason?: string | null },
  ) => Promise<void>;
  onDeleteCard: (id: string) => Promise<void>;
  invalidateCardActivity: (cardId: string) => void;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export default function KanbanCardDetail({
  card: selectedCard,
  tr,
  locale,
  agents,
  dispatches,
  editor,
  setEditor,
  savingCard,
  setSavingCard,
  retryingCard,
  setRetryingCard,
  redispatching,
  setRedispatching,
  redispatchReason,
  setRedispatchReason,
  retryAssigneeId,
  setRetryAssigneeId,
  actionError,
  setActionError,
  auditLog,
  ghComments,
  reviewData,
  setReviewData,
  reviewDecisions,
  setReviewDecisions,
  timelineFilter,
  setTimelineFilter,
  setCancelConfirm,
  onClose,
  onUpdateCard,
  onRetryCard,
  onRedispatchCard,
  onDeleteCard,
  invalidateCardActivity,
}: KanbanCardDetailProps) {

  const agentMap = new Map(agents.map((agent) => [agent.id, agent]));
  const inputChrome = {
    borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
    color: "var(--th-text-primary)",
  } as const;
  const githubIssueUrl = buildGitHubIssueUrl(
    selectedCard.github_repo,
    selectedCard.github_issue_number,
    selectedCard.github_issue_url,
  );

  const getAgentLabel = (agentId: string | null | undefined) => {
    if (!agentId) return tr("미할당", "Unassigned");
    const agent = agentMap.get(agentId);
    if (!agent) return agentId;
    return localeName(locale, agent);
  };

  const handleSaveCard = async () => {
    setSavingCard(true);
    setActionError(null);
    try {
      const metadata = {
        ...parseCardMetadata(selectedCard.metadata_json),
        review_checklist: editor.review_checklist
          .map((item, index) => ({
            id: item.id || `check-${index}`,
            label: item.label.trim(),
            done: item.done,
          }))
          .filter((item) => item.label),
      } satisfies KanbanCardMetadata;

      await onUpdateCard(selectedCard.id, {
        title: editor.title.trim(),
        description: editor.description.trim() || null,
        assignee_agent_id: editor.assignee_agent_id || null,
        priority: editor.priority,
        metadata_json: stringifyCardMetadata(metadata),
      });
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("카드 저장에 실패했습니다.", "Failed to save card."));
    } finally {
      setSavingCard(false);
    }
  };

  const handleRetryCard = async () => {
    setRetryingCard(true);
    setActionError(null);
    try {
      await onRetryCard(selectedCard.id, {
        assignee_agent_id: retryAssigneeId || selectedCard.assignee_agent_id,
        request_now: true,
      });
      invalidateCardActivity(selectedCard.id);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("재시도에 실패했습니다.", "Failed to retry card."));
    } finally {
      setRetryingCard(false);
    }
  };

  const handleDeleteCard = async () => {
    const confirmed = window.confirm(tr("이 카드를 삭제할까요?", "Delete this card?"));
    if (!confirmed) return;
    setSavingCard(true);
    setActionError(null);
    try {
      await onDeleteCard(selectedCard.id);
      onClose();
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("카드 삭제에 실패했습니다.", "Failed to delete card."));
    } finally {
      setSavingCard(false);
    }
  };

  const handleRedispatch = async () => {
    setRedispatching(true);
    setActionError(null);
    try {
      await onRedispatchCard(selectedCard.id, {
        reason: redispatchReason.trim() || null,
      });
      invalidateCardActivity(selectedCard.id);
      setRedispatchReason("");
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("재디스패치에 실패했습니다.", "Failed to redispatch."));
    }
    setRedispatching(false);
  };

  return (
    <div className="fixed inset-0 z-50 flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={onClose}>
      <div
        data-testid="kanban-card-drawer"
        onClick={(e) => e.stopPropagation()}
        className="mb-[-20px] w-full max-w-3xl min-h-[calc(100svh-5.25rem)] max-h-[90svh] overflow-y-auto rounded-t-3xl border p-5 sm:mb-0 sm:min-h-0 sm:max-h-[90vh] sm:rounded-3xl sm:p-6 space-y-4"
        style={{
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
          minHeight: "calc(100svh - 5.25rem)",
          maxHeight: "90svh",
          paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
        }}
        role="dialog" aria-modal="true" aria-label="Card details"
      >
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap gap-2">
              <SurfaceMetricPill
                label={tr("상태", "Status")}
                value={labelForStatus(selectedCard.status, tr)}
                tone="info"
                className="min-w-[120px]"
              />
              <SurfaceMetricPill
                label={tr("우선순위", "Priority")}
                value={priorityLabel(selectedCard.priority, tr)}
                tone="warn"
                className="min-w-[120px]"
              />
              {selectedCard.github_repo && (
                <SurfaceMetricPill
                  label="GitHub"
                  value={selectedCard.github_repo}
                  tone="neutral"
                  className="min-w-[140px]"
                />
              )}
            </div>
            <h3 className="mt-2 text-xl font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {selectedCard.title}
            </h3>
          </div>
          <SurfaceActionButton onClick={onClose} tone="neutral" className="shrink-0 whitespace-nowrap">
            {tr("닫기", "Close")}
          </SurfaceActionButton>
        </div>

        {/* Pipeline progress - removed with PipelineConfigView */}
        {false && selectedCard.pipeline_stage_id && (
          <div />
        )}

        {actionError && (
          <SurfaceNotice tone="danger">
            {actionError}
          </SurfaceNotice>
        )}

        <div className="grid gap-3 md:grid-cols-2">
          <label className="space-y-1">
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("제목", "Title")}</span>
            <input
              value={editor.title}
              onChange={(event) => setEditor((prev) => ({ ...prev, title: event.target.value }))}
              className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
              style={inputChrome}
            />
          </label>
          <div className="space-y-1">
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("상태 전환", "Status")}</span>
            <div className="flex flex-wrap gap-1.5">
              {(STATUS_TRANSITIONS[selectedCard.status] ?? []).map((target) => {
                const style = TRANSITION_STYLE[target] ?? TRANSITION_STYLE.backlog;
                return (
                  <button
                    key={target}
                    type="button"
                    disabled={savingCard}
                    onClick={async () => {
                      if (target === "done" && editor.review_checklist.some((item) => !item.done)) {
                        setActionError(tr("review checklist를 모두 완료해야 done으로 이동할 수 있습니다.", "Complete the review checklist before moving to done."));
                        return;
                      }
                      if (target === "backlog") {
                        setCancelConfirm({ cardIds: [selectedCard.id], source: "single" });
                        return;
                      }
                      setSavingCard(true);
                      setActionError(null);
                      try {
                        await onUpdateCard(selectedCard.id, { status: target });
                        invalidateCardActivity(selectedCard.id);
                        setEditor((prev) => ({ ...prev, status: target }));
                      } catch (error) {
                        setActionError(error instanceof Error ? error.message : tr("상태 전환에 실패했습니다.", "Failed to change status."));
                      } finally {
                        setSavingCard(false);
                      }
                    }}
                    className="rounded-lg px-3 py-1.5 text-xs font-medium border transition-opacity hover:opacity-80 disabled:opacity-40"
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
          </div>
        </div>

        <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
          <label className="space-y-1">
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("담당자", "Assignee")}</span>
            <select
              value={editor.assignee_agent_id}
              onChange={(event) => setEditor((prev) => ({ ...prev, assignee_agent_id: event.target.value }))}
              className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
              style={inputChrome}
            >
              <option value="">{tr("없음", "None")}</option>
              {agents.map((agent) => (
                <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
              ))}
            </select>
          </label>
          <label className="space-y-1">
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("우선순위", "Priority")}</span>
            <select
              value={editor.priority}
              onChange={(event) => setEditor((prev) => ({ ...prev, priority: event.target.value as KanbanCardPriority }))}
              className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
              style={inputChrome}
            >
              {PRIORITY_OPTIONS.map((priority) => (
                <option key={priority} value={priority}>{priorityLabel(priority, tr)}</option>
              ))}
            </select>
          </label>
          <SurfaceCard className="p-3" style={{ background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("GitHub", "GitHub")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>
              {githubIssueUrl ? (
                <a href={githubIssueUrl} target="_blank" rel="noreferrer" className="hover:underline" style={{ color: "#93c5fd" }}>
                  #{selectedCard.github_issue_number ?? "-"}
                </a>
              ) : (
                selectedCard.github_issue_number ? `#${selectedCard.github_issue_number}` : "-"
              )}
            </div>
          </SurfaceCard>
        </div>

        {/* Blocked reason */}
        {selectedCard.blocked_reason && (
          <SurfaceNotice tone="danger" className="block">
            <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{ color: "#ef4444" }}>
              {tr("수동 개입 사유", "Manual Intervention Reason")}
            </div>
            <div className="text-sm" style={{ color: "#fca5a5" }}>
              {selectedCard.blocked_reason}
            </div>
          </SurfaceNotice>
        )}

        <KanbanCardReviewPanel
          card={selectedCard}
          dispatches={dispatches}
          reviewData={reviewData}
          setReviewData={setReviewData}
          reviewDecisions={reviewDecisions}
          setReviewDecisions={setReviewDecisions}
          setActionError={setActionError}
          tr={tr}
        />

        {/* Description / Issue Sections */}
        <CardIssueContent
          card={selectedCard}
          editor={editor}
          setEditor={setEditor}
          tr={tr}
        />

        <div
          className="rounded-2xl border overflow-hidden"
          style={{
            borderColor: "var(--th-border-subtle)",
            backgroundColor: "rgba(255,255,255,0.02)",
          }}
        >
          <TurnTranscriptPanel
            source={{
              type: "card",
              id: selectedCard.id,
              refreshSeed: `${selectedCard.latest_dispatch_id ?? ""}:${selectedCard.updated_at}`,
            }}
            tr={tr}
            isKo={locale === "ko"}
            title={tr("연결된 턴 트랜스크립트", "Linked Turn Transcript")}
          />
        </div>

        {canRedispatchCard(selectedCard) && (
          <SurfaceCard className="space-y-3">
            <div>
              <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                {tr("이슈 변경 후 재전송", "Resend with Updated Issue")}
              </h4>
              <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "이슈 본문을 수정한 뒤, 기존 dispatch를 취소하고 새로 전송합니다.",
                  "Cancel current dispatch and resend with the updated issue body.",
                )}
              </p>
            </div>
            <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto]">
              <input
                type="text"
                placeholder={tr("사유 (선택)", "Reason (optional)")}
                value={redispatchReason}
                onChange={(e) => setRedispatchReason(e.target.value)}
                className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
                style={inputChrome}
              />
              <SurfaceActionButton
                type="button"
                onClick={() => void handleRedispatch()}
                disabled={redispatching}
                tone="warn"
                className="whitespace-nowrap"
              >
                {redispatching ? tr("전송 중...", "Sending...") : tr("재전송", "Resend")}
              </SurfaceActionButton>
            </div>
          </SurfaceCard>
        )}

        {canRetryCard(selectedCard) && (
          <SurfaceCard className="space-y-3">
            <div>
              <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                {tr("재시도 / 담당자 변경", "Retry / Change Assignee")}
              </h4>
              <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {tr("동일 내용으로 재전송하거나 다른 에이전트에게 전환합니다.", "Resend as-is or switch to another agent.")}
              </p>
            </div>
            <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto]">
              <select
                value={retryAssigneeId}
                onChange={(event) => setRetryAssigneeId(event.target.value)}
                className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
                style={inputChrome}
              >
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                ))}
              </select>
              <SurfaceActionButton
                type="button"
                onClick={() => void handleRetryCard()}
                disabled={retryingCard || !(retryAssigneeId || selectedCard.assignee_agent_id)}
                tone="accent"
                className="whitespace-nowrap"
              >
                {retryingCard ? tr("전송 중...", "Sending...") : tr("재시도", "Retry")}
              </SurfaceActionButton>
            </div>
          </SurfaceCard>
        )}

        <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4 text-sm">
          <SurfaceMetricPill label={tr("생성", "Created")} value={formatIso(selectedCard.created_at, locale)} />
          <SurfaceMetricPill label={tr("요청", "Requested")} value={formatIso(selectedCard.requested_at, locale)} />
          <SurfaceMetricPill label={tr("시작", "Started")} value={formatIso(selectedCard.started_at, locale)} />
          <SurfaceMetricPill label={tr("완료", "Completed")} value={formatIso(selectedCard.completed_at, locale)} />
        </div>

        <KanbanCardActivitySections
          card={selectedCard}
          dispatches={dispatches}
          auditLog={auditLog}
          tr={tr}
          locale={locale}
          getAgentLabel={getAgentLabel}
        />

        {/* Unified GitHub comment timeline */}
        <CardTimeline
          ghComments={ghComments}
          timelineFilter={timelineFilter}
          setTimelineFilter={setTimelineFilter}
          tr={tr}
          locale={locale}
          onRefresh={() => invalidateCardActivity(selectedCard.id)}
        />

        <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div className="flex gap-2">
            <SurfaceActionButton
              onClick={handleDeleteCard}
              disabled={savingCard}
              tone="danger"
              className="text-sm"
              style={{ color: "#fecaca" }}
            >
              {tr("카드 삭제", "Delete card")}
            </SurfaceActionButton>
            {selectedCard.status !== "done" && (
              <SurfaceActionButton
                onClick={() => setCancelConfirm({ cardIds: [selectedCard.id], source: "single" })}
                disabled={savingCard}
                tone="neutral"
                className="text-sm"
              >
                {tr("카드 취소", "Cancel card")}
              </SurfaceActionButton>
            )}
          </div>
          <div className="flex flex-col-reverse gap-2 sm:flex-row">
            <SurfaceActionButton
              onClick={onClose}
              tone="neutral"
              className="text-sm"
            >
              {tr("닫기", "Close")}
            </SurfaceActionButton>
            <SurfaceActionButton
              onClick={() => void handleSaveCard()}
              disabled={savingCard || !editor.title.trim()}
              tone="info"
              className="text-sm"
              style={{ color: "#dbeafe" }}
            >
              {savingCard ? tr("저장 중", "Saving") : tr("저장", "Save")}
            </SurfaceActionButton>
          </div>
        </div>
      </div>
    </div>
  );
}
