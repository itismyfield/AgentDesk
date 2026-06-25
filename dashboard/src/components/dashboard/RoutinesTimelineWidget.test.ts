import { describe, expect, it } from "vitest";
import type { RoutineRecord } from "../../api";
import {
  describeRoutinePurpose,
  describeRoutineSchedule,
  sortRoutinesChronologically,
  summarizeRoutineRunResult,
} from "./RoutinesTimelineWidget";
import type { TFunction } from "./model";

const koT: TFunction = (translations) => translations.ko;

function routineFixture(
  patch: Partial<RoutineRecord> & Pick<RoutineRecord, "id" | "name">,
): RoutineRecord {
  return {
    agent_id: null,
    script_ref: `${patch.id}.js`,
    status: "enabled",
    execution_strategy: "fresh",
    schedule: null,
    next_due_at: null,
    last_run_at: null,
    last_result: null,
    checkpoint: null,
    discord_thread_id: null,
    timeout_secs: null,
    in_flight_run_id: null,
    created_at: "2026-05-17T00:00:00Z",
    updated_at: "2026-05-17T00:00:00Z",
    ...patch,
  };
}

describe("RoutinesTimelineWidget helpers", () => {
  it("sorts routines by next due time and keeps manual routines last", () => {
    const sorted = sortRoutinesChronologically([
      routineFixture({
        id: "manual",
        name: "Manual",
        next_due_at: null,
        last_run_at: "2026-05-17T03:00:00Z",
      }),
      routineFixture({
        id: "later",
        name: "Later",
        next_due_at: "2026-05-17T09:00:00Z",
      }),
      routineFixture({
        id: "soon",
        name: "Soon",
        next_due_at: "2026-05-17T05:00:00Z",
      }),
    ]);

    expect(sorted.map((routine) => routine.id)).toEqual([
      "soon",
      "later",
      "manual",
    ]);
  });

  it("uses the most recent last run as the tie-breaker", () => {
    const sorted = sortRoutinesChronologically([
      routineFixture({
        id: "old",
        name: "Old",
        next_due_at: "2026-05-17T05:00:00Z",
        last_run_at: "2026-05-16T01:00:00Z",
      }),
      routineFixture({
        id: "recent",
        name: "Recent",
        next_due_at: "2026-05-17T05:00:00Z",
        last_run_at: "2026-05-16T04:00:00Z",
      }),
    ]);

    expect(sorted.map((routine) => routine.id)).toEqual(["recent", "old"]);
  });

  it("renders common schedule strings as user-facing labels", () => {
    expect(describeRoutineSchedule("0 15 * * *", "ko")).toBe("매일 15:00");
    expect(describeRoutineSchedule("30 9 * * 1-5", "en")).toBe(
      "Weekdays 09:30",
    );
    expect(describeRoutineSchedule("@every 15m", "ko")).toBe("15분마다");
    expect(describeRoutineSchedule("0 */6 * * *", "ko")).toBe(
      "6시간마다 정각",
    );
    expect(describeRoutineSchedule("30 */4 * * *", "en")).toBe(
      "Every 4h at :30",
    );
    expect(describeRoutineSchedule("0,30 12-20 * * *", "ko")).toBe(
      "매일 12:00~20:30, 30분마다",
    );
    expect(describeRoutineSchedule("15,45 12-20 * * *", "ko")).toBe(
      "매일 12:15~20:45, 30분마다",
    );
    expect(describeRoutineSchedule("0 9 * * 0", "ko")).toBe(
      "매주 일요일 09:00",
    );
    expect(describeRoutineSchedule("0 10 * * 1", "ko")).toBe(
      "매주 월요일 10:00",
    );
    expect(describeRoutineSchedule(null, "en")).toBe("Manual run");
  });

  it("summarizes routine purpose from known routine families", () => {
    expect(
      describeRoutinePurpose(
        routineFixture({
          id: "morning",
          name: "family-morning-briefing-obujang",
          script_ref: "migrated-launchd/family-morning-briefing-obujang.js",
        }),
        koT,
      ),
    ).toContain("아침 브리핑");

    expect(
      describeRoutinePurpose(
        routineFixture({
          id: "janitor",
          name: "harness-worktree-janitor",
          script_ref: "local-worktree-gc.js",
        }),
        koT,
      ),
    ).toContain("worktree");
  });

  it("summarizes known routine result fields without requiring raw JSON first", () => {
    const result = summarizeRoutineRunResult({
      status: "ok",
      outcome_summary: "성공 요약: 새 자동화 추천 후보 없음",
      decision_summary: "임계값 이하라 보류",
      scoring_summary: "scored=4, deduped=2",
      observation_count: 12,
      active_candidate_count: 3,
      recommendations_today: 1,
    });

    expect(result.structured).toBe(true);
    expect(result.summary).toBe("성공 요약: 새 자동화 추천 후보 없음");
    expect(result.notes).toContainEqual({
      key: "decision",
      value: "임계값 이하라 보류",
    });
    expect(result.notes).toContainEqual({
      key: "scoring",
      value: "scored=4, deduped=2",
    });
    expect(result.facts).toEqual(
      expect.arrayContaining([
        { key: "status", value: "ok", mono: false },
        { key: "observations", value: "12", mono: false },
        { key: "active_candidates", value: "3", mono: false },
        { key: "recommendations_today", value: "1", mono: false },
      ]),
    );
  });

  it("extracts agent completion previews and routing metadata", () => {
    const result = summarizeRoutineRunResult({
      status: "completed",
      agent_id: "codex",
      attempt_kind: "primary",
      turn_id: "turn-123456",
      duration_ms: 1234,
      assistant_message_preview: "자동화 후보는 보류합니다.\n근거가 부족합니다.",
    });

    expect(result.structured).toBe(true);
    expect(result.assistantPreview).toBe(
      "자동화 후보는 보류합니다.\n근거가 부족합니다.",
    );
    expect(result.facts).toEqual(
      expect.arrayContaining([
        { key: "status", value: "completed", mono: false },
        { key: "agent", value: "codex", mono: false },
        { key: "attempt", value: "primary", mono: false },
        { key: "turn", value: "turn-123456", mono: true },
        { key: "duration", value: "1.2s", mono: false },
      ]),
    );
  });

  it("falls back to raw preview for unknown JSON shapes", () => {
    const result = summarizeRoutineRunResult({
      nested: { value: true },
    });

    expect(result.structured).toBe(false);
    expect(result.summary).toBeNull();
    expect(result.rawPreview).toContain('"nested"');
  });
});
