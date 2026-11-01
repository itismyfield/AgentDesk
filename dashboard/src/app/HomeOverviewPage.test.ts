import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { clearCachedGet, getTokenAnalytics, onApiError } from "../api/client";
import { loadHomeTokenAnalytics } from "./HomeOverviewPage";

const URL_7D = "/api/token-analytics?period=7d";

// A fetch whose answers wait until the test settles them, and that rejects like the browser once aborted.
function heldFetch() {
  const pending: Array<(response: Response) => void> = [];
  const fetchMock = vi.fn((_url: string, init: RequestInit = {}) => new Promise<Response>((resolve, reject) => {
    init.signal?.addEventListener("abort", () => reject(Object.assign(new Error("aborted"), { name: "AbortError" })));
    pending.push(resolve);
  }));
  vi.stubGlobal("fetch", fetchMock);
  return { fetchMock, answerAll: (response: () => Response) => pending.forEach((resolve) => resolve(response())) };
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

beforeEach(() => {
  clearCachedGet(URL_7D);
});

afterEach(() => {
  onApiError(null);
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("home token analytics load", () => {
  it("shares one in-flight 7d request with another caller", async () => {
    const { fetchMock, answerAll } = heldFetch();
    const seen: unknown[] = [];
    const stop = loadHomeTokenAnalytics((next) => seen.push(next));
    const other = getTokenAnalytics("7d", { suppressErrorToast: true });
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(1);

    answerAll(() => new Response(JSON.stringify({ period: "7d" })));
    await other;
    await flush();
    expect(seen).toEqual([{ period: "7d" }]);
    stop();
  });

  it("after stop, a late answer or failure neither reaches the page nor raises a toast", async () => {
    const { answerAll } = heldFetch();
    const onData = vi.fn();
    const toast = vi.fn();
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    onApiError(toast);
    const stop = loadHomeTokenAnalytics(onData);
    stop();

    answerAll(() => new Response("boom", { status: 500 }));
    await flush();
    await flush();
    expect(onData).not.toHaveBeenCalled();
    expect(toast).not.toHaveBeenCalled();
    expect(consoleError).not.toHaveBeenCalled();
  });
});
