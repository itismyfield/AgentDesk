// @vitest-environment happy-dom

/**
 * #5718 prerequisite guard.
 *
 * The strict override PUT rejects undeclared top-level keys, and the normalized
 * GET stops echoing retired ones. That alone does not rescue a browser that
 * still holds an `agentdesk.fsm.v2` draft written against the old permissive
 * GET: `PipelineVisualEditor.applySnapshot` used to let the persisted extras win
 * outright, so the retired key was replayed into the next save and the whole
 * request came back 400 before writing.
 *
 * These tests mount the real editor with a real persisted draft in localStorage,
 * stub only the HTTP layer and the presentational view, and assert on the
 * payload that actually leaves `handleSave`. Recorded server keys authorize
 * retirement; pre-field drafts migrate only the known stage_failure_policy key.
 * Locally created keys keep their provenance through later server responses.
 */

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import type { PipelineConfigFull } from "../../types";
import PipelineVisualEditor from "./PipelineVisualEditor";
import { buildOverridePayload } from "./pipeline-visual-editor-model";
import {
  buildFsmDraftScopeKey,
  reconcileDraftOverrideExtras,
} from "./pipeline-visual-editor-persistence";
import type { PersistedFsmDraftEntry } from "./pipeline-visual-editor-types";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const view = vi.hoisted(() => ({ current: null as { ctx: any; actions: any } | null }));

vi.mock("./PipelineVisualEditorView", () => ({
  default: (props: { ctx: any; actions: any }) => {
    view.current = props;
    return null;
  },
}));

const REPO = "itismyfield/AgentDesk";
const SCOPE_KEY = buildFsmDraftScopeKey(REPO, "repo", null);
const EDITED_LABEL = "Edited before upgrade";

function makePipeline(): PipelineConfigFull {
  return {
    name: "default",
    version: 1,
    states: [
      { id: "ready", label: "Ready" },
      { id: "done", label: "Done", terminal: true },
    ],
    transitions: [{ from: "ready", to: "done", type: "free", gates: [] }],
    gates: {},
    hooks: {},
    events: {},
    clocks: {},
    timeouts: {},
    phase_gate: {
      dispatch_to: "self",
      dispatch_type: "phase-gate",
      pass_verdict: "phase_gate_passed",
      checks: [],
    },
  };
}

/** What the new normalized GET returns: the retired key is gone, the declared ones stay. */
function normalizedGet(): Record<string, unknown> {
  return {
    ...buildOverridePayload(makePipeline()),
    fsm_edge_bindings: { "ready->done": { event: "on_dispatch" } },
    retry_budget: { max: 3 },
  };
}

/** What today's permissive GET returns for the same row. */
function permissiveGet(): Record<string, unknown> {
  return { ...normalizedGet(), stage_failure_policy: { default: "fail" } };
}

/** A draft persisted before the upgrade: unsaved label edit plus edited extras. */
function historicalDraft(): PersistedFsmDraftEntry {
  const pipeline = makePipeline();
  pipeline.states[0].label = EDITED_LABEL;
  return {
    repo: REPO,
    level: "repo",
    agentId: null,
    updatedAtMs: 1,
    pipeline,
    stageDrafts: [],
    selection: { kind: "transition", index: 0 },
    overrideExtras: {
      stage_failure_policy: { default: "fail" },
      fsm_edge_bindings: { "ready->done": { event: "on_error" } },
      retry_budget: { max: 7 },
    },
    // The permissive GET that seeded this draft carried exactly these keys.
    serverExtraKeys: ["stage_failure_policy", "fsm_edge_bindings", "retry_budget"],
  };
}

// happy-dom does not expose `window.localStorage` here, matching the stub the
// existing `useLocalStorage` React test installs.
const storageValues: Record<string, string> = {};
const localStorageMock = {
  getItem: (key: string) => storageValues[key] ?? null,
  setItem: (key: string, value: string) => {
    storageValues[key] = value;
  },
  removeItem: (key: string) => {
    delete storageValues[key];
  },
  clear: () => {
    Object.keys(storageValues).forEach((key) => delete storageValues[key]);
  },
};

