import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from "react";

import * as api from "../../api";
import { localeName } from "../../i18n";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { useLocalStorage } from "../../lib/useLocalStorage";
import type {
  Agent,
  PipelineConfigFull,
  PipelineStage,
  UiLanguage,
} from "../../types";
import {
  PIPELINE_VISUAL_EDITOR_MOBILE_BREAKPOINT,
  buildOverridePayload,
  buildFsmEdgeBindingKey,
  buildPipelineGraph,
  buildStageSavePayload,
  clonePipelineConfig,
  createNewStateId,
  createNewStateLabel,
  emptyStageDraft,
  extractOverrideExtras,
  filterVisibleStages,
  hasRawOverride,
  inferFsmEventName,
  stageDraftFromApi,
  type FsmEdgeBinding,
  type Selection,
  type StageDraft,
} from "./pipeline-visual-editor-model";
import { fsmStateTone, transitionAccent } from "./pipeline-visual-editor-styles";
import PipelineFlowCanvas from "./PipelineFlowCanvas";
import { SurfaceCard } from "../common/SurfacePrimitives";
import AgentAvatar from "../AgentAvatar";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repo?: string;
  agents: Agent[];
  selectedAgentId?: string | null;
  variant?: "advanced" | "fsm";
}

type EditLevel = "repo" | "agent";

interface EditorSnapshot {
  pipeline: PipelineConfigFull;
  layers: { default: boolean; repo: boolean; agent: boolean };
  rawOverride: unknown;
  repoStages: PipelineStage[];
}

interface PersistedFsmDraftEntry {
  repo: string;
  level: EditLevel;
  agentId: string | null;
  updatedAtMs: number;
  pipeline: PipelineConfigFull;
  stageDrafts: StageDraft[];
  selection: Selection;
  overrideExtras: Record<string, unknown>;
}

interface PersistedFsmDraftStore {
  version: 2;
  entries: Record<string, PersistedFsmDraftEntry>;
}

interface PersistedPipelineSnapshotEntry {
  repo: string;
  level: EditLevel;
  agentId: string | null;
  updatedAtMs: number;
  snapshot: EditorSnapshot;
}

interface PersistedPipelineSnapshotStore {
  version: 1;
  entries: Record<string, PersistedPipelineSnapshotEntry>;
}

const INPUT_CLASS =
  "w-full rounded-xl border bg-transparent px-3 py-2 text-sm outline-none";
const TEXTAREA_CLASS =
  "w-full rounded-xl border bg-transparent px-3 py-2 text-sm outline-none resize-y";

const INPUT_STYLE = {
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
  color: "var(--th-text-primary)",
  backgroundColor: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
} as const;

const FSM_INPUT_STYLE = {
  borderColor: "color-mix(in srgb, var(--th-border) 80%, transparent)",
  color: "var(--th-text-primary)",
  backgroundColor: "#11141b",
} as const;

const MUTED_TEXT_STYLE = {
  color: "var(--th-text-muted)",
} as const;

const SECTION_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
};

const PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
};

const PANEL_SOFT_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
  background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
};

const EMPTY_PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
  background: "color-mix(in srgb, var(--th-overlay-subtle) 92%, transparent)",
  color: "var(--th-text-muted)",
};

const STATUS_INFO_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
  background: "color-mix(in srgb, var(--th-overlay-subtle) 88%, transparent)",
  color: "var(--th-text-secondary)",
};

const STATUS_SUCCESS_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 28%, var(--th-border) 72%)",
  background: "color-mix(in srgb, var(--th-badge-emerald-bg) 84%, var(--th-card-bg) 16%)",
  color: "var(--th-text-primary)",
};

const STATUS_ERROR_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-danger) 32%, var(--th-border) 68%)",
  background: "color-mix(in srgb, rgba(255, 107, 107, 0.16) 84%, var(--th-card-bg) 16%)",
  color: "var(--th-text-primary)",
};

const BUTTON_NEUTRAL_STYLE: CSSProperties = {
  ...PANEL_SOFT_STYLE,
  color: "var(--th-text-primary)",
};

const BUTTON_ACCENT_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
  background: "var(--th-accent-primary-soft)",
};

const BUTTON_INFO_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-info) 30%, var(--th-border) 70%)",
};

const BUTTON_WARN_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-warn) 30%, var(--th-border) 70%)",
};

const BUTTON_DANGER_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-danger) 32%, var(--th-border) 68%)",
};

const FSM_PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 78%, transparent)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, #090b0f 3%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, #05070a 2%) 100%)",
};

const FSM_INSPECTOR_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 40%, var(--th-border) 60%)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, #0b0d10 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, #080a0d 2%) 100%)",
};

const FSM_DETAIL_PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
  background: "#11141b",
};

const EMPTY_FSM_DRAFT_STORE: PersistedFsmDraftStore = {
  version: 2,
  entries: {},
};

const EMPTY_PIPELINE_SNAPSHOT_STORE: PersistedPipelineSnapshotStore = {
  version: 1,
  entries: {},
};

const FSM_EDGE_BINDINGS_KEY = "fsm_edge_bindings";

const FSM_EVENT_OPTIONS = [
  "on_enqueue",
  "on_dispatch",
  "on_submit",
  "on_approve",
  "on_changes_request",
  "on_error",
  "on_recover",
] as const;

const FSM_HOOK_OPTIONS = [
  "OnQueueReady",
  "OnDispatchRequested",
  "OnDispatchCompleted",
  "OnReviewEnter",
  "OnReviewApproved",
  "OnChangesRequested",
  "OnPipelineError",
  "OnRecoverFromFailure",
] as const;

function cloneStageDrafts(stages: StageDraft[]) {
  return stages.map((stage) => ({ ...stage }));
}

function clonePipelineStages(stages: PipelineStage[]) {
  return stages.map((stage) => ({ ...stage }));
}

function cloneJsonValue<T>(value: T): T {
  if (typeof value === "undefined") {
    return value;
  }
  try {
    return JSON.parse(JSON.stringify(value)) as T;
  } catch {
    return value;
  }
}

function cloneEditorSnapshot(snapshot: EditorSnapshot): EditorSnapshot {
  return {
    pipeline: clonePipelineConfig(snapshot.pipeline),
    layers: { ...snapshot.layers },
    rawOverride: cloneJsonValue(snapshot.rawOverride),
    repoStages: clonePipelineStages(snapshot.repoStages),
  };
}

