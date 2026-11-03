import { afterEach, beforeEach, expect, it, vi } from "vitest";

const fetchMock = vi.fn<typeof fetch>();
const json = (status: number, payload: unknown) => new Response(JSON.stringify(payload), { status, headers: { "Content-Type": "application/json" } });
const summary = { status: "healthy", version: "2.4.1", db: true };
const detail = { ...summary, uptime_secs: 60, providers: [{ name: "claude", connected: true }], dispatch_outbox: { pending: 2, retrying: 0, permanent_failures: 0, oldest_pending_age: 0 } };

// The 403 memo lives in module state, so each test starts from a fresh module.
async function load() {
  vi.resetModules();
  const http = await import("./httpClient");
  const toast = vi.fn();
  http.onApiError(toast);
  return { ...(await import("./settingsRuntime")), toast };
}
beforeEach(() => { fetchMock.mockReset(); vi.stubGlobal("fetch", fetchMock); });
afterEach(() => vi.unstubAllGlobals());
const urls = () => fetchMock.mock.calls.map(([url]) => String(url));

it("falls back to the public summary on 403 without a toast, and stops asking for detail", async () => {
  const { getHealthDetail, toast } = await load();
  fetchMock.mockImplementation(async (url) => String(url) === "/api/health/detail"
    ? json(403, { error: "auth_token required for non-loopback host" })
    : json(200, summary));

  const first = await getHealthDetail();
  expect(first).toMatchObject({ status: "healthy", publicOnly: "forbidden", providers: [] });
  const second = await getHealthDetail();
  expect(second.publicOnly).toBe("forbidden");
  expect(urls()).toEqual(["/api/health/detail", "/api/health", "/api/health"]);
  expect(toast).not.toHaveBeenCalled();
});

it("keeps asking for detail after a transient failure and marks the summary as a fallback", async () => {
  const { getHealthDetail } = await load();
  let detailUp = false;
  fetchMock.mockImplementation(async (url) => String(url) === "/api/health/detail"
    ? detailUp ? json(200, detail) : json(400, { error: "bad gateway config" })
    : json(200, summary));

  expect((await getHealthDetail()).publicOnly).toBe("failed");
  detailUp = true;
  const recovered = await getHealthDetail();
  expect(recovered.publicOnly).toBeUndefined();
  expect(recovered.dispatch_outbox?.pending).toBe(2);
  expect(urls().at(-1)).toBe("/api/health/detail");
});
