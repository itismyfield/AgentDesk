import { describe, expect, it } from "vitest";
import {
  analyzeReplyInvitation,
  classifyRawOutputStop,
  classifyTranscriptStop,
  computePromptCacheStatus,
  detectTurnLoop,
  formatLoopSummary,
  formatPromptCacheLabel,
  parseQuotaResetMs,
  searchTranscriptEvents,
  stepsFromLiveTurnEvents,
  stepsFromTranscriptEvents,
} from "./agentObservability";
import type { SessionTranscriptEvent } from "../api/agentActivity";

const tr = (ko: string, _en: string) => ko;
const trEn = (_ko: string, en: string) => en;

describe("detectTurnLoop", () => {
  it("flags 4 identical consecutive tool calls and ignores polling sleep/tail", () => {
    const repeat = Array.from({ length: 4 }, () => ({
      kind: "tool",
      status: "success",
      tool_name: "Read",
      summary: "src/main.rs",
    }));
    const finding = detectTurnLoop(stepsFromLiveTurnEvents(repeat));
    expect(finding).not.toBeNull();
    expect(finding?.pattern).toBe("same");
    expect(finding?.count).toBe(4);
    expect(formatLoopSummary(finding!, tr)).toContain("Read 동일 호출이 4회");

    const polling = Array.from({ length: 6 }, () => ({
      kind: "tool",
      status: "success",
      tool_name: "Bash",
      summary: "sleep 10",
    }));
    expect(detectTurnLoop(stepsFromLiveTurnEvents(polling))).toBeNull();
  });

  it("flags 3 consecutive contract argument errors on Edit and clears when a new different call is running", () => {
    const events: SessionTranscriptEvent[] = [];
    for (let i = 0; i < 3; i += 1) {
      events.push(
        { kind: "tool_use", tool_name: "Edit", summary: `attempt ${i}`, content: `file.ts#${i}`, status: "error", is_error: false },
        { kind: "tool_result", tool_name: "Edit", summary: null, content: "Error: String to replace not found in file.", status: "error", is_error: true },
      );
    }
    const steps = stepsFromTranscriptEvents(events);
    const loop = detectTurnLoop(steps);
    expect(loop?.pattern).toBe("contract");
    expect(loop?.tool).toBe("Edit");

    const movedOn = [
      ...steps,
      ...stepsFromTranscriptEvents([
        { kind: "tool_use", tool_name: "Read", summary: "file.ts", content: "file.ts", status: "running", is_error: false },
      ]),
    ];
    expect(detectTurnLoop(movedOn)).toBeNull();
  });

  it("flags 12 alternating calls as pingpong", () => {
    const alternating = Array.from({ length: 12 }, (_, idx) => ({
      kind: "tool",
      status: "success",
      tool_name: idx % 2 === 0 ? "Read" : "Grep",
      summary: idx % 2 === 0 ? "a.ts" : "pattern",
    }));
    const finding = detectTurnLoop(stepsFromLiveTurnEvents(alternating));
    expect(finding?.pattern).toBe("pingpong");
    expect(finding?.count).toBe(12);
  });
});

