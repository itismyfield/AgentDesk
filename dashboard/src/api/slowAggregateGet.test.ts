import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { request, SLOW_AGGREGATE_GET } from "./httpClient";
import { getTokenAnalytics } from "./stats";

// A fetch that answers after `delayMs`, or never, and rejects like the browser once aborted.
function serverTaking(delayMs: number | null) {
  return vi.fn((_url: string, init: RequestInit = {}) => new Promise<Response>((resolve, reject) => {
    init.signal?.addEventListener("abort", () => reject(Object.assign(new Error("aborted"), { name: "AbortError" })));
    if (delayMs !== null) setTimeout(() => resolve(new Response(JSON.stringify({ ok: true }))), delayMs);
  }));
}

function settled<T>(promise: Promise<T>) {
  const state: { value?: T; error?: string; done: boolean } = { done: false };
  promise.then(
    (value) => Object.assign(state, { value, done: true }),
    (error: Error) => Object.assign(state, { error: error.message, done: true }),
  );
  return state;
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("slow aggregate GETs", () => {
  it("sends one request and gives up at the timeout without retrying", async () => {
    const fetchMock = serverTaking(null);
    vi.stubGlobal("fetch", fetchMock);
    const outcome = settled(request("/api/receipt?period=month", { ...SLOW_AGGREGATE_GET, suppressErrorToast: true }));

    await vi.advanceTimersByTimeAsync(119_000);
    expect(outcome.done).toBe(false);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(outcome.error).toBe("Request timeout: /api/receipt?period=month");
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("accepts an answer that takes the measured 78 seconds", async () => {
    const fetchMock = serverTaking(78_000);
    vi.stubGlobal("fetch", fetchMock);
    const outcome = settled(request("/api/receipt?period=month", { ...SLOW_AGGREGATE_GET, suppressErrorToast: true }));

    await vi.advanceTimersByTimeAsync(78_000);
    expect(outcome).toEqual({ value: { ok: true }, done: true });
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("token analytics waits for its slow aggregate once", async () => {
    const fetchMock = serverTaking(null);
    vi.stubGlobal("fetch", fetchMock);
    const outcome = settled(getTokenAnalytics("30d", { suppressErrorToast: true }));

    await vi.advanceTimersByTimeAsync(60_000);
    expect(outcome.done).toBe(false);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(outcome.error).toBe("Request timeout: /api/token-analytics?period=30d");
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("leaves other GETs on their 15 second timeout with two retries", async () => {
    const fetchMock = serverTaking(null);
    vi.stubGlobal("fetch", fetchMock);
    const outcome = settled(request("/api/stats", { suppressErrorToast: true }));
    // [elapsed ms, fetches sent by then]: a 15s timeout, then 500ms and 1000ms backoff before each retry.
    const schedule: Array<[number, number]> = [
      [14_999, 1], [15_499, 1], [15_500, 2], [30_499, 2], [31_499, 2], [31_500, 3], [46_499, 3],
    ];
    let now = 0;
    for (const [at, calls] of schedule) {
      await vi.advanceTimersByTimeAsync(at - now);
      now = at;
      expect(fetchMock, `at ${at}ms`).toHaveBeenCalledTimes(calls);
    }
    expect(outcome.done).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    expect(outcome.error).toBe("Request timeout: /api/stats");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });
});
