import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { ZodError } from "zod";
import { beginAuthentication } from "./authState";
import { ApiRequestError, onApiError, readCachedGet } from "./httpClient";
import {
  getServerCampaign, getServerCampaigns, serverCampaignDocumentSchema,
  type ServerCampaignDetailResponse, type ServerCampaignListResponse,
} from "./serverCampaigns";
import { serverCampaignFixtures as fixtures } from "./serverCampaignsFixtures";
import sharedPreparedDetail from "../../../tests/fixtures/server_campaigns/detail-prepared.json";

const listUrl = "/api/server-campaigns";
const detailUrl = `${listUrl}/release`;
const fetchMock = vi.fn<typeof fetch>();
function response(payload: unknown, status = 200) {
  return new Response(JSON.stringify(payload), { status, headers: { "Content-Type": "application/json" } });
}
beforeEach(() => {
  beginAuthentication(null);
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});
afterEach(() => {
  onApiError(null);
  beginAuthentication(null);
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("reads the server list and encoded detail with GET and no body", async () => {
  fetchMock
    .mockResolvedValueOnce(response({ campaigns: [fixtures.prepared] }))
    .mockResolvedValueOnce(response({ campaign: fixtures.prepared }));
  const list = await getServerCampaigns();
  const detail = await getServerCampaign("release /한글?#%");
  expect(list[0].definition.title).toBe("Release");
  expect(detail.view.execution_mode).toBe("server");
  expect(fetchMock.mock.calls.map(([url]) => url)).toEqual([
    listUrl, `${listUrl}/release%20%2F%ED%95%9C%EA%B8%80%3F%23%25`,
  ]);
  for (const [, options] of fetchMock.mock.calls) {
    expect(options?.method).toBe("GET");
    expect(options?.body).toBeUndefined();
  }
  expect(readCachedGet<ServerCampaignListResponse>(listUrl)?.data.campaigns).toEqual(list);
});

it.each(Object.entries(fixtures))("parses the %s fixture through an actual fetch Response", async (_name, fixture) => {
  fetchMock.mockResolvedValueOnce(response({ campaign: fixture }));
  const parsed = await getServerCampaign("release");
  expect(parsed).toEqual(fixture);
  expect(readCachedGet<ServerCampaignDetailResponse>(detailUrl)?.data.campaign).toEqual(parsed);
});

it("parses the shared B prepared detail fixture through the GET adapter and schema boundary", async () => {
  fetchMock.mockResolvedValueOnce(response(sharedPreparedDetail));
  const parsed = await getServerCampaign(sharedPreparedDetail.campaign.view.campaign_id);
  expect(fetchMock).toHaveBeenCalledExactlyOnceWith("/api/server-campaigns/campaign", expect.objectContaining({ method: "GET" }));
  expect(parsed).toEqual(sharedPreparedDetail.campaign);
  expect(parsed.view).toMatchObject({
    execution_mode: "server", control: "prepared", display: "prepared", execution_readiness: "unavailable",
  });
  expect(parsed.view.commands).toContainEqual({ command: "edit_definition", rejection: null });
  expect(parsed.view.commands).toContainEqual({ command: "start", rejection: "execution_unavailable" });
});

function change(path: string, value: unknown, omit = false): unknown {
  const document = structuredClone(fixtures.stoppingUnknown);
  const keys = path.split(".");
  let parent: unknown = document;
  for (const key of keys.slice(0, -1)) parent = (parent as Record<string, unknown>)[key];
  const record = parent as Record<string, unknown>;
  if (omit) delete record[keys.at(-1)!];
  else record[keys.at(-1)!] = value;
  return document;
}

it.each([
  "definition", "view", "definition.nodes.0.group", "view.as_of", "view.run_id",
  "view.control_version", "view.last_event_at", "view.stop_mode", "view.commands.0.rejection",
  "view.nodes.0.runtime.current_attempt", "view.nodes.0.runtime.wait_reason",
  "view.sessions.0.identity.incarnation", "view.sessions.0.identity.gate.incarnation",
  "view.sessions.0.observation.last_observed_at", "view.unreconciled_actions.0.target",
  "view.unreconciled_actions.0.binding.hashes", "view.unreconciled_actions.0.initiation_receipt",
  "view.unreconciled_actions.0.reconciliation",
])("rejects missing serialized field %s instead of inventing defaults", (path) => {
  expect(() => serverCampaignDocumentSchema.parse(change(path, undefined, true))).toThrow(ZodError);
});

it.each([
  ["view.execution_mode", "legacy"], ["view.control", "active"], ["view.display", "completed"],
  ["view.execution_readiness", "ready"], ["view.stop_mode", "drain"], ["view.unknown", "false"],
  ["view.as_of", "yesterday"], ["view.run_id", 1], ["view.control_version", "2"],
  ["view.definition_revision", -1], ["view.last_event_sequence", Number.MAX_SAFE_INTEGER + 1],
  ["view.nodes.0.runtime.series", 4_294_967_296], ["view.failed_count", 1.5],
  ["view.reasons.0.code", "ready"], ["view.reasons.0.target", null],
  ["view.commands.0.command", "complete"], ["view.commands.0.rejection", "ok"],
  ["view.nodes.0.runtime.stage", "deploy"], ["view.nodes.0.runtime.disposition", "done"],
  ["view.nodes.0.runtime.main_integrated", null], ["view.nodes", null], ["view.reasons", null],
  ["view.sessions.0.observation.state", "dead"], ["view.sessions.0.observation.last_observed_at", 0],
  ["view.unreconciled_actions.0.state", "pending"], ["view.unreconciled_actions.0.binding.role", "coordinator"],
  ["view.unreconciled_actions.0.binding.repository", "other"],
  ["definition.nodes.0.acceptance.0.verification", "manual"], ["definition.nodes.0.dependencies", null],
  ["definition.nodes.0.group", 1],
])("rejects invalid enum/type/null at %s (%s)", (path, value) => {
  expect(() => serverCampaignDocumentSchema.parse(change(path as string, value))).toThrow(ZodError);
});

it.each(["alive", "gone"] as const)("requires %s observation evidence fields", (state) => {
  const observation = state === "alive" ? { state } : { state, observed_at: "2026-10-28T05:00:00Z" };
  expect(() => serverCampaignDocumentSchema.parse(change("view.sessions.0.observation", observation))).toThrow(ZodError);
});

it.each(["list", "detail"] as const)("rejects malformed %s reads before they can overwrite a validated cache", async (kind) => {
  vi.useFakeTimers();
  const read = () => kind === "list" ? getServerCampaigns() : getServerCampaign("release");
  const envelope = (document: unknown) => kind === "list" ? { campaigns: [document] } : { campaign: document };
  const url = kind === "list" ? listUrl : detailUrl;
  fetchMock.mockResolvedValueOnce(response(envelope(fixtures.prepared)));
  await read();
  const cached = readCachedGet(url);
  // Each retry receives the same malformed JSON in a fresh, unconsumed Response.
  fetchMock.mockImplementation(async () => response(envelope(change("view.display", "completed"))));
  const result = read().catch((error: unknown) => error);
  await vi.runAllTimersAsync();
  expect(await result).toBeInstanceOf(ZodError);
  expect(fetchMock).toHaveBeenCalledTimes(4);
  expect(readCachedGet(url)).toEqual(cached);
  expect(fetchMock.mock.calls.every(([url]) => String(url).startsWith(listUrl))).toBe(true);
});

it("accepts an explicit empty list and rejects a missing list field", async () => {
  vi.useFakeTimers();
  fetchMock.mockResolvedValueOnce(response({ campaigns: [] }));
  expect(await getServerCampaigns()).toEqual([]);
  fetchMock.mockImplementation(async () => response({}));
  const result = getServerCampaigns().catch((error: unknown) => error);
  await vi.runAllTimersAsync();
  expect(await result).toBeInstanceOf(ZodError);
  expect(readCachedGet<ServerCampaignListResponse>(listUrl)?.data.campaigns).toEqual([]);
});

it.each([404, 503])("preserves HTTP %s status, message and server code without falling back", async (status) => {
  vi.useFakeTimers();
  fetchMock.mockResolvedValueOnce(response({ campaign: fixtures.prepared }));
  await getServerCampaign("release");
  const cached = readCachedGet(detailUrl);
  fetchMock.mockImplementation(async () => response({ error: "Server refused read", code: "campaign_read_failed" }, status));
  const result = getServerCampaign("release").catch((error: unknown) => error);
  await vi.runAllTimersAsync();
  const error = await result;
  expect(error).toBeInstanceOf(ApiRequestError);
  expect(error).toMatchObject({ status, message: "Server refused read", serverCode: "campaign_read_failed" });
  expect(fetchMock).toHaveBeenCalledTimes(status === 404 ? 2 : 4);
  expect(readCachedGet(detailUrl)).toEqual(cached);
  expect(fetchMock.mock.calls.every(([url]) => url === detailUrl)).toBe(true);
});

it.each(["list", "detail"] as const)("starts a separate %s fetch while an older read is in flight", async (kind) => {
  let finishOld!: (value: Response) => void;
  const envelope = (document: unknown) => kind === "list" ? { campaigns: [document] } : { campaign: document };
  const read = () => kind === "list" ? getServerCampaigns() : getServerCampaign("release");
  fetchMock
    .mockImplementationOnce(() => new Promise<Response>((resolve) => { finishOld = resolve; }))
    .mockResolvedValueOnce(response(envelope(fixtures.eventAdvanced)));
  const older = read();
  const newer = read();
  try {
    expect(fetchMock).toHaveBeenCalledTimes(2);
    const current = await newer;
    finishOld(response(envelope(fixtures.stoppingUnknown)));
    const previous = await older;
    const currentDocument = Array.isArray(current) ? current[0] : current;
    const previousDocument = Array.isArray(previous) ? previous[0] : previous;
    expect(currentDocument.view.last_event_sequence).toBe(24);
    expect(previousDocument.view.last_event_sequence).toBe(23);
    expect(currentDocument.view.definition_revision).toBe(previousDocument.view.definition_revision);
    expect(currentDocument.view.control_version).toBe(previousDocument.view.control_version);
  } finally {
    finishOld(response(envelope(fixtures.stoppingUnknown)));
    await Promise.allSettled([older, newer]);
  }
});

it.each(["list", "detail"] as const)("forwards cancellation for %s reads to the common client", async (kind) => {
  const controller = new AbortController();
  fetchMock.mockImplementation((_url, options) => new Promise((_resolve, reject) => {
    options?.signal?.addEventListener("abort", () => reject(new DOMException("Aborted", "AbortError")), { once: true });
  }));
  const pending = kind === "list" ? getServerCampaigns({ signal: controller.signal })
    : getServerCampaign("release", { signal: controller.signal });
  const rejected = expect(pending).rejects.toMatchObject({ name: "AbortError" });
  controller.abort();
  expect(fetchMock.mock.calls[0][1]?.signal?.aborted).toBe(true);
  await rejected;
  expect(fetchMock).toHaveBeenCalledOnce();
});

it.each(["list", "detail"] as const)("forwards suppressErrorToast for %s reads", async (kind) => {
  const listener = vi.fn();
  onApiError(listener);
  fetchMock.mockImplementation(async () => response({ error: "Missing" }, 404));
  const read = (suppressErrorToast?: boolean) => kind === "list" ? getServerCampaigns({ suppressErrorToast })
    : getServerCampaign("release", { suppressErrorToast });
  await expect(read()).rejects.toBeInstanceOf(ApiRequestError);
  expect(listener).not.toHaveBeenCalled();
  await expect(read(false)).rejects.toBeInstanceOf(ApiRequestError);
  expect(listener).toHaveBeenCalledExactlyOnceWith(kind === "list" ? listUrl : detailUrl, expect.any(ApiRequestError));
});

it("preserves server progress, reasons and command refusals across independent branches", async () => {
  fetchMock.mockResolvedValueOnce(response({ campaigns: [fixtures.failedRunning, fixtures.failedBlockedComplete, fixtures.reviewWaiting] }));
  const [running, blocked, waiting] = (await getServerCampaigns()).map((document) => document.view);
  expect(running.display).toBe("running");
  expect(blocked.display).toBe("unable_to_progress");
  expect(blocked).toMatchObject({ control: "running", failed_count: 1, blocked_count: 1 });
  expect(blocked.reasons).toContainEqual({ code: "predecessor_failed", target: "B" });
  expect(waiting.display).toBe("waiting");
  expect(waiting.reasons).toContainEqual({ code: "review_resources", target: "A" });
  expect(waiting.commands).toContainEqual({ command: "start", rejection: "execution_unavailable" });
  expect(waiting.commands).toContainEqual({ command: "stop_finish_stage", rejection: null });
});

it("keeps main integration separate from final acceptance and server successor readiness", async () => {
  fetchMock.mockResolvedValueOnce(response({ campaign: fixtures.mainPendingAcceptance }));
  const { view } = await getServerCampaign("release");
  expect(view.display).toBe("running");
  expect(view.nodes[0]).toMatchObject({
    runtime: { main_integrated: true, acceptance_complete: false, stage: "final_verification" },
    direct_successors: ["B"], ready_after_main: ["B"],
  });
  expect(view.execution_readiness).toBe("unavailable");
});

it("keeps Unknown sessions and old action bindings through stop until Gone and reconciliation", async () => {
  fetchMock.mockResolvedValueOnce(response({ campaign: fixtures.stoppingUnknown }));
  const { view } = await getServerCampaign("release");
  expect(view).toMatchObject({ display: "stopping", control: "stopping", unknown: true, stop_confirmed: false, control_version: 2 });
  expect(view.unreconciled_actions[0].binding.control_version).toBe(1);
  expect(view.sessions[0]).toMatchObject({
    identity: { incarnation: "session-3", gate: { incarnation: "boot-9" } },
    observation: { state: "unknown", last_observed_at: null },
  });
  expect(view.commands).toContainEqual({ command: "edit_definition", rejection: "invalid_state" });
  fetchMock.mockResolvedValueOnce(response({ campaign: fixtures.stoppedGone }));
  const stopped = (await getServerCampaign("release")).view;
  expect(stopped).toMatchObject({ display: "stopped", stop_confirmed: true, unknown: false, unreconciled_actions: [] });
  expect(stopped.sessions[0].observation.state).toBe("gone");
  expect(stopped.commands).toContainEqual({ command: "edit_definition", rejection: null });
});

it("distinguishes retry preparation from resume without resetting a failed series in the browser", async () => {
  fetchMock.mockResolvedValueOnce(response({ campaigns: [fixtures.resumeWithoutRetry, fixtures.retryPrepared] }));
  const [resume, retry] = (await getServerCampaigns()).map((document) => document.view);
  expect(resume.nodes[0].runtime).toMatchObject({ series: 1, disposition: "failed", repair_cycles: 3 });
  expect(retry.nodes[0].runtime).toMatchObject({ series: 2, disposition: "pending", repair_cycles: 0 });
  for (const view of [resume, retry]) {
    expect(view.control).toBe("stopped");
    expect(view.commands).toContainEqual({ command: "resume", rejection: "execution_unavailable" });
    expect(view.commands).toContainEqual({ command: "prepare_retry", rejection: "execution_unavailable" });
  }
});

it("retains distinct nodes sharing an issue URL", async () => {
  fetchMock.mockResolvedValueOnce(response({ campaign: fixtures.sameIssueDistinctNodes }));
  const { definition, view } = await getServerCampaign("release");
  expect(definition.nodes[0].issue_url).toBe(definition.nodes[1].issue_url);
  expect(view.nodes.map((node) => node.id)).toEqual(["A", "B", "C"]);
  expect(definition.nodes[1].dependencies).toEqual(["A"]);
});
