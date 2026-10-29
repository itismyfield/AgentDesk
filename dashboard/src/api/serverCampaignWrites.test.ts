import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { beginAuthentication } from "./authState";
import { onApiError } from "./httpClient";
import {
  createServerCampaign, editServerCampaignDefinition, getServerCampaignEvents, isDefinitiveAnswer,
  sendServerCampaignCommand, serverCampaignRejection, type ServerCampaignRejection,
} from "./serverCampaigns";
import { serverCampaignFixtures as fixtures } from "./serverCampaignsFixtures";

const fetchMock = vi.fn<typeof fetch>();
function response(payload: unknown, status = 200) {
  return new Response(JSON.stringify(payload), { status, headers: { "Content-Type": "application/json" } });
}
const result = { definition_revision: 8, run_id: null, control_version: null, event_sequence: 2, control: "prepared" };
function sent(index = 0) {
  const [url, init] = fetchMock.mock.calls[index];
  return { url, method: init?.method, body: JSON.parse(String(init?.body)) };
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

it("creates a definition with its own idempotency key and no execution fields", async () => {
  fetchMock.mockResolvedValueOnce(response({ result }, 201));
  const definition = fixtures.prepared.definition;
  await expect(createServerCampaign("release-2", definition, "key-1")).resolves.toEqual(result);
  expect(sent()).toEqual({
    url: "/api/server-campaigns", method: "POST",
    body: { id: "release-2", idempotency_key: "key-1", definition },
  });
});

it("edits only the definition against the expected revision with the change reason", async () => {
  fetchMock.mockResolvedValueOnce(response({ result }));
  const change = { reference: "https://github.com/itismyfield/AgentDesk/issues/7438", explanation: "Split AC" };
  await editServerCampaignDefinition("release", 7, fixtures.prepared.definition, change, "key-2");
  expect(sent()).toEqual({
    url: "/api/server-campaigns/release/definition", method: "PUT",
    body: { idempotency_key: "key-2", expected_definition_revision: 7, definition: fixtures.prepared.definition, change },
  });
});

it("sends a stop for the run and definition revision only", async () => {
  fetchMock.mockResolvedValueOnce(response({ result: { ...result, run_id: "run-1", control: "stopping" } }, 202));
  await sendServerCampaignCommand("release", {
    command: "stop", expected_definition_revision: 7, expected_run: "run-1", mode: "finish_stage",
  }, "stop-key");
  expect(sent()).toEqual({
    url: "/api/server-campaigns/release/commands", method: "POST",
    body: {
      idempotency_key: "stop-key",
      command: { command: "stop", expected_definition_revision: 7, expected_run: "run-1", mode: "finish_stage" },
    },
  });
});

it("reads the server's refusals in the shapes the server sends", async () => {
  const refused = (status: number, context: unknown) => response({ error: "server campaign command rejected", code: "x", context }, status);
  fetchMock
    .mockResolvedValueOnce(refused(400, { rejection: "invalid_definition", target: null, definition_error: { code: "cycle", target: null, path: ["a", "a"] } }))
    .mockResolvedValueOnce(refused(409, { rejection: "stale_definition", target: null }))
    .mockResolvedValueOnce(response({ error: "idempotency_key already used for a different request", code: "conflict" }, 409))
    .mockResolvedValueOnce(refused(503, {}));
  const settle = (call: Promise<unknown>) => call.catch(serverCampaignRejection);
  expect(await settle(createServerCampaign("x", fixtures.prepared.definition, "k"))).toEqual({
    status: 400, rejection: "invalid_definition", target: null,
    definitionError: { code: "cycle", target: null, path: ["a", "a"] },
  });
  expect(await settle(editServerCampaignDefinition("release", 6, fixtures.prepared.definition, null, "k2")))
    .toEqual({ status: 409, rejection: "stale_definition", target: null, definitionError: null });
  const reused = await settle(sendServerCampaignCommand("release", { command: "start", expected_definition_revision: 7 }, "k3"));
  expect(reused).toEqual({ status: 409, rejection: null, target: null, definitionError: null });
  const unavailable = await settle(sendServerCampaignCommand("release", { command: "start", expected_definition_revision: 7 }, "k4"));
  expect(isDefinitiveAnswer(reused as ServerCampaignRejection)).toBe(true);
  expect(isDefinitiveAnswer(unavailable as ServerCampaignRejection)).toBe(false);
  expect(serverCampaignRejection(new TypeError("Failed to fetch"))).toBeNull();
  expect(isDefinitiveAnswer(null)).toBe(false);
});

it("reads the latest events after a sequence", async () => {
  const event = {
    sequence: 3, run_id: "run-1", command_id: "stop-key", observed_at: "2026-10-28T05:00:00Z",
    kind: { event: "stop_requested", mode: "immediate" },
  };
  fetchMock.mockResolvedValueOnce(response({ events: [event] }));
  await expect(getServerCampaignEvents("release", { after: 2, runId: "run-1", limit: 20 })).resolves.toEqual([event]);
  expect(fetchMock.mock.calls[0][0]).toBe("/api/server-campaigns/release/events?after=2&limit=20&run_id=run-1");
});
