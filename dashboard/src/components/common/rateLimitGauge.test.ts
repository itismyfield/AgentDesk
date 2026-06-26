import { describe, expect, it } from "vitest";
import { rateLimitFillWidth, rateLimitProjectionWidth } from "./rateLimitGauge";

describe("rateLimitGauge", () => {
  it("keeps zero and unknown utilization empty", () => {
    expect(rateLimitFillWidth(null)).toBe("0%");
    expect(rateLimitFillWidth(0)).toBe("0%");
  });

  it("keeps nonzero utilization visible while preserving normal values", () => {
    expect(rateLimitFillWidth(1)).toBe("6%");
    expect(rateLimitFillWidth(42)).toBe("42%");
    expect(rateLimitFillWidth(130)).toBe("100%");
  });

  it("never renders projected utilization behind the current fill", () => {
    expect(rateLimitProjectionWidth(3, 9)).toBe("9%");
    expect(rateLimitProjectionWidth(3, null)).toBe("6%");
    expect(rateLimitProjectionWidth(null, 9)).toBe("0%");
  });
});
