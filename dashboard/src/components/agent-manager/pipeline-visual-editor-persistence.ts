import type { PipelineConfigFull, PipelineStage } from "../../types";
import {
  clonePipelineConfig,
  extractOverrideExtras,
  hasRawOverride,
  type Selection,
  type StageDraft,
} from "./pipeline-visual-editor-model";
import type {
  EditLevel,
  EditorSnapshot,
  PersistedFsmDraftEntry,
  PersistedFsmDraftStore,
  PersistedPipelineSnapshotEntry,
  PersistedPipelineSnapshotStore,
} from "./pipeline-visual-editor-types";

export const EMPTY_FSM_DRAFT_STORE: PersistedFsmDraftStore = {
  version: 2,
  entries: {},
};

export const EMPTY_PIPELINE_SNAPSHOT_STORE: PersistedPipelineSnapshotStore = {
  version: 1,
  entries: {},
};

export const LEGACY_SERVER_EXTRA_KEYS: readonly string[] = ["stage_failure_policy"];

export function cloneStageDrafts(stages: StageDraft[]) {
  return stages.map((stage) => ({ ...stage }));
}

export function clonePipelineStages(stages: PipelineStage[]) {
  return stages.map((stage) => ({ ...stage }));
}

export function cloneJsonValue<T>(value: T): T {
  if (typeof value === "undefined") {
    return value;
  }
  try {
    return JSON.parse(JSON.stringify(value)) as T;
  } catch {
    return value;
  }
}

/** JSON object ordering is immaterial; array ordering is part of the value. */
export function equalJsonValues(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (!left || !right || typeof left !== "object" || typeof right !== "object") return false;
  if (Array.isArray(left) || Array.isArray(right)) {
    return Array.isArray(left) && Array.isArray(right)
      && left.length === right.length
      && left.every((value, index) => equalJsonValues(value, right[index]));
  }
  const leftObject = left as Record<string, unknown>;
  const rightObject = right as Record<string, unknown>;
  const keys = Object.keys(leftObject);
  return keys.length === Object.keys(rightObject).length && keys.every((key) =>
    Object.hasOwn(rightObject, key) && equalJsonValues(leftObject[key], rightObject[key]),
  );
}

export function cloneEditorSnapshot(snapshot: EditorSnapshot): EditorSnapshot {
  return {
    pipeline: clonePipelineConfig(snapshot.pipeline),
    layers: { ...snapshot.layers },
    rawOverride: cloneJsonValue(snapshot.rawOverride),
    repoStages: clonePipelineStages(snapshot.repoStages),
  };
}

export function normalizeSelection(selection: unknown): Selection {
  if (!selection || typeof selection !== "object") {
    return null;
  }
  const parsed = selection as Partial<Exclude<Selection, null>>;
  if (parsed.kind === "phase_gate") {
    return { kind: "phase_gate" };
  }
  if (parsed.kind === "state" && typeof parsed.stateId === "string") {
    return { kind: "state", stateId: parsed.stateId };
  }
  if (parsed.kind === "transition" && typeof parsed.index === "number") {
    return { kind: "transition", index: parsed.index };
  }
  return null;
}

export function normalizePersistedFsmDraftStore(value: unknown): PersistedFsmDraftStore {
  if (!value || typeof value !== "object") {
    return EMPTY_FSM_DRAFT_STORE;
  }
  const rawEntries =
    "entries" in value && value.entries && typeof value.entries === "object"
      ? (value.entries as Record<string, unknown>)
      : {};
  const entries: Record<string, PersistedFsmDraftEntry> = {};

  Object.entries(rawEntries).forEach(([scopeKey, entry]) => {
    if (!entry || typeof entry !== "object") {
      return;
    }
    const parsed = entry as Partial<PersistedFsmDraftEntry>;
    if (typeof parsed.repo !== "string" || (parsed.level !== "repo" && parsed.level !== "agent")) {
      return;
    }
    if (!parsed.pipeline || typeof parsed.pipeline !== "object" || !Array.isArray(parsed.stageDrafts)) {
      return;
    }

    entries[scopeKey] = {
      repo: parsed.repo,
      level: parsed.level,
      agentId: typeof parsed.agentId === "string" ? parsed.agentId : null,
      updatedAtMs: typeof parsed.updatedAtMs === "number" ? parsed.updatedAtMs : 0,
      pipeline: clonePipelineConfig(parsed.pipeline as PipelineConfigFull),
      stageDrafts: cloneStageDrafts(parsed.stageDrafts as StageDraft[]),
      selection: normalizeSelection(parsed.selection),
      overrideExtras:
        parsed.overrideExtras && typeof parsed.overrideExtras === "object"
          ? { ...(parsed.overrideExtras as Record<string, unknown>) }
          : {},
      serverExtraKeys: Array.isArray(parsed.serverExtraKeys)
        ? parsed.serverExtraKeys.filter((key): key is string => typeof key === "string")
        : undefined,
    };
  });

  return { version: 2, entries };
}

