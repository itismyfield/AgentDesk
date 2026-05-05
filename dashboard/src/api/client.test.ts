import { describe, expect, it } from "vitest";

import { formatAssignmentTransitionWarning } from "./client";

describe("formatAssignmentTransitionWarning", () => {
  it("ignores successful or unattempted transitions", () => {
    expect(formatAssignmentTransitionWarning(undefined)).toBeNull();
    expect(formatAssignmentTransitionWarning({ attempted: false, ok: true })).toBeNull();
    expect(formatAssignmentTransitionWarning({ attempted: true, ok: true })).toBeNull();
  });

  it("formats partial-success transition failures as dashboard warnings", () => {
    expect(
      formatAssignmentTransitionWarning({
        attempted: true,
        ok: false,
        failed_step: "requested",
        error: "transition from done to requested is not allowed.",
      }),
    ).toBe(
      "Assignment succeeded, but transition failed: transition from done to requested is not allowed. Next action: inspect the pipeline or transition manually.",
    );
  });

  it("includes explicit next_action when the API provides one", () => {
    expect(
      formatAssignmentTransitionWarning({
        attempted: true,
        ok: false,
        failed_step: "requested",
        next_action: "inspect_pipeline_or_transition_manually",
      }),
    ).toBe(
      "Assignment succeeded, but transition failed: failed at step requested. Next action: inspect_pipeline_or_transition_manually.",
    );
  });
});
