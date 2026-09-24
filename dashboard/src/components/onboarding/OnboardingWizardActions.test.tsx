// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useOnboardingWizardActions } from "./OnboardingWizardActions";

type Args = Parameters<typeof useOnboardingWizardActions>[0];
let root: Root | undefined;
let container: HTMLDivElement;
let actions: ReturnType<typeof useOnboardingWizardActions>;
const fetchMock = vi.fn();

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", fetchMock);
  fetchMock.mockReset().mockResolvedValue({ json: async () => ({ valid: true, ok: true, guilds: [] }) });
});

afterEach(async () => {
  await act(async () => root?.unmount());
  container?.remove();
  vi.unstubAllGlobals();
});

async function render(overrides: Partial<Args> = {}) {
  const noop = () => undefined;
  const args: Args = {
    agents: [], announceToken: "  test-announce\n", channelAssignments: [],
    commandBots: [
      { provider: "claude", token: " test-command ", botInfo: null },
      { provider: "codex", token: "\ttest-command-2\n", botInfo: null },
    ],
    completionReady: true, confirmRerunOverwrite: false, customDesc: "", customDescEn: "",
    customName: "", customNameEn: "", hasExistingSetup: false, notifyToken: " test-notify ",
    onComplete: vi.fn(), ownerId: "", primaryProvider: "claude", selectedGuild: "test-guild",
    selectedTemplate: null, setAgents: noop, setAnnounceBotInfo: noop,
    setChannelAssignments: noop, setCheckingProviders: noop, setCommandBots: noop,
    setCompleting: noop, setCompletionChecklist: noop, setCustomDesc: noop,
    setCustomDescEn: noop, setCustomName: noop, setCustomNameEn: noop,
    setDraftNoticeVisible: noop, setError: vi.fn(), setExpandedAgent: noop,
    setGeneratingPrompt: noop, setGuilds: noop, setNotifyBotInfo: noop,
    setProviderStatuses: noop, setResumeState: noop, setSelectedGuild: noop,
    setSelectedTemplate: noop, setValidating: noop, tr: (_ko, en) => en, ...overrides,
  };
  function Probe() { actions = useOnboardingWizardActions(args); return null; }
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => { root!.render(<Probe />); });
  return args;
}

function payloads(path: string) {
  return fetchMock.mock.calls.filter(([url]) => url === path)
    .map(([, init]) => JSON.parse(init.body));
}

it("uses the same normalized tokens for validation, channel lookup and completion", async () => {
  await render();
  await act(async () => { await actions.validateStep1(); await actions.fetchChannels(); await actions.handleComplete(); });
  expect(payloads("/api/onboarding/validate-token").map((value) => value.token))
    .toEqual(["test-command", "test-command-2", "test-announce", "test-notify"]);
  expect(payloads("/api/onboarding/channels")).toEqual([{ token: "test-announce" }]);
  expect(payloads("/api/onboarding/complete")[0]).toMatchObject({
    token: "test-command", command_token_2: "test-command-2",
    announce_token: "test-announce", notify_token: "test-notify",
  });
});

it("rejects whitespace-only required command tokens without sending a request", async () => {
  const args = await render({ commandBots: [{ provider: "claude", token: " \n\t", botInfo: null }] });
  await act(async () => { await actions.validateStep1(); });
  expect(fetchMock).not.toHaveBeenCalled();
  expect(args.setError).toHaveBeenLastCalledWith("Enter token for Command Bot 1.");
});

it("treats a whitespace-only optional token as absent", async () => {
  await render({ notifyToken: " \n\t" });
  await act(async () => { await actions.validateStep1(); await actions.handleComplete(); });
  expect(payloads("/api/onboarding/validate-token")).toHaveLength(3);
  expect(payloads("/api/onboarding/complete")[0].notify_token).toBeNull();
});

it("rejects an empty communication token and uses the command token for channel lookup", async () => {
  const args = await render({ announceToken: " \n" });
  await act(async () => { await actions.validateStep1(); await actions.fetchChannels(); });
  expect(payloads("/api/onboarding/validate-token")).toHaveLength(2);
  expect(args.setError).toHaveBeenLastCalledWith("Enter communication bot token.");
  expect(payloads("/api/onboarding/channels")).toEqual([{ token: "test-command" }]);
});
