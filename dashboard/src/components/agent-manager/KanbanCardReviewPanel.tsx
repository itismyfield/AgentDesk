import { useState } from "react";
import * as api from "../../api";
import type { KanbanReview } from "../../api";
import type {
  KanbanCard,
  TaskDispatch,
} from "../../types";
import {
  KANBAN_STATUS_TONES,
  REVIEW_STATUS_TONES,
} from "../../theme/statusTokens";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceNotice,
} from "../common/SurfacePrimitives";

interface KanbanCardReviewPanelProps {
  card: KanbanCard;
  dispatches: TaskDispatch[];
  reviewData: KanbanReview | null;
  setReviewData: React.Dispatch<React.SetStateAction<KanbanReview | null>>;
  reviewDecisions: Record<string, "accept" | "reject">;
  setReviewDecisions: React.Dispatch<React.SetStateAction<Record<string, "accept" | "reject">>>;
  setActionError: React.Dispatch<React.SetStateAction<string | null>>;
  tr: (ko: string, en: string) => string;
}

export default function KanbanCardReviewPanel({
  card,
  dispatches,
  reviewData,
  setReviewData,
  reviewDecisions,
  setReviewDecisions,
  setActionError,
  tr,
}: KanbanCardReviewPanelProps) {
  const [reviewBusy, setReviewBusy] = useState(false);

  return (
    <>
      {card.status === "review" && card.review_status && (() => {
        const reviewTone =
          card.review_status === "dilemma_pending" || card.review_status === "suggestion_pending"
            ? REVIEW_STATUS_TONES.blocked
            : card.review_status === "improve_rework"
              ? REVIEW_STATUS_TONES.rework
              : REVIEW_STATUS_TONES.review;
        return (
          <SurfaceNotice
            tone={
              (card.review_status === "dilemma_pending" || card.review_status === "suggestion_pending")
                ? "warn"
                : card.review_status === "improve_rework"
                  ? "danger"
                  : "success"
            }
            className="block"
          >
            <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{
              color: reviewTone.accent,
            }}>
              {tr("카운터 모델 리뷰", "Counter-Model Review")}
            </div>
            <div className="text-sm" style={{
              color: reviewTone.text,
            }}>
              {card.review_status === "reviewing" && (() => {
                const reviewDispatch = dispatches.find(
                  (d) => d.parent_dispatch_id === card.latest_dispatch_id && d.dispatch_type === "review",
                );
                const verdictStatus = !reviewDispatch
                  ? tr("verdict 대기중", "verdict pending")
                  : reviewDispatch.status === "completed"
                    ? tr("verdict 전달됨", "verdict delivered")
                    : tr("verdict 미전달 — 에이전트가 아직 회신하지 않음", "verdict not delivered — agent hasn't responded");
                return <>{tr("카운터 모델이 코드를 리뷰하고 있습니다...", "Counter model is reviewing...")} <span style={{ opacity: 0.7 }}>({verdictStatus})</span></>;
              })()}
              {card.review_status === "awaiting_dod" && tr("DoD 항목이 모두 완료되면 자동 리뷰가 시작됩니다.", "Auto review starts when all DoD items are complete.")}
              {card.review_status === "improve_rework" && tr("개선 사항이 발견되어 원본 모델에 재작업을 요청했습니다.", "Improvements needed — rework dispatched to original model.")}
              {card.review_status === "suggestion_pending" && tr("카운터 모델이 검토 항목을 추출했습니다. 수용/불수용을 결정해 주세요.", "Counter model extracted review findings. Decide accept/reject for each.")}
              {card.review_status === "dilemma_pending" && tr("판단이 어려운 항목이 있습니다. 수동으로 결정해 주세요.", "Dilemma items found — manual decision needed.")}
              {card.review_status === "decided" && tr("리뷰 결정이 완료되었습니다.", "Review decision completed.")}
            </div>
          </SurfaceNotice>
        );
      })()}

      {(card.review_status === "suggestion_pending" || card.review_status === "dilemma_pending") && reviewData && (() => {
        const items: Array<{ id: string; category: string; summary: string; detail?: string; suggestion?: string; pros?: string; cons?: string; decision?: string }> =
          reviewData.items_json ? JSON.parse(reviewData.items_json) : [];
        const actionableItems = items.filter((i) => i.category !== "pass");
        if (actionableItems.length === 0) return null;
        const allDecided = actionableItems.every((i) => reviewDecisions[i.id]);
        return (
          <SurfaceCard
            className="space-y-4"
            style={{
              borderColor: `${REVIEW_STATUS_TONES.blocked.accent}59`,
              backgroundColor: "rgba(234,179,8,0.06)",
            }}
          >
            <div className="flex items-center justify-between gap-2">
              <div className="text-xs font-semibold uppercase tracking-widest" style={{ color: REVIEW_STATUS_TONES.blocked.accent }}>
                {tr("리뷰 제안 사항", "Review Suggestions")}
              </div>
              <span className="text-xs px-2 py-0.5 rounded-full" style={{
                backgroundColor: allDecided ? KANBAN_STATUS_TONES.done.bg : "rgba(234,179,8,0.18)",
                color: allDecided ? KANBAN_STATUS_TONES.done.text : REVIEW_STATUS_TONES.blocked.text,
              }}>
                {Object.keys(reviewDecisions).filter((k) => actionableItems.some((d) => d.id === k)).length}/{actionableItems.length}
              </span>
            </div>
            <div className="space-y-3">
              {actionableItems.map((item) => {
                const decision = reviewDecisions[item.id];
                return (
                  <SurfaceCard key={item.id} className="space-y-2 p-3" style={{
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
                      <SurfaceActionButton
                        onClick={() => {
                          setReviewDecisions((prev) => ({ ...prev, [item.id]: "accept" }));
                          void api.saveReviewDecisions(reviewData.id, [{ item_id: item.id, decision: "accept" }]).catch(() => {});
                        }}
                        tone={decision === "accept" ? "success" : "neutral"}
                        className="flex-1"
                        style={{
                          color: decision === "accept" ? "#4ade80" : "var(--th-text-secondary)",
                        }}
                      >
                        {tr("수용", "Accept")}
                      </SurfaceActionButton>
                      <SurfaceActionButton
                        onClick={() => {
                          setReviewDecisions((prev) => ({ ...prev, [item.id]: "reject" }));
                          void api.saveReviewDecisions(reviewData.id, [{ item_id: item.id, decision: "reject" }]).catch(() => {});
                        }}
                        tone={decision === "reject" ? "danger" : "neutral"}
                        className="flex-1"
                        style={{
                          color: decision === "reject" ? "#f87171" : "var(--th-text-secondary)",
                        }}
                      >
                        {tr("불수용", "Reject")}
                      </SurfaceActionButton>
                    </div>
                  </SurfaceCard>
                );
              })}
            </div>
            <SurfaceActionButton
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
              tone="warn"
              className="w-full py-2.5 text-sm"
              style={{
                color: allDecided ? "#fef3c7" : "var(--th-text-muted)",
              }}
            >
              {reviewBusy
                ? tr("재디스패치 중...", "Dispatching rework...")
                : allDecided
                  ? tr("결정 완료 → 재디스패치", "Decisions Complete → Dispatch Rework")
                  : tr("모든 항목에 결정을 내려주세요", "Decide all items first")}
            </SurfaceActionButton>
          </SurfaceCard>
        );
      })()}
    </>
  );
}
