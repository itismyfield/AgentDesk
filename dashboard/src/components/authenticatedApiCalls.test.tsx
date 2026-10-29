// @vitest-environment happy-dom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { beginAuthentication, getAuthSnapshot, requireAuthentication } from "../api/authState";
import { saveVoiceConfig, VoiceConfigApiError } from "../api/settingsRuntime";
import type { CompanySettings, VoiceConfigPutBody } from "../types";
import ClaudeAccountsWidget from "./dashboard/ClaudeAccountsWidget";
import { MiniRateLimitBar } from "./dashboard/MiniRateLimitBar";
import ReceiptWidget from "./dashboard/ReceiptWidget";
import OnboardingWizard from "./OnboardingWizard";
import SettingsView from "./SettingsView";

// A server with auth_token set: every /api call except the public probe needs this Bearer.
const TOKEN = "tab-only-token";
type Reply = { status?: number; body: unknown };
type Handler = (init: RequestInit) => Reply;
type Call = { key: string; authorization: string | null; body?: unknown };

let routes: Record<string, Handler> = {};
let calls: Call[] = [];
let root: Root | undefined;
let container: HTMLDivElement | undefined;

const ok = (body: unknown): Handler => () => ({ body });
const t = (text: { en: string }) => text.en;

function serve(table: Record<string, Handler>) {
  routes = table;
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit = {}) => {
    const key = `${(init.method ?? "GET").toUpperCase()} ${url}`;
    const authorization = new Headers(init.headers).get("Authorization");
    calls.push({ key, authorization, body: typeof init.body === "string" ? JSON.parse(init.body) : undefined });
    const reply: Reply = authorization === `Bearer ${TOKEN}`
      ? (routes[key]?.(init) ?? { body: {} })
      : { status: 401, body: { error: "unauthorized", message: "Bearer token required" } };
    return new Response(JSON.stringify(reply.body), { status: reply.status ?? 200 });
  }));
}

function callsTo(key: string) {
  return calls.filter((call) => call.key === key);
}

function expectBearer(key: string) {
  const matching = callsTo(key);
  expect(matching.length, key).toBeGreaterThan(0);
  expect(matching.map((call) => call.authorization)).toEqual(matching.map(() => `Bearer ${TOKEN}`));
}

async function settle(rounds = 8) {
  for (let i = 0; i < rounds; i += 1) {
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });
  }
}

async function render(element: ReactNode) {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => { root!.render(element); });
  await settle();
  return container;
}

async function unmount() {
  await act(async () => root?.unmount());
  root = undefined;
  container?.remove();
  container = undefined;
}

function buttonByText(scope: HTMLElement, text: string) {
  const button = [...scope.querySelectorAll("button")].find((node) => node.textContent?.trim() === text);
  if (!button) throw new Error(`button not found: ${text}`);
  return button;
}

function typeInto(input: HTMLInputElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  setter.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() { return values.size; },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => { values.delete(key); },
    setItem: (key, value) => { values.set(key, String(value)); },
  };
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("localStorage", memoryStorage());
  calls = [];
  beginAuthentication(TOKEN);
});

afterEach(async () => {
  await unmount();
  beginAuthentication(null);
  vi.unstubAllGlobals();
});

const receipt = (label: string) => ({
  period_label: label, period_start: "2026-10-01", period_end: "2026-10-29",
  models: [{ model: "m", display_name: `${label} model`, total_tokens: 1200, cost: 1.5, provider: "claude" }],
  subtotal: 1.5, cache_discount: 0, total: 1.5,
  stats: { total_messages: 3, total_sessions: 1 },
  providers: [{ provider: "claude", tokens: 1200, percentage: 100 }],
  agents: [],
});

const accounts = {
  schemaVersion: 1, status: "ok", hostname: "node-a", fetchedAt: new Date().toISOString(),
  servedAt: new Date().toISOString(), cacheTtlSeconds: 30, usageDataStale: false,
  accounts: [
    { number: 1, email: "active@example.com", active: true },
    { number: 2, email: "spare@example.com", active: false },
  ],
};

