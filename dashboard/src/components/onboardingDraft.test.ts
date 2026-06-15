import { describe, expect, it } from "vitest";

import {
  isMeaningfulOnboardingDraft,
  pickPreferredOnboardingDraft,
  restoreServerDraftTokens,
  serverDraftToLocalDraft,
  type OnboardingDraft,
  type OnboardingStatusResponse,
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

  describe("restoreServerDraftTokens", () => {
    const emptyStatus: OnboardingStatusResponse = {};

    it("restores tokens from the local draft when the newer server draft is redacted", () => {
      // #3440 codex round 3 [High]: server newer but tokens cleared, local older
      // but holds the raw tokens — must NOT blank them out, and status (None per
      // slot in prod) is no help.
      const localDraft = makeDraft({
        updatedAtMs: 10,
        commandBots: [{ provider: "claude", token: "local-cmd-token", botInfo: null }],
        announceToken: "local-announce",
        notifyToken: "local-notify",
      });
      const serverDraft = makeDraft({
        updatedAtMs: 20,
        commandBots: [{ provider: "claude", token: "", botInfo: null }],
        announceToken: "",
        notifyToken: "",
      });

      const restored = restoreServerDraftTokens(serverDraft, localDraft, emptyStatus, false);

      expect(restored.commandBots[0].token).toBe("local-cmd-token");
      expect(restored.announceToken).toBe("local-announce");
      expect(restored.notifyToken).toBe("local-notify");
      // Inputs are not mutated.
      expect(serverDraft.commandBots[0].token).toBe("");
    });

    it("keeps non-empty server tokens and only fills the gaps", () => {
      const localDraft = makeDraft({
        commandBots: [
          { provider: "claude", token: "local-1", botInfo: null },
          { provider: "codex", token: "local-2", botInfo: null },
        ],
      });
      const serverDraft = makeDraft({
        commandBots: [
          { provider: "claude", token: "server-keeps", botInfo: null },
          { provider: "codex", token: "", botInfo: null },
        ],
      });

      const restored = restoreServerDraftTokens(serverDraft, localDraft, emptyStatus, false);

      expect(restored.commandBots[0].token).toBe("server-keeps");
      expect(restored.commandBots[1].token).toBe("local-2");
    });

    it("falls back to status tokens when local has none on a fresh setup", () => {
      const serverDraft = makeDraft({
        commandBots: [{ provider: "claude", token: "", botInfo: null }],
      });
      const status: OnboardingStatusResponse = {
        bot_tokens: { command: "status-cmd", announce: "status-announce" },
      };

      const restored = restoreServerDraftTokens(serverDraft, null, status, false);

      expect(restored.commandBots[0].token).toBe("status-cmd");
      expect(restored.announceToken).toBe("status-announce");
    });
  });
});
