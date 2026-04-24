import { describe, expect, it } from "vitest";

import {
  buildDuplicateBody,
  buildSetupBody,
  detectProviderSuffix,
  parseSkills,
  validateAllSteps,
  validateWizardStep,
  type WizardDraft,
} from "./setupWizardHelpers";

function draftFixture(overrides: Partial<WizardDraft> = {}): WizardDraft {
  return {
    agentId: "adk-researcher",
    name: "Researcher",
    nameKo: "리서처",
    departmentId: "",
    provider: "claude",
    channelId: "123456789012345678",
    promptTemplatePath: "~/.adk/release/config/agents/_shared.prompt.md",
    promptContent: "",
    skillsText: "",
    cronEnabled: false,
    cronSpec: "0 9 * * 1-5",
    ...overrides,
  };
}

describe("detectProviderSuffix", () => {
  it("maps -cc channel names to claude", () => {
    const result = detectProviderSuffix("adk-cc", null);
    expect(result.provider).toBe("claude");
    expect(result.suffix).toBe("-cc");
    expect(result.source).toBe("channelName");
  });

  it("maps -cdx channel names to codex", () => {
    const result = detectProviderSuffix("agentdesk-cdx", null);
    expect(result.provider).toBe("codex");
    expect(result.suffix).toBe("-cdx");
    expect(result.source).toBe("channelName");
  });

  it("is case-insensitive", () => {
    const result = detectProviderSuffix("ADK-CC", null);
    expect(result.provider).toBe("claude");
  });

  it("falls back to agent_id suffix when channel name is absent", () => {
    const result = detectProviderSuffix(null, "research-cdx");
    expect(result.provider).toBe("codex");
    expect(result.source).toBe("agentId");
  });

  it("prefers the channel name over the agent_id when both match", () => {
    const result = detectProviderSuffix("chan-cc", "agent-cdx");
    expect(result.provider).toBe("claude");
    expect(result.source).toBe("channelName");
  });

  it("returns null provider when no suffix matches", () => {
    const result = detectProviderSuffix("general-channel", "someone");
    expect(result.provider).toBeNull();
    expect(result.suffix).toBeNull();
    expect(result.source).toBeNull();
  });

  it("handles gemini suffix", () => {
    const result = detectProviderSuffix("news-gem", null);
    expect(result.provider).toBe("gemini");
  });
});

describe("parseSkills", () => {
  it("splits on commas and newlines", () => {
    expect(parseSkills("github, playwright,\nmemory-read")).toEqual([
      "github",
      "playwright",
      "memory-read",
    ]);
  });

  it("filters empty entries", () => {
    expect(parseSkills(" , \n , foo,,")).toEqual(["foo"]);
  });
});

describe("validateWizardStep", () => {
  it("accepts a valid role step", () => {
    const result = validateWizardStep("role", draftFixture(), "create");
    expect(result.valid).toBe(true);
    expect(result.errors).toEqual([]);
  });

  it("rejects an invalid agent_id", () => {
    const result = validateWizardStep(
      "role",
      draftFixture({ agentId: "!!" }),
      "create",
    );
    expect(result.valid).toBe(false);
    expect(result.errors[0]).toMatch(/agent_id/);
  });

  it("rejects an empty display name", () => {
    const result = validateWizardStep(
      "role",
      draftFixture({ name: "  " }),
      "create",
    );
    expect(result.valid).toBe(false);
    expect(result.errors.join(" ")).toMatch(/display name/);
  });

  it("rejects a non-numeric channel id", () => {
    const result = validateWizardStep(
      "discord",
      draftFixture({ channelId: "not-a-number" }),
      "create",
    );
    expect(result.valid).toBe(false);
    expect(result.errors[0]).toMatch(/channel_id/);
  });

  it("requires a prompt template path in create mode", () => {
    const result = validateWizardStep(
      "prompt",
      draftFixture({ promptTemplatePath: "" }),
      "create",
    );
    expect(result.valid).toBe(false);
  });

  it("permits an empty prompt template path in duplicate mode", () => {
    const result = validateWizardStep(
      "prompt",
      draftFixture({ promptTemplatePath: "" }),
      "duplicate",
    );
    expect(result.valid).toBe(true);
  });

  it("requires 5+ fields when cron is enabled", () => {
    const result = validateWizardStep(
      "cron",
      draftFixture({ cronEnabled: true, cronSpec: "0 9" }),
      "create",
    );
    expect(result.valid).toBe(false);
    expect(result.errors[0]).toMatch(/cron/);
  });

  it("skips cron validation when disabled", () => {
    const result = validateWizardStep(
      "cron",
      draftFixture({ cronEnabled: false, cronSpec: "bad" }),
      "create",
    );
    expect(result.valid).toBe(true);
  });

  it("aggregates errors in the preview step", () => {
    const result = validateWizardStep(
      "preview",
      draftFixture({ agentId: "!!", channelId: "x" }),
      "create",
    );
    expect(result.valid).toBe(false);
    expect(result.errors.length).toBeGreaterThanOrEqual(2);
  });
});

describe("validateAllSteps", () => {
  it("returns results for all 6 wizard steps", () => {
    const results = validateAllSteps(draftFixture(), "create");
    expect(results).toHaveLength(6);
    expect(results.every((r) => r.valid)).toBe(true);
  });
});

describe("buildSetupBody / buildDuplicateBody", () => {
  it("trims agent_id and skills", () => {
    const body = buildSetupBody(
      draftFixture({ agentId: "  spaced-id  ", skillsText: " a ,b \n c " }),
      true,
    );
    expect(body.agent_id).toBe("spaced-id");
    expect(body.skills).toEqual(["a", "b", "c"]);
    expect(body.dry_run).toBe(true);
  });

  it("propagates the dry_run flag", () => {
    const body = buildSetupBody(draftFixture(), false);
    expect(body.dry_run).toBe(false);
  });

  it("falls back nameKo to name when empty", () => {
    const body = buildDuplicateBody(
      draftFixture({ name: "Researcher", nameKo: "" }),
      true,
    );
    expect(body.name_ko).toBe("Researcher");
  });

  it("preserves department_id null when unset", () => {
    const body = buildDuplicateBody(
      draftFixture({ departmentId: "" }),
      false,
    );
    expect(body.department_id).toBeNull();
  });
});
