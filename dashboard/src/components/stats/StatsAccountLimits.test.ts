import { expect, it } from "vitest";
import { accountLimits, claudeWindowLabel, maskAccountEmail } from "./StatsAccountLimits";
import { msg } from "./statsModel";

const t = (messages: ReturnType<typeof msg>) => messages.ko;

it("keys each account window apart, explains unmeasured slots, masks emails, and maps Fable", () => {
  expect([maskAccountEmail("operator.long@example.com"), claudeWindowLabel("7d Fable", t)]).toEqual(["oper***@example.com", "7일 상위 모델 (Fable)"]);
  const rows = accountLimits({
    accounts: [
      { number: 2, email: "operator.long@example.com", usage: { fiveHour: null, sevenDay: { pct: 99.6, resetsAt: "2026-11-23T02:30:00Z" } } },
      { number: 5, email: "operator.long@example.com", active: true },
    ],
  }, t, Date.parse("2026-11-23T00:00:00Z"), { maskEmail: true });
  expect(rows.map((row) => [row.group, row.label, row.used, row.detail])).toEqual([
    ["oper***@example.com · #2", "5시간", null, "최근 호출 없음 · 첫 요청 후 집계돼요"],
    ["oper***@example.com · #2", "7일", 100, "2시간 30분 후 초기화"],
    ["oper***@example.com · #5 · 사용 중", "5시간", null, "최근 호출 없음 · 첫 요청 후 집계돼요"],
    ["oper***@example.com · #5 · 사용 중", "7일", null, "최근 호출 없음 · 첫 요청 후 집계돼요"],
  ]);
});
