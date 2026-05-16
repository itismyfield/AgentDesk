import { describe, expect, it } from "vitest";

import type { KanbanReview } from "../../api";
import { latestActionableReview, reviewDecisionMap } from "./useKanbanCardActivity";

function makeReview(overrides: Partial<KanbanReview> = {}): KanbanReview {
  return {
    id: "review-1",
    card_id: "card-1",
    round: 1,
    original_dispatch_id: null,
    original_agent_id: null,
    original_provider: null,
    review_dispatch_id: null,
    reviewer_agent_id: null,
    reviewer_provider: null,
    verdict: "pass",
    items_json: null,
    github_comment_id: null,
    created_at: 1,
    completed_at: null,
    ...overrides,
  };
}

describe("kanban card activity helpers", () => {
  it("returns the newest actionable review", () => {
    const result = latestActionableReview([
      makeReview({ id: "pass", round: 3, verdict: "pass" }),
      makeReview({ id: "older", round: 1, verdict: "improve" }),
      makeReview({ id: "newer", round: 2, verdict: "decided" }),
    ]);

    expect(result?.id).toBe("newer");
  });

  it("maps accepted and rejected review item decisions", () => {
    const result = reviewDecisionMap(makeReview({
      items_json: JSON.stringify([
        { id: "a", category: "bug", decision: "accept" },
        { id: "b", category: "style", decision: "reject" },
        { id: "c", category: "note", decision: "open" },
      ]),
    }));

    expect(result).toEqual({ a: "accept", b: "reject" });
  });
});