function seedDraft(entry: PersistedFsmDraftEntry) {
  window.localStorage.setItem(
    STORAGE_KEYS.fsmDraft,
    JSON.stringify({ version: 2, entries: { [SCOPE_KEY]: entry } }),
  );
}

function seedCachedSnapshot(rawOverride: unknown) {
  window.localStorage.setItem(
    STORAGE_KEYS.settingsPipelineVisualCache,
    JSON.stringify({ version: 1, entries: { [SCOPE_KEY]: {
      repo: REPO, level: "repo", agentId: null, updatedAtMs: 0,
      snapshot: {
        pipeline: makePipeline(),
        layers: { default: true, repo: true, agent: false },
        rawOverride,
        repoStages: [],
      },
    } } }),
  );
}

function mockApi(rawOverride: unknown, pipeline: PipelineConfigFull = makePipeline()) {
  vi.spyOn(api, "getEffectivePipeline").mockResolvedValue({
    pipeline,
    layers: { default: true, repo: true, agent: false },
  });
  vi.spyOn(api, "getRepoPipeline").mockResolvedValue({ repo: REPO, pipeline_config: rawOverride });
  vi.spyOn(api, "getPipelineStages").mockResolvedValue([]);
  vi.spyOn(api, "setRepoPipeline").mockResolvedValue({ ok: true });
}

let container: HTMLDivElement | null = null;
let root: Root | null = null;

async function mountEditor(selectedAgentId: string | null = null) {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root?.render(
      <PipelineVisualEditor
        tr={(ko: string) => ko}
        locale="ko"
        repo={REPO}
        agents={[]}
        selectedAgentId={selectedAgentId}
        variant="fsm"
      />,
    );
  });
  await act(async () => {
    await Promise.resolve();
  });
}

async function saveAndReadPayload(): Promise<Record<string, unknown>> {
  await act(async () => {
    await view.current?.actions.handleSave();
  });
  const calls = vi.mocked(api.setRepoPipeline).mock.calls;
  expect(calls.length).toBeGreaterThan(0);
  return calls[calls.length - 1][1] as Record<string, unknown>;
}

function persistedDraftEntry(scopeKey: string = SCOPE_KEY): PersistedFsmDraftEntry | null {
  const raw = window.localStorage.getItem(STORAGE_KEYS.fsmDraft) ?? "{}";
  const store = JSON.parse(raw) as { entries?: Record<string, PersistedFsmDraftEntry> };
  return store.entries?.[scopeKey] ?? null;
}

function persistedDraftExtras(): Record<string, unknown> {
  return persistedDraftEntry()?.overrideExtras ?? {};
}

beforeEach(() => {
  Object.defineProperty(window, "localStorage", { configurable: true, value: localStorageMock });
  localStorageMock.clear();
  view.current = null;
});

async function unmountEditor() {
  if (root) {
    await act(async () => {
      root?.unmount();
    });
    root = null;
  }
  container?.remove();
  container = null;
  view.current = null;
}

async function refreshInPlace() {
  await act(async () => {
    view.current?.actions.setReloadKey((current: number) => current + 1);
  });
}

afterEach(async () => {
  await unmountEditor();
  vi.restoreAllMocks();
});

