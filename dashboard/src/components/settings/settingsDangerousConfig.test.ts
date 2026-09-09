import { describe, expect, it } from "vitest";
import {
  getDangerousConfigKeys,
  getDangerousConfigLabel,
  isDangerousConfigKey,
} from "./settingsDangerousConfig";

describe("settingsDangerousConfig", () => {
  it("detects only dangerous edits", () => {
    expect(
      getDangerousConfigKeys({
        review_enabled: true,
        githubRepoCacheSec: "300",
        context_clear_percent: "95",
      }),
    ).toEqual(["review_enabled", "context_clear_percent"]);
  });

  it("narrows known dangerous keys", () => {
    expect(isDangerousConfigKey("pm_decision_gate_enabled")).toBe(true);
    expect(isDangerousConfigKey("server_port")).toBe(false);
  });

  it("returns localized labels and falls back to the raw key", () => {
    expect(getDangerousConfigLabel("review_enabled", true)).toBe("리뷰 게이트");
    expect(getDangerousConfigLabel("review_enabled", false)).toBe("Review gate");
    expect(getDangerousConfigLabel("unknown_key", true)).toBe("unknown_key");
  });
});
