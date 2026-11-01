import { describe, expect, it } from "vitest";
import { parsePgTimestamp, pgTimestampToIso } from "./pgTimestamp";

describe("PG timestamps", () => {
  it("rewrites PG text as ISO and leaves ISO and dates alone", () => {
    expect(pgTimestampToIso("2026-11-01 11:05:23.123456+09")).toBe("2026-11-01T11:05:23.123456+09:00");
    expect(pgTimestampToIso("2026-11-01 11:05:23-03")).toBe("2026-11-01T11:05:23-03:00");
    expect(pgTimestampToIso("2026-11-01T02:05:23Z")).toBe("2026-11-01T02:05:23Z");
    expect(pgTimestampToIso("2026-11-01T11:05:23+09:00")).toBe("2026-11-01T11:05:23+09:00");
    expect(pgTimestampToIso("2026-11-01")).toBe("2026-11-01");
  });

  it("reads PG text and ISO to the same instant, and gives null for unreadable text", () => {
    expect(parsePgTimestamp("2026-11-01 11:05:23.123456+09")).toBe(Date.UTC(2026, 10, 1, 2, 5, 23, 123));
    expect(parsePgTimestamp("2026-11-01T02:05:23.123Z")).toBe(Date.UTC(2026, 10, 1, 2, 5, 23, 123));
    expect(parsePgTimestamp("not a time")).toBeNull();
    expect(parsePgTimestamp(null)).toBeNull();
  });
});
