import type { PipelineConfigFull, PipelineStage } from "../../types";
import {
  clonePipelineConfig,
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
    };
  });

  return { version: 2, entries };
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
