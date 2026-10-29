// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { beginAuthentication } from "../../api/authState";
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
  beginAuthentication(null);
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

it("keeps a Korean display name and assigns the ASCII custom-agent fallback", async () => {
  const setAgents = vi.fn();
  const args = await render({ customName: "요리사", setAgents, setExpandedAgent: vi.fn() });
  await act(async () => { actions.addCustomAgent(); });
  const added = setAgents.mock.calls[0][0]([]);
  expect(added).toHaveLength(1);
  expect(added[0]).toMatchObject({ id: "agent-1", name: "요리사", custom: true });
  expect(args.setExpandedAgent).toHaveBeenLastCalledWith("agent-1");
});

it("uses ASCII parts of a mixed-language custom name as its ID", async () => {
  const setAgents = vi.fn();
  await render({ customName: "요리 Team 42", setAgents });
  await act(async () => { actions.addCustomAgent(); });
  expect(setAgents.mock.calls[0][0]([])[0].id).toBe("team-42");
});

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

it("sends the tab token on every wizard action and still renders the server's failure body", async () => {
  beginAuthentication("tab-only-token");
  const replies: Record<string, { status: number; body: unknown }> = {
    "/api/onboarding/validate-token": { status: 200, body: { valid: true, bot_id: "1", bot_name: "bot" } },
    "/api/onboarding/check-provider": { status: 200, body: { installed: true, logged_in: true } },
    "/api/onboarding/channels": { status: 200, body: { guilds: [{ id: "g1", name: "Guild", channels: [] }] } },
    "/api/onboarding/generate-prompt": { status: 200, body: { prompt: "generated" } },
    "/api/onboarding/complete": { status: 500, body: { ok: false, error: "discord apply failed", partial_apply: true } },
  };
  fetchMock.mockImplementation(async (url: string, init: RequestInit) => {
    const authorized = new Headers(init.headers).get("Authorization") === "Bearer tab-only-token";
    const reply = authorized ? replies[url] : { status: 401, body: { error: "unauthorized" } };
    return new Response(JSON.stringify(reply.body), { status: reply.status });
  });
  const setGuilds = vi.fn();
  const setProviderStatuses = vi.fn();
  const setAgents = vi.fn();
  const args = await render({
    agents: [{ id: "a1", name: "Agent", description: "desc", prompt: "", custom: true }],
    setGuilds, setProviderStatuses, setAgents,
  });
  await act(async () => {
    await actions.validateStep1();
    await actions.checkProviders();
    await actions.fetchChannels();
    await actions.generateAiPrompt("a1");
    await actions.handleComplete();
  });
  expect(new Set(fetchMock.mock.calls.map(([url]) => url))).toEqual(new Set(Object.keys(replies)));
  for (const [, init] of fetchMock.mock.calls) {
    expect(new Headers(init.headers).get("Authorization")).toBe("Bearer tab-only-token");
  }
  expect(setProviderStatuses).toHaveBeenLastCalledWith({
    claude: { installed: true, logged_in: true }, codex: { installed: true, logged_in: true },
  });
  expect(setGuilds).toHaveBeenLastCalledWith([{ id: "g1", name: "Guild", channels: [] }]);
  expect(setAgents.mock.calls.at(-1)![0]([{ id: "a1", prompt: "" }])).toEqual([{ id: "a1", prompt: "generated" }]);
  expect(args.setError).toHaveBeenLastCalledWith(expect.stringMatching(/^discord apply failed Setup was partially applied/));
});
