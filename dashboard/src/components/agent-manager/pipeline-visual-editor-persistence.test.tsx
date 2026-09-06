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
 * payload that actually leaves `handleSave`. The reconciliation is keyed on what
 * the fetched override still carries, so no key name is special-cased.
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

afterEach(async () => {
  await unmountEditor();
  vi.restoreAllMocks();
});

describe("restored draft extras vs. fetched override", () => {
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

  it("keeps every key the fetched override still declares", () => {
    expect(reconcileDraftOverrideExtras(persisted, normalizedGet())).toEqual({ retry_budget: { max: 7 } });
  });

  it("drops every extra when the fetched override declares none", () => {
    expect(reconcileDraftOverrideExtras(persisted, buildOverridePayload(makePipeline()))).toEqual({});
  });

  it("keeps the draft untouched when the fetch returned no override document", () => {
    expect(reconcileDraftOverrideExtras(persisted, null)).toEqual(persisted);
    expect(reconcileDraftOverrideExtras(persisted, undefined)).toEqual(persisted);
    expect(reconcileDraftOverrideExtras(persisted, [1, 2])).toEqual(persisted);
  });

  it("is safe for absent, null and empty draft extras", () => {
    expect(reconcileDraftOverrideExtras(undefined, normalizedGet())).toEqual({});
    expect(reconcileDraftOverrideExtras(null, normalizedGet())).toEqual({});
    expect(reconcileDraftOverrideExtras({}, normalizedGet())).toEqual({});
    expect(reconcileDraftOverrideExtras({}, null)).toEqual({});
  });
});
