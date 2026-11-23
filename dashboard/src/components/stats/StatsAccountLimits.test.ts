import { expect, it } from "vitest";
import { accountLimits } from "./StatsAccountLimits";
import { msg } from "./statsModel";

const t = (messages: ReturnType<typeof msg>) => messages.ko;

it("keys each account window apart when cswap repeats an email and says when it resets", () => {
  const rows = accountLimits({
    accounts: [
      { number: 2, email: "a@x", usage: { fiveHour: null, sevenDay: { pct: 99.6, resetsAt: "2026-11-23T02:30:00Z" } } },
      { number: 5, email: "a@x", active: true },
    ],
  }, t, Date.parse("2026-11-23T00:00:00Z"));
  expect(rows.map((row) => [row.group, row.label, row.used, row.detail])).toEqual([
    ["a@x · #2", "5시간", null, undefined],
    ["a@x · #2", "7일", 100, "2시간 30분 후 초기화"],
    ["a@x · #5 · 사용 중", "5시간", null, undefined],
    ["a@x · #5 · 사용 중", "7일", null, undefined],
  ]);
});
