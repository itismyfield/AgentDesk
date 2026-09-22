import { afterEach, describe, expect, it, vi } from "vitest";
import { beginAuthentication, credentialScope, getAuthSnapshot, requireAuthentication, resolveAuthentication } from "./authState";
import { getDashboardSession, getDashboardSocketTicket } from "./dashboardAuth";
import { readCachedGet, request } from "./httpClient";

afterEach(() => { beginAuthentication(null); vi.unstubAllGlobals(); });
const response = (value: unknown) => new Response(JSON.stringify(value), { status: 200 });

describe("dashboard credential boundary", () => {
  it("uses Bearer for HTTP and obtains only a short-lived socket ticket", async () => {
    beginAuthentication("tab-secret");
    const fetcher = vi.fn().mockResolvedValue(response({ ticket: "t".repeat(64), expires_in: 15 }));
    vi.stubGlobal("fetch", fetcher);
    await getDashboardSocketTicket(new AbortController().signal);
    expect(fetcher).toHaveBeenCalledOnce();
    const [url, options] = fetcher.mock.calls[0];
    expect(url).toBe("/api/auth/ws-ticket");
    expect(options.headers.get("Authorization")).toBe("Bearer tab-secret");
    expect(options.cache).toBe("no-store");
    expect(url).not.toContain("tab-secret");
    expect(options.body).toBe("{}");
  });

  it("rejects stale responses even when fetch ignores cancellation; new requests do not dedupe to the old token", async () => {
    beginAuthentication("old-token");
    let finishOld!: (response: Response) => void;
    const fetcher = vi.fn().mockImplementationOnce(() => new Promise<Response>((resolve) => { finishOld = resolve; }))
      .mockResolvedValueOnce(response({ owner: "new" }));
    vi.stubGlobal("fetch", fetcher);
    const old = request("/api/credential-race");
    const rejected = expect(old).rejects.toMatchObject({ name: "AbortError" });
    const oldSignal = fetcher.mock.calls[0][1].signal as AbortSignal;
    beginAuthentication("new-token");
    expect(oldSignal.aborted).toBe(true);
    const current = await request("/api/credential-race");
    finishOld(response({ owner: "old" }));
    await rejected;
    expect(current).toEqual({ owner: "new" });
    expect(readCachedGet("/api/credential-race")?.data).toEqual(current);
    requireAuthentication("logged out");
    expect(readCachedGet("/api/credential-race")).toBeNull();
    expect(credentialScope().token).toBeNull();
  });

  it("stops on 401 and requires reauthentication without retrying", async () => {
    beginAuthentication("expired");
    const fetcher = vi.fn().mockResolvedValue(new Response('{"error":"unauthorized"}', { status: 401 }));
    vi.stubGlobal("fetch", fetcher);
    await expect(request("/api/protected")).rejects.toMatchObject({ name: "AbortError" });
    expect(fetcher).toHaveBeenCalledOnce();
    expect(getAuthSnapshot().phase).toBe("required");
    expect(credentialScope().token).toBeNull();
  });

  it("validates the session response and does not accept an older authentication result", async () => {
    beginAuthentication("old");
    const oldGeneration = getAuthSnapshot().generation;
    beginAuthentication("new");
    resolveAuthentication(oldGeneration, true, true);
    expect(getAuthSnapshot().phase).toBe("checking");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response({ ok: true, csrf_token: "" })));
    await expect(getDashboardSession()).rejects.toThrow();
    resolveAuthentication(getAuthSnapshot().generation, false, true);
    expect(getAuthSnapshot().phase).toBe("required");
    expect(credentialScope().token).toBeNull();
  });
});