describe("restored draft extras vs. fetched override", () => {
  it("migrates a pre-field retired key through refresh and remount without dropping local extras", async () => {
    const draft = historicalDraft();
    delete draft.serverExtraKeys;
    draft.overrideExtras.local_extension = { note: "unsaved" };
    seedDraft(draft);
    mockApi(normalizedGet());

    await mountEditor();
    await refreshInPlace();
    await unmountEditor();
    await mountEditor();
    await refreshInPlace();

    const restoredExtras = persistedDraftExtras();
    const payload = await saveAndReadPayload();
    expect(Object.hasOwn(payload, "stage_failure_policy")).toBe(false);
    expect(Object.hasOwn(restoredExtras, "stage_failure_policy")).toBe(false);
    expect(payload.local_extension).toEqual({ note: "unsaved" });
    expect(payload.fsm_edge_bindings).toEqual(draft.overrideExtras.fsm_edge_bindings);
    expect(payload.retry_budget).toEqual({ max: 7 });
    expect((payload as unknown as PipelineConfigFull).states[0].label).toBe(EDITED_LABEL);
  });

  it("preserves persisted extras when stages refresh fails after displaying a stale cache", async () => {
    const draft = historicalDraft();
    seedDraft(draft);
    seedCachedSnapshot(buildOverridePayload(makePipeline()));
    mockApi(normalizedGet());
    vi.mocked(api.getPipelineStages).mockRejectedValueOnce(new Error("transient stages failure"));

    await mountEditor();

    expect(api.getRepoPipeline).toHaveResolvedWith({ repo: REPO, pipeline_config: normalizedGet() });
    expect(view.current?.ctx.error).toBe("transient stages failure");
    expect(view.current?.ctx.loading).toBe(false);
    expect(persistedDraftExtras()).toEqual(draft.overrideExtras);
  });

  it("saves edited bindings after a failed cached refresh and a successful reload", async () => {
    seedDraft(historicalDraft());
    seedCachedSnapshot(buildOverridePayload(makePipeline()));
    mockApi(normalizedGet());
    vi.mocked(api.getPipelineStages).mockRejectedValueOnce(new Error("transient stages failure"));

    await mountEditor();
    expect(view.current?.ctx.error).toBe("transient stages failure");
    await unmountEditor();
    await mountEditor();
    expect(view.current?.ctx.error).toBe(null);
    expect(view.current?.ctx.loading).toBe(false);
    const payload = await saveAndReadPayload();

    expect(payload.fsm_edge_bindings).toEqual({ "ready->done": { event: "on_error" } });
    expect(payload.retry_budget).toEqual({ max: 7 });
    expect(Object.hasOwn(payload, "stage_failure_policy")).toBe(false);
    expect((payload as unknown as PipelineConfigFull).states[0].label).toBe(EDITED_LABEL);
  });

  it("keeps edited extras and transition gates when a cached refresh returns no document", async () => {
    const draft = historicalDraft();
    draft.pipeline.transitions[0].gates = ["draft_gate"];
    draft.pipeline.gates.draft_gate = { type: "builtin" };
    seedDraft(draft);
    seedCachedSnapshot(buildOverridePayload(makePipeline()));
    mockApi(null);

    await mountEditor();
    expect(persistedDraftExtras()).toEqual(draft.overrideExtras);
    const payload = await saveAndReadPayload();

    expect(payload.fsm_edge_bindings).toEqual({ "ready->done": { event: "on_error" } });
    expect(payload.retry_budget).toEqual({ max: 7 });
    expect(payload.stage_failure_policy).toEqual({ default: "fail" });
    expect((payload as unknown as PipelineConfigFull).transitions[0].gates).toEqual(["draft_gate"]);
  });

  it("drops only the key the normalized GET no longer returns", async () => {
    seedDraft(historicalDraft());
    mockApi(normalizedGet());

    await mountEditor();
    const payload = await saveAndReadPayload();

    expect(Object.hasOwn(payload, "stage_failure_policy")).toBe(false);
    // Retained keys keep the user's edited values, not the server's.
    expect(payload.fsm_edge_bindings).toEqual({ "ready->done": { event: "on_error" } });
    expect(payload.retry_budget).toEqual({ max: 7 });
    // The unsaved draft edit itself survives.
    expect((payload as unknown as PipelineConfigFull).states[0].label).toBe(EDITED_LABEL);
    expect(view.current?.ctx.preservedKeys.slice().sort()).toEqual([
      "fsm_edge_bindings",
      "retry_budget",
    ]);
  });

  it("purges the retired key from the draft left in localStorage", async () => {
    seedDraft(historicalDraft());
    mockApi(normalizedGet());

    await mountEditor();

    expect(Object.keys(persistedDraftExtras()).slice().sort()).toEqual([
      "fsm_edge_bindings",
      "retry_budget",
    ]);
  });

  it("keeps the legacy key while the permissive GET still returns it", async () => {
    seedDraft(historicalDraft());
    mockApi(permissiveGet());

    await mountEditor();
    const payload = await saveAndReadPayload();

    expect(payload.stage_failure_policy).toEqual({ default: "fail" });
    expect(payload.fsm_edge_bindings).toEqual({ "ready->done": { event: "on_error" } });
    expect(payload.retry_budget).toEqual({ max: 7 });
    expect((payload as unknown as PipelineConfigFull).states[0].label).toBe(EDITED_LABEL);
  });

  it("retires a pre-field legacy key after a permissive GET returned a different value", async () => {
    const draft = historicalDraft();
    delete draft.serverExtraKeys;
    draft.overrideExtras.stage_failure_policy = { default: "edited" };
    seedDraft(draft);
    mockApi(permissiveGet());

    await mountEditor();
    expect(persistedDraftExtras().stage_failure_policy).toEqual({ default: "edited" });
    vi.mocked(api.getRepoPipeline).mockResolvedValue({ repo: REPO, pipeline_config: normalizedGet() });
    await refreshInPlace();
    const payload = await saveAndReadPayload();
    expect(Object.hasOwn(payload, "stage_failure_policy")).toBe(false);
    expect(payload.fsm_edge_bindings).toEqual(draft.overrideExtras.fsm_edge_bindings);
  });

  it("leaves the no-draft path on the fetched extras", async () => {
    mockApi(normalizedGet());

    await mountEditor();
    expect(view.current?.ctx.preservedKeys.slice().sort()).toEqual([
      "fsm_edge_bindings",
      "retry_budget",
    ]);

    await act(async () => {
      view.current?.actions.updateState("ready", { label: "Edited after upgrade" });
    });
    const payload = await saveAndReadPayload();

    expect(Object.hasOwn(payload, "stage_failure_policy")).toBe(false);
    expect(payload.fsm_edge_bindings).toEqual({ "ready->done": { event: "on_dispatch" } });
    expect(payload.retry_budget).toEqual({ max: 3 });
    expect((payload as unknown as PipelineConfigFull).states[0].label).toBe("Edited after upgrade");
  });
});

