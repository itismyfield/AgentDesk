import { describe, expect, it } from "vitest";

import { buildBottlenecks, describeDegradedReason } from "./OpsPageModel";

describe("describeDegradedReason", () => {
  it("names provider and outbox reasons in Korean", () => {
    expect(describeDegradedReason("provider:codex:pending_queue_depth:2", true)).toBe("codex 처리 대기 2건");
    expect(describeDegradedReason("dispatch_outbox_oldest_pending_age:61", true)).toBe("전달 대기 1분 1초째");
  });

  it("hides channel ids and session internals behind readable labels", () => {
    expect(describeDegradedReason("tui_o:boundary_pending:1509350778043895902", true)).toBe("TUI 출력 마무리 대기");
    expect(describeDegradedReason("relay_verdict_unknown_claude_1490141479707086938", true)).toBe("claude 응답 전달 확인 안 됨");
    expect(
      describeDegradedReason("turn_presence_unknown:codex:adk-dashboard:session_identity_unresolved:exact_canonical_mismatch:1:11328", true),
    ).toBe("adk-dashboard 턴 상태 확인 불가 (codex)");
    expect(describeDegradedReason("brand_new_reason:1234567890123", false)).toBe("brand new reason");
  });
});

describe("buildBottlenecks", () => {
  it("keeps the raw degraded key as the row id and a readable label", () => {
    const rows = buildBottlenecks(
      { status: "degraded", degraded_reasons: ["tui_o:unresolved:1479671298497183835"] },
      true,
    );
    expect(rows).toEqual([
      expect.objectContaining({ kind: "tui_o:unresolved:1479671298497183835", label: "TUI 출력 확인 필요" }),
    ]);
  });
});
