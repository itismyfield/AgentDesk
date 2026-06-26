import { describe, expect, it } from "vitest";
import {
  formatRateLimitResetLabel,
  normalizeMiniRateLimitProviderLabel,
  projectRateLimitBucketAtReset,
  transformRLProviders,
} from "./MiniRateLimitBarModel";

describe("OfficeInsightPanel mini rate-limit helpers", () => {
  it("normalizes provider labels for compact cards", () => {
    expect(normalizeMiniRateLimitProviderLabel("gemini")).toBe("Gemini");
    expect(normalizeMiniRateLimitProviderLabel("qwen")).toBe("Qwen");
  });

  it("drops unsupported providers without measurable buckets but keeps stale measurable rows", () => {
    const rows = transformRLProviders([
      {
        provider: "qwen",
        buckets: [],
        stale: false,
        unsupported: true,
        reason: "No Qwen rate-limit telemetry source is implemented yet.",
      },
      {
        provider: "gemini",
        buckets: [{ name: "1h", limit: 100, used: 92, remaining: 8, reset: 1_700_000_600 }],
        stale: false,
      },
      {
        provider: "qwen",
        buckets: [{ name: "1h", limit: 50, used: 10, remaining: 40, reset: 1_700_000_600 }],
        stale: true,
        unsupported: true,
        reason: "Rendering last known measurable bucket until live telemetry lands.",
      },
    ]);

    expect(rows).toHaveLength(2);
    expect(rows[0]?.provider).toBe("Gemini");
    expect(rows[0]?.buckets[0]).toMatchObject({
      label: "1h",
      utilization: 92,
      level: "warning",
    });
    expect(rows[1]).toMatchObject({
      provider: "Qwen",
      stale: true,
      unsupported: true,
      reason: "Rendering last known measurable bucket until live telemetry lands.",
    });
    expect(rows[1]?.buckets[0]).toMatchObject({
      label: "1h",
      utilization: 20,
      level: "normal",
    });
  });

  it("treats negative usage sentinels as unknown utilization", () => {
    const rows = transformRLProviders([
      {
        provider: "gemini",
        buckets: [{ name: "rpm", limit: 15, used: -1, remaining: -1, reset: 0 }],
        stale: false,
      },
    ]);

    expect(rows[0]?.buckets[0]).toMatchObject({
      label: "rpm",
      utilization: null,
      level: "normal",
    });
  });

  it("keeps 5h/7d bucket labels and projects reset utilization from window pace", () => {
    const nowMs = Date.UTC(2026, 5, 25, 0, 0, 0);
    const rows = transformRLProviders(
      [
        {
          provider: "claude",
          buckets: [
            {
              name: "5h",
              limit: 100,
              used: 50,
              remaining: 50,
              reset: Math.floor((nowMs + 3_600_000) / 1000),
            },
            {
              name: "7d",
              limit: 100,
              used: 25,
              remaining: 75,
              reset: Math.floor((nowMs + 86_400_000) / 1000),
            },
          ],
          stale: false,
        },
      ],
      { nowMs },
    );

    expect(rows[0]?.buckets[0]).toMatchObject({
      label: "5h",
      utilization: 50,
      projectedUtilization: 63,
      projectedLevel: "normal",
    });
    expect(rows[0]?.buckets[1]).toMatchObject({
      label: "7d",
      utilization: 25,
      projectedUtilization: 29,
      projectedLevel: "normal",
    });
  });

  it("formats reset text with absolute time and relative distance", () => {
    const nowMs = Date.UTC(2026, 5, 25, 0, 0, 0);
    const label = formatRateLimitResetLabel(nowMs + 90 * 60_000, true, nowMs);

    expect(label).toContain("초기화");
    expect(label).toContain("1h 30m 후");
  });

  it("does not project unsupported bucket windows", () => {
    const nowMs = Date.UTC(2026, 5, 25, 0, 0, 0);

    expect(
      projectRateLimitBucketAtReset(
        {
          label: "requests",
          utilization: 50,
          resetAtMs: nowMs + 3_600_000,
        },
        nowMs,
      ),
    ).toBeNull();
  });
});
