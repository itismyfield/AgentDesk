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

function mockApi(rawOverride: unknown) {
  vi.spyOn(api, "getEffectivePipeline").mockResolvedValue({
    pipeline: makePipeline(),
    layers: { default: true, repo: true, agent: false },
  });
  vi.spyOn(api, "getRepoPipeline").mockResolvedValue({ repo: REPO, pipeline_config: rawOverride });
  vi.spyOn(api, "getPipelineStages").mockResolvedValue([]);
  vi.spyOn(api, "setRepoPipeline").mockResolvedValue({ ok: true });
}

let container: HTMLDivElement | null = null;
let root: Root | null = null;

async function mountEditor() {
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
        selectedAgentId={null}
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

function persistedDraftExtras(): Record<string, unknown> {
  const raw = window.localStorage.getItem(STORAGE_KEYS.fsmDraft) ?? "{}";
  const store = JSON.parse(raw) as { entries?: Record<string, PersistedFsmDraftEntry> };
  return store.entries?.[SCOPE_KEY]?.overrideExtras ?? {};
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

  it("keeps local bindings after a GET briefly carries the same key with a different value", async () => {
    mockApi(buildOverridePayload(makePipeline()));
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
        fsm_edge_bindings: { "ready->done": { event: "on_remote" } },
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