function normalizeSelection(selection: unknown): Selection {
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

function normalizePersistedFsmDraftStore(value: unknown): PersistedFsmDraftStore {
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
    if (typeof parsed.repo !== "string") {
      return;
    }
    if (parsed.level !== "repo" && parsed.level !== "agent") {
      return;
    }
    if (!parsed.pipeline || typeof parsed.pipeline !== "object") {
      return;
    }
    if (!Array.isArray(parsed.stageDrafts)) {
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

  return {
    version: 2,
    entries,
  };
}

function normalizePersistedPipelineSnapshotStore(value: unknown): PersistedPipelineSnapshotStore {
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
    if (typeof parsed.repo !== "string" || (parsed.level !== "repo" && parsed.level !== "agent")) {
      return;
    }
    const rawSnapshot = parsed.snapshot;
    if (!rawSnapshot || typeof rawSnapshot !== "object") {
      return;
    }
    const snapshot = rawSnapshot as Partial<EditorSnapshot>;
    if (!snapshot.pipeline || typeof snapshot.pipeline !== "object") {
      return;
    }
    if (!snapshot.layers || typeof snapshot.layers !== "object") {
      return;
    }
    if (!Array.isArray(snapshot.repoStages)) {
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

  return {
    version: 1,
    entries,
  };
}

function buildFsmDraftScopeKey(
  repo: string,
  level: EditLevel,
  selectedAgentId?: string | null,
) {
  return `${repo}::${level}::${selectedAgentId ?? "repo"}`;
}

function removeDraftScope(
  store: PersistedFsmDraftStore,
  scopeKey: string,
): PersistedFsmDraftStore {
  if (!(scopeKey in store.entries)) {
    return store;
  }
  const nextEntries = { ...store.entries };
  delete nextEntries[scopeKey];
  return {
    version: 2,
    entries: nextEntries,
  };
}

function coerceSelectionForPipeline(
  pipeline: PipelineConfigFull,
  selection: Selection,
): Selection | null {
  if (!selection) {
    return null;
  }
  if (selection.kind === "phase_gate") {
    return selection;
  }
  if (
    selection.kind === "state"
    && pipeline.states.some((state) => state.id === selection.stateId)
  ) {
    return selection;
  }
  if (
    selection.kind === "transition"
    && Boolean(pipeline.transitions[selection.index])
  ) {
    return selection;
  }
  return null;
}

function parseCommaSeparated(value: string) {
  return value
    .split(",")
    .map((part) => part.trim())
    .filter(Boolean);
}

function joinCommaSeparated(value: string[] | undefined) {
  return value && value.length > 0 ? value.join(", ") : "";
}

function downloadTextFile(filename: string, content: string) {
  const blob = new Blob([content], { type: "application/json;charset=utf-8" });
  const href = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = href;
  link.download = filename;
  link.click();
  URL.revokeObjectURL(href);
}

function normalizeFsmEdgeBindings(value: unknown): Record<string, FsmEdgeBinding> {
  if (!value || typeof value !== "object") {
    return {};
  }

  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>).flatMap(([key, entry]) => {
      if (!entry || typeof entry !== "object" || typeof (entry as FsmEdgeBinding).event !== "string") {
        return [];
      }
      return [[key, { event: (entry as FsmEdgeBinding).event }]];
    }),
  );
}

function formatSelectionTitle(
  tr: Props["tr"],
  selection: Selection,
  pipeline: PipelineConfigFull | null,
) {
  if (!pipeline || !selection) {
    return tr("속성을 편집할 요소를 선택하세요", "Select a node or edge to edit");
  }
  if (selection.kind === "state") {
    return `${tr("상태", "State")} · ${selection.stateId}`;
  }
  if (selection.kind === "transition") {
    const transition = pipeline.transitions[selection.index];
    if (!transition) {
      return tr("전환 편집", "Transition editor");
    }
    return `${transition.from} → ${transition.to}`;
  }
  return tr("Phase Gate", "Phase Gate");
}

function selectedAgentInfo(
  agents: Agent[],
  locale: UiLanguage,
  selectedAgentId?: string | null,
): { agent: Agent; name: string } | null {
  const agent = selectedAgentId
    ? agents.find((candidate) => candidate.id === selectedAgentId)
    : null;
  if (!agent) {
    return null;
  }
  return { agent, name: localeName(locale, agent) };
}

export default function PipelineVisualEditor({
  tr,
  locale,
  repo,
  agents,
  selectedAgentId,
  variant = "advanced",
}: Props) {
  const isFsmVariant = variant === "fsm";
  const [level, setLevel] = useState<EditLevel>("repo");
  const [pipelineDraft, setPipelineDraft] = useState<PipelineConfigFull | null>(null);
  const [savedPipeline, setSavedPipeline] = useState<PipelineConfigFull | null>(null);
  const [layers, setLayers] = useState({
    default: true,
    repo: false,
    agent: false,
  });
  const [overrideExtras, setOverrideExtras] = useState<Record<string, unknown>>({});
  const [overrideExists, setOverrideExists] = useState(false);
  const [allRepoStages, setAllRepoStages] = useState<PipelineStage[]>([]);
  const [stageDrafts, setStageDrafts] = useState<StageDraft[]>([]);
  const [savedStageDrafts, setSavedStageDrafts] = useState<StageDraft[]>([]);
  const [selection, setSelection] = useState<Selection>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [compactGraph, setCompactGraph] = useState(false);
  const [collapsed, setCollapsed] = useState(!isFsmVariant);
  const [rawPersistedFsmDraftStore, setPersistedFsmDraftStore] = useLocalStorage<PersistedFsmDraftStore>(
    STORAGE_KEYS.fsmDraft,
    EMPTY_FSM_DRAFT_STORE,
  );
  const [rawPersistedPipelineSnapshotStore, setPersistedPipelineSnapshotStore] =
    useLocalStorage<PersistedPipelineSnapshotStore>(
      STORAGE_KEYS.settingsPipelineVisualCache,
      EMPTY_PIPELINE_SNAPSHOT_STORE,
    );

  const persistedFsmDraftStore = useMemo(
    () => normalizePersistedFsmDraftStore(rawPersistedFsmDraftStore),
    [rawPersistedFsmDraftStore],
  );
  const persistedFsmDraftStoreRef = useRef(persistedFsmDraftStore);
  const persistedPipelineSnapshotStore = useMemo(
    () => normalizePersistedPipelineSnapshotStore(rawPersistedPipelineSnapshotStore),
    [rawPersistedPipelineSnapshotStore],
  );
  const persistedPipelineSnapshotStoreRef = useRef(persistedPipelineSnapshotStore);
  const buildScopeKey = useCallback(
    (nextLevel: EditLevel) => (repo ? buildFsmDraftScopeKey(repo, nextLevel, selectedAgentId) : null),
    [repo, selectedAgentId],
  );
  const fsmDraftScopeKey = useMemo(() => buildScopeKey(level), [buildScopeKey, level]);

  useEffect(() => {
    persistedFsmDraftStoreRef.current = persistedFsmDraftStore;
  }, [persistedFsmDraftStore]);

  useEffect(() => {
    persistedPipelineSnapshotStoreRef.current = persistedPipelineSnapshotStore;
  }, [persistedPipelineSnapshotStore]);

  useEffect(() => {
    const updateLayoutMode = () => {
      setCompactGraph(window.innerWidth < PIPELINE_VISUAL_EDITOR_MOBILE_BREAKPOINT);
    };
    updateLayoutMode();
    window.addEventListener("resize", updateLayoutMode);
    return () => window.removeEventListener("resize", updateLayoutMode);
  }, []);

  useEffect(() => {
    if (level === "agent" && !selectedAgentId) {
      setLevel("repo");
    }
  }, [level, selectedAgentId]);

  useEffect(() => {
    if (!success) {
      return undefined;
    }
    const timeout = window.setTimeout(() => setSuccess(null), 2600);
    return () => window.clearTimeout(timeout);
  }, [success]);

  useEffect(() => {
    setCollapsed(!isFsmVariant);
  }, [isFsmVariant, repo, selectedAgentId]);

  async function fetchSnapshot(nextLevel: EditLevel): Promise<EditorSnapshot> {
    if (!repo) {
      throw new Error(tr("레포를 먼저 선택하세요.", "Select a repository first."));
    }

    const [effective, rawOverrideResponse, repoStages] = await Promise.all([
      api.getEffectivePipeline(
        repo,
        nextLevel === "agent" ? selectedAgentId ?? undefined : undefined,
      ),
      nextLevel === "agent" && selectedAgentId
        ? api.getAgentPipeline(selectedAgentId)
        : api.getRepoPipeline(repo),
      api.getPipelineStages(repo),
    ]);

    return {
      pipeline: clonePipelineConfig(effective.pipeline),
      layers: effective.layers,
      rawOverride: rawOverrideResponse.pipeline_config,
      repoStages,
    };
  }

  function resetEditorState() {
    setPipelineDraft(null);
    setSavedPipeline(null);
    setLayers({
      default: true,
      repo: false,
      agent: false,
    });
    setOverrideExtras({});
    setOverrideExists(false);
    setAllRepoStages([]);
    setStageDrafts([]);
    setSavedStageDrafts([]);
    setSelection(null);
  }

  function applySnapshot(
    snapshot: EditorSnapshot,
    persistedDraft: PersistedFsmDraftEntry | null = null,
  ) {
    const visibleStages = filterVisibleStages(snapshot.repoStages, selectedAgentId).map(
      stageDraftFromApi,
    );
    const draftPipeline = persistedDraft
      ? clonePipelineConfig(persistedDraft.pipeline)
      : snapshot.pipeline;
    const draftStageDrafts = persistedDraft
      ? cloneStageDrafts(persistedDraft.stageDrafts)
      : cloneStageDrafts(visibleStages);
    const persistedSelection = persistedDraft
      ? coerceSelectionForPipeline(draftPipeline, persistedDraft.selection)
      : null;

    setPipelineDraft(draftPipeline);
    setSavedPipeline(clonePipelineConfig(snapshot.pipeline));
    setLayers(snapshot.layers);
    setOverrideExtras(
      persistedDraft
        ? { ...persistedDraft.overrideExtras }
        : extractOverrideExtras(snapshot.rawOverride),
    );
    setOverrideExists(hasRawOverride(snapshot.rawOverride));
    setAllRepoStages(snapshot.repoStages);
    setStageDrafts(draftStageDrafts);
    setSavedStageDrafts(cloneStageDrafts(visibleStages));
    setSelection((current) => {
      if (persistedSelection) {
        return persistedSelection;
      }
      if (isFsmVariant) {
        if (draftPipeline.transitions[0]) {
          return { kind: "transition", index: 0 };
        }
        if (draftPipeline.states[0]) {
          return { kind: "state", stateId: draftPipeline.states[0].id };
        }
        return { kind: "phase_gate" };
      }
      if (current?.kind === "state") {
        return draftPipeline.states.some((state) => state.id === current.stateId)
          ? current
          : draftPipeline.states[0]
            ? { kind: "state", stateId: draftPipeline.states[0].id }
            : { kind: "phase_gate" };
      }
      if (current?.kind === "transition") {
        return draftPipeline.transitions[current.index]
          ? current
          : draftPipeline.states[0]
            ? { kind: "state", stateId: draftPipeline.states[0].id }
            : { kind: "phase_gate" };
      }
      if (current?.kind === "phase_gate") {
        return current;
      }
      return draftPipeline.states[0]
        ? { kind: "state", stateId: draftPipeline.states[0].id }
        : { kind: "phase_gate" };
    });
  }

  const persistSnapshot = useCallback((
    scopeKey: string,
    nextLevel: EditLevel,
    snapshot: EditorSnapshot,
  ) => {
    if (!repo) {
      return;
    }

    const nextEntry: PersistedPipelineSnapshotEntry = {
      repo,
      level: nextLevel,
      agentId: selectedAgentId ?? null,
      updatedAtMs: Date.now(),
      snapshot: cloneEditorSnapshot(snapshot),
    };

    setPersistedPipelineSnapshotStore((currentStore) => {
      const normalizedStore = normalizePersistedPipelineSnapshotStore(currentStore);
      const currentEntry = normalizedStore.entries[scopeKey];
      const currentSignature = currentEntry ? JSON.stringify(currentEntry) : null;
      const nextSignature = JSON.stringify(nextEntry);
      if (currentSignature === nextSignature) {
        return normalizedStore;
      }
      return {
        version: 1,
        entries: {
          ...normalizedStore.entries,
          [scopeKey]: nextEntry,
        },
      };
    });
  }, [repo, selectedAgentId, setPersistedPipelineSnapshotStore]);

  useEffect(() => {
    if (!repo) {
      resetEditorState();
      setLoading(false);
      return;
    }

    let cancelled = false;
    const persistedDraft = fsmDraftScopeKey
      ? persistedFsmDraftStoreRef.current.entries[fsmDraftScopeKey] ?? null
      : null;
    const cachedSnapshot = fsmDraftScopeKey
      ? persistedPipelineSnapshotStoreRef.current.entries[fsmDraftScopeKey]?.snapshot ?? null
      : null;

    setLoading(true);
    setError(null);
    if (cachedSnapshot) {
      applySnapshot(cloneEditorSnapshot(cachedSnapshot), persistedDraft);
    } else {
      resetEditorState();
    }

    void (async () => {
      try {
        const snapshot = await fetchSnapshot(level);
        if (cancelled) {
          return;
        }
        if (fsmDraftScopeKey) {
          persistSnapshot(fsmDraftScopeKey, level, snapshot);
        }
        applySnapshot(snapshot, persistedDraft);
      } catch (cause) {
        if (!cancelled) {
          setError(
            cause instanceof Error
              ? cause.message
              : tr("파이프라인 정보를 불러오지 못했습니다.", "Failed to load pipeline data."),
          );
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [fsmDraftScopeKey, level, persistSnapshot, reloadKey, repo, selectedAgentId]);

  const selectedAgentDetail = selectedAgentInfo(agents, locale, selectedAgentId);
  const useScrollableMobileFsmCanvas = isFsmVariant && compactGraph;
  const graph = useMemo(
    () =>
      (pipelineDraft
        ? buildPipelineGraph(
            pipelineDraft,
            compactGraph && !isFsmVariant,
          )
        : null),
    [compactGraph, isFsmVariant, pipelineDraft],
  );
  const selectedState =
    selection?.kind === "state" && pipelineDraft
      ? pipelineDraft.states.find((state) => state.id === selection.stateId) ?? null
      : null;
  const selectedTransition =
    selection?.kind === "transition" && pipelineDraft
      ? pipelineDraft.transitions[selection.index] ?? null
      : null;
  const selectedTransitionIndex =
    selection?.kind === "transition" ? selection.index : -1;
  const selectedTransitionGates = selectedTransition?.gates ?? [];

  const pipelineDraftSignature = useMemo(
    () => (pipelineDraft ? JSON.stringify(pipelineDraft) : null),
    [pipelineDraft],
  );
  const savedPipelineSignature = useMemo(
    () => (savedPipeline ? JSON.stringify(savedPipeline) : null),
    [savedPipeline],
  );
  const stageDraftSignature = useMemo(() => JSON.stringify(stageDrafts), [stageDrafts]);
  const savedStageDraftSignature = useMemo(
    () => JSON.stringify(savedStageDrafts),
    [savedStageDrafts],
  );
  const pipelineChanged =
    pipelineDraftSignature !== null &&
    savedPipelineSignature !== null &&
    pipelineDraftSignature !== savedPipelineSignature;
  const stagesChanged = stageDraftSignature !== savedStageDraftSignature;
  const visibleStagesChanged = !isFsmVariant && stagesChanged;
  const hasVisibleChanges = pipelineChanged || visibleStagesChanged;
  const activeLayers = [
    layers.default ? "default" : null,
    layers.repo ? "repo" : null,
    layers.agent ? "agent" : null,
  ].filter(Boolean) as string[];
  const preservedKeys = Object.keys(overrideExtras);
  const fsmEdgeBindings = useMemo(
    () => normalizeFsmEdgeBindings(overrideExtras[FSM_EDGE_BINDINGS_KEY]),
    [overrideExtras],
  );
  const selectedFsmEvent = useMemo(() => {
    if (!selectedTransition) {
      return "";
    }
    const bindingKey = buildFsmEdgeBindingKey(selectedTransition.from, selectedTransition.to);
    return (
      fsmEdgeBindings[bindingKey]?.event
      ?? inferFsmEventName(selectedTransition.from, selectedTransition.to)
    );
  }, [fsmEdgeBindings, selectedTransition]);
  const selectedFsmHooks = useMemo(
    () => (selectedFsmEvent && pipelineDraft ? pipelineDraft.events[selectedFsmEvent] ?? [] : []),
    [pipelineDraft, selectedFsmEvent],
  );
  const selectedFsmHook = selectedFsmHooks[0] ?? "";
  const fsmQuickTransitions = useMemo(
    () =>
      pipelineDraft?.transitions.map((transition, index) => {
        const bindingKey = buildFsmEdgeBindingKey(transition.from, transition.to);
        return {
          ...transition,
          index,
          event:
            fsmEdgeBindings[bindingKey]?.event
            ?? inferFsmEventName(transition.from, transition.to),
        };
      }) ?? [],
    [fsmEdgeBindings, pipelineDraft],
  );
  const fsmEventOptions = useMemo(
    () =>
      Array.from(
        new Set([
          ...FSM_EVENT_OPTIONS,
          ...Object.keys(pipelineDraft?.events ?? {}),
          selectedFsmEvent,
        ].filter(Boolean)),
      ).sort(),
    [pipelineDraft, selectedFsmEvent],
  );
  const fsmHookOptions = useMemo(
    () =>
      Array.from(
        new Set([
          ...FSM_HOOK_OPTIONS,
          ...Object.values(pipelineDraft?.events ?? {}).flat(),
          ...selectedFsmHooks,
        ].filter(Boolean)),
      ).sort(),
    [pipelineDraft, selectedFsmHooks],
  );
  const editorTitle = isFsmVariant
    ? tr("FSM 비주얼 에디터", "FSM visual editor")
    : tr("고급 / Agent별 파이프라인 편집기", "Advanced / agent-specific pipeline editor");
  const editorHelpText = isFsmVariant
    ? tr(
        "엣지를 선택해 우측 280px 패널에서 event, hook, policy를 조정합니다. 기본 FSM 저장은 기존 파이프라인 override 엔드포인트를 사용합니다.",
        "Select an edge and tune its event, hook, and policy from the 280px side panel. Saving uses the existing pipeline override endpoints.",
      )
    : tr(
        "노드는 상태, 화살표는 전환입니다. 노드/전환을 눌러 우측 속성을 수정하고, 하단에서 스테이지를 함께 편집합니다.",
        "Nodes are states, arrows are transitions. Click a node or edge to edit it, then adjust stages below in the same editor.",
      );
  const graphGridClass = isFsmVariant
    ? "grid min-w-0 gap-4 xl:grid-cols-[minmax(0,1fr)_280px]"
    : "grid min-w-0 gap-4 xl:grid-cols-[minmax(0,1.45fr)_minmax(0,0.95fr)]";
  const graphPanelNote = isFsmVariant
    ? tr(
        useScrollableMobileFsmCanvas
          ? "모바일은 편집 패널을 먼저 보여주고, FSM 캔버스는 아래에서 가로 스크롤 가능한 프리뷰로 유지합니다."
          : "FSM 캔버스는 1100×420 viewBox로 고정되고, 좁은 화면에서는 패널이 아래로 떨어집니다.",
        useScrollableMobileFsmCanvas
          ? "Mobile leads with the editor panel, and keeps the FSM canvas below as a horizontally scrollable preview."
          : "The FSM canvas uses a fixed 1100×420 viewBox, and the side panel drops below on narrow screens.",
      )
    : tr(
        "그래프는 화면 폭에 맞춰 자동 압축됩니다. 모바일은 가로 스크롤 없이 1열 레이아웃을 사용합니다.",
        "The graph automatically collapses to fit the screen width. Mobile uses a single-column layout without horizontal scrolling.",
      );

  useEffect(() => {
    if (!repo || !fsmDraftScopeKey) {
      return;
    }

    if (!pipelineDraft || loading) {
      return;
    }

    if (!pipelineChanged && !stagesChanged) {
      setPersistedFsmDraftStore((currentStore) =>
        removeDraftScope(normalizePersistedFsmDraftStore(currentStore), fsmDraftScopeKey),
      );
      return;
    }

    const nextEntry: PersistedFsmDraftEntry = {
      repo,
      level,
      agentId: selectedAgentId ?? null,
      updatedAtMs: Date.now(),
      pipeline: clonePipelineConfig(pipelineDraft),
      stageDrafts: cloneStageDrafts(stageDrafts),
      selection,
      overrideExtras: { ...overrideExtras },
    };

    setPersistedFsmDraftStore((currentStore) => {
      const normalizedStore = normalizePersistedFsmDraftStore(currentStore);
      const currentEntry = normalizedStore.entries[fsmDraftScopeKey];
      const currentSignature = currentEntry ? JSON.stringify(currentEntry) : null;
      const nextSignature = JSON.stringify(nextEntry);
      if (currentSignature === nextSignature) {
        return normalizedStore;
      }
      return {
        version: 2,
        entries: {
          ...normalizedStore.entries,
          [fsmDraftScopeKey]: nextEntry,
        },
      };
    });
  }, [
    fsmDraftScopeKey,
    level,
    loading,
    overrideExtras,
    pipelineChanged,
    pipelineDraft,
    repo,
    selectedAgentId,
    selection,
    setPersistedFsmDraftStore,
    stageDrafts,
    stagesChanged,
  ]);

  if (!repo) {
    return null;
  }

  function updateStage(index: number, patch: Partial<StageDraft>) {
    setStageDrafts((current) =>
      current.map((stage, stageIndex) =>
        stageIndex === index ? { ...stage, ...patch } : stage,
      ),
    );
  }

  function removeStage(index: number) {
    setStageDrafts((current) => current.filter((_, stageIndex) => stageIndex !== index));
  }

  function moveStage(index: number, direction: -1 | 1) {
    setStageDrafts((current) => {
      const next = [...current];
      const target = index + direction;
      if (target < 0 || target >= next.length) {
        return current;
      }
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  }

  function addStage() {
    setStageDrafts((current) => [...current, emptyStageDraft()]);
  }

  function updateState(stateId: string, patch: Partial<PipelineConfigFull["states"][number]>) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      const target = next.states.find((state) => state.id === stateId);
      if (!target) {
        return current;
      }
      Object.assign(target, patch);
      return next;
    });
  }

  function updateStateHooks(
    stateId: string,
    hookType: "on_enter" | "on_exit",
    value: string,
  ) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      const existing = next.hooks[stateId] ?? { on_enter: [], on_exit: [] };
      const updated = {
        ...existing,
        [hookType]: parseCommaSeparated(value),
      };
      if (updated.on_enter.length === 0 && updated.on_exit.length === 0) {
        delete next.hooks[stateId];
      } else {
        next.hooks[stateId] = updated;
      }
      return next;
    });
  }

  function clearStateHooks(stateId: string) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      delete next.hooks[stateId];
      return next;
    });
  }

  function updateStateClock(
    stateId: string,
    patch: Partial<PipelineConfigFull["clocks"][string]>,
  ) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.clocks[stateId] = {
        set: patch.set ?? next.clocks[stateId]?.set ?? "",
        mode: patch.mode ?? next.clocks[stateId]?.mode,
      };
      if (!next.clocks[stateId].mode) {
        delete next.clocks[stateId].mode;
      }
      return next;
    });
  }

  function clearStateClock(stateId: string) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      delete next.clocks[stateId];
      return next;
    });
  }

  function updateStateTimeout(
    stateId: string,
    patch: Partial<PipelineConfigFull["timeouts"][string]>,
  ) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.timeouts[stateId] = {
        duration: patch.duration ?? next.timeouts[stateId]?.duration ?? "",
        clock: patch.clock ?? next.timeouts[stateId]?.clock ?? "",
        max_retries:
          patch.max_retries !== undefined
            ? patch.max_retries
            : next.timeouts[stateId]?.max_retries,
        on_exhaust:
          patch.on_exhaust !== undefined
            ? patch.on_exhaust
            : next.timeouts[stateId]?.on_exhaust,
        condition:
          patch.condition !== undefined
            ? patch.condition
            : next.timeouts[stateId]?.condition,
      };
      if (next.timeouts[stateId].max_retries === undefined) {
        delete next.timeouts[stateId].max_retries;
      }
      if (!next.timeouts[stateId].on_exhaust) {
        delete next.timeouts[stateId].on_exhaust;
      }
      if (!next.timeouts[stateId].condition) {
        delete next.timeouts[stateId].condition;
      }
      return next;
    });
  }

  function clearStateTimeout(stateId: string) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      delete next.timeouts[stateId];
      return next;
    });
  }

  function addState() {
    let nextStateId = "";
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      nextStateId = createNewStateId(next.states);
      next.states.push({
        id: nextStateId,
        label: createNewStateLabel(next.states),
        terminal: false,
      });
      return next;
    });
    if (nextStateId) {
      setSelection({ kind: "state", stateId: nextStateId });
    }
  }

  function removeState(stateId: string) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.states = next.states.filter((state) => state.id !== stateId);
      next.transitions = next.transitions.filter(
        (transition) => transition.from !== stateId && transition.to !== stateId,
      );
      delete next.hooks[stateId];
      delete next.clocks[stateId];
      delete next.timeouts[stateId];
      return next;
    });
    setSelection({ kind: "phase_gate" });
  }

  function addTransition() {
    if (!pipelineDraft || pipelineDraft.states.length < 2) {
      return;
    }
    let nextIndex = -1;
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.transitions.push({
        from: next.states[0].id,
        to: next.states[1].id,
        type: "free",
        gates: [],
      });
      nextIndex = next.transitions.length - 1;
      return next;
    });
    if (nextIndex >= 0) {
      setSelection({ kind: "transition", index: nextIndex });
    }
  }

  function addTransitionBetween(fromId: string, toId: string) {
    if (!pipelineDraft || fromId === toId) return;
    const exists = pipelineDraft.transitions.some(
      (t) => t.from === fromId && t.to === toId,
    );
    if (exists) return;
    let nextIndex = -1;
    setPipelineDraft((current) => {
      if (!current) return current;
      const next = clonePipelineConfig(current);
      next.transitions.push({ from: fromId, to: toId, type: "free", gates: [] });
      nextIndex = next.transitions.length - 1;
      return next;
    });
    if (nextIndex >= 0) {
      setSelection({ kind: "transition", index: nextIndex });
    }
  }

  function updateTransition(
    index: number,
    patch: Partial<PipelineConfigFull["transitions"][number]>,
  ) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      const target = next.transitions[index];
      if (!target) {
        return current;
      }
      next.transitions[index] = {
        ...target,
        ...patch,
        gates:
          patch.type && patch.type !== "gated"
            ? []
            : patch.gates ?? target.gates ?? [],
      };
      return next;
    });
  }

  function updateTransitionGates(index: number, value: string) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      const target = next.transitions[index];
      if (!target) {
        return current;
      }
      const gates = parseCommaSeparated(value);
      target.gates = gates;
      for (const gateName of gates) {
        if (!next.gates[gateName]) {
          next.gates[gateName] = {
            type: "builtin",
          };
        }
      }
      return next;
    });
  }

  function updateFsmTransitionEvent(index: number, nextEvent: string) {
    const transition = pipelineDraft?.transitions[index];
    if (!transition) {
      return;
    }

    const bindingKey = buildFsmEdgeBindingKey(transition.from, transition.to);
    const inferredEvent = inferFsmEventName(transition.from, transition.to);

    setOverrideExtras((current) => {
      const next = { ...current };
      const bindings = normalizeFsmEdgeBindings(current[FSM_EDGE_BINDINGS_KEY]);

      if (!nextEvent || nextEvent === inferredEvent) {
        delete bindings[bindingKey];
      } else {
        bindings[bindingKey] = { event: nextEvent };
      }

      if (Object.keys(bindings).length === 0) {
        delete next[FSM_EDGE_BINDINGS_KEY];
      } else {
        next[FSM_EDGE_BINDINGS_KEY] = bindings;
      }
      return next;
    });

    setPipelineDraft((current) => {
      if (!current || !nextEvent || current.events[nextEvent]) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.events[nextEvent] = [];
      return next;
    });
  }

  function updateFsmEventHook(eventName: string, hookName: string) {
    if (!eventName) {
      return;
    }

    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      if (!hookName) {
        delete next.events[eventName];
      } else {
        next.events[eventName] = [hookName];
      }
      return next;
    });
  }

  function addGate(index: number) {
    const transition = pipelineDraft?.transitions[index];
    if (!transition || !pipelineDraft) {
      return;
    }
    const base = `${transition.from}_${transition.to}_gate`
      .replace(/[^a-zA-Z0-9_]/g, "_")
      .toLowerCase();
    let gateName = base;
    let suffix = 2;
    while (pipelineDraft.gates[gateName]) {
      gateName = `${base}_${suffix}`;
      suffix += 1;
    }

    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      const target = next.transitions[index];
      if (!target) {
        return current;
      }
      target.gates = [...(target.gates ?? []), gateName];
      next.gates[gateName] = {
        type: "builtin",
      };
      if (target.type !== "gated") {
        target.type = "gated";
      }
      return next;
    });
  }

  function updateGate(
    gateName: string,
    patch: Partial<PipelineConfigFull["gates"][string]>,
  ) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.gates[gateName] = {
        type: patch.type ?? next.gates[gateName]?.type ?? "builtin",
        check:
          patch.check !== undefined ? patch.check : next.gates[gateName]?.check,
        description:
          patch.description !== undefined
            ? patch.description
            : next.gates[gateName]?.description,
      };
      if (!next.gates[gateName].check) {
        delete next.gates[gateName].check;
      }
      if (!next.gates[gateName].description) {
        delete next.gates[gateName].description;
      }
      return next;
    });
  }

  function removeTransition(index: number) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.transitions = next.transitions.filter((_, transitionIndex) => transitionIndex !== index);
      return next;
    });
    setSelection({ kind: "phase_gate" });
  }

  function updatePhaseGate(patch: Partial<PipelineConfigFull["phase_gate"]>) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.phase_gate = {
        ...next.phase_gate,
        ...patch,
      };
      return next;
    });
  }

  async function refreshAfterMutation(nextLevel: EditLevel = level) {
    const snapshot = await fetchSnapshot(nextLevel);
    const nextScopeKey = buildScopeKey(nextLevel);
    if (nextScopeKey) {
      persistSnapshot(nextScopeKey, nextLevel, snapshot);
    }
    applySnapshot(snapshot);
  }

  async function handleSave() {
    if (!repo || !pipelineDraft) {
      return;
    }

    setSaving(true);
    setError(null);

    try {
      if (pipelineChanged) {
        const payload = buildOverridePayload(pipelineDraft, overrideExtras);
        if (level === "agent" && selectedAgentId) {
          await api.setAgentPipeline(selectedAgentId, payload);
        } else {
          await api.setRepoPipeline(repo, payload);
        }
      }

      if (!isFsmVariant && stagesChanged) {
        const payload = buildStageSavePayload(allRepoStages, stageDrafts, selectedAgentId);
        await api.savePipelineStages(repo, payload);
      }

      await refreshAfterMutation(level);
      setSuccess(
        tr("비주얼 파이프라인 편집 내용을 저장했습니다.", "Saved visual pipeline editor changes."),
      );
    } catch (cause) {
      setError(
        cause instanceof Error
          ? cause.message
          : tr("파이프라인 저장에 실패했습니다.", "Failed to save pipeline."),
      );
    } finally {
      setSaving(false);
    }
  }

  function handleExportJson() {
    if (!pipelineDraft || !repo) {
      return;
    }
    const payload = buildOverridePayload(pipelineDraft, overrideExtras);
    const scope = level === "agent" && selectedAgentId ? selectedAgentId : "repo";
    downloadTextFile(
      `${repo.replace(/\//g, "__")}-${scope}-${variant}.json`,
      JSON.stringify(payload, null, 2),
    );
  }

  async function handleClearOverride() {
    if (!repo) {
      return;
    }

    setSaving(true);
    setError(null);
    try {
      if (level === "agent" && selectedAgentId) {
        await api.setAgentPipeline(selectedAgentId, null);
      } else {
        await api.setRepoPipeline(repo, null);
      }
      await refreshAfterMutation(level);
      setSuccess(tr("오버라이드를 상속 상태로 초기화했습니다.", "Override cleared."));
    } catch (cause) {
      setError(
        cause instanceof Error
          ? cause.message
          : tr("오버라이드 초기화에 실패했습니다.", "Failed to clear override."),
      );
    } finally {
      setSaving(false);
    }
  }

  async function handleClearStages() {
    if (!repo) {
      return;
    }

    setSaving(true);
    setError(null);
    try {
      if (selectedAgentId) {
        const payload = buildStageSavePayload(allRepoStages, [], selectedAgentId);
        await api.savePipelineStages(repo, payload);
      } else {
        await api.deletePipelineStages(repo);
      }
      await refreshAfterMutation(level);
      setSuccess(
        tr("보이는 파이프라인 스테이지를 정리했습니다.", "Cleared visible pipeline stages."),
      );
    } catch (cause) {
      setError(
        cause instanceof Error
          ? cause.message
          : tr("스테이지 정리에 실패했습니다.", "Failed to clear stages."),
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <section
      className="min-w-0 overflow-hidden rounded-[28px] border p-4 sm:p-5 space-y-5"
      style={SECTION_STYLE}
    >
      <button
        type="button"
        onClick={() => setCollapsed((v) => !v)}
        className="flex w-full items-center justify-between gap-3 text-left"
      >
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {editorTitle}
            </h3>
            {pipelineDraft && (
              <span
                className="rounded-full border px-2.5 py-1 text-[11px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
                  background: "var(--th-accent-primary-soft)",
                  color: "var(--th-text-primary)",
                }}
              >
                {pipelineDraft.states.length} {tr("상태", "states")} /{" "}
                {pipelineDraft.transitions.length} {tr("전환", "transitions")}
                {!isFsmVariant && (
                  <>
                    {" / "}
                    {stageDrafts.length} {tr("스테이지", "stages")}
                  </>
                )}
              </span>
            )}
            {activeLayers.length > 1 && (
              <span
                className="rounded-full border px-2.5 py-1 text-[11px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-warn) 30%, var(--th-border) 70%)",
                  background: "color-mix(in srgb, var(--th-badge-amber-bg) 84%, var(--th-card-bg) 16%)",
                  color: "var(--th-text-primary)",
                }}
              >
                {activeLayers.join(" → ")}
              </span>
            )}
          </div>
        </div>
        <span
          className="shrink-0 text-lg transition-transform"
          style={{ color: "var(--th-text-muted)", transform: collapsed ? "rotate(0deg)" : "rotate(180deg)" }}
        >
          ▼
        </span>
      </button>

      {!collapsed && (
      <>
      <SurfaceCard
        className="rounded-[24px] p-4 sm:p-5"
        style={isFsmVariant ? FSM_PANEL_STYLE : PANEL_STYLE}
      >
        {isFsmVariant ? (
          <div className="space-y-4">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="min-w-0 space-y-2">
                <div
                  className="text-[11px] font-bold uppercase tracking-[0.22em]"
                  style={{
                    color: "var(--th-accent-primary)",
                    fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                  }}
                >
                  Pipeline FSM
                </div>
                <div className="text-[15px] font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {tr("칸반 상태 머신 · 비주얼 에디터", "Kanban state machine · visual editor")}
                </div>
                <div className="flex flex-wrap items-center gap-2 text-[11px] leading-5" style={MUTED_TEXT_STYLE}>
                  <span>
                    {tr(
                      "엣지를 클릭해 훅/정책을 편집합니다.",
                      "Select an edge to edit hook and policy.",
                    )}
                  </span>
                  <code
                    className="rounded-md border px-2 py-0.5"
                    style={{
                      borderColor: "color-mix(in srgb, var(--th-border) 78%, transparent)",
                      background: "color-mix(in srgb, var(--th-overlay-subtle) 90%, transparent)",
                      color: "var(--th-text-secondary)",
                      fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                    }}
                  >
                    kv_meta.kanban_fsm
                  </code>
                </div>
                {selectedAgentDetail && (
                  <p
                    className="flex flex-wrap items-center gap-1.5 text-xs leading-5"
                    style={MUTED_TEXT_STYLE}
                  >
                    {tr("현재 선택된 에이전트", "Selected agent")}:{" "}
                    <AgentAvatar
                      agent={selectedAgentDetail.agent}
                      agents={agents}
                      size={18}
                    />
                    <span style={{ color: "var(--th-text-primary)" }}>
                      {selectedAgentDetail.name}
                    </span>
                  </p>
                )}
                <div className="flex flex-wrap gap-2">
                  {activeLayers.length > 1 && (
                    <span
                      className="rounded-md border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.16em]"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-accent-warn) 36%, var(--th-border) 64%)",
                        background: "color-mix(in srgb, var(--th-badge-amber-bg) 72%, var(--th-card-bg) 28%)",
                        color: "var(--th-text-primary)",
                        fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                      }}
                    >
                      {activeLayers.join(" → ")}
                    </span>
                  )}
                  {pipelineDraft && (
                    <span
                      className="rounded-md border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.16em]"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
                        background: "color-mix(in srgb, var(--th-overlay-subtle) 88%, transparent)",
                        color: "var(--th-text-secondary)",
                        fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                      }}
                    >
                      {pipelineDraft.states.length} {tr("states", "states")} · {pipelineDraft.transitions.length}{" "}
                      {tr("edges", "edges")}
                    </span>
                  )}
                </div>
              </div>

              <div className="fsm-actions flex flex-wrap items-center justify-end gap-2">
                <button
                  onClick={addState}
                  className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
                  style={BUTTON_INFO_STYLE}
                >
                  + {tr("상태 추가", "State")}
                </button>
                <button
                  onClick={addTransition}
                  className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
                  style={BUTTON_ACCENT_STYLE}
                >
                  + {tr("전환 추가", "Edge")}
                </button>
                <button
                  onClick={handleExportJson}
                  disabled={!pipelineDraft}
                  className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
                  style={{
                    ...BUTTON_NEUTRAL_STYLE,
                    opacity: pipelineDraft ? 1 : 0.45,
                  }}
                >
                  {tr("JSON 내보내기", "Export JSON")}
                </button>
                <button
                  onClick={() => void handleClearOverride()}
                  disabled={saving || !overrideExists}
                  className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
                  style={{
                    ...BUTTON_NEUTRAL_STYLE,
                    opacity: saving || !overrideExists ? 0.45 : 1,
                  }}
                >
                  {tr("기본값", "Reset")}
                </button>
              </div>
            </div>

            <div className="flex flex-wrap items-center justify-between gap-2">
              <div
                className="inline-flex rounded-full border p-1"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                  background: "color-mix(in srgb, var(--th-overlay-subtle) 86%, transparent)",
                }}
              >
                <button
                  onClick={() => setLevel("repo")}
                  className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
                  style={{
                    background: level === "repo" ? "var(--th-accent-primary-soft)" : "transparent",
                    color: level === "repo" ? "var(--th-text-primary)" : "var(--th-text-muted)",
                  }}
                >
                  {tr("레포 레벨", "Repo level")}
                </button>
                <button
                  onClick={() => setLevel("agent")}
                  disabled={!selectedAgentId}
                  className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
                  style={{
                    background: level === "agent" ? "var(--th-accent-primary-soft)" : "transparent",
                    color: level === "agent" ? "var(--th-text-primary)" : "var(--th-text-muted)",
                    opacity: selectedAgentId ? 1 : 0.45,
                  }}
                >
                  {tr("에이전트 레벨", "Agent level")}
                </button>
              </div>

              <div className="flex flex-wrap items-center justify-end gap-2">
                <button
                  onClick={() => setReloadKey((current) => current + 1)}
                  className="rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
                  style={BUTTON_NEUTRAL_STYLE}
                >
                  {tr("새로고침", "Refresh")}
                </button>
                <button
                  onClick={() => void handleSave()}
                  disabled={saving || !hasVisibleChanges}
                  className="rounded-lg border px-3 py-1.5 text-[11px] font-semibold disabled:opacity-50"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
                    background: "var(--th-accent-primary-soft)",
                    color: "var(--th-text-primary)",
                  }}
                >
                  {saving
                    ? tr("저장 중…", "Saving…")
                    : hasVisibleChanges
                      ? tr("저장", "Save")
                      : tr("변경 없음", "No changes")}
                </button>
              </div>
            </div>
          </div>
        ) : (
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="space-y-2">
              <p className="text-xs leading-6 sm:text-sm sm:leading-6" style={MUTED_TEXT_STYLE}>
                {editorHelpText}
              </p>
              {selectedAgentDetail && (
                <p
                  className="flex flex-wrap items-center gap-1.5 text-xs leading-6 sm:text-sm sm:leading-6"
                  style={MUTED_TEXT_STYLE}
                >
                  {tr("현재 선택된 에이전트", "Selected agent")}:{" "}
                  <AgentAvatar
                    agent={selectedAgentDetail.agent}
                    agents={agents}
                    size={20}
                  />
                  <span style={{ color: "var(--th-text-primary)" }}>
                    {selectedAgentDetail.name}
                  </span>
                </p>
              )}
            </div>

            <div className="flex flex-wrap items-center justify-end gap-2">
              <div
                className="inline-flex rounded-full border p-1"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                  background: "color-mix(in srgb, var(--th-overlay-subtle) 86%, transparent)",
                }}
              >
                <button
                  onClick={() => setLevel("repo")}
                  className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
                  style={{
                    background: level === "repo" ? "var(--th-accent-primary-soft)" : "transparent",
                    color: level === "repo" ? "var(--th-text-primary)" : "var(--th-text-muted)",
                  }}
                >
                  {tr("레포 레벨", "Repo level")}
                </button>
                <button
                  onClick={() => setLevel("agent")}
                  disabled={!selectedAgentId}
                  className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
                  style={{
                    background: level === "agent" ? "var(--th-accent-primary-soft)" : "transparent",
                    color: level === "agent" ? "var(--th-text-primary)" : "var(--th-text-muted)",
                    opacity: selectedAgentId ? 1 : 0.45,
                  }}
                >
                  {tr("에이전트 레벨", "Agent level")}
                </button>
              </div>

              <button
                onClick={() => setReloadKey((current) => current + 1)}
                className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                style={PANEL_SOFT_STYLE}
              >
                {tr("새로고침", "Refresh")}
              </button>

              <button
                onClick={() => void handleClearOverride()}
                disabled={saving || !overrideExists}
                className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                style={{
                  ...PANEL_SOFT_STYLE,
                  borderColor: "color-mix(in srgb, var(--th-accent-warn) 30%, var(--th-border) 70%)",
                  color: "var(--th-text-primary)",
                  opacity: saving || !overrideExists ? 0.45 : 1,
                }}
              >
                {tr("오버라이드 상속", "Clear override")}
              </button>

              <button
                onClick={() => void handleSave()}
                disabled={saving || !hasVisibleChanges}
                className="rounded-xl border px-3.5 py-1.5 text-xs font-semibold disabled:opacity-50"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
                  background: "var(--th-accent-primary-soft)",
                  color: "var(--th-text-primary)",
                }}
              >
                {saving
                  ? tr("저장 중…", "Saving…")
                  : hasVisibleChanges
                    ? tr("변경 저장", "Save changes")
                    : tr("변경 없음", "No changes")}
              </button>
            </div>
          </div>
        )}
      </SurfaceCard>

      {(error || success || preservedKeys.length > 0) && (
        <div className="space-y-2">
          {error && (
            <div
              className="rounded-[22px] border px-4 py-3 text-xs leading-6 sm:text-sm"
              style={STATUS_ERROR_STYLE}
            >
              {error}
            </div>
          )}
          {success && (
            <div
              className="rounded-[22px] border px-4 py-3 text-xs leading-6 sm:text-sm"
              style={STATUS_SUCCESS_STYLE}
            >
              {success}
            </div>
          )}
          {preservedKeys.length > 0 && (
            <div
              className="rounded-[22px] border px-4 py-3 text-xs leading-6 sm:text-sm"
              style={STATUS_INFO_STYLE}
            >
              {tr("시각 편집기 밖의 override 키는 저장 시 유지됩니다.", "Non-visual override keys are preserved on save.")}{" "}
              <span style={{ color: "var(--th-text-primary)" }}>
                {preservedKeys.join(", ")}
              </span>
            </div>
          )}
        </div>
      )}

      {loading && pipelineDraft && graph && (
        <div
          data-testid="pipeline-refresh-indicator"
          className="flex items-center gap-2 rounded-[20px] border px-3.5 py-2 text-xs sm:text-sm"
          style={STATUS_INFO_STYLE}
        >
          <span
            className="inline-block h-3.5 w-3.5 animate-spin rounded-full border-2 border-current border-t-transparent"
            aria-hidden="true"
          />
          <span>
            {tr(
              "마지막 성공값을 먼저 보여주고 최신 값을 불러오는 중입니다…",
              "Showing the last successful pipeline while refreshing…",
            )}
          </span>
        </div>
      )}

      {!pipelineDraft || !graph ? (
        <div
          className="rounded-[24px] border px-4 py-8 text-sm text-center"
          style={error ? STATUS_ERROR_STYLE : EMPTY_PANEL_STYLE}
        >
          <div className="flex items-center justify-center gap-2">
            {loading && (
              <span
                className="inline-block h-3.5 w-3.5 animate-spin rounded-full border-2 border-current border-t-transparent"
                aria-hidden="true"
              />
            )}
            <span>
              {error ?? tr("비주얼 파이프라인을 불러오는 중…", "Loading visual pipeline…")}
            </span>
          </div>
        </div>
      ) : (
        <>
          <div className={graphGridClass}>
            <div
              className={`min-w-0 rounded-[24px] border p-4 sm:p-5 space-y-4 ${useScrollableMobileFsmCanvas ? "order-2 xl:order-1" : ""}`}
              style={isFsmVariant ? FSM_PANEL_STYLE : PANEL_STYLE}
            >
              {!isFsmVariant && (
                <div className="flex flex-wrap items-center gap-2">
                  <button
                    onClick={addState}
                    className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                    style={BUTTON_INFO_STYLE}
                  >
                    + {tr("상태", "State")}
                  </button>
                  <button
                    onClick={addTransition}
                    className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                    style={BUTTON_ACCENT_STYLE}
                  >
                    + {tr("전환", "Transition")}
                  </button>
                  <button
                    onClick={() => setSelection({ kind: "phase_gate" })}
                    className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                    style={BUTTON_WARN_STYLE}
                  >
                    {tr("Phase Gate", "Phase Gate")}
                  </button>
                </div>
              )}

              <PipelineFlowCanvas
                compactGraph={compactGraph}
                fsmEdgeBindings={fsmEdgeBindings}
                graph={graph}
                graphPanelNote={graphPanelNote}
                isFsmVariant={isFsmVariant}
                onConnectTransition={addTransitionBetween}
                onSelectionChange={setSelection}
                selection={selection}
                tr={tr}
                useScrollableMobileFsmCanvas={useScrollableMobileFsmCanvas}
              />
            </div>

            <div
              className={`min-w-0 rounded-[24px] border p-4 sm:p-5 space-y-4 ${useScrollableMobileFsmCanvas ? "order-1 xl:order-2" : ""}`}
              style={isFsmVariant ? FSM_INSPECTOR_STYLE : PANEL_STYLE}
            >
              {useScrollableMobileFsmCanvas && pipelineDraft && (
                <div
                  className="fsm-stack-mobile rounded-[20px] border p-3 space-y-4"
                  style={FSM_DETAIL_PANEL_STYLE}
                  data-testid="fsm-mobile-selector"
                >
                  <div className="space-y-1">
                    <div
                      className="text-[11px] font-semibold uppercase tracking-[0.18em]"
                      style={{
                        ...MUTED_TEXT_STYLE,
                        fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                      }}
                    >
                      {tr("모바일 빠른 편집", "Mobile quick edit")}
                    </div>
                    <p className="text-xs leading-5" style={MUTED_TEXT_STYLE}>
                      {tr(
                        "모바일에서는 그래프를 직접 누르기보다 아래 목록에서 전환/상태를 고른 뒤 바로 편집합니다.",
                        "On mobile, pick a transition or state from the list below instead of targeting the graph directly.",
                      )}
                    </p>
                  </div>

                  <div className="space-y-2">
                    <div className="flex items-center justify-between gap-2">
                      <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
                        {tr("전환", "Transitions")}
                      </h5>
                      <span className="text-[11px]" style={MUTED_TEXT_STYLE}>
                        {`${fsmQuickTransitions.length}`}
                      </span>
                    </div>
                    <div className="grid gap-2">
                      {fsmQuickTransitions.map((transition) => {
                        const accent = transitionAccent(transition.type);
                        const isSelected =
                          selection?.kind === "transition" && selection.index === transition.index;
                        return (
                          <button
                            key={`${transition.from}-${transition.to}-${transition.index}`}
                            type="button"
                            onClick={() => setSelection({ kind: "transition", index: transition.index })}
                            aria-pressed={isSelected}
                            data-testid={`fsm-mobile-transition-button-${transition.index}`}
                            className="w-full rounded-[18px] border px-3 py-3 text-left transition-colors"
                            style={
                              isSelected
                                ? {
                                    borderColor:
                                      "color-mix(in srgb, var(--th-accent-primary) 50%, var(--th-border) 50%)",
                                    background:
                                      "color-mix(in srgb, var(--th-accent-primary-soft) 84%, #11141b 16%)",
                                    color: "var(--th-text-primary)",
                                  }
                                : {
                                    borderColor:
                                      "color-mix(in srgb, var(--th-border) 82%, transparent)",
                                    background: "#11141b",
                                    color: "var(--th-text-primary)",
                                  }
                            }
                          >
                            <div className="flex items-start justify-between gap-3">
                              <div>
                                <div className="text-sm font-medium">
                                  {transition.from} → {transition.to}
                                </div>
                                <div className="mt-1 text-[11px]" style={MUTED_TEXT_STYLE}>
                                  {transition.event}
                                </div>
                              </div>
                              <span
                                className="rounded-md border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.16em]"
                                style={{
                                  borderColor: accent.stroke,
                                  background: accent.background,
                                  color: accent.text,
                                  fontFamily:
                                    "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                                }}
                              >
                                {transition.type}
                              </span>
                            </div>
                          </button>
                        );
                      })}
                    </div>
                  </div>

                  <div className="space-y-2">
                    <div className="flex items-center justify-between gap-2">
                      <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
                        {tr("상태", "States")}
                      </h5>
                      <span className="text-[11px]" style={MUTED_TEXT_STYLE}>
                        {`${pipelineDraft.states.length}`}
                      </span>
                    </div>
                    <div className="grid grid-cols-2 gap-2">
                      {pipelineDraft.states.map((state) => {
                        const tone = fsmStateTone(state.id);
                        const isSelected =
                          selection?.kind === "state" && selection.stateId === state.id;
                        return (
                          <button
                            key={state.id}
                            type="button"
                            onClick={() => setSelection({ kind: "state", stateId: state.id })}
                            aria-pressed={isSelected}
                            data-testid={`fsm-mobile-state-button-${state.id}`}
                            className="rounded-[18px] border px-3 py-3 text-left transition-colors"
                            style={
                              isSelected
                                ? {
                                    borderColor: tone.stroke,
                                    background: tone.glow,
                                    color: "var(--th-text-primary)",
                                  }
                                : {
                                    borderColor:
                                      "color-mix(in srgb, var(--th-border) 82%, transparent)",
                                    background: "#11141b",
                                    color: "var(--th-text-primary)",
                                  }
                            }
                          >
                            <div className="text-sm font-medium">{state.label}</div>
                            <div className="mt-1 text-[11px]" style={MUTED_TEXT_STYLE}>
                              {state.id}
                            </div>
                          </button>
                        );
                      })}
                    </div>
                  </div>
                </div>
              )}

              <div className="flex flex-wrap items-center justify-between gap-2">
                <h4
                  className="text-sm font-semibold"
                  style={{ color: "var(--th-text-heading)" }}
                  data-testid="pipeline-selection-title"
                >
                  {formatSelectionTitle(tr, selection, pipelineDraft)}
                </h4>
                {isFsmVariant && useScrollableMobileFsmCanvas ? (
                  <span className="text-xs" style={MUTED_TEXT_STYLE}>
                    {tr(
                      "모바일은 위 목록으로 선택하고 이 패널에서 바로 수정합니다.",
                      "Use the quick selector above, then edit here.",
                    )}
                  </span>
                ) : selection?.kind === "state" ? (
                  <span className="text-xs" style={MUTED_TEXT_STYLE}>
                    {tr("노드 클릭으로 선택됨", "Selected from graph")}
                  </span>
                ) : null}
              </div>

              {selectedState && (
                <div className="space-y-3">
                  <div className="grid gap-3 sm:grid-cols-2">
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("상태 ID", "State ID")}
                      </label>
                      <div
                        className="rounded-xl border px-3 py-2 text-sm font-mono"
                        style={{ ...PANEL_SOFT_STYLE, color: "var(--th-text-primary)" }}
                      >
                        {selectedState.id}
                      </div>
                    </div>
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("레이블", "Label")}
                      </label>
                      <input
                        value={selectedState.label}
                        onChange={(event) =>
                          updateState(selectedState.id, { label: event.target.value })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                      />
                    </div>
                  </div>

                  <label className="flex items-center gap-2 text-sm" style={{ color: "var(--th-text-primary)" }}>
                    <input
                      type="checkbox"
                      checked={!!selectedState.terminal}
                      onChange={(event) =>
                        updateState(selectedState.id, { terminal: event.target.checked })
                      }
                    />
                    {tr("터미널 상태", "Terminal state")}
                  </label>

                  <div className="grid gap-3 sm:grid-cols-2">
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("on_enter 훅", "on_enter hooks")}
                      </label>
                      <textarea
                        rows={3}
                        value={joinCommaSeparated(pipelineDraft.hooks[selectedState.id]?.on_enter)}
                        onChange={(event) =>
                          updateStateHooks(selectedState.id, "on_enter", event.target.value)
                        }
                        className={TEXTAREA_CLASS}
                        style={INPUT_STYLE}
                        placeholder="OnCardTransition, OnReviewEnter"
                      />
                    </div>
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("on_exit 훅", "on_exit hooks")}
                      </label>
                      <textarea
                        rows={3}
                        value={joinCommaSeparated(pipelineDraft.hooks[selectedState.id]?.on_exit)}
                        onChange={(event) =>
                          updateStateHooks(selectedState.id, "on_exit", event.target.value)
                        }
                        className={TEXTAREA_CLASS}
                        style={INPUT_STYLE}
                        placeholder="OnStateExit"
                      />
                    </div>
                  </div>

                  <div className="flex flex-wrap gap-2">
                    <button
                      onClick={() => clearStateHooks(selectedState.id)}
                      className="rounded-xl border px-3 py-1.5 text-xs"
                      style={BUTTON_NEUTRAL_STYLE}
                    >
                      {tr("훅 비우기", "Clear hooks")}
                    </button>
                    <button
                      onClick={() => clearStateClock(selectedState.id)}
                      className="rounded-xl border px-3 py-1.5 text-xs"
                      style={BUTTON_NEUTRAL_STYLE}
                    >
                      {tr("클록 비우기", "Clear clock")}
                    </button>
                    <button
                      onClick={() => clearStateTimeout(selectedState.id)}
                      className="rounded-xl border px-3 py-1.5 text-xs"
                      style={BUTTON_NEUTRAL_STYLE}
                    >
                      {tr("타임아웃 비우기", "Clear timeout")}
                    </button>
                  </div>

                  <div className="grid gap-3 sm:grid-cols-2">
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("클록 필드", "Clock field")}
                      </label>
                      <input
                        value={pipelineDraft.clocks[selectedState.id]?.set ?? ""}
                        onChange={(event) =>
                          updateStateClock(selectedState.id, { set: event.target.value })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                        placeholder="started_at"
                      />
                    </div>
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("클록 모드", "Clock mode")}
                      </label>
                      <input
                        value={pipelineDraft.clocks[selectedState.id]?.mode ?? ""}
                        onChange={(event) =>
                          updateStateClock(selectedState.id, {
                            mode: event.target.value || undefined,
                          })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                        placeholder="coalesce"
                      />
                    </div>
                  </div>

                  <div className="rounded-[20px] border p-4 space-y-3" style={PANEL_SOFT_STYLE}>
                    <div className="flex items-center justify-between gap-2">
                      <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
                        {tr("타임아웃", "Timeout")}
                      </h5>
                      <span className="text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("gate, timeout 등 노드 속성", "Node properties like gates and timeout")}
                      </span>
                    </div>
                    <div className="grid gap-3 sm:grid-cols-2">
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("지속 시간", "Duration")}
                        </label>
                        <input
                          value={pipelineDraft.timeouts[selectedState.id]?.duration ?? ""}
                          onChange={(event) =>
                            updateStateTimeout(selectedState.id, {
                              duration: event.target.value,
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          placeholder="30m"
                        />
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("참조 클록", "Clock key")}
                        </label>
                        <input
                          value={pipelineDraft.timeouts[selectedState.id]?.clock ?? ""}
                          onChange={(event) =>
                            updateStateTimeout(selectedState.id, {
                              clock: event.target.value,
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          placeholder="review_entered_at"
                        />
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("최대 재시도", "Max retries")}
                        </label>
                        <input
                          type="number"
                          value={pipelineDraft.timeouts[selectedState.id]?.max_retries ?? ""}
                          onChange={(event) =>
                            updateStateTimeout(selectedState.id, {
                              max_retries:
                                event.target.value === ""
                                  ? undefined
                                  : Number(event.target.value),
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          min={0}
                        />
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("소진 시 이동", "On exhaust")}
                        </label>
                        <select
                          value={pipelineDraft.timeouts[selectedState.id]?.on_exhaust ?? ""}
                          onChange={(event) =>
                            updateStateTimeout(selectedState.id, {
                              on_exhaust: event.target.value || undefined,
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                        >
                          <option value="">{tr("없음", "None")}</option>
                          {pipelineDraft.states.map((state) => (
                            <option key={state.id} value={state.id}>
                              {state.id}
                            </option>
                          ))}
                        </select>
                      </div>
                    </div>

                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("조건식", "Condition")}
                      </label>
                      <input
                        value={pipelineDraft.timeouts[selectedState.id]?.condition ?? ""}
                        onChange={(event) =>
                          updateStateTimeout(selectedState.id, {
                            condition: event.target.value || undefined,
                          })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                        placeholder="review_status = 'awaiting_dod'"
                      />
                    </div>
                  </div>

                  <button
                    onClick={() => removeState(selectedState.id)}
                    className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                    style={BUTTON_DANGER_STYLE}
                  >
                    {tr("이 상태 삭제", "Delete state")}
                  </button>
                </div>
              )}

              {selectedTransition && (
                <div className="space-y-3">
                  {isFsmVariant ? (
                    <>
                      <div className="rounded-[20px] border p-4 space-y-4" style={FSM_DETAIL_PANEL_STYLE}>
                        <div className="flex items-start justify-between gap-3">
                          <div>
                            <h5
                              className="text-[11px] font-semibold uppercase tracking-[0.18em]"
                              style={{
                                ...MUTED_TEXT_STYLE,
                                fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                              }}
                            >
                              {tr("선택된 전환", "Selected transition")}
                            </h5>
                            <p className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                              {selectedTransition.from} → {selectedTransition.to}
                            </p>
                          </div>
                          <span
                            className="rounded-md border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.16em]"
                            style={{
                              borderColor: "color-mix(in srgb, var(--th-border) 80%, transparent)",
                              background: "color-mix(in srgb, var(--th-overlay-subtle) 82%, transparent)",
                              color: "var(--th-text-secondary)",
                              fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                            }}
                          >
                            edge
                          </span>
                        </div>

                        <div className="grid gap-3">
                          <div>
                            <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                              {tr("Event", "Event")}
                            </label>
                            <select
                              value={selectedFsmEvent}
                              onChange={(event) =>
                                updateFsmTransitionEvent(selectedTransitionIndex, event.target.value)
                              }
                              className={INPUT_CLASS}
                              style={FSM_INPUT_STYLE}
                            >
                              {fsmEventOptions.map((eventName) => (
                                <option key={eventName} value={eventName}>
                                  {eventName}
                                </option>
                              ))}
                            </select>
                          </div>

                          <div>
                            <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                              {tr("Hook", "Hook")}
                            </label>
                            <select
                              value={selectedFsmHook}
                              onChange={(event) =>
                                updateFsmEventHook(selectedFsmEvent, event.target.value)
                              }
                              className={INPUT_CLASS}
                              style={FSM_INPUT_STYLE}
                            >
                              <option value="">{tr("없음", "None")}</option>
                              {fsmHookOptions.map((hookName) => (
                                <option key={hookName} value={hookName}>
                                  {hookName}
                                </option>
                              ))}
                            </select>
                            <p className="mt-1 text-[11px]" style={MUTED_TEXT_STYLE}>
                              {tr(
                                "FSM 모드에서는 선택된 event에 연결된 대표 hook 1개를 빠르게 편집합니다.",
                                "FSM mode edits a single representative hook for the selected event.",
                              )}
                            </p>
                          </div>

                          <div>
                            <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                              {tr("Policy", "Policy")}
                            </label>
                            <select
                              value={selectedTransition.type}
                              onChange={(event) =>
                                updateTransition(selectedTransitionIndex, {
                                  type: event.target.value as PipelineConfigFull["transitions"][number]["type"],
                                })
                              }
                              className={INPUT_CLASS}
                              style={FSM_INPUT_STYLE}
                            >
                              <option value="free">free</option>
                              <option value="gated">gated</option>
                              <option value="force_only">force_only</option>
                            </select>
                          </div>

                          {selectedTransition.type === "gated" && (
                            <div>
                              <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                                {tr("게이트 / 조건", "Gates / conditions")}
                              </label>
                              <div className="flex flex-wrap gap-1.5">
                                {Array.from(new Set([
                                  ...Object.keys(pipelineDraft.gates),
                                  ...selectedTransitionGates,
                                ])).map((name) => {
                                  const active = selectedTransitionGates.includes(name);
                                  return (
                                    <button
                                      key={name}
                                      type="button"
                                      onClick={() => {
                                        const next = active
                                          ? selectedTransitionGates.filter((gate) => gate !== name)
                                          : [...selectedTransitionGates, name];
                                        updateTransitionGates(selectedTransitionIndex, next.join(", "));
                                      }}
                                      className="rounded-lg border px-2 py-1 text-xs font-mono transition-colors"
                                      style={
                                        active
                                          ? BUTTON_WARN_STYLE
                                          : {
                                              ...FSM_DETAIL_PANEL_STYLE,
                                              color: "var(--th-text-muted)",
                                            }
                                      }
                                    >
                                      {name}
                                    </button>
                                  );
                                })}
                              </div>
                            </div>
                          )}
                        </div>

                        <div className="flex flex-wrap gap-2 pt-1">
                          <button
                            onClick={() => removeTransition(selectedTransitionIndex)}
                            className="rounded-lg border px-3 py-1.5 text-[11px] font-medium"
                            style={BUTTON_DANGER_STYLE}
                          >
                            {tr("전환 삭제", "Delete edge")}
                          </button>
                        </div>
                      </div>
                    </>
                  ) : (
                    <>
                      <div className="grid gap-3 sm:grid-cols-2">
                        <div>
                          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                            {tr("시작 상태", "From")}
                          </label>
                          <select
                            value={selectedTransition.from}
                            onChange={(event) =>
                              updateTransition(selectedTransitionIndex, {
                                from: event.target.value,
                              })
                            }
                            className={INPUT_CLASS}
                            style={INPUT_STYLE}
                          >
                            {pipelineDraft.states.map((state) => (
                              <option key={state.id} value={state.id}>
                                {state.id}
                              </option>
                            ))}
                          </select>
                        </div>
                        <div>
                          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                            {tr("도착 상태", "To")}
                          </label>
                          <select
                            value={selectedTransition.to}
                            onChange={(event) =>
                              updateTransition(selectedTransitionIndex, {
                                to: event.target.value,
                              })
                            }
                            className={INPUT_CLASS}
                            style={INPUT_STYLE}
                          >
                            {pipelineDraft.states.map((state) => (
                              <option key={state.id} value={state.id}>
                                {state.id}
                              </option>
                            ))}
                          </select>
                        </div>
                      </div>

                      <div className="grid gap-3 sm:grid-cols-2">
                        <div>
                          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                            {tr("전환 타입", "Transition type")}
                          </label>
                          <select
                            value={selectedTransition.type}
                            onChange={(event) =>
                              updateTransition(selectedTransitionIndex, {
                                type: event.target.value as PipelineConfigFull["transitions"][number]["type"],
                              })
                            }
                            className={INPUT_CLASS}
                            style={INPUT_STYLE}
                          >
                            <option value="free">free</option>
                            <option value="gated">gated</option>
                            <option value="force_only">force_only</option>
                          </select>
                        </div>
                        <div>
                          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                            {tr("게이트 / 조건", "Gates / conditions")}
                          </label>
                          <div className="flex flex-wrap gap-1.5">
                            {Array.from(new Set([
                              ...Object.keys(pipelineDraft.gates),
                              ...selectedTransitionGates,
                            ])).map((name) => {
                              const active = selectedTransitionGates.includes(name);
                              return (
                                <button
                                  key={name}
                                  type="button"
                                  onClick={() => {
                                    const next = active
                                      ? selectedTransitionGates.filter((g) => g !== name)
                                      : [...selectedTransitionGates, name];
                                    updateTransitionGates(selectedTransitionIndex, next.join(", "));
                                  }}
                                  className="rounded-lg border px-2 py-1 text-xs font-mono transition-colors"
                                  style={
                                    active
                                      ? BUTTON_WARN_STYLE
                                      : {
                                          ...BUTTON_NEUTRAL_STYLE,
                                          background: "transparent",
                                          color: "var(--th-text-muted)",
                                        }
                                  }
                                >
                                  {name}
                                </button>
                              );
                            })}
                          </div>
                        </div>
                      </div>

                      <div className="rounded-[20px] border p-4 space-y-3" style={PANEL_SOFT_STYLE}>
                        <div className="flex flex-wrap items-center justify-between gap-2">
                          <div>
                            <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
                              {tr("게이트 정의", "Gate definitions")}
                            </h5>
                            <p className="text-xs" style={MUTED_TEXT_STYLE}>
                              {tr(
                                "전환 클릭 시 조건과 트리거를 이 영역에서 편집합니다.",
                                "Edit transition conditions and triggers here.",
                              )}
                            </p>
                          </div>
                          <button
                            onClick={() => addGate(selectedTransitionIndex)}
                            className="rounded-xl border px-3 py-1.5 text-xs"
                            style={BUTTON_WARN_STYLE}
                          >
                            + {tr("게이트", "Gate")}
                          </button>
                        </div>

                        {selectedTransitionGates.length === 0 ? (
                          <p className="text-xs" style={MUTED_TEXT_STYLE}>
                            {tr(
                              "이 전환에는 연결된 게이트가 없습니다. gated 타입이면 게이트를 추가하세요.",
                              "This transition has no gates. Add one if the transition should be gated.",
                            )}
                          </p>
                        ) : (
                          selectedTransitionGates.map((gateName) => (
                            <div
                              key={gateName}
                              className="rounded-xl border p-3 space-y-2"
                              style={PANEL_SOFT_STYLE}
                            >
                              <div
                                className="text-xs font-mono"
                                style={{ color: "var(--th-text-primary)" }}
                              >
                                {gateName}
                              </div>
                              <div className="grid gap-3 sm:grid-cols-2">
                                <div>
                                  <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                                    {tr("게이트 타입", "Gate type")}
                                  </label>
                                  <select
                                    value={pipelineDraft.gates[gateName]?.type ?? ""}
                                    onChange={(event) =>
                                      updateGate(gateName, { type: event.target.value })
                                    }
                                    className={INPUT_CLASS}
                                    style={INPUT_STYLE}
                                  >
                                    <option value="">-</option>
                                    {Array.from(new Set([
                                      "builtin",
                                      ...Object.values(pipelineDraft.gates).map((g) => g?.type).filter(Boolean) as string[],
                                    ])).map((opt) => (
                                      <option key={opt} value={opt}>{opt}</option>
                                    ))}
                                  </select>
                                </div>
                                <div>
                                  <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                                    {tr("체크", "Check")}
                                  </label>
                                  <select
                                    value={pipelineDraft.gates[gateName]?.check ?? ""}
                                    onChange={(event) =>
                                      updateGate(gateName, {
                                        check: event.target.value || undefined,
                                      })
                                    }
                                    className={INPUT_CLASS}
                                    style={INPUT_STYLE}
                                  >
                                    <option value="">-</option>
                                    {Array.from(new Set([
                                      "has_active_dispatch",
                                      "review_verdict_pass",
                                      "review_verdict_rework",
                                      ...Object.values(pipelineDraft.gates).map((g) => g?.check).filter(Boolean) as string[],
                                    ])).map((opt) => (
                                      <option key={opt} value={opt}>{opt}</option>
                                    ))}
                                  </select>
                                </div>
                              </div>
                              <div>
                                <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                                  {tr("설명", "Description")}
                                </label>
                                <input
                                  value={pipelineDraft.gates[gateName]?.description ?? ""}
                                  onChange={(event) =>
                                    updateGate(gateName, {
                                      description: event.target.value || undefined,
                                    })
                                  }
                                  className={INPUT_CLASS}
                                  style={INPUT_STYLE}
                                  placeholder={tr("게이트 설명", "Gate description")}
                                />
                              </div>
                            </div>
                          ))
                        )}
                      </div>

                      <button
                        onClick={() => removeTransition(selectedTransitionIndex)}
                        className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                        style={BUTTON_DANGER_STYLE}
                      >
                        {tr("이 전환 삭제", "Delete transition")}
                      </button>
                    </>
                  )}
                </div>
              )}

              {isFsmVariant && !selectedTransition && !selectedState && (
                <div
                  className="rounded-[20px] border px-4 py-6 text-sm"
                  style={{
                    ...EMPTY_PANEL_STYLE,
                    borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
                    background: "#11141b",
                  }}
                >
                  {tr(
                    useScrollableMobileFsmCanvas
                      ? "모바일은 위 빠른 선택 목록에서 전환이나 상태를 고른 뒤 이 패널에서 event, hook, policy를 편집합니다."
                      : "전환선을 선택하면 우측 280px 패널에서 event, hook, policy를 바로 편집할 수 있습니다.",
                    useScrollableMobileFsmCanvas
                      ? "On mobile, choose a transition or state from the quick selector above, then edit its event, hook, and policy here."
                      : "Select an edge to edit its event, hook, and policy in the 280px side panel.",
                  )}
                </div>
              )}

              {!isFsmVariant && selection?.kind === "phase_gate" && pipelineDraft && (
                <div className="space-y-3">
                  <p className="text-xs" style={MUTED_TEXT_STYLE}>
                    {tr(
                      "visual editor 안에서 override의 phase gate 블록도 함께 편집합니다.",
                      "The visual editor also edits the override phase gate block in place.",
                    )}
                  </p>
                  <div className="grid gap-3 sm:grid-cols-2">
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("dispatch_to", "dispatch_to")}
                      </label>
                      <input
                        value={pipelineDraft.phase_gate.dispatch_to}
                        onChange={(event) =>
                          updatePhaseGate({ dispatch_to: event.target.value })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                      />
                    </div>
                    <div>
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("dispatch_type", "dispatch_type")}
                      </label>
                      <input
                        value={pipelineDraft.phase_gate.dispatch_type}
                        onChange={(event) =>
                          updatePhaseGate({ dispatch_type: event.target.value })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                      />
                    </div>
                    <div className="sm:col-span-2">
                      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                        {tr("pass_verdict", "pass_verdict")}
                      </label>
                      <input
                        value={pipelineDraft.phase_gate.pass_verdict}
                        onChange={(event) =>
                          updatePhaseGate({ pass_verdict: event.target.value })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                      />
                    </div>
                  </div>
                  <div>
                    <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                      {tr("checks", "checks")}
                    </label>
                    <div className="flex flex-wrap gap-1.5">
                      {Array.from(new Set([
                        "merge_verified",
                        "issue_closed",
                        "build_passed",
                        ...(pipelineDraft.phase_gate.checks ?? []),
                      ])).map((checkName) => {
                        const active = (pipelineDraft.phase_gate.checks ?? []).includes(checkName);
                        return (
                          <button
                            key={checkName}
                            type="button"
                            onClick={() => {
                              const current = pipelineDraft.phase_gate.checks ?? [];
                              const next = active
                                ? current.filter((c) => c !== checkName)
                                : [...current, checkName];
                              updatePhaseGate({ checks: next });
                            }}
                            className="rounded-lg border px-2 py-1 text-xs font-mono transition-colors"
                            style={
                              active
                                ? BUTTON_INFO_STYLE
                                : {
                                    ...BUTTON_NEUTRAL_STYLE,
                                    background: "transparent",
                                    color: "var(--th-text-muted)",
                                  }
                            }
                          >
                            {checkName}
                          </button>
                        );
                      })}
                    </div>
                  </div>
                </div>
              )}
            </div>
          </div>

          {!isFsmVariant && (
          <div className="min-w-0 rounded-[24px] border p-4 sm:p-5 space-y-4" style={PANEL_STYLE}>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <h4 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {tr("파이프라인 스테이지", "Pipeline Stages")}
                </h4>
                <p className="text-xs" style={MUTED_TEXT_STYLE}>
                  {selectedAgentDetail
                    ? tr(
                        "선택된 에이전트에 보이는 스테이지만 편집합니다. 저장 시 다른 에이전트 전용 스테이지는 유지됩니다.",
                        "You are editing only stages visible to the selected agent. Saving preserves other-agent stages.",
                      )
                    : tr(
                        "상태머신과 같은 카드 안에서 스테이지 실행 순서를 함께 관리합니다.",
                        "Manage stage execution order in the same card as the state machine.",
                      )}
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                <button
                  onClick={addStage}
                  className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                  style={BUTTON_INFO_STYLE}
                >
                  + {tr("스테이지", "Stage")}
                </button>
                <button
                  onClick={() => void handleClearStages()}
                  disabled={saving || (stageDrafts.length === 0 && allRepoStages.length === 0)}
                  className="rounded-xl border px-3 py-1.5 text-xs"
                  style={{
                    ...BUTTON_DANGER_STYLE,
                    opacity:
                      saving || (stageDrafts.length === 0 && allRepoStages.length === 0) ? 0.45 : 1,
                  }}
                >
                  {tr("보이는 스테이지 정리", "Clear visible stages")}
                </button>
              </div>
            </div>

            {stageDrafts.length === 0 ? (
              <div
                className="rounded-[20px] border px-4 py-6 text-center text-sm"
                style={EMPTY_PANEL_STYLE}
              >
                {tr(
                  "스테이지가 없습니다. 아래의 + 버튼으로 자동 실행 단계를 추가하세요.",
                  "No stages yet. Add an automated stage with the + button.",
                )}
              </div>
            ) : (
              <div className="grid min-w-0 gap-3 xl:grid-cols-2">
                {stageDrafts.map((stage, index) => (
                  <div
                    key={`${stage.stage_name}-${index}`}
                    className="min-w-0 rounded-[20px] border p-4 space-y-3"
                    style={PANEL_SOFT_STYLE}
                  >
                    <div className="flex items-center gap-2">
                      <span
                        className="inline-flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold"
                        style={{
                          background: "var(--th-accent-primary-soft)",
                          color: "var(--th-text-primary)",
                        }}
                      >
                        {index + 1}
                      </span>
                      <input
                        value={stage.stage_name}
                        onChange={(event) =>
                          updateStage(index, { stage_name: event.target.value })
                        }
                        className={INPUT_CLASS}
                        style={INPUT_STYLE}
                        placeholder={tr("스테이지 이름", "Stage name")}
                      />
                    </div>

                    <div className="grid gap-3 sm:grid-cols-2">
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("스킬", "Skill")}
                        </label>
                        <input
                          value={stage.entry_skill}
                          onChange={(event) =>
                            updateStage(index, { entry_skill: event.target.value })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          placeholder="claude-code-plan"
                        />
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("프로바이더", "Provider")}
                        </label>
                        <input
                          value={stage.provider}
                          onChange={(event) =>
                            updateStage(index, { provider: event.target.value })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          placeholder="self / counter"
                        />
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("트리거", "Trigger")}
                        </label>
                        <select
                          value={stage.trigger_after}
                          onChange={(event) =>
                            updateStage(index, {
                              trigger_after: event.target.value as StageDraft["trigger_after"],
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                        >
                          <option value="ready">{tr("카드 준비 시", "On ready")}</option>
                          <option value="review_pass">{tr("리뷰 통과 후", "After review pass")}</option>
                        </select>
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("타임아웃(분)", "Timeout (min)")}
                        </label>
                        <input
                          type="number"
                          value={stage.timeout_minutes}
                          onChange={(event) =>
                            updateStage(index, {
                              timeout_minutes: Math.max(1, Number(event.target.value) || 60),
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          min={1}
                        />
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("담당 에이전트 override", "Agent override")}
                        </label>
                        <select
                          value={stage.agent_override_id}
                          onChange={(event) =>
                            updateStage(index, { agent_override_id: event.target.value })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                        >
                          <option value="">{tr("카드 담당자", "Card assignee")}</option>
                          {agents.map((agent) => (
                            <option key={agent.id} value={agent.id}>
                              {localeName(locale, agent)}
                            </option>
                          ))}
                        </select>
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("적용 대상 에이전트", "Applies to agent")}
                        </label>
                        <select
                          value={stage.applies_to_agent_id}
                          onChange={(event) =>
                            updateStage(index, { applies_to_agent_id: event.target.value })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                        >
                          <option value="">{tr("전체", "All agents")}</option>
                          {agents.map((agent) => (
                            <option key={agent.id} value={agent.id}>
                              {localeName(locale, agent)}
                            </option>
                          ))}
                        </select>
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("실패 시", "On failure")}
                        </label>
                        <select
                          value={stage.on_failure}
                          onChange={(event) =>
                            updateStage(index, {
                              on_failure: event.target.value as StageDraft["on_failure"],
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                        >
                          <option value="fail">{tr("실패 처리", "Fail")}</option>
                          <option value="retry">{tr("재시도", "Retry")}</option>
                          <option value="previous">{tr("이전 스테이지", "Previous stage")}</option>
                          <option value="goto">{tr("지정 스테이지", "Go to stage")}</option>
                        </select>
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("최대 재시도", "Max retries")}
                        </label>
                        <input
                          type="number"
                          value={stage.max_retries}
                          onChange={(event) =>
                            updateStage(index, {
                              max_retries: Math.max(0, Number(event.target.value) || 0),
                            })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          min={0}
                        />
                      </div>
                      {stage.on_failure === "goto" && (
                        <div className="sm:col-span-2">
                          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                            {tr("이동 대상", "Goto target")}
                          </label>
                          <select
                            value={stage.on_failure_target}
                            onChange={(event) =>
                              updateStage(index, { on_failure_target: event.target.value })
                            }
                            className={INPUT_CLASS}
                            style={INPUT_STYLE}
                          >
                            <option value="">{tr("선택", "Select")}</option>
                            {stageDrafts
                              .filter((_, stageIndex) => stageIndex !== index)
                              .map((candidate) => (
                                <option key={candidate.stage_name} value={candidate.stage_name}>
                                  {candidate.stage_name}
                                </option>
                              ))}
                          </select>
                        </div>
                      )}
                    </div>

                    <div className="grid gap-3 sm:grid-cols-2">
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("스킵 조건", "Skip condition")}
                        </label>
                        <input
                          value={stage.skip_condition}
                          onChange={(event) =>
                            updateStage(index, { skip_condition: event.target.value })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                          placeholder="label:hotfix"
                        />
                      </div>
                      <div>
                        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
                          {tr("병렬 스테이지", "Parallel with")}
                        </label>
                        <select
                          value={stage.parallel_with}
                          onChange={(event) =>
                            updateStage(index, { parallel_with: event.target.value })
                          }
                          className={INPUT_CLASS}
                          style={INPUT_STYLE}
                        >
                          <option value="">{tr("없음", "None")}</option>
                          {stageDrafts
                            .filter((_, stageIndex) => stageIndex !== index)
                            .map((candidate) => (
                              <option key={candidate.stage_name} value={candidate.stage_name}>
                                {candidate.stage_name}
                              </option>
                            ))}
                        </select>
                      </div>
                    </div>

                    <div className="flex flex-wrap gap-2">
                      {index > 0 && (
                        <button
                          onClick={() => moveStage(index, -1)}
                          className="rounded-xl border px-3 py-1.5 text-xs"
                          style={BUTTON_NEUTRAL_STYLE}
                        >
                          ↑ {tr("앞으로", "Earlier")}
                        </button>
                      )}
                      {index < stageDrafts.length - 1 && (
                        <button
                          onClick={() => moveStage(index, 1)}
                          className="rounded-xl border px-3 py-1.5 text-xs"
                          style={BUTTON_NEUTRAL_STYLE}
                        >
                          ↓ {tr("뒤로", "Later")}
                        </button>
                      )}
                      <button
                        onClick={() => removeStage(index)}
                        className="rounded-xl border px-3 py-1.5 text-xs"
                        style={BUTTON_DANGER_STYLE}
                      >
                        {tr("삭제", "Delete")}
                      </button>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
          )}
        </>
      )}
      </>
      )}
    </section>
  );
}
