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

  it("ignores freeform comment keywords when semantic fields are non-warning", () => {
    expect(
      formatAuditResult(
        JSON.stringify({ decision: "accept", comment: "error keyword should not drive tone" }),
        tr,
      ),
    ).toEqual({
      text: "Accepted review feedback: error keyword should not drive tone",
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

  it("defaults detail-only audit JSON to neutral tone", () => {
    expect(
      formatAuditResult(
        JSON.stringify({ comment: "error keyword should stay neutral without structured fields" }),
        tr,
      ),
    ).toEqual({
      text: "error keyword should stay neutral without structured fields",
      tone: "default",
    });
  });

  it("prefers success status codes over freeform warning keywords", () => {
    expect(
      formatAuditResult(
        JSON.stringify({
          status: "approved",
          message: "blocked error keywords should not override approved status",
        }),
        tr,
      ),
    ).toEqual({
      text: "blocked error keywords should not override approved status",
      tone: "default",
    });
  });

  it("keeps locale-specific warning text neutral when status is successful", () => {
    expect(
      formatAuditResult(
        JSON.stringify({
          status: "approved",
          message: "차단 또는 실패라는 단어가 있어도 승인 상태가 우선",
        }),
        trKo,
      ),
    ).toEqual({
      text: "차단 또는 실패라는 단어가 있어도 승인 상태가 우선",
      tone: "default",
    });
  });

  it("keeps failure status codes dangerous even with neutral detail", () => {
    expect(
      formatAuditResult(
        JSON.stringify({
          status: "failed",
          message: "Looks good on manual inspection",
        }),
        tr,
      ),
    ).toEqual({
      text: "Looks good on manual inspection",
      tone: "danger",
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

  it("keeps blocked machine markers as danger", () => {
    expect(formatAuditResult("BLOCKED: waiting for reviewer", tr)).toEqual({
      text: "Blocked: waiting for reviewer",
      tone: "danger",
    });
  });

  it("suppresses plain OK audit results", () => {
    expect(formatAuditResult("OK", tr)).toBeNull();
  });
});
