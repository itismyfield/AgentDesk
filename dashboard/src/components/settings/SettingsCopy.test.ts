import { describe, expect, it } from "vitest";
import { connectorReason, providerDisplayName, usageBucketLabel } from "./SettingsCopy";

const ko = (text: string) => text;
const en = (_ko: string, text: string) => text;

describe("settings copy", () => {
  it("spells out usage windows instead of 5h / 7d shorthand", () => {
    expect(usageBucketLabel("5h", ko)).toBe("5시간 한도");
    expect(usageBucketLabel("7d", ko)).toBe("7일 한도");
    expect(usageBucketLabel("7d Fable", ko)).toBe("7일 한도 · Fable 모델");
    expect(usageBucketLabel("7d Fable", en)).toBe("7-day limit · Fable model");
    expect(usageBucketLabel("monthly", ko)).toBe("monthly");
  });

  it("names providers and connector problems in plain words", () => {
    expect(providerDisplayName("claude")).toBe("Claude");
    expect(providerDisplayName("opencode")).toBe("OpenCode");
    expect(providerDisplayName("custom")).toBe("custom");
    expect(connectorReason("missing_skill_files", ko)).toBe("폴더에 스킬 파일(SKILL.md)이 없어요.");
    expect(connectorReason("something_new", ko)).toBe("something_new");
  });
});