describe("reconcileDraftOverrideExtras", () => {
  const persisted = { stage_failure_policy: { default: "fail" }, retry_budget: { max: 7 } };
  const carried = ["stage_failure_policy", "retry_budget"];

  it("keeps every key the fetched override still declares", () => {
    expect(reconcileDraftOverrideExtras(persisted, normalizedGet(), carried)).toEqual({ retry_budget: { max: 7 } });
  });

  it("drops every server-carried extra when the fetched override declares none", () => {
    expect(reconcileDraftOverrideExtras(persisted, buildOverridePayload(makePipeline()), carried)).toEqual({});
  });

  it("keeps the draft untouched when the fetch returned no override document", () => {
    expect(reconcileDraftOverrideExtras(persisted, null, carried)).toEqual(persisted);
    expect(reconcileDraftOverrideExtras(persisted, undefined, carried)).toEqual(persisted);
    expect(reconcileDraftOverrideExtras(persisted, [1, 2], carried)).toEqual(persisted);
  });

  it("migrates only the known retired field when the draft recorded no server-carried set", () => {
    expect(reconcileDraftOverrideExtras(persisted, buildOverridePayload(makePipeline())))
      .toEqual({ retry_budget: { max: 7 } });
    expect(reconcileDraftOverrideExtras(persisted, normalizedGet(), null))
      .toEqual({ retry_budget: { max: 7 } });
    expect(reconcileDraftOverrideExtras(persisted, permissiveGet())).toEqual(persisted);
  });

  it("keeps a key the draft never recorded as server-carried", () => {
    expect(reconcileDraftOverrideExtras(persisted, buildOverridePayload(makePipeline()), [])).toEqual(persisted);
    expect(reconcileDraftOverrideExtras(persisted, normalizedGet(), ["stage_failure_policy"]))
      .toEqual({ retry_budget: { max: 7 } });
  });

  it("is safe for absent, null and empty draft extras", () => {
    expect(reconcileDraftOverrideExtras(undefined, normalizedGet(), carried)).toEqual({});
    expect(reconcileDraftOverrideExtras(null, normalizedGet(), carried)).toEqual({});
    expect(reconcileDraftOverrideExtras({}, normalizedGet(), carried)).toEqual({});
    expect(reconcileDraftOverrideExtras({}, null, carried)).toEqual({});
  });
});

