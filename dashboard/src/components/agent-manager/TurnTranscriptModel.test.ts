import { describe, expect, it } from "vitest";
import type { SessionTranscript, SessionTranscriptEvent } from "../../api";
import { eventBodyLabel, formatAxisOffset, orderTicks, timeAxis, turnSpans } from "./TurnTranscriptModel";

const tr = (_ko: string, en: string) => en;
const transcript = (turn: string, createdAt: string, durationMs: number | null): SessionTranscript => ({
  id: 1, turn_id: turn, session_key: null, channel_id: null, agent_id: null, provider: "codex", dispatch_id: null, kanban_card_id: "card",
  dispatch_title: null, card_title: null, github_issue_number: null, user_message: "", assistant_message: "", events: [], duration_ms: durationMs, created_at: createdAt,
});

describe("turn transcript time axis", () => {
  it("starts each turn at created_at minus its duration on one shared axis", () => {
    const { spans, total } = turnSpans([
      transcript("late", "2026-11-17T10:30:00Z", 10 * 60_000),
      transcript("early", "2026-11-17T10:05:00Z", 5 * 60_000),
      transcript("unknown", "2026-11-17T10:10:00Z", null),
    ]);
    expect(total).toBe(30 * 60);
    expect(spans.map((span) => [span.transcript.turn_id, span.start, span.duration])).toEqual([
      ["late", 20 * 60, 10 * 60],
      ["early", 0, 5 * 60],
      ["unknown", 10 * 60, null],
    ]);
  });

  it("picks about six ticks with a margin and labels offsets by unit", () => {
    expect(timeAxis(38)).toEqual({ range: 50, ticks: [0, 10, 20, 30, 40, 50] });
    expect(timeAxis(0).ticks[0]).toBe(0);
    const day = timeAxis(26 * 3600);
    expect(day.ticks.length).toBeGreaterThanOrEqual(5);
    expect(day.range).toBeGreaterThanOrEqual(26 * 3600 * 1.08);
    expect(formatAxisOffset(0, true)).toBe("0초");
    expect(formatAxisOffset(90, false)).toBe("1m 30s");
    expect(formatAxisOffset(7200, true)).toBe("2시간");
    expect(formatAxisOffset(30 * 3600, true)).toBe("1일 6시간");
  });

  it("numbers the order axis with the first, the last and evenly spaced events", () => {
    expect(orderTicks(0)).toEqual([]);
    expect(orderTicks(3)).toEqual([1, 2, 3]);
    expect(orderTicks(39)).toEqual([1, 11, 20, 30, 39]);
  });

  it("names a step body by what it carries", () => {
    const event = (kind: SessionTranscriptEvent["kind"], isError = false): SessionTranscriptEvent => ({ kind, content: "x", is_error: isError });
    expect(eventBodyLabel(event("tool_use"), tr)).toBe("Input");
    expect(eventBodyLabel(event("tool_result", true), tr)).toBe("Error");
    expect(eventBodyLabel(event("assistant"), tr)).toBe("Output");
  });
});
