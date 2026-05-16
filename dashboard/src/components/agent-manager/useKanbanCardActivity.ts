import { useCallback, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import * as api from "../../api";
import type { KanbanReview } from "../../api";
import type { KanbanCard } from "../../types";

const CARD_COMMENTS_STALE_MS = 5 * 60_000;

const kanbanCardActivityQueryKey = (cardId: string) =>
  ["kanban", "card", cardId, "activity"] as const;
const kanbanCardAuditLogQueryKey = (cardId: string) =>
  [...kanbanCardActivityQueryKey(cardId), "audit-log"] as const;
const kanbanCardGitHubCommentsQueryKey = (cardId: string) =>
  [...kanbanCardActivityQueryKey(cardId), "github-comments"] as const;
const kanbanCardReviewsQueryKey = (cardId: string) =>
  [...kanbanCardActivityQueryKey(cardId), "reviews"] as const;

interface UseKanbanCardActivityOptions {
  selectedCard: KanbanCard | null;
  selectedCardId: string | null;
}

export function latestActionableReview(reviews: KanbanReview[]): KanbanReview | null {
  return reviews
    .filter((review) =>
      review.verdict === "improve" ||
      review.verdict === "dilemma" ||
      review.verdict === "mixed" ||
      review.verdict === "decided",
    )
    .sort((a, b) => b.round - a.round)[0] ?? null;
}

export function reviewDecisionMap(review: KanbanReview | null): Record<string, "accept" | "reject"> {
  if (!review?.items_json) return {};
  try {
    const items = JSON.parse(review.items_json) as Array<{
      id: string;
      category: string;
      decision?: string;
    }>;
    const decisions: Record<string, "accept" | "reject"> = {};
    for (const item of items) {
      if (item.decision === "accept" || item.decision === "reject") {
        decisions[item.id] = item.decision;
      }
    }
    return decisions;
  } catch {
    return {};
  }
}

export function useKanbanCardActivity({
  selectedCard,
  selectedCardId,
}: UseKanbanCardActivityOptions) {
  const queryClient = useQueryClient();
  const [activityRefreshTick, setActivityRefreshTick] = useState(0);
  const selectedCardNeedsReviewData =
    selectedCard?.review_status === "suggestion_pending" ||
    selectedCard?.review_status === "dilemma_pending" ||
    selectedCard?.review_status === "decided";
  const auditLogQuery = useQuery({
    queryKey: selectedCardId
      ? kanbanCardAuditLogQueryKey(selectedCardId)
      : ["kanban", "card", "none", "activity", "audit-log"],
    queryFn: () => api.getCardAuditLog(selectedCardId!),
    enabled: Boolean(selectedCardId),
    staleTime: 60_000,
  });
  const githubCommentsQuery = useQuery({
    queryKey: selectedCardId
      ? kanbanCardGitHubCommentsQueryKey(selectedCardId)
      : ["kanban", "card", "none", "activity", "github-comments"],
    queryFn: () => api.getCardGitHubComments(selectedCardId!),
    enabled: Boolean(selectedCardId && selectedCard?.github_issue_number),
    staleTime: CARD_COMMENTS_STALE_MS,
  });
  const reviewsQuery = useQuery({
    queryKey: selectedCardId
      ? kanbanCardReviewsQueryKey(selectedCardId)
      : ["kanban", "card", "none", "activity", "reviews"],
    queryFn: () => api.getKanbanReviews(selectedCardId!),
    enabled: Boolean(selectedCardId && selectedCardNeedsReviewData),
    staleTime: 30_000,
  });
  const reviewData = useMemo(
    () => latestActionableReview(reviewsQuery.data ?? []),
    [reviewsQuery.data],
  );
  const invalidateCardActivity = useCallback((cardId: string) => {
    void queryClient.invalidateQueries({ queryKey: kanbanCardActivityQueryKey(cardId) });
    if (selectedCardId === cardId) {
      setActivityRefreshTick((prev) => prev + 1);
    }
  }, [queryClient, selectedCardId]);
  const clearCardReviews = useCallback((cardId: string) => {
    queryClient.setQueryData(kanbanCardReviewsQueryKey(cardId), []);
  }, [queryClient]);

  return {
    activityRefreshTick,
    auditLog: auditLogQuery.data ?? [],
    clearCardReviews,
    ghComments: githubCommentsQuery.data?.comments ?? [],
    githubIssueBody: githubCommentsQuery.data?.body,
    invalidateCardActivity,
    reviewData,
  };
}