/**
 * The provenance axis. A key the user just created locally has never appeared in
 * any override document, so a GET that omits it carries no verdict about it.
 * Only keys the draft itself recorded as server-carried may be migrated away.
 */
describe("locally created override extras", () => {
  it("keeps an edge rename the override document never carried across a refresh", async () => {
    mockApi(buildOverridePayload(makePipeline()));

    await mountEditor();
    expect(view.current?.ctx.preservedKeys).toEqual([]);

    await act(async () => {
      view.current?.actions.updateFsmTransitionEvent(0, "on_locally_named");
    });
    expect(persistedDraftExtras().fsm_edge_bindings).toEqual({
      "ready->done": { event: "on_locally_named" },
    });

    await refreshInPlace();

    expect(view.current?.ctx.preservedKeys).toEqual(["fsm_edge_bindings"]);
    expect(persistedDraftExtras().fsm_edge_bindings).toEqual({
      "ready->done": { event: "on_locally_named" },
    });
    const payload = await saveAndReadPayload();
    expect(payload.fsm_edge_bindings).toEqual({ "ready->done": { event: "on_locally_named" } });
  });

  it("still drops a key this draft recorded as server-carried once the GET stops returning it", async () => {
    mockApi({ ...buildOverridePayload(makePipeline()), retry_budget: { max: 3 } });

    await mountEditor();
    await act(async () => {
      view.current?.actions.updateState("ready", { label: "Edited after upgrade" });
    });
    expect(persistedDraftExtras().retry_budget).toEqual({ max: 3 });

    vi.mocked(api.getRepoPipeline).mockResolvedValue({
      repo: REPO,
      pipeline_config: buildOverridePayload(makePipeline()),
    });
    await refreshInPlace();

    expect(view.current?.ctx.preservedKeys).toEqual([]);
    expect(Object.hasOwn(persistedDraftExtras(), "retry_budget")).toBe(false);
    const payload = await saveAndReadPayload();
    expect(Object.hasOwn(payload, "retry_budget")).toBe(false);
    expect((payload as unknown as PipelineConfigFull).states[0].label).toBe("Edited after upgrade");
  });

  it.each([
    { caseName: "existing override, different value", initialOverride: buildOverridePayload(makePipeline()), fetchedEvent: "on_remote" },
    { caseName: "no override, equal value", initialOverride: null, fetchedEvent: "on_locally_named" },
  ])("keeps local bindings after a GET briefly carries the same key ($caseName)", async ({ initialOverride, fetchedEvent }) => {
    mockApi(initialOverride);
    await mountEditor();
    await act(async () => {
      view.current?.actions.updateFsmTransitionEvent(0, "on_locally_named");
    });
    const localBindings = { "ready->done": { event: "on_locally_named" } };
    expect(persistedDraftExtras().fsm_edge_bindings).toEqual(localBindings);

    vi.mocked(api.getRepoPipeline).mockResolvedValue({
      repo: REPO,
      pipeline_config: {
        ...buildOverridePayload(makePipeline()),
        fsm_edge_bindings: { "ready->done": { event: fetchedEvent } },
      },
    });
    await refreshInPlace();
    expect(persistedDraftExtras().fsm_edge_bindings).toEqual(localBindings);
    await unmountEditor();
    vi.mocked(api.getRepoPipeline).mockResolvedValue({
      repo: REPO,
      pipeline_config: buildOverridePayload(makePipeline()),
    });
    await mountEditor();
    await refreshInPlace();

    const restoredExtras = persistedDraftExtras();
    const payload = await saveAndReadPayload();
    expect(payload.fsm_edge_bindings).toEqual(localBindings);
    expect(restoredExtras.fsm_edge_bindings).toEqual(localBindings);
  });

  it.each([false, true])("learns a restored draft's provenance before retirement (reordered object keys: %s)", async (reordered) => {
    const draft = historicalDraft();
    delete draft.serverExtraKeys;
    draft.overrideExtras = { retry_budget: { max: 7, window: 2 } };
    seedDraft(draft);
    seedCachedSnapshot(buildOverridePayload(makePipeline()));
    mockApi({
      ...buildOverridePayload(makePipeline()),
      retry_budget: reordered ? { window: 2, max: 7 } : { max: 7, window: 2 },
    });

    await mountEditor();
    expect(persistedDraftExtras().retry_budget).toEqual({ max: 7, window: 2 });
    await unmountEditor();
    vi.mocked(api.getRepoPipeline).mockResolvedValue({
      repo: REPO,
      pipeline_config: buildOverridePayload(makePipeline()),
    });
    await mountEditor();
    await refreshInPlace();

    const restoredExtras = persistedDraftExtras();
    const payload = await saveAndReadPayload();
    expect(Object.hasOwn(payload, "retry_budget")).toBe(false);
    expect(Object.hasOwn(restoredExtras, "retry_budget")).toBe(false);
    expect((payload as unknown as PipelineConfigFull).states[0].label).toBe(EDITED_LABEL);
  });
});

