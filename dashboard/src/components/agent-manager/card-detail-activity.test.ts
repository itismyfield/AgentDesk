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

import {
  buildCardActivity,
  cardActivityDayLabel,
  countCardActivity,
  filterCardActivity,
  groupCardActivityByDay,
} from "./card-detail-activity";
import type { CardAuditLogEntry, GitHubComment } from "../../api";
import type { TaskDispatch } from "../../types";

describe("unified card activity", () => {
  const base = Date.parse("2026-11-17T10:00:00Z");
  const dispatch = (id: string, at: number, patch: Partial<TaskDispatch> = {}): TaskDispatch => ({
    id, kanban_card_id: "card-1", from_agent_id: "pipeline", to_agent_id: "agent-ada", dispatch_type: "implementation", status: "completed",
    title: "", context_file: null, result_file: null, result_summary: null, parent_dispatch_id: null, chain_depth: 0, created_at: at,
    dispatched_at: at, completed_at: null, ...patch,
  });
  const audit = (id: number, at: number | null, from: string, to: string, result: string | null = null): CardAuditLogEntry => ({
    id, card_id: "card-1", from_status: from, to_status: to, source: "pipeline", result, created_at: at == null ? null : new Date(at).toISOString(),
  });
  const comment = (at: number, body: string, login = "luna"): GitHubComment => ({ author: { login }, body, createdAt: new Date(at).toISOString() });
  const records = buildCardActivity({
    card: { id: "card-1", latest_dispatch_id: "d-2" },
    dispatches: [
      dispatch("d-1", base - 3 * 3_600_000, { status: "failed", result_summary: " tmux 세션 없음 \n\n 재시도 필요 " }),
      dispatch("d-2", base - 60_000, { status: "in_progress", title: "[구현] 워처 재부착" }),
      dispatch("other", base, { kanban_card_id: "card-2" }),
    ],
    auditLog: [audit(1, base - 2 * 3_600_000, "in_progress", "blocked", "BLOCKED: tmux 없음"), audit(2, null, "blocked", "in_progress")],
    ghComments: [
      comment(base - 30 * 60_000, "리뷰했습니다. blocking finding 1건\n\n1. `src/a.rs:1` 문제"),
      comment(base - 10 * 60_000, "상태를 review로 변경했습니다", "ada"),
      comment(base - 9 * 60_000, "우선순위를 high로 변경했습니다", "ada"),
    ],
    tr,
    getAgentLabel: (id) => (id === "agent-ada" ? "Ada" : id ?? ""),
  });

  it("merges the card's dispatches, transitions and comments newest first, timeless records last", () => {
    expect(records.map((record) => record.id)).toEqual([
      "dispatch:d-2", expect.stringMatching(/^comment:ada-/), expect.stringMatching(/^comment:luna-/), "status:1", "dispatch:d-1", "status:2",
    ]);
  });

  it("describes each source in one sentence with an actor and a status badge", () => {
    const latest = records[0];
    expect(latest).toMatchObject({ source: "dispatch", text: "[구현] 워처 재부착", actor: "Ada", badge: { label: "In progress", tone: "info", live: true } });
    expect(latest.detail).toMatchObject({ kind: "dispatch", latest: true });
    const failed = records.find((record) => record.id === "dispatch:d-1")!;
    expect(failed).toMatchObject({ text: "implementation → Ada", badge: { label: "Failed", tone: "danger" } });
    expect(failed.detail).toMatchObject({ summary: "tmux 세션 없음\n재시도 필요" });
    const blocked = records.find((record) => record.id === "status:1")!;
    expect(blocked).toMatchObject({ text: "In Progress → Blocked", actor: "pipeline", sub: "Blocked: tmux 없음", badge: { tone: "danger" } });
    const review = records.find((record) => record.source === "review")!;
    expect(review).toMatchObject({ actor: "luna", badge: { label: "Changes requested", tone: "warning" } });
    expect(review.text).toMatch(/^리뷰 피드백: /);
    const coalesced = records.find((record) => record.source === "general")!;
    expect(coalesced.text).toMatch(/^2 changes · /);
    expect(coalesced.badge).toMatchObject({ label: "Coalesced" });
  });

  it("counts before searching and filters by source, then by sentence, actor and second fact", () => {
    expect(countCardActivity(records)).toEqual({ all: 6, status: 2, dispatch: 2, review: 1, pm: 0, work: 0, general: 1 });
    expect(filterCardActivity(records, "dispatch", "", tr).map((record) => record.id)).toEqual(["dispatch:d-2", "dispatch:d-1"]);
    expect(filterCardActivity(records, "all", "  ADA ", tr)).toHaveLength(3);
    expect(filterCardActivity(records, "status", "tmux", tr).map((record) => record.id)).toEqual(["status:1"]);
    expect(filterCardActivity(records, "review", "nothing like this", tr)).toEqual([]);
  });

  it("groups by local day and names today, yesterday and older days", () => {
    const groups = groupCardActivityByDay(records);
    expect(groups.at(-1)).toMatchObject({ key: null });
    expect(groups.reduce((sum, group) => sum + group.items.length, 0)).toBe(records.length);
    const now = new Date(2026, 10, 17, 9).getTime();
    expect(cardActivityDayLabel(new Date(2026, 10, 17).getTime(), now, "en", tr)).toBe("Today");
    expect(cardActivityDayLabel(new Date(2026, 10, 16).getTime(), now, "ko", trKo)).toBe("어제");
    expect(cardActivityDayLabel(new Date(2026, 10, 12).getTime(), now, "en", tr)).toMatch(/Nov(ember)? 12/);
    expect(cardActivityDayLabel(null, now, "ko", trKo)).toBe("시각 없음");
  });
});
