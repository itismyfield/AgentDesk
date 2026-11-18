import { describe, expect, it } from "vitest";
import type { RoutineRecord } from "../../api";
import type { TFunction } from "./model";
import {
  bucketRoutineMarkers,
  formatRelative,
  formatScheduleAbsolute,
  MARKER_TARGET_PX,
  routineTimelineMarkers,
  timelineHourTicks,
} from "./routineModel";

const koT: TFunction = (translations) => translations.ko;
const HOUR = 3_600_000;
// Local noon, so day boundaries do not depend on the test machine's time zone.
const NOW = new Date(2026, 10, 18, 12, 0, 0).getTime();
const iso = (offset: number) => new Date(NOW + offset).toISOString();

function routine(patch: Partial<RoutineRecord> & Pick<RoutineRecord, "id">): RoutineRecord {
  return {
    agent_id: null,
    script_ref: `${patch.id}.js`,
    name: patch.id,
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
    created_at: iso(-24 * HOUR),
    updated_at: iso(-HOUR),
    ...patch,
  };
}

const lastRun = (id: string, status: string, offset: number) => ({
  id: `${id}-run`,
  routine_id: id,
  status,
  action: null,
  turn_id: null,
  lease_expires_at: null,
  result_json: null,
  error: null,
  discord_log_status: null,
  discord_log_error: null,
  discord_message_id: null,
  discord_log_sections: null,
  started_at: iso(offset),
  finished_at: null,
  created_at: iso(offset),
  updated_at: iso(offset),
});

describe("routine timeline markers", () => {
  it("uses only the last run and next_due_at, inside four hours back and eight ahead", () => {
    const markers = routineTimelineMarkers(
      [
        routine({ id: "done", last_run_at: iso(-2 * HOUR), last_run: lastRun("done", "succeeded", -2 * HOUR), next_due_at: iso(2 * HOUR) }),
        routine({ id: "failed", last_run: lastRun("failed", "failed", -3 * HOUR), next_due_at: iso(9 * HOUR) }),
        routine({ id: "running", in_flight_run_id: "r1", last_run: lastRun("running", "running", -6 * HOUR) }),
        routine({ id: "paused", status: "paused", next_due_at: iso(HOUR), last_run_at: iso(-5 * HOUR) }),
      ],
      NOW,
    );
    expect(markers.map((marker) => [marker.routine.id, marker.state, (marker.at - NOW) / HOUR])).toEqual([
      ["running", "running", -4],
      ["failed", "failed", -3],
      ["done", "past", -2],
      ["done", "future", 2],
    ]);
    expect(markers[0].ratio).toBe(0);
    expect(markers[3].ratio).toBeCloseTo(0.5);
  });

  it("groups markers into fixed 44px columns so targets never overlap", () => {
    const markers = routineTimelineMarkers(
      [
        routine({ id: "a", next_due_at: iso(2 * HOUR) }),
        routine({ id: "b", next_due_at: iso(2 * HOUR + 5 * 60_000) }),
        routine({ id: "c", next_due_at: iso(6 * HOUR) }),
      ],
      NOW,
    );
    const buckets = bucketRoutineMarkers(markers, 480);
    expect(buckets.map((bucket) => bucket.markers.map((marker) => marker.routine.id))).toEqual([["a", "b"], ["c"]]);
    for (const bucket of buckets) expect(bucket.center).toBe((bucket.index + 0.5) * MARKER_TARGET_PX);
    // A marker at the very end still lands in the last whole column.
    const end = routineTimelineMarkers([routine({ id: "end", next_due_at: iso(8 * HOUR) })], NOW);
    expect(bucketRoutineMarkers(end, 100)[0].index).toBe(1);
  });

  it("labels days relative to now and keeps 24h clocks", () => {
    expect(formatScheduleAbsolute(NOW + 2 * HOUR, NOW, "ko-KR", koT)).toBe("오늘 14:00");
    expect(formatScheduleAbsolute(NOW + 21 * HOUR, NOW, "ko-KR", koT)).toBe("내일 09:00");
    expect(formatScheduleAbsolute(NOW - 15 * HOUR, NOW, "ko-KR", koT)).toBe("어제 21:00");
    expect(formatScheduleAbsolute(NOW + 3 * 24 * HOUR, NOW, "ko-KR", koT)).toMatch(/^[월화수목금토일] 12:00$/);
    expect(formatScheduleAbsolute(NOW + 20 * 24 * HOUR, NOW, "ko-KR", koT)).toBe("12월 8일 12:00");
    expect(formatRelative(NOW + 2 * HOUR, "ko-KR", NOW)).toBe("2시간 후");
    expect(formatRelative(iso(-30 * 60_000), "en-US", NOW)).toBe("30 minutes ago");
  });

  it("puts hour ticks every 2h, or every 4h on narrow charts", () => {
    expect(timelineHourTicks(NOW, 600).map((tick) => new Date(tick).getHours())).toEqual([10, 12, 14, 16, 18]);
    expect(timelineHourTicks(NOW, 360).map((tick) => new Date(tick).getHours())).toEqual([12, 16]);
  });
});