describe("dashboard widgets under token authentication", () => {
  it("rate limit bar sends the tab token and renders the providers", async () => {
    serve({
      "GET /api/rate-limits": ok({
        providers: [{ provider: "claude", buckets: [{ name: "5h", limit: 100, used: 40, remaining: 60, reset: 0 }], stale: false }],
      }),
    });
    const view = await render(<MiniRateLimitBar isKo={false} />);
    expectBearer("GET /api/rate-limits");
    expect(view.textContent).toContain("Claude");
    expect(view.textContent).toContain("40%");
  });

  it("receipt sends the tab token, a 401 starts re-login, and the next credential never sees the old receipt", async () => {
    serve({ "GET /api/receipt?period=month": ok(receipt("first-credential")) });
    let view = await render(<ReceiptWidget t={t} />);
    expectBearer("GET /api/receipt?period=month");
    expect(view.textContent).toContain("first-credential");
    await unmount();

    requireAuthentication("logged out");
    beginAuthentication(TOKEN);
    serve({ "GET /api/receipt?period=month": () => ({ status: 404, body: { error: "receipt unavailable" } }) });
    view = await render(<ReceiptWidget t={t} />);
    expect(view.textContent).not.toContain("first-credential");
    expect(view.textContent).toContain("Could not load receipt data.");
    await unmount();

    beginAuthentication("expired-token");
    await render(<ReceiptWidget t={t} />);
    expect(getAuthSnapshot().phase).toBe("required");
  });

  it("claude accounts load and switch send the tab token and render the results", async () => {
    serve({
      "GET /api/claude-accounts": ok(accounts),
      "POST /api/claude-accounts/switch": ok({ status: "ok", switched: true, hostname: "node-a" }),
    });
    const view = await render(<ClaudeAccountsWidget t={t} />);
    expectBearer("GET /api/claude-accounts");
    expect(view.textContent).toContain("spare@example.com");

    await act(async () => { buttonByText(view, "Switch").click(); });
    await settle();
    expectBearer("POST /api/claude-accounts/switch");
    expect(callsTo("POST /api/claude-accounts/switch")[0].body).toEqual({ account: "spare@example.com" });
    expect(view.textContent).toContain("Applied on node-a");
  });

  it("claude accounts keep rendering the server's failure body", async () => {
    serve({
      "GET /api/claude-accounts": () => ({
        status: 503,
        body: { status: "not_installed", error: "cswap missing", install: { command: "uv tool install claude-swap" } },
      }),
    });
    const view = await render(<ClaudeAccountsWidget t={t} />);
    expect(callsTo("GET /api/claude-accounts")).toHaveLength(1);
    expect(view.textContent).toContain("cswap is not installed");
    expect(view.textContent).toContain("uv tool install claude-swap · cswap missing");
  });
});

describe("settings under token authentication", () => {
  const entry = { key: "kanban_probe_label", value: "before", category: "pipeline", label_ko: "프로브", label_en: "Probe label" };

  it("loads and saves pipeline config with the tab token", async () => {
    serve({
      "GET /api/settings/config": ok({ entries: [entry] }),
      "PATCH /api/settings/config": ok({ ok: true }),
      "GET /api/github-repos": ok({ repos: [], viewer_login: "" }),
      "GET /api/agents": ok({ agents: [] }),
    });
    const notify = vi.fn();
    const settings = { companyName: "", ceoName: "", language: "en", theme: "dark" } as unknown as CompanySettings;
    const queries = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const view = await render(
      <QueryClientProvider client={queries}>
        <SettingsView settings={settings} onSave={async () => {}} isKo={false} onNotify={notify} />
      </QueryClientProvider>,
    );
    expectBearer("GET /api/settings/config");
    const input = [...view.querySelectorAll("input")].find((node) => node.value === "before");
    expect(input).toBeDefined();

    await act(async () => { typeInto(input!, "after"); });
    await act(async () => { buttonByText(view, "Save").click(); });
    await settle();
    expectBearer("PATCH /api/settings/config");
    expect(callsTo("PATCH /api/settings/config")[0].body).toEqual({ kanban_probe_label: "after" });
    expect(notify).toHaveBeenCalledWith("Saved pipeline settings.", "success");
  });

  it("saves voice config with the tab token and keeps the server's conflict details", async () => {
    serve({
      "PUT /api/voice/config": () => ({ status: 409, body: { message: "voice config changed elsewhere" } }),
    });
    const body = {} as VoiceConfigPutBody;
    const error = await saveVoiceConfig(body).catch((reason: unknown) => reason);
    expectBearer("PUT /api/voice/config");
    expect(error).toBeInstanceOf(VoiceConfigApiError);
    expect(error).toMatchObject({ status: 409, message: "voice config changed elsewhere" });
  });
});

describe("onboarding wizard under token authentication", () => {
  it("hydrates from status and syncs the draft with the tab token", async () => {
    serve({
      "GET /api/onboarding/status": ok({ setup_mode: "fresh", bot_tokens: { command: "server-command-token" } }),
      "GET /api/onboarding/draft": ok({ draft: null }),
      "PUT /api/onboarding/draft": ok({ ok: true }),
    });
    const view = await render(<OnboardingWizard isKo={false} onComplete={() => {}} />);
    expectBearer("GET /api/onboarding/status");
    expectBearer("GET /api/onboarding/draft");
    const input = [...view.querySelectorAll<HTMLInputElement>("input[type=password]")]
      .find((node) => node.value === "server-command-token");
    expect(input).toBeDefined();

    await act(async () => { typeInto(input!, "edited-command-token"); });
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 350)); });
    await settle();
    expectBearer("PUT /api/onboarding/draft");
  });
});
