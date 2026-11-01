import { afterEach, describe, expect, it, vi } from "vitest";
import { clearCachedGet, request } from "./httpClient";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("ETag revalidation", () => {
  it("sends the last ETag back and reuses the remembered body on 304", async () => {
    const url = "/api/campaigns/etag-a";
    const seen: Array<string | null> = [];
    const fetchMock = vi.fn((_url: string, init: RequestInit = {}) => {
      const tag = new Headers(init.headers).get("If-None-Match");
      seen.push(tag);
      if (tag === '"v1"') return Promise.resolve(new Response(null, { status: 304, headers: { ETag: '"v1"' } }));
      return Promise.resolve(new Response(JSON.stringify({ id: "a", round: 1 }), { status: 200, headers: { ETag: '"v1"' } }));
    });
    vi.stubGlobal("fetch", fetchMock);

    const first = await request<{ id: string; round: number }>(url, { shareInflight: false });
    const second = await request<{ id: string; round: number }>(url, { shareInflight: false });

    expect(seen).toEqual([null, '"v1"']);
    expect(second).toEqual(first);
    clearCachedGet(url);
  });

  it("drops the remembered body when a response has no ETag", async () => {
    const url = "/api/campaigns/etag-b";
    const seen: Array<string | null> = [];
    let withTag = true;
    vi.stubGlobal("fetch", vi.fn((_url: string, init: RequestInit = {}) => {
      seen.push(new Headers(init.headers).get("If-None-Match"));
      const headers: Record<string, string> = withTag ? { ETag: '"v1"' } : {};
      return Promise.resolve(new Response(JSON.stringify({ ok: true }), { status: 200, headers }));
    }));

    await request(url, { shareInflight: false });
    withTag = false;
    await request(url, { shareInflight: false });
    await request(url, { shareInflight: false });

    expect(seen).toEqual([null, '"v1"', null]);
  });
});