/**
 * #5743. Rebinding an FSM edge to an event the server pipeline already declares
 * writes only `overrideExtras`: `updateFsmTransitionEvent` leaves `pipelineDraft`
 * alone when `events[nextEvent]` exists, so neither the pipeline nor the stage
 * signature moves and the persistence effect used to delete the whole draft
 * scope. Detection must run against the server snapshot's extras, so that going
 * back to the server value still retires the scope instead of stranding a draft.
 */
describe("override-only FSM edge edits", () => {
  const SERVER_BINDINGS = { "ready->done": { event: "on_dispatch" } };

  /** The server already declares both events, so neither rebind touches the pipeline. */
  function boundPipeline(): PipelineConfigFull {
    const pipeline = makePipeline();
    pipeline.events = { on_dispatch: [], on_error: [] };
    return pipeline;
  }

  function mockBoundApi() {
    mockApi(buildOverridePayload(boundPipeline(), { fsm_edge_bindings: SERVER_BINDINGS }), boundPipeline());
  }

  async function rebind(event: string) {
    await act(async () => {
      view.current?.actions.updateFsmTransitionEvent(0, event);
    });
  }

  it("keeps a rebind to an already declared event across a remount", async () => {
    mockBoundApi();
    await mountEditor();
    expect(view.current?.ctx.fsmEdgeBindings).toEqual(SERVER_BINDINGS);

    await rebind("on_error");

    // The edit is override-only: no pipeline or stage signature moved.
    expect(view.current?.ctx.hasVisibleChanges).toBe(false);
    expect(view.current?.ctx.pipelineDraft?.events).toEqual({ on_dispatch: [], on_error: [] });
    expect(persistedDraftExtras().fsm_edge_bindings).toEqual({ "ready->done": { event: "on_error" } });

    await unmountEditor();
    await mountEditor();

    expect(view.current?.ctx.fsmEdgeBindings).toEqual({ "ready->done": { event: "on_error" } });
    expect(persistedDraftExtras().fsm_edge_bindings).toEqual({ "ready->done": { event: "on_error" } });
  });

  // Out of scope here: `handleSave` still PUTs only when `pipelineChanged`, so an
  // override-only edit survives a remount but has no way to reach the server yet.

  it("retires the draft scope once the extras match the server snapshot again", async () => {
    mockBoundApi();
    await mountEditor();
    expect(persistedDraftEntry()).toBeNull();

    await rebind("on_error");
    expect(persistedDraftEntry()).not.toBeNull();

    await rebind("on_dispatch");
    expect(view.current?.ctx.fsmEdgeBindings).toEqual(SERVER_BINDINGS);
    expect(persistedDraftEntry()).toBeNull();
  });

  /**
   * The scope-transition guard. On the render that moves `fsmDraftScopeKey`, the
   * editor still holds the previous scope's draft, extras and `loading=false`:
   * the loading effect's `setLoading(true)`/`resetEditorState()` only land on the
   * next commit, so they cannot stop the persistence effect that runs beside them.
   * Without a scope-applied marker the repo edit is written under the agent scope,
   * and a later successful GET restores it there as an agent draft.
   */
  it("never records a repo-scope edit under the agent scope it switches to", async () => {
    const agentId = "agent-1";
    // The scope key carries the selected agent on both levels.
    const repoScopeKey = buildFsmDraftScopeKey(REPO, "repo", agentId);
    const agentScopeKey = buildFsmDraftScopeKey(REPO, "agent", agentId);
    const repoEdit = { "ready->done": { event: "on_error" } };
    mockBoundApi();
    const agentPipeline = vi.spyOn(api, "getAgentPipeline")
      .mockRejectedValue(new Error("transient agent pipeline failure"));

    await mountEditor(agentId);
    await rebind("on_error");
    expect(persistedDraftEntry(repoScopeKey)?.overrideExtras.fsm_edge_bindings).toEqual(repoEdit);

    // Agent scope has no snapshot cache, and its GET fails.
    await act(async () => {
      view.current?.actions.setLevel("agent");
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(view.current?.ctx.error).toBe("transient agent pipeline failure");
    expect(persistedDraftEntry(agentScopeKey)).toBeNull();
    expect(persistedDraftEntry(repoScopeKey)?.overrideExtras.fsm_edge_bindings).toEqual(repoEdit);

    // Re-entering the agent scope with a working GET shows the server binding.
    agentPipeline.mockResolvedValue({
      agent_id: agentId,
      pipeline_config: buildOverridePayload(boundPipeline(), { fsm_edge_bindings: SERVER_BINDINGS }),
    });
    await refreshInPlace();

    expect(view.current?.ctx.error).toBe(null);
    expect(view.current?.ctx.fsmEdgeBindings).toEqual(SERVER_BINDINGS);
    expect(persistedDraftEntry(agentScopeKey)).toBeNull();
  });

  /**
   * The mutation-refresh freshness guard. `refreshAfterMutation` runs outside the
   * loading effect, so nothing cancels it when the editor leaves the scope it was
   * started for. A response that lands late used to apply the old scope's snapshot
   * over the current screen *and* set `appliedScopeKey` to the old scope, after
   * which the persistence effect rejected every later edit of the scope actually
   * on screen — those edits were silently lost when the editor closed.
   */
  it("discards a mutation refresh that resolves after the editor left its scope", async () => {
    const agentId = "agent-1";
    const agentScopeKey = buildFsmDraftScopeKey(REPO, "agent", agentId);
    const repoPipeline = boundPipeline();
    repoPipeline.states[0].label = "Repo server";
    const agentPipeline = boundPipeline();
    agentPipeline.states[0].label = "Agent server";
    const overrideFor = (pipeline: PipelineConfigFull) =>
      buildOverridePayload(pipeline, { fsm_edge_bindings: SERVER_BINDINGS });

    vi.spyOn(api, "getEffectivePipeline").mockImplementation(async (_repo, forAgentId) => ({
      pipeline: forAgentId ? agentPipeline : repoPipeline,
      layers: { default: true, repo: true, agent: false },
    }));
    vi.spyOn(api, "getRepoPipeline").mockResolvedValue({ repo: REPO, pipeline_config: overrideFor(repoPipeline) });
    vi.spyOn(api, "getAgentPipeline").mockResolvedValue({ agent_id: agentId, pipeline_config: overrideFor(agentPipeline) });
    vi.spyOn(api, "getPipelineStages").mockResolvedValue([]);
    vi.spyOn(api, "setRepoPipeline").mockResolvedValue({ ok: true });

    await mountEditor(agentId);
    expect(view.current?.ctx.pipelineDraft?.states[0].label).toBe("Repo server");

    await act(async () => {
      view.current?.actions.updateState("ready", { label: "Repo edit" });
    });

    // The repo PUT succeeds, but hold its follow-up GET open.
    let releaseRepoGet = () => {};
    const repoGetGate = new Promise<void>((resolve) => {
      releaseRepoGet = resolve;
    });
    vi.mocked(api.getRepoPipeline).mockImplementationOnce(async () => {
      await repoGetGate;
      return { repo: REPO, pipeline_config: overrideFor(repoPipeline) };
    });

    let savePromise: Promise<void> | undefined;
    await act(async () => {
      savePromise = view.current?.actions.handleSave();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(api.setRepoPipeline).toHaveBeenCalledTimes(1);

    // The level switch is not disabled while saving, so the agent scope loads first.
    await act(async () => {
      view.current?.actions.setLevel("agent");
    });
    await act(async () => {
      await Promise.resolve();
    });
    expect(view.current?.ctx.loading).toBe(false);
    expect(view.current?.ctx.pipelineDraft?.states[0].label).toBe("Agent server");

    await act(async () => {
      releaseRepoGet();
      await savePromise;
    });

    // The repo response neither reaches the agent screen nor claims it.
    expect(view.current?.ctx.pipelineDraft?.states[0].label).toBe("Agent server");
    expect(persistedDraftEntry(agentScopeKey)).toBeNull();

    await act(async () => {
      view.current?.actions.updateState("ready", { label: "Agent edit" });
    });

    expect(persistedDraftEntry(agentScopeKey)?.pipeline.states[0].label).toBe("Agent edit");
  });

  /**
   * The generation half of the same guard. Two mutation refreshes in one scope
   * carry the same scope key, so only the request generation separates the newer
   * response from the older one. Without it the first request's stale snapshot
   * lands last and silently reverts the screen to pre-mutation server state.
   */
  it("keeps the newest mutation refresh when an earlier one resolves last", async () => {
    const serverV1 = boundPipeline();
    serverV1.states[0].label = "Server v1";
    const serverV2 = boundPipeline();
    serverV2.states[0].label = "Server v2";
    const overrideFor = (pipeline: PipelineConfigFull) =>
      buildOverridePayload(pipeline, { fsm_edge_bindings: SERVER_BINDINGS });
    const effective = (pipeline: PipelineConfigFull) => ({
      pipeline,
      layers: { default: true, repo: true, agent: false },
    });

    vi.spyOn(api, "getEffectivePipeline").mockResolvedValue(effective(serverV1));
    vi.spyOn(api, "getRepoPipeline").mockResolvedValue({ repo: REPO, pipeline_config: overrideFor(serverV1) });
    vi.spyOn(api, "getPipelineStages").mockResolvedValue([]);
    vi.spyOn(api, "setRepoPipeline").mockResolvedValue({ ok: true });

    await mountEditor();
    await act(async () => {
      view.current?.actions.updateState("ready", { label: "Repo edit" });
    });

    // The save PUT succeeds, but hold its follow-up GET open.
    let releaseSaveGet = () => {};
    const saveGetGate = new Promise<void>((resolve) => {
      releaseSaveGet = resolve;
    });
    vi.mocked(api.getRepoPipeline).mockImplementationOnce(async () => {
      await saveGetGate;
      return { repo: REPO, pipeline_config: overrideFor(serverV1) };
    });

    let savePromise: Promise<void> | undefined;
    await act(async () => {
      savePromise = view.current?.actions.handleSave();
      await Promise.resolve();
      await Promise.resolve();
    });

    // A later mutation on the same scope completes while the first still waits.
    vi.mocked(api.getEffectivePipeline).mockResolvedValue(effective(serverV2));
    vi.mocked(api.getRepoPipeline).mockResolvedValue({ repo: REPO, pipeline_config: overrideFor(serverV2) });
    await act(async () => {
      await view.current?.actions.handleClearOverride();
    });
    expect(view.current?.ctx.pipelineDraft?.states[0].label).toBe("Server v2");

    await act(async () => {
      releaseSaveGet();
      await savePromise;
    });

    expect(view.current?.ctx.pipelineDraft?.states[0].label).toBe("Server v2");
  });
});
