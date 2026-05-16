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
        merge_strategy: "squash",
      }),
    ).toEqual(["review_enabled", "merge_strategy"]);
  });

  it("narrows known dangerous keys", () => {
    expect(isDangerousConfigKey("pm_decision_gate_enabled")).toBe(true);
    expect(isDangerousConfigKey("server_port")).toBe(false);
  });

  it("returns localized labels and falls back to the raw key", () => {
    expect(getDangerousConfigLabel("merge_automation_enabled", true)).toBe("자동 머지");
    expect(getDangerousConfigLabel("merge_automation_enabled", false)).toBe("Merge automation");
    expect(getDangerousConfigLabel("unknown_key", true)).toBe("unknown_key");
  });
});
