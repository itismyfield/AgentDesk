import { useState } from "react";
import * as api from "../../api";
import type { KanbanReview } from "../../api";
import CardIssueContent from "./CardIssueContent";
import CardTimeline from "./CardTimeline";
import TurnTranscriptPanel from "./TurnTranscriptPanel";
import { localeName } from "../../i18n";
import type {
  Agent,
  KanbanCard,
  KanbanCardMetadata,
  KanbanCardPriority,
  KanbanCardStatus,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import {
  hasManualInterventionReason,
  PRIORITY_OPTIONS,
  STATUS_TRANSITIONS,
  TRANSITION_STYLE,
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
  auditLog: api.CardAuditLogEntry[];
  ghComments: api.GitHubComment[];
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
  const [reviewBusy, setReviewBusy] = useState(false);

  const agentMap = new Map(agents.map((agent) => [agent.id, agent]));

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
    <div className="fixed inset-0 z-50 backdrop-blur-sm flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={onClose}>
      <div
        onClick={(e) => e.stopPropagation()}
        className="w-full max-w-3xl max-h-[88svh] overflow-y-auto rounded-t-3xl border p-5 sm:max-h-[90vh] sm:rounded-3xl sm:p-6 space-y-4"
        style={{
          backgroundColor: "var(--th-bg-surface)",
          borderColor: "rgba(148,163,184,0.24)",
          paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
        }}
        role="dialog" aria-modal="true" aria-label="Card details"
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <div className="flex flex-wrap items-center gap-2">
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
                {labelForStatus(selectedCard.status, tr)}
              </span>
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
                {priorityLabel(selectedCard.priority, tr)}
              </span>
              {selectedCard.github_repo && (
                <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {selectedCard.github_repo}
                </span>
              )}
            </div>
            <h3 className="mt-2 text-xl font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {selectedCard.title}
            </h3>
          </div>
          <button
            onClick={onClose}
            className="shrink-0 whitespace-nowrap rounded-xl px-3 py-2 text-sm bg-surface-medium"
            style={{ color: "var(--th-text-secondary)" }}
          >
            {tr("닫기", "Close")}
          </button>
        </div>

        <div className="grid gap-3 md:grid-cols-2">
          <label className="space-y-1">
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("제목", "Title")}</span>
            <input
              value={editor.title}
              onChange={(event) => setEditor((prev) => ({ ...prev, title: event.target.value }))}
              className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
              style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
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
              style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
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
              style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
            >
              {PRIORITY_OPTIONS.map((priority) => (
                <option key={priority} value={priority}>{priorityLabel(priority, tr)}</option>
              ))}
            </select>
          </label>
          <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("GitHub", "GitHub")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>
              {selectedCard.github_issue_url ? (
                <a href={selectedCard.github_issue_url} target="_blank" rel="noreferrer" className="hover:underline" style={{ color: "#93c5fd" }}>
                  #{selectedCard.github_issue_number ?? "-"}
                </a>
              ) : (
                selectedCard.github_issue_number ? `#${selectedCard.github_issue_number}` : "-"
              )}
            </div>
          </div>
        </div>

        {/* Blocked reason */}
        {hasManualInterventionReason(selectedCard) && selectedCard.blocked_reason && (
          <div className="rounded-2xl border p-4" style={{ backgroundColor: "rgba(239,68,68,0.08)", borderColor: "rgba(239,68,68,0.3)" }}>
            <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{ color: "#ef4444" }}>
              {tr("수동 개입 사유", "Manual Intervention Reason")}
            </div>
            <div className="text-sm" style={{ color: "#fca5a5" }}>
              {selectedCard.blocked_reason}
            </div>
          </div>
        )}

        {/* Review status */}
        {selectedCard.status === "review" && selectedCard.review_status && (
          <div className="rounded-2xl border p-4" style={{
            backgroundColor: (selectedCard.review_status === "dilemma_pending" || selectedCard.review_status === "suggestion_pending") ? "rgba(234,179,8,0.08)" : selectedCard.review_status === "improve_rework" ? "rgba(249,115,22,0.08)" : "rgba(20,184,166,0.08)",
            borderColor: (selectedCard.review_status === "dilemma_pending" || selectedCard.review_status === "suggestion_pending") ? "rgba(234,179,8,0.3)" : selectedCard.review_status === "improve_rework" ? "rgba(249,115,22,0.3)" : "rgba(20,184,166,0.3)",
          }}>
            <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{
              color: (selectedCard.review_status === "dilemma_pending" || selectedCard.review_status === "suggestion_pending") ? "#eab308" : selectedCard.review_status === "improve_rework" ? "#f97316" : "#14b8a6",
            }}>
              {tr("카운터 모델 리뷰", "Counter-Model Review")}
            </div>
            <div className="text-sm" style={{
              color: (selectedCard.review_status === "dilemma_pending" || selectedCard.review_status === "suggestion_pending") ? "#fde047" : selectedCard.review_status === "improve_rework" ? "#fdba74" : "#5eead4",
            }}>
              {selectedCard.review_status === "reviewing" && (() => {
                const reviewDispatch = dispatches.find(
                  (d) => d.parent_dispatch_id === selectedCard.latest_dispatch_id && d.dispatch_type === "review",
                );
                const verdictStatus = !reviewDispatch
                  ? tr("verdict 대기중", "verdict pending")
                  : reviewDispatch.status === "completed"
                    ? tr("verdict 전달됨", "verdict delivered")
                    : tr("verdict 미전달 — 에이전트가 아직 회신하지 않음", "verdict not delivered — agent hasn't responded");
                return <>{tr("카운터 모델이 코드를 리뷰하고 있습니다...", "Counter model is reviewing...")} <span style={{ opacity: 0.7 }}>({verdictStatus})</span></>;
              })()}
              {selectedCard.review_status === "awaiting_dod" && tr("DoD 항목이 모두 완료되면 자동 리뷰가 시작됩니다.", "Auto review starts when all DoD items are complete.")}
              {selectedCard.review_status === "improve_rework" && tr("개선 사항이 발견되어 원본 모델에 재작업을 요청했습니다.", "Improvements needed — rework dispatched to original model.")}
              {selectedCard.review_status === "suggestion_pending" && tr("카운터 모델이 검토 항목을 추출했습니다. 수용/불수용을 결정해 주세요.", "Counter model extracted review findings. Decide accept/reject for each.")}
              {selectedCard.review_status === "dilemma_pending" && tr("판단이 어려운 항목이 있습니다. 수동으로 결정해 주세요.", "Dilemma items found — manual decision needed.")}
              {selectedCard.review_status === "decided" && tr("리뷰 결정이 완료되었습니다.", "Review decision completed.")}
            </div>
          </div>
        )}

        {/* Review suggestion decision UI */}
        {(selectedCard.review_status === "suggestion_pending" || selectedCard.review_status === "dilemma_pending") && reviewData && (() => {
          const items: Array<{ id: string; category: string; summary: string; detail?: string; suggestion?: string; pros?: string; cons?: string; decision?: string }> =
            reviewData.items_json ? JSON.parse(reviewData.items_json) : [];
          const actionableItems = items.filter((i) => i.category !== "pass");
          if (actionableItems.length === 0) return null;
          const allDecided = actionableItems.every((i) => reviewDecisions[i.id]);
          return (
            <div className="rounded-2xl border p-4 space-y-4" style={{
              borderColor: "rgba(234,179,8,0.35)",
              backgroundColor: "rgba(234,179,8,0.06)",
            }}>
              <div className="flex items-center justify-between gap-2">
                <div className="text-xs font-semibold uppercase tracking-widest" style={{ color: "#eab308" }}>
                  {tr("리뷰 제안 사항", "Review Suggestions")}
                </div>
                <span className="text-xs px-2 py-0.5 rounded-full" style={{
                  backgroundColor: allDecided ? "rgba(34,197,94,0.18)" : "rgba(234,179,8,0.18)",
                  color: allDecided ? "#4ade80" : "#fde047",
                }}>
                  {Object.keys(reviewDecisions).filter((k) => actionableItems.some((d) => d.id === k)).length}/{actionableItems.length}
                </span>
              </div>
              <div className="space-y-3">
                {actionableItems.map((item) => {
                  const decision = reviewDecisions[item.id];
                  return (
                    <div key={item.id} className="rounded-xl border p-3 space-y-2" style={{
                      borderColor: decision === "accept" ? "rgba(34,197,94,0.35)" : decision === "reject" ? "rgba(239,68,68,0.35)" : "rgba(148,163,184,0.22)",
                      backgroundColor: decision === "accept" ? "rgba(34,197,94,0.06)" : decision === "reject" ? "rgba(239,68,68,0.06)" : "rgba(255,255,255,0.03)",
                    }}>
                      <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                        {item.summary}
                      </div>
                      {item.detail && (
                        <div className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                          {item.detail}
                        </div>
                      )}
                      {item.suggestion && (
                        <div className="text-xs px-2 py-1 rounded-lg" style={{ backgroundColor: "rgba(96,165,250,0.08)", color: "#93c5fd" }}>
                          {tr("제안", "Suggestion")}: {item.suggestion}
                        </div>
                      )}
                      {(item.pros || item.cons) && (
                        <div className="grid grid-cols-2 gap-2 text-xs">
                          {item.pros && (
                            <div className="px-2 py-1 rounded-lg" style={{ backgroundColor: "rgba(34,197,94,0.08)", color: "#86efac" }}>
                              {tr("장점", "Pros")}: {item.pros}
                            </div>
                          )}
                          {item.cons && (
                            <div className="px-2 py-1 rounded-lg" style={{ backgroundColor: "rgba(239,68,68,0.08)", color: "#fca5a5" }}>
                              {tr("단점", "Cons")}: {item.cons}
                            </div>
                          )}
                        </div>
                      )}
                      <div className="flex gap-2 pt-1">
                        <button
                          onClick={() => {
                            setReviewDecisions((prev) => ({ ...prev, [item.id]: "accept" }));
                            void api.saveReviewDecisions(reviewData.id, [{ item_id: item.id, decision: "accept" }]).catch(() => {});
                          }}
                          className="flex-1 rounded-lg px-3 py-1.5 text-xs font-medium border transition-colors"
                          style={{
                            borderColor: decision === "accept" ? "rgba(34,197,94,0.6)" : "rgba(148,163,184,0.28)",
                            backgroundColor: decision === "accept" ? "rgba(34,197,94,0.2)" : "transparent",
                            color: decision === "accept" ? "#4ade80" : "var(--th-text-secondary)",
                          }}
                        >
                          {tr("수용", "Accept")}
                        </button>
                        <button
                          onClick={() => {
                            setReviewDecisions((prev) => ({ ...prev, [item.id]: "reject" }));
                            void api.saveReviewDecisions(reviewData.id, [{ item_id: item.id, decision: "reject" }]).catch(() => {});
                          }}
                          className="flex-1 rounded-lg px-3 py-1.5 text-xs font-medium border transition-colors"
                          style={{
                            borderColor: decision === "reject" ? "rgba(239,68,68,0.6)" : "rgba(148,163,184,0.28)",
                            backgroundColor: decision === "reject" ? "rgba(239,68,68,0.2)" : "transparent",
                            color: decision === "reject" ? "#f87171" : "var(--th-text-secondary)",
                          }}
                        >
                          {tr("불수용", "Reject")}
                        </button>
                      </div>
                    </div>
                  );
                })}
              </div>
              <button
                disabled={!allDecided || reviewBusy}
                onClick={async () => {
                  setReviewBusy(true);
                  setActionError(null);
                  try {
                    await api.triggerDecidedRework(reviewData.id);
                    setReviewData(null);
                    setReviewDecisions({});
                  } catch (error) {
                    setActionError(error instanceof Error ? error.message : tr("재디스패치에 실패했습니다.", "Failed to trigger rework."));
                  } finally {
                    setReviewBusy(false);
                  }
                }}
                className="w-full rounded-xl px-4 py-2.5 text-sm font-medium text-white disabled:opacity-40 transition-colors"
                style={{
                  backgroundColor: allDecided ? "#eab308" : "rgba(234,179,8,0.3)",
                }}
              >
                {reviewBusy
                  ? tr("재디스패치 중...", "Dispatching rework...")
                  : allDecided
                    ? tr("결정 완료 → 재디스패치", "Decisions Complete → Dispatch Rework")
                    : tr("모든 항목에 결정을 내려주세요", "Decide all items first")}
              </button>
            </div>
          );
        })()}

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
          <div className="rounded-2xl border p-4 bg-surface-subtle space-y-3" style={{ borderColor: "var(--th-border-subtle)" }}>
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
                style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
              />
              <button
                type="button"
                onClick={() => void handleRedispatch()}
                disabled={redispatching}
                className="rounded-xl px-4 py-2 text-sm font-medium text-white disabled:opacity-50 whitespace-nowrap"
                style={{ backgroundColor: "#d97706" }}
              >
                {redispatching ? tr("전송 중...", "Sending...") : tr("재전송", "Resend")}
              </button>
            </div>
          </div>
        )}

        {canRetryCard(selectedCard) && (
          <div className="rounded-2xl border p-4 bg-surface-subtle space-y-3" style={{ borderColor: "var(--th-border-subtle)" }}>
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
                style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
              >
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                ))}
              </select>
              <button
                type="button"
                onClick={() => void handleRetryCard()}
                disabled={retryingCard || !(retryAssigneeId || selectedCard.assignee_agent_id)}
                className="rounded-xl px-4 py-2 text-sm font-medium text-white disabled:opacity-50 whitespace-nowrap"
                style={{ backgroundColor: "#7c3aed" }}
              >
                {retryingCard ? tr("전송 중...", "Sending...") : tr("재시도", "Retry")}
              </button>
            </div>
          </div>
        )}

        <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4 text-sm">
          <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("생성", "Created")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.created_at, locale)}</div>
          </div>
          <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("요청", "Requested")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.requested_at, locale)}</div>
          </div>
          <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("시작", "Started")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.started_at, locale)}</div>
          </div>
          <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("완료", "Completed")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.completed_at, locale)}</div>
          </div>
        </div>

        {/* Dispatch history — all dispatches for this card */}
        {(() => {
          const cardDispatches = dispatches
            .filter((d) => d.kanban_card_id === selectedCard.id)
            .sort((a, b) => {
              const ta = typeof a.created_at === "number" ? a.created_at : new Date(a.created_at).getTime();
              const tb = typeof b.created_at === "number" ? b.created_at : new Date(b.created_at).getTime();
              return tb - ta;
            });
          const hasAny = cardDispatches.length > 0 || selectedCard.latest_dispatch_status;
          if (!hasAny) return null;

          const dispatchStatusColor: Record<string, string> = {
            pending: "#fbbf24",
            dispatched: "#38bdf8",
            in_progress: "#f59e0b",
            completed: "#4ade80",
            failed: "#f87171",
            cancelled: "#9ca3af",
          };

          return (
            <div className="rounded-2xl border p-4 bg-surface-subtle space-y-3" style={{ borderColor: "var(--th-border-subtle)" }}>
              <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                {tr("Dispatch 이력", "Dispatch history")}
                {cardDispatches.length > 0 && (
                  <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
                    ({cardDispatches.length})
                  </span>
                )}
              </h4>
              {parseCardMetadata(selectedCard.metadata_json).timed_out_reason && (
                <div className="rounded-xl px-3 py-2 text-sm" style={{ color: "#fdba74", backgroundColor: "rgba(154,52,18,0.18)" }}>
                  {parseCardMetadata(selectedCard.metadata_json).timed_out_reason}
                </div>
              )}
              {cardDispatches.length > 0 ? (
                <div className="space-y-2 max-h-64 overflow-y-auto">
                  {cardDispatches.map((d) => (
                    <div
                      key={d.id}
                      className="rounded-xl border px-3 py-2 text-sm"
                      style={{ borderColor: "rgba(148,163,184,0.12)", backgroundColor: d.id === selectedCard.latest_dispatch_id ? "rgba(37,99,235,0.08)" : "transparent" }}
                    >
                      <div className="flex items-center gap-2 flex-wrap">
                        <span
                          className="inline-block w-2 h-2 rounded-full shrink-0"
                          style={{ backgroundColor: dispatchStatusColor[d.status] ?? "#94a3b8" }}
                        />
                        <span className="font-mono text-xs" style={{ color: "var(--th-text-muted)" }}>
                          #{d.id.slice(0, 8)}
                        </span>
                        <span
                          className="px-1.5 py-0.5 rounded text-xs font-medium"
                          style={{ backgroundColor: "rgba(148,163,184,0.12)", color: dispatchStatusColor[d.status] ?? "#94a3b8" }}
                        >
                          {d.status}
                        </span>
                        {d.dispatch_type && (
                          <span className="px-1.5 py-0.5 rounded text-xs" style={{ backgroundColor: "rgba(148,163,184,0.08)", color: "var(--th-text-secondary)" }}>
                            {d.dispatch_type}
                          </span>
                        )}
                        {d.to_agent_id && (
                          <span className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                            → {getAgentLabel(d.to_agent_id)}
                          </span>
                        )}
                      </div>
                      <div className="flex items-center gap-3 mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                        <span>{formatIso(d.created_at, locale)}</span>
                        {d.chain_depth > 0 && <span>depth {d.chain_depth}</span>}
                      </div>
                      {d.result_summary && (
                        <div className="mt-1 text-xs truncate" style={{ color: "var(--th-text-secondary)" }}>
                          {d.result_summary}
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              ) : (
                <div className="grid gap-2 md:grid-cols-2 text-sm">
                  <div>{tr("dispatch 상태", "Dispatch status")}: {selectedCard.latest_dispatch_status ?? "-"}</div>
                  <div>{tr("최신 dispatch", "Latest dispatch")}: {selectedCard.latest_dispatch_id ? `#${selectedCard.latest_dispatch_id.slice(0, 8)}` : "-"}</div>
                </div>
              )}
            </div>
          );
        })()}

        {/* State transition history (audit log) */}
        {auditLog.length > 0 && (
          <div className="rounded-2xl border p-4 bg-surface-subtle space-y-3" style={{ borderColor: "var(--th-border-subtle)" }}>
            <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
              {tr("상태 전환 이력", "State Transition History")}
              <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
                ({auditLog.length})
              </span>
            </h4>
            <div className="space-y-1.5 max-h-48 overflow-y-auto">
              {auditLog.map((log) => (
                <div key={log.id} className="flex items-center gap-2 text-xs px-2 py-1.5 rounded-lg" style={{ backgroundColor: "rgba(255,255,255,0.03)" }}>
                  <span className="shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    {formatIso(log.created_at, locale)}
                  </span>
                  <span style={{ color: TRANSITION_STYLE[log.from_status ?? ""]?.text ?? "var(--th-text-secondary)" }}>
                    {log.from_status ? labelForStatus(log.from_status as KanbanCardStatus, tr) : "—"}
                  </span>
                  <span style={{ color: "var(--th-text-muted)" }}>→</span>
                  <span style={{ color: TRANSITION_STYLE[log.to_status ?? ""]?.text ?? "var(--th-text-secondary)" }}>
                    {log.to_status ? labelForStatus(log.to_status as KanbanCardStatus, tr) : "—"}
                  </span>
                  <span className="ml-auto px-1.5 py-0.5 rounded text-xs" style={{ backgroundColor: "rgba(148,163,184,0.12)", color: "var(--th-text-muted)" }}>
                    {log.source}
                  </span>
                  {log.result && log.result !== "OK" && (
                    <span className="text-xs" style={{ color: "#f87171" }}>{log.result}</span>
                  )}
                </div>
              ))}
            </div>
          </div>
        )}

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
            <button
              onClick={handleDeleteCard}
              disabled={savingCard}
              className="rounded-xl px-4 py-2 text-sm font-medium"
              style={{ color: "#fecaca", backgroundColor: "rgba(127,29,29,0.32)" }}
            >
              {tr("카드 삭제", "Delete card")}
            </button>
            {selectedCard.status !== "done" && (
              <button
                onClick={() => setCancelConfirm({ cardIds: [selectedCard.id], source: "single" })}
                disabled={savingCard}
                className="rounded-xl px-4 py-2 text-sm font-medium"
                style={{ color: "#9ca3af", backgroundColor: "rgba(107,114,128,0.18)" }}
              >
                {tr("백로그로 되돌리기", "Move to backlog")}
              </button>
            )}
          </div>
          <div className="flex flex-col-reverse gap-2 sm:flex-row">
            <button
              onClick={onClose}
              className="rounded-xl px-4 py-2 text-sm bg-surface-medium"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {tr("닫기", "Close")}
            </button>
            <button
              onClick={() => void handleSaveCard()}
              disabled={savingCard || !editor.title.trim()}
              className="rounded-xl px-4 py-2 text-sm font-medium text-white disabled:opacity-50"
              style={{ backgroundColor: "#2563eb" }}
            >
              {savingCard ? tr("저장 중", "Saving") : tr("저장", "Save")}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
