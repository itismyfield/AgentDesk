import { describe, expect, it } from "vitest";

import {
  isMeaningfulOnboardingDraft,
  pickPreferredOnboardingDraft,
  serverDraftToLocalDraft,
  type OnboardingDraft,
} from "./onboardingDraft";

function makeDraft(overrides: Partial<OnboardingDraft> = {}): OnboardingDraft {
  return {
    version: 1,
    updatedAtMs: 1,
    step: 1,
    commandBots: [{ provider: "claude", token: "", botInfo: null }],
    announceToken: "",
    notifyToken: "",
    announceBotInfo: null,
    notifyBotInfo: null,
    providerStatuses: {},
    selectedTemplate: null,
    agents: [],
    customName: "",
    customDesc: "",
    customNameEn: "",
    customDescEn: "",
    expandedAgent: null,
    selectedGuild: "",
    channelAssignments: [],
    ownerId: "",
    hasExistingSetup: false,
    confirmRerunOverwrite: false,
    ...overrides,
  };
}

describe("onboardingDraft", () => {
  it("prefers the newer server draft when restoring", () => {
    const localDraft = makeDraft({
      updatedAtMs: 10,
      step: 2,
      commandBots: [{ provider: "claude", token: "local-token", botInfo: null }],
    });
    const serverDraft = serverDraftToLocalDraft({
      version: 1,
      updated_at_ms: 20,
      step: 4,
      command_bots: [{ provider: "codex", token: "server-token" }],
      announce_token: "announce-token",
      selected_template: "operations",
      selected_guild: "guild-1",
      channel_assignments: [
        {
          agent_id: "adk-cdx",
          agent_name: "Dispatch Desk",
          recommended_name: "adk-cdx-cdx",
          channel_id: "1234",
          channel_name: "dispatch-room",
        },
      ],
    });

    const preferred = pickPreferredOnboardingDraft(localDraft, serverDraft);
    expect(preferred?.updatedAtMs).toBe(20);
    expect(preferred?.commandBots[0]?.provider).toBe("codex");
    expect(preferred?.commandBots[0]?.token).toBe("server-token");
    expect(preferred?.channelAssignments[0]?.channelId).toBe("1234");
  });

  it("does not treat status-only prefill as a meaningful draft", () => {
    expect(
      isMeaningfulOnboardingDraft(
        makeDraft({
          selectedGuild: "guild-1",
          ownerId: "42",
          hasExistingSetup: true,
        }),
      ),
    ).toBe(false);

    expect(
      isMeaningfulOnboardingDraft(
        makeDraft({
          step: 4,
          selectedGuild: "guild-1",
        }),
      ),
    ).toBe(true);
  });
});
