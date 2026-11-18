import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { beginAuthentication } from "./authState";
import { ApiRequestError, onApiError } from "./httpClient";
import { pauseRoutine, resumeRoutine } from "./routines";

const fetchMock = vi.fn<typeof fetch>();
function response(payload: unknown, status = 200) {
  return new Response(JSON.stringify(payload), { status, headers: { "Content-Type": "application/json" } });
}
function sent(index = 0) {
  const [url, init] = fetchMock.mock.calls[index];
  return { url, method: init?.method, body: init?.body };
}

beforeEach(() => {
  beginAuthentication(null);
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});
afterEach(() => {
  onApiError(null);
  vi.unstubAllGlobals();
});

it("pauses with a bare POST to the routine's pause endpoint", async () => {
  fetchMock.mockResolvedValueOnce(response({ ok: true, routine_id: "a/b", discord_log: null }));
  await expect(pauseRoutine("a/b")).resolves.toEqual({ ok: true, routine_id: "a/b", discord_log: null });
  expect(sent()).toEqual({ url: "/api/routines/a%2Fb/pause", method: "POST", body: undefined });
});

it("resumes with an empty body so the stored next_due_at is kept", async () => {
  fetchMock.mockResolvedValueOnce(response({ ok: true, routine_id: "rt-1" }));
  await resumeRoutine("rt-1");
  expect(sent()).toEqual({ url: "/api/routines/rt-1/resume", method: "POST", body: "{}" });
});

it("rejects once with the server error and leaves the toast to the caller", async () => {
  const globalToast = vi.fn();
  onApiError(globalToast);
  fetchMock.mockResolvedValueOnce(response({ error: "enabled routine rt-1 not found", code: "not_found" }, 404));
  const failure = await pauseRoutine("rt-1").catch((error: unknown) => error);
  expect(failure).toBeInstanceOf(ApiRequestError);
  expect((failure as ApiRequestError).status).toBe(404);
  expect((failure as ApiRequestError).message).toBe("enabled routine rt-1 not found");

  fetchMock.mockResolvedValueOnce(response({ error: "boom" }, 503));
  await expect(resumeRoutine("rt-1")).rejects.toThrow("boom");
  expect(fetchMock).toHaveBeenCalledTimes(2);
  expect(globalToast).not.toHaveBeenCalled();
});