/**
 * #5718 prerequisite. A persisted draft carries the override extras that were
 * extracted from whatever the override GET returned when the draft was written.
 * Once the backend stops echoing a key, replaying those stale extras puts the
 * key back into the next save and the strict PUT rejects the whole request.
 *
 * Dropping a key needs provenance, not merely absence.
 * `serverExtraKeysAtDraftTime` is the set of extra keys the override document
 * carried when this draft was written, so a key is migrated away only when it
 * was server-carried back then and the current GET no longer returns it. Keys
 * the user created locally were never in that set and therefore survive.
 *
 * Pre-field drafts need the #5718 compatibility migration for the known retired
 * stage_failure_policy field. Other unrecorded extras may be local and survive.
 * A GET with no override document carries no deletion authority, even for that
 * migration. The caller must use a successful fetch, never a cached snapshot.
 */
export function reconcileDraftOverrideExtras(
  draftExtras: Record<string, unknown> | null | undefined,
  rawOverride: unknown,
  serverExtraKeysAtDraftTime?: readonly string[] | null,
): Record<string, unknown> {
  const persisted =
    draftExtras && typeof draftExtras === "object" ? (draftExtras as Record<string, unknown>) : {};
  if (!hasRawOverride(rawOverride)) {
    return { ...persisted };
  }
  const serverCarriedAtDraftTime = new Set(serverExtraKeysAtDraftTime ?? LEGACY_SERVER_EXTRA_KEYS);
  const serverExtras = extractOverrideExtras(rawOverride);
  const reconciled: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(persisted)) {
    if (serverCarriedAtDraftTime.has(key) && !Object.hasOwn(serverExtras, key)) {
      continue;
    }
    reconciled[key] = value;
  }
  return reconciled;
}

export function normalizePersistedPipelineSnapshotStore(
  value: unknown,
): PersistedPipelineSnapshotStore {
  if (!value || typeof value !== "object") {
    return EMPTY_PIPELINE_SNAPSHOT_STORE;
  }
  const rawEntries =
    "entries" in value && value.entries && typeof value.entries === "object"
      ? (value.entries as Record<string, unknown>)
      : {};
  const entries: Record<string, PersistedPipelineSnapshotEntry> = {};

  Object.entries(rawEntries).forEach(([scopeKey, entry]) => {
    if (!entry || typeof entry !== "object") {
      return;
    }
    const parsed = entry as Partial<PersistedPipelineSnapshotEntry>;
    const rawSnapshot = parsed.snapshot;
    if (typeof parsed.repo !== "string" || (parsed.level !== "repo" && parsed.level !== "agent")) {
      return;
    }
    if (!rawSnapshot || typeof rawSnapshot !== "object") {
      return;
    }
    const snapshot = rawSnapshot as Partial<EditorSnapshot>;
    if (!snapshot.pipeline || !snapshot.layers || !Array.isArray(snapshot.repoStages)) {
      return;
    }

    entries[scopeKey] = {
      repo: parsed.repo,
      level: parsed.level,
      agentId: typeof parsed.agentId === "string" ? parsed.agentId : null,
      updatedAtMs: typeof parsed.updatedAtMs === "number" ? parsed.updatedAtMs : 0,
      snapshot: cloneEditorSnapshot({
        pipeline: snapshot.pipeline as PipelineConfigFull,
        layers: {
          default: Boolean((snapshot.layers as EditorSnapshot["layers"]).default),
          repo: Boolean((snapshot.layers as EditorSnapshot["layers"]).repo),
          agent: Boolean((snapshot.layers as EditorSnapshot["layers"]).agent),
        },
        rawOverride: cloneJsonValue(snapshot.rawOverride),
        repoStages: clonePipelineStages(snapshot.repoStages as PipelineStage[]),
      }),
    };
  });

  return { version: 1, entries };
}

export function buildFsmDraftScopeKey(
  repo: string,
  level: EditLevel,
  selectedAgentId?: string | null,
) {
  return `${repo}::${level}::${selectedAgentId ?? "repo"}`;
}

export function removeDraftScope(
  store: PersistedFsmDraftStore,
  scopeKey: string,
): PersistedFsmDraftStore {
  if (!(scopeKey in store.entries)) {
    return store;
  }
  const nextEntries = { ...store.entries };
  delete nextEntries[scopeKey];
  return { version: 2, entries: nextEntries };
}

export function coerceSelectionForPipeline(
  pipeline: PipelineConfigFull,
  selection: Selection,
): Selection | null {
  if (!selection) {
    return null;
  }
  if (selection.kind === "phase_gate") {
    return selection;
  }
  if (selection.kind === "state" && pipeline.states.some((state) => state.id === selection.stateId)) {
    return selection;
  }
  if (selection.kind === "transition" && Boolean(pipeline.transitions[selection.index])) {
    return selection;
  }
  return null;
}
