import { describe, expect, it } from "vitest";
import { connectorReason, connectorSetupSteps, providerDisplayName, usageBucketLabel } from "./SettingsCopy";

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

  it("writes connector setup steps in Korean and keeps the server's steps otherwise", () => {
    const folder = { id: "obsidian_skill_root", state: "missing_path", reason: "missing_path", env_var: "AGENTDESK_OBSIDIAN_SKILL_ROOT", source: "/vault/99_Skills", setup_actions: ["Create the configured directory: /vault/99_Skills"] };
    expect(connectorSetupSteps(folder, true)).toEqual({
      steps: ["지정한 폴더를 만드세요: /vault/99_Skills", "AGENTDESK_OBSIDIAN_SKILL_ROOT를 지우거나 이미 있는 폴더로 바꾸세요."],
      translated: true,
    });
    expect(connectorSetupSteps(folder, false)).toEqual({ steps: folder.setup_actions, translated: false });
    const kakao = { id: "kakao_scheduled_delivery", state: "invalid_config", reason: "consent_required", env_var: "AGENTDESK_KAKAO_ENABLED", source: null, setup_actions: ["Check …"] };
    expect(connectorSetupSteps(kakao, true).steps[1]).toBe("고친 뒤 AgentDesk를 다시 시작하세요.");
    const unknown = { ...folder, reason: "something_new" };
    expect(connectorSetupSteps(unknown, true)).toEqual({ steps: folder.setup_actions, translated: false });
  });
});
