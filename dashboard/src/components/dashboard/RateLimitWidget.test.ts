import { describe, expect, it } from "vitest";
import { normalizeRateLimitProviderLabel, transformRawData } from "./RateLimitWidget";

describe("RateLimitWidget helpers", () => {
  it("normalizes provider labels for supported dashboard providers", () => {
    expect(normalizeRateLimitProviderLabel("claude")).toBe("Claude");
    expect(normalizeRateLimitProviderLabel("gemini")).toBe("Gemini");
    expect(normalizeRateLimitProviderLabel("qwen")).toBe("Qwen");
  });

  it("keeps unsupported providers visible and coalesces duplicate provider rows by name", () => {
    const data = transformRawData(
      {
        providers: [
          {
            provider: "qwen",
            buckets: [],
            fetched_at: 1_700_000_000,
            stale: false,
            unsupported: true,
            reason: "No Qwen rate-limit telemetry source is implemented yet.",
          },
          {
            provider: "gemini",
            buckets: [
              { name: "1h", limit: 200, used: 50, remaining: 150, reset: 1_700_000_600 },
            ],
            fetched_at: 1_700_000_000,
            stale: false,
          },
          {
            provider: "qwen",
            buckets: [{ name: "1h", limit: 120, used: 24, remaining: 96, reset: 1_700_000_600 }],
            fetched_at: 1_700_000_000,
            stale: true,
            unsupported: true,
            reason: "Rendering last known measurable bucket until live telemetry lands.",
          },
        ],
      },
      80,
      95,
    );

    // Two rows out (one per normalized provider name) — coalesces the two
    // "qwen" entries so React rendering doesn't see duplicate keys.
    expect(data.providers).toHaveLength(2);

    // Qwen is index 0 because it was the first row in the input. The
    // measurable second row wins over the empty placeholder.
    expect(data.providers[0]).toMatchObject({
      provider: "Qwen",
      stale: true,
      unsupported: true,
    });
    expect(data.providers[0]?.buckets[0]).toMatchObject({
      label: "1h",
      utilization: 20,
      level: "normal",
    });

    expect(data.providers[1]).toMatchObject({ provider: "Gemini", unsupported: false });
    expect(data.providers[1]?.buckets[0]).toMatchObject({
      label: "1h",
      utilization: 25,
      level: "normal",
    });
  });

  it("keeps an unsupported provider with no measurable buckets visible (instead of dropping it)", () => {
    const data = transformRawData(
      {
        providers: [
          {
            provider: "qwen",
            buckets: [],
            fetched_at: 1_700_000_000,
            stale: false,
            unsupported: true,
            reason: "No Qwen rate-limit telemetry source is implemented yet.",
          },
        ],
      },
      80,
      95,
    );

    expect(data.providers).toHaveLength(1);
    expect(data.providers[0]).toMatchObject({
      provider: "Qwen",
      unsupported: true,
      reason: "No Qwen rate-limit telemetry source is implemented yet.",
    });
    expect(data.providers[0]?.buckets).toHaveLength(0);
  });

  it("maps negative usage sentinels to unknown utilization instead of negative percentages", () => {
    const data = transformRawData(
      {
        providers: [
          {
            provider: "gemini",
            buckets: [{ name: "rpm", limit: 15, used: -1, remaining: -1, reset: 0 }],
            fetched_at: 1_700_000_000,
            stale: false,
          },
        ],
      },
      80,
      95,
    );

    expect(data.providers[0]?.buckets[0]).toMatchObject({
      label: "rpm",
      utilization: null,
      level: "normal",
    });
  });
});
