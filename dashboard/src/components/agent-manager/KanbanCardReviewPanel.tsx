import { useState } from "react";
import { Check, X } from "lucide-react";
import * as api from "../../api";
import type { KanbanReview } from "../../api";
import type {
  KanbanCard,
  TaskDispatch,
} from "../../types";
import { Badge, type BadgeTone } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { DetailHeading, KIT_TOUCH_BUTTON } from "./CardDetailParts";

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

type ReviewItem = { id: string; category: string; summary: string; detail?: string; suggestion?: string; pros?: string; cons?: string; decision?: string };

const NOTICE_CLASS: Record<"info" | "warning" | "danger" | "success", string> = {
  info: "bg-[color-mix(in_oklch,var(--info)_12%,transparent)] shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--info)_40%,transparent)]",
  warning: "bg-[color-mix(in_oklch,var(--warn)_12%,transparent)] shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--warn)_40%,transparent)]",
  danger: "bg-[color-mix(in_oklch,var(--err)_12%,transparent)] shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--err)_40%,transparent)]",
  success: "bg-[color-mix(in_oklch,var(--ok)_12%,transparent)] shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--ok)_40%,transparent)]",
};

function parseItems(itemsJson: string | null): ReviewItem[] {
  if (!itemsJson) return [];
  try {
    const parsed = JSON.parse(itemsJson) as unknown;
    return Array.isArray(parsed) ? (parsed as ReviewItem[]) : [];
  } catch {
    return [];
  }
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
  const pending = card.review_status === "dilemma_pending" || card.review_status === "suggestion_pending";
  const tone: "warning" | "danger" | "success" = pending ? "warning" : card.review_status === "improve_rework" ? "danger" : "success";

  const decide = (item: ReviewItem, decision: "accept" | "reject") => {
    if (!reviewData) return;
    setReviewDecisions((prev) => ({ ...prev, [item.id]: decision }));
    void api.saveReviewDecisions(reviewData.id, [{ item_id: item.id, decision }]).catch(() => {});
  };

  const statusLine = (() => {
    switch (card.review_status) {
      case "reviewing": {
        const reviewDispatch = dispatches.find(
          (d) => d.parent_dispatch_id === card.latest_dispatch_id && d.dispatch_type === "review",
        );
        const verdictStatus = !reviewDispatch
          ? tr("verdict 대기중", "verdict pending")
          : reviewDispatch.status === "completed"
            ? tr("verdict 전달됨", "verdict delivered")
            : tr("verdict 미전달 — 에이전트가 아직 회신하지 않음", "verdict not delivered — agent hasn't responded");
        return <>{tr("카운터 모델이 코드를 리뷰하고 있습니다...", "Counter model is reviewing...")} <span className="text-th-text-muted">({verdictStatus})</span></>;
      }
      case "improve_rework": return tr("개선 사항이 발견되어 원본 모델에 재작업을 요청했습니다.", "Improvements needed — rework dispatched to original model.");
      case "suggestion_pending": return tr("카운터 모델이 검토 항목을 추출했습니다. 수용/불수용을 결정해 주세요.", "Counter model extracted review findings. Decide accept/reject for each.");
      case "dilemma_pending": return tr("판단이 어려운 항목이 있습니다. 수동으로 결정해 주세요.", "Dilemma items found — manual decision needed.");
      case "decided": return tr("리뷰 결정이 완료되었습니다.", "Review decision completed.");
      default: return null;
    }
  })();

  const actionableItems = pending && reviewData ? parseItems(reviewData.items_json).filter((item) => item.category !== "pass") : [];
  const decidedCount = actionableItems.filter((item) => reviewDecisions[item.id]).length;
  const allDecided = actionableItems.length > 0 && decidedCount === actionableItems.length;

  return (
    <>
      {card.status === "review" && card.review_status && (
        <section role="status" className={`flex min-w-0 flex-col gap-1.5 rounded-xl px-4 py-3 ${NOTICE_CLASS[tone]}`} data-testid="kanban-review-status">
          <div className="flex items-center gap-2">
            <Badge tone={tone as BadgeTone} dot live={card.review_status === "reviewing"}>{tr("카운터 모델 리뷰", "Counter-Model Review")}</Badge>
          </div>
          {statusLine && <p className="m-0 text-sm leading-5 text-th-text-primary [word-break:keep-all]">{statusLine}</p>}
        </section>
      )}

      {actionableItems.length > 0 && reviewData && (
        <section className="flex min-w-0 flex-col gap-3" aria-labelledby="kanban-review-suggestions" data-testid="kanban-review-suggestions">
          <DetailHeading id="kanban-review-suggestions" aside={<Badge tone={allDecided ? "success" : "warning"}>{`${decidedCount}/${actionableItems.length}`}</Badge>}>
            {tr("리뷰 제안 사항", "Review Suggestions")}
          </DetailHeading>
          <ul className="m-0 flex list-none flex-col gap-2.5 p-0">
            {actionableItems.map((item) => {
              const decision = reviewDecisions[item.id];
              return (
                <li key={item.id} className={`flex min-w-0 flex-col gap-2 rounded-xl px-3.5 py-3 ${decision === "accept" ? NOTICE_CLASS.success : decision === "reject" ? NOTICE_CLASS.danger : "bg-surface-subtle shadow-[inset_0_0_0_0.5px_var(--th-border-subtle)]"}`}>
                  <p className="m-0 text-sm font-medium text-th-text-heading [overflow-wrap:anywhere]">{item.summary}</p>
                  {item.detail && <p className="m-0 text-xs leading-5 text-th-text-secondary [overflow-wrap:anywhere]">{item.detail}</p>}
                  {item.suggestion && (
                    <p className="m-0 rounded-lg bg-surface-subtle px-2.5 py-1.5 text-xs leading-5 text-th-text-primary [overflow-wrap:anywhere]">
                      <span className="font-medium">{tr("제안", "Suggestion")}:</span> {item.suggestion}
                    </p>
                  )}
                  {(item.pros || item.cons) && (
                    <div className="grid gap-2 text-xs sm:grid-cols-2">
                      {item.pros && <p className={`m-0 rounded-lg px-2.5 py-1.5 leading-5 text-th-text-primary ${NOTICE_CLASS.success}`}>{tr("장점", "Pros")}: {item.pros}</p>}
                      {item.cons && <p className={`m-0 rounded-lg px-2.5 py-1.5 leading-5 text-th-text-primary ${NOTICE_CLASS.danger}`}>{tr("단점", "Cons")}: {item.cons}</p>}
                    </div>
                  )}
                  <div className="flex gap-2 pt-1">
                    <Button variant={decision === "accept" ? "primary" : "secondary"} size="lg" className={`flex-1 ${KIT_TOUCH_BUTTON}`} icon={<Check />}
                      aria-pressed={decision === "accept"} onClick={() => decide(item, "accept")}>
                      {tr("수용", "Accept")}
                    </Button>
                    <Button variant={decision === "reject" ? "destructive" : "secondary"} size="lg" className={`flex-1 ${KIT_TOUCH_BUTTON}`} icon={<X />}
                      aria-pressed={decision === "reject"} onClick={() => decide(item, "reject")}>
                      {tr("불수용", "Reject")}
                    </Button>
                  </div>
                </li>
              );
            })}
          </ul>
          <Button
            variant="primary"
            size="lg"
            className={`w-full ${KIT_TOUCH_BUTTON}`}
            disabled={!allDecided}
            loading={reviewBusy}
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
          >
            {reviewBusy
              ? tr("재디스패치 중...", "Dispatching rework...")
              : allDecided
                ? tr("결정 완료 → 재디스패치", "Decisions Complete → Dispatch Rework")
                : tr("모든 항목에 결정을 내려주세요", "Decide all items first")}
          </Button>
        </section>
      )}
    </>
  );
}
