import { afterEach, expect, it, vi } from "vitest";
import { getAutoQueueStatus } from "./autoQueue";

function response(payload: unknown) {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}
afterEach(() => vi.unstubAllGlobals());

it("sends a fresh status read even while the same GET is in flight", async () => {
  let releasePoll: (value: Response) => void = () => {};
  const fetchMock = vi
    .fn<typeof fetch>()
    .mockImplementationOnce(() => new Promise((resolve) => (releasePoll = resolve)))
    .mockImplementation(() => Promise.resolve(response({ run: { id: "run-new" }, entries: [] })));
  vi.stubGlobal("fetch", fetchMock);

  const polled = getAutoQueueStatus("owner/repo");
  const joined = getAutoQueueStatus("owner/repo");
  const fresh = getAutoQueueStatus("owner/repo", null, { fresh: true });
  await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
  await expect(fresh).resolves.toMatchObject({ run: { id: "run-new" } });

  releasePoll(response({ run: null, entries: [] }));
  await expect(polled).resolves.toMatchObject({ run: null });
  await expect(joined).resolves.toMatchObject({ run: null });
  expect(fetchMock).toHaveBeenCalledTimes(2);
});