describe("classifyTranscriptStop & parseQuotaResetMs", () => {
  const now = Date.parse("2026-11-27T05:00:00Z");

  it("parses relative and zoned wall-clock quota resets into KST", () => {
    const rel = parseQuotaResetMs("You have hit your usage limit. Please try again in 2 hours 30 minutes.", now);
    expect(rel?.atMs).toBe(now + (2 * 60 + 30) * 60_000);

    const zoned = classifyTranscriptStop(
      {
        user_message: "Continue",
        assistant_message: "You've hit your limit · resets 5pm (Asia/Seoul)",
        events: [],
        created_at: "2026-11-27T05:00:00Z",
      },
      tr,
      now,
    );
    expect(zoned?.kind).toBe("quota");
    expect(zoned?.resetKstLabel).toBe("11/27 17:00 KST 복구");
  });

  it("does not misclassify work summaries about rate limits or context-length errors as quota stops", () => {
    const summaryTurn = classifyTranscriptStop(
      {
        user_message: "Fix 429 handling",
        assistant_message: "Rate limit exceeded errors are now handled with exponential backoff and unit tests.",
        events: [],
        created_at: "2026-11-27T05:00:00Z",
      },
      trEn,
      now,
    );
    expect(summaryTurn).toBeNull();

    const ctxError = classifyTranscriptStop(
      {
        user_message: "Summarize",
        assistant_message: "",
        events: [{ kind: "error", content: "Prompt is too long: maximum context length exceeded", is_error: true }],
        created_at: "2026-11-27T05:00:00Z",
      },
      trEn,
      now,
    );
    expect(ctxError).toBeNull();
  });

  it("distinguishes silent and unfinished turns while skipping slash commands", () => {
    expect(
      classifyTranscriptStop(
        { user_message: "/compact", assistant_message: "", events: [], created_at: "2026-11-27T05:00:00Z" },
        tr,
        now,
      ),
    ).toBeNull();

    expect(
      classifyTranscriptStop(
        { user_message: "Build module", assistant_message: "", events: [], created_at: "2026-11-27T05:00:00Z" },
        tr,
        now,
      )?.kind,
    ).toBe("silent");

    expect(
      classifyTranscriptStop(
        {
          user_message: "Run tests",
          assistant_message: "",
          events: [
            { kind: "tool_use", tool_name: "Bash", content: "npm test", is_error: false },
            { kind: "tool_result", tool_name: "Bash", content: "PASS", is_error: false },
          ],
          created_at: "2026-11-27T05:00:00Z",
        },
        tr,
        now,
      )?.kind,
    ).toBe("unfinished");

    expect(classifyRawOutputStop("Error: You've hit your weekly limit · resets 6pm (Asia/Seoul)", tr, now)?.kind).toBe("quota");
  });
});

describe("analyzeReplyInvitation", () => {
  it("detects Korean and English go-ahead / decision questions while ignoring courtesy sign-offs and masking tokens", () => {
    expect(analyzeReplyInvitation("수정을 마쳤습니다. 추가로 궁금한 점이 있으시면 언제든 말씀해 주세요.")).toBeNull();
    expect(analyzeReplyInvitation("Done! Let me know if you have any other questions.")).toBeNull();

    const goAheadKo = analyzeReplyInvitation("설계 검증을 끝냈습니다. 이제 PR 병합을 진행할까요?");
    expect(goAheadKo).toEqual({
      asksUser: true,
      nudge: "go-ahead",
      quote: "이제 PR 병합을 진행할까요?",
    });

    const redacted = analyzeReplyInvitation("Using Bearer secret_token_1234567890abcdef — shall I deploy this config?");
    expect(redacted?.asksUser).toBe(true);
    expect(redacted?.quote).toContain("[REDACTED]");
    expect(redacted?.quote).not.toContain("secret_token_1234567890abcdef");
  });
});

describe("computePromptCacheStatus & searchTranscriptEvents", () => {
  it("computes warm, expiring, and cold Claude cache states", () => {
    const now = 1_000_000;
    const warm = computePromptCacheStatus({ provider: "claude", lastActivityMs: now - 120_000, nowMs: now });
    expect(warm?.phase).toBe("warm");
    expect(formatPromptCacheLabel(warm!, tr)).toBe("캐시 3분");

    const expiring = computePromptCacheStatus({ provider: "claude", lastActivityMs: now - 265_000, nowMs: now });
    expect(expiring?.phase).toBe("expiring");
    expect(formatPromptCacheLabel(expiring!, tr)).toBe("캐시 35초");

    const compacted = computePromptCacheStatus({
      provider: "claude",
      lastActivityMs: now - 10_000,
      recentOutput: "Conversation compacted (/compact)",
      nowMs: now,
    });
    expect(compacted).toEqual({ phase: "cold", remainingMs: 0, coldReason: "compacted" });

    expect(computePromptCacheStatus({ provider: "codex", lastActivityMs: now - 10_000, nowMs: now })).toBeNull();
  });

  it("searches transcript events with multi-word and quoted phrase queries", () => {
    const events: SessionTranscriptEvent[] = [
      { kind: "tool_use", tool_name: "Read", summary: "src/app/AppShell.tsx", content: "", is_error: false },
      { kind: "tool_result", tool_name: "Bash", summary: "cargo check", content: "error[E0308]: mismatched types", is_error: true },
      { kind: "assistant", tool_name: null, summary: null, content: "Fixed mismatched types in AppShell.", is_error: false },
    ];
    expect(searchTranscriptEvents(events, "\"mismatched types\" error")).toEqual([1]);
    expect(searchTranscriptEvents(events, "appshell")).toEqual([0, 2]);
  });
});
