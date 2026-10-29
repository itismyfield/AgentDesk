// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { beginAuthentication } from "../../api/authState";
import ReceiptWidget from "./ReceiptWidget";

const t = (text: { en: string }) => text.en;
let root: Root | undefined;
let container: HTMLDivElement | undefined;

const receipt = {
  period_label: "This Month", period_start: "2026-10-01", period_end: "2026-10-29",
  models: [{ model: "m", display_name: "Model M", total_tokens: 10, cost: 1, provider: "claude" }],
  subtotal: 1, cache_discount: 0, total: 1, stats: { total_messages: 1, total_sessions: 1 },
  providers: [{ provider: "claude", tokens: 10, percentage: 100 }], agents: [],
};
// A /api/receipt server: `answer` replies for a period, or returns null to hang until aborted.
function receiptServer(answer: (period: string) => Response | null) {
  return vi.fn((url: string, init: RequestInit = {}) => new Promise<Response>((resolve, reject) => {
    init.signal?.addEventListener("abort", () => reject(Object.assign(new Error("aborted"), { name: "AbortError" })));
    const reply = answer(new URL(url, "http://local").searchParams.get("period") ?? "");
    if (reply) resolve(reply);
  }));
}
const receiptCalls = (fetchMock: ReturnType<typeof receiptServer>) =>
  fetchMock.mock.calls.filter(([url]) => String(url).startsWith("/api/receipt")).map(([url]) => String(url));

async function mount() {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => root!.render(<ReceiptWidget t={t} />));
  return container;
}
const advance = (ms: number) => act(async () => { await vi.advanceTimersByTimeAsync(ms); });

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  beginAuthentication(null); // also drops the widget's module-level receipt cache and failure times
  vi.useFakeTimers();
});

afterEach(async () => {
  await act(async () => root?.unmount());
  container?.remove();
  root = undefined;
  container = undefined;
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("ReceiptWidget slow aggregation", () => {
  it("keeps one receipt request in flight across its 60 second poll and shows the elapsed wait", async () => {
    const fetchMock = receiptServer(() => null);
    vi.stubGlobal("fetch", fetchMock);
    const view = await mount();

    await advance(119_000);
    expect(receiptCalls(fetchMock)).toHaveLength(1);
    expect(view.textContent).toContain("The server is aggregating the receipt");
    expect(view.textContent).toContain("119s");
  });

  it("does not let its poll start another aggregation until three minutes after a failure", async () => {
    const fetchMock = receiptServer(() => null);
    vi.stubGlobal("fetch", fetchMock);
    const view = await mount();

    await advance(120_000);
    expect(view.textContent).toContain("Could not load receipt data.");
    await advance(179_000);
    expect(receiptCalls(fetchMock)).toHaveLength(1);
    await advance(1_000);
    expect(receiptCalls(fetchMock)).toHaveLength(2);
  });

  it("does not hold back the next credential's poll after a sign-in change aborts a load", async () => {
    const fetchMock = receiptServer(() => null);
    vi.stubGlobal("fetch", fetchMock);
    await mount();

    await advance(10_000);
    await act(async () => beginAuthentication("next-token"));
    await advance(50_000);
    expect(receiptCalls(fetchMock)).toHaveLength(2);
  });

  it("sends a failed receipt load once instead of retrying it", async () => {
    const fetchMock = receiptServer(() => new Response(JSON.stringify({ error: "gateway timeout" }), { status: 504 }));
    vi.stubGlobal("fetch", fetchMock);
    const view = await mount();

    await advance(5_000);
    expect(receiptCalls(fetchMock)).toHaveLength(1);
    expect(view.textContent).toContain("Could not load receipt data.");
  });

  it("restarts the elapsed time when another period starts loading", async () => {
    const fetchMock = receiptServer((period) => period === "month" ? new Response(JSON.stringify(receipt)) : null);
    vi.stubGlobal("fetch", fetchMock);
    const view = await mount();
    const press = (label: string) => act(async () => {
      [...view.querySelectorAll("button")].find((button) => button.textContent === label)!.click();
    });

    await press("This Week");
    await advance(40_000);
    expect(view.textContent).toContain("SYNCING · 40s");
    await press("All");
    await advance(2_000);
    expect(view.textContent).toContain("SYNCING · 2s");
    expect(receiptCalls(fetchMock)).toEqual([
      "/api/receipt?period=month", "/api/receipt?period=week", "/api/receipt?period=all",
    ]);
  });
});
