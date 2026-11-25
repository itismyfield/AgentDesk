import { describe, expect, it } from "vitest";
import { aggregateDegradedReasons, describeDegradedReason } from "./healthReasons";

describe("describeDegradedReason", () => {
  it("translates known runtime keys without leaking 19-digit snowflakes", () => {
    expect(describeDegradedReason("db_slow", true)).toBe("데이터베이스 응답 지연");
    expect(describeDegradedReason("db_slow", false)).toBe("Database response slow");
    expect(describeDegradedReason("provider:codex:pending_queue_depth:2", true)).toBe("codex 처리 대기 2건");
    expect(describeDegradedReason("relay_verdict_unknown_antigravity_1490141479707086938", true)).toBe(
      "antigravity 응답 전달 확인 안 됨",
    );
    expect(describeDegradedReason("tui_o:reader_retention_stalled:1479671298497183835", true)).toBe(
      "TUI 리더 보존 지연",
    );
    expect(
      describeDegradedReason("turn_presence_unknown:claude:1490141479707086938:binding_unreadable:none:1:61", true),
    ).toBe("claude 턴 상태 미확인 세션");
    expect(
      describeDegradedReason(
        "turn_presence_unknown:codex:adk-dashboard:session_identity_unresolved:exact_canonical_mismatch:1:11328",
        true,
      ),
    ).toBe("adk-dashboard 턴 상태 확인 불가 (codex)");
  });
});

describe("aggregateDegradedReasons", () => {
  it("collapses repeated turn_presence_unknown signals per provider and strips snowflakes", () => {
    const claudeReasons = Array.from(
      { length: 20 },
      (_, idx) => `turn_presence_unknown:claude:14901414797070869${String(idx).padStart(2, "0")}:session_identity_unresolved:none:1:90`,
    );
    const codexReasons = Array.from(
      { length: 5 },
      (_, idx) => `turn_presence_unknown:codex:lane-${idx}:session_identity_unresolved:none:1:45`,
    );
    const aggregated = aggregateDegradedReasons([...claudeReasons, ...codexReasons], true);
    expect(aggregated.map((item) => [item.summary, item.count])).toEqual([
      ["claude 턴 상태 미확인 세션 20개", 20],
      ["codex 턴 상태 미확인 세션 5개", 5],
    ]);
    expect(aggregated.some((item) => /\d{15,}/.test(item.summary))).toBe(false);
  });

  it("merges duplicate relay_verdict_unknown labels into a single row with joined rawKey tooltip", () => {
    const rows = aggregateDegradedReasons(
      [
        "relay_verdict_unknown_antigravity_1490141479707086938",
        "relay_verdict_unknown_antigravity_1490141479707086939",
      ],
      true,
    );
    expect(rows).toEqual([
      {
        groupKey: "label:antigravity 응답 전달 확인 안 됨",
        label: "antigravity 응답 전달 확인 안 됨",
        summary: "antigravity 응답 전달 확인 안 됨 2건",
        count: 2,
        rawKeys: [
          "relay_verdict_unknown_antigravity_1490141479707086938",
          "relay_verdict_unknown_antigravity_1490141479707086939",
        ],
        rawKey:
          "relay_verdict_unknown_antigravity_1490141479707086938, relay_verdict_unknown_antigravity_1490141479707086939",
      },
    ]);
  });
});
