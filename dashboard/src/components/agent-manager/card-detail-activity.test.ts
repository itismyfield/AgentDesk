import { describe, expect, it } from "vitest";

import {
  formatAuditResult,
  formatDispatchSummary,
} from "./card-detail-activity";

const tr = (_ko: string, en: string) => en;
const trKo = (ko: string, _en: string) => ko;

describe("card-detail-activity helpers", () => {
  it("keeps multi-line dispatch summaries readable", () => {
    expect(formatDispatchSummary("  first line\n\n second line  ")).toBe(
      "first line\nsecond line",
    );
  });

  it("formats review-decision comments from audit JSON", () => {
    expect(
      formatAuditResult(
        JSON.stringify({ decision: "accept", comment: "Ship the fix after review" }),
        tr,
      ),
    ).toEqual({
      text: "Accepted review feedback: Ship the fix after review",
      tone: "default",
    });
  });

  it("formats PM rework reasons from audit JSON", () => {
    expect(
      formatAuditResult(
        JSON.stringify({ pm_decision: "rework", comment: "Handle the retry edge case" }),
        tr,
      ),
    ).toEqual({
      text: "PM requested rework: Handle the retry edge case",
      tone: "warn",
    });
  });

  it("keeps warning tone under Korean translations", () => {
    expect(
      formatAuditResult(
        JSON.stringify({ pm_decision: "rework", comment: "재시도 엣지 케이스를 처리" }),
        trKo,
      ),
    ).toEqual({
      text: "PM 재작업 요청: 재시도 엣지 케이스를 처리",
      tone: "warn",
    });
  });

  it("humanizes cancellation reason codes", () => {
    expect(
      formatAuditResult(
        JSON.stringify({ reason: "auto_cancelled_on_terminal_card" }),
        tr,
      ),
    ).toEqual({
      text: "Auto-cancelled during terminal cleanup",
      tone: "warn",
    });
  });

  it("humanizes force transition audit markers", () => {
    expect(formatAuditResult("OK (force)", tr)).toEqual({
      text: "Forced transition",
      tone: "warn",
    });
  });

  it("suppresses plain OK audit results", () => {
    expect(formatAuditResult("OK", tr)).toBeNull();
  });
});
