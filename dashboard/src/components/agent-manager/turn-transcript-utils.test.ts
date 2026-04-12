import { describe, expect, it } from "vitest";
import type { SessionTranscript } from "../../api";
import {
  buildTranscriptCardLabel,
  normalizeActiveEventIndex,
  transcriptSelectionLabel,
} from "./turn-transcript-utils";

function makeTranscript(
  overrides: Partial<SessionTranscript> = {},
): SessionTranscript {
  return {
    id: 1,
    turn_id: "discord:test:1",
    session_key: null,
    channel_id: null,
    agent_id: null,
    provider: "codex",
    dispatch_id: "dispatch-1",
    kanban_card_id: "b922134f-b244-4cc6-8904-123456789abc",
    dispatch_title: null,
    card_title: null,
    github_issue_number: null,
    user_message: "",
    assistant_message: "",
    events: [],
    duration_ms: null,
    created_at: "2026-04-13T10:15:00Z",
    ...overrides,
  };
}

describe("turn transcript label helpers", () => {
  it("builds an issue-first card label when both issue number and card title exist", () => {
    expect(
      buildTranscriptCardLabel(
        makeTranscript({
          card_title: "에이전트 카드 UX 대대적 개편",
          github_issue_number: 511,
        }),
      ),
    ).toBe("#511 에이전트 카드 UX 대대적 개편");
  });

  it("replaces raw review UUID suffixes with the issue/card label", () => {
    const transcript = makeTranscript({
      dispatch_title: "[Review R1] b922134f-b244-4cc6-8904-123456789abc",
      card_title: "타임라인 단일 패널",
      github_issue_number: 525,
    });

    expect(transcriptSelectionLabel(transcript, true)).toBe(
      "[Review R1] #525 타임라인 단일 패널",
    );
  });

  it("replaces raw kanban card ids in review titles with the issue/card label", () => {
    const transcript = makeTranscript({
      kanban_card_id: "card-335-rereview",
      dispatch_title: "[Review R2] card-335-rereview",
      card_title: "Review Card",
      github_issue_number: 335,
    });

    expect(transcriptSelectionLabel(transcript, true)).toBe(
      "[Review R2] #335 Review Card",
    );
  });

  it("keeps readable dispatch titles unchanged", () => {
    const transcript = makeTranscript({
      dispatch_title: "enhance: 에이전트 카드 — 타임라인 단일 패널 + 사용자 요청 접기",
      card_title: "타임라인 단일 패널",
      github_issue_number: 525,
    });

    expect(transcriptSelectionLabel(transcript, true)).toBe(
      "enhance: 에이전트 카드 — 타임라인 단일 패널 + 사용자 요청 접기",
    );
  });

  it("falls back to the issue/card label when dispatch title is absent", () => {
    const transcript = makeTranscript({
      dispatch_title: null,
      card_title: "타임라인 단일 패널",
      github_issue_number: 525,
    });

    expect(transcriptSelectionLabel(transcript, true)).toBe(
      "#525 타임라인 단일 패널",
    );
  });
});

describe("normalizeActiveEventIndex", () => {
  it("defaults to the first event when nothing is selected", () => {
    expect(normalizeActiveEventIndex(null, 3)).toBe(0);
  });

  it("clamps out-of-range selections to the last event", () => {
    expect(normalizeActiveEventIndex(9, 3)).toBe(2);
  });

  it("returns null when there are no events", () => {
    expect(normalizeActiveEventIndex(0, 0)).toBeNull();
  });
});
