import { describe, expect, it } from "vitest";
import type { RoutineRecord } from "../../api";
import {
  describeRoutineSchedule,
  sortRoutinesChronologically,
} from "./RoutinesTimelineWidget";

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
    expect(describeRoutineSchedule(null, "en")).toBe("Manual run");
  });
});
