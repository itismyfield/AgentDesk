import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import * as api from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { useLocalStorage } from "../../lib/useLocalStorage";
import type { PipelineConfigFull, PipelineStage } from "../../types";
import {
  PIPELINE_VISUAL_EDITOR_MOBILE_BREAKPOINT,
  buildFsmEdgeBindingKey,
  buildPipelineGraph,
  clonePipelineConfig,
  extractOverrideExtras,
  filterVisibleStages,
  hasRawOverride,
  inferFsmEventName,
  stageDraftFromApi,
  type Selection,
  type StageDraft,
} from "./pipeline-visual-editor-model";
import {
  EMPTY_FSM_DRAFT_STORE,
  EMPTY_PIPELINE_SNAPSHOT_STORE,
  buildFsmDraftScopeKey,
  cloneEditorSnapshot,
  cloneStageDrafts,
  coerceSelectionForPipeline,
  normalizePersistedFsmDraftStore,
  normalizePersistedPipelineSnapshotStore,
  removeDraftScope,
} from "./pipeline-visual-editor-persistence";
import type {
  EditLevel,
  EditorSnapshot,
  PersistedFsmDraftEntry,
  PersistedFsmDraftStore,
  PersistedPipelineSnapshotEntry,
  PersistedPipelineSnapshotStore,
  PipelineVisualEditorProps,
} from "./pipeline-visual-editor-types";
import {
  FSM_EDGE_BINDINGS_KEY,
  FSM_EVENT_OPTIONS,
  FSM_HOOK_OPTIONS,
  normalizeFsmEdgeBindings,
  selectedAgentInfo,
} from "./pipeline-visual-editor-ui";
import PipelineVisualEditorView from "./PipelineVisualEditorView";
import { usePipelineVisualEditorActions } from "./usePipelineVisualEditorActions";

export default function PipelineVisualEditor({
  tr,
  locale,
  repo,
  agents,
  selectedAgentId,
  variant = "advanced",
}: PipelineVisualEditorProps) {
  const isFsmVariant = variant === "fsm";
  const [level, setLevel] = useState<EditLevel>("repo");
  const [pipelineDraft, setPipelineDraft] = useState<PipelineConfigFull | null>(null);
  const [savedPipeline, setSavedPipeline] = useState<PipelineConfigFull | null>(null);
  const [layers, setLayers] = useState({ default: true, repo: false, agent: false });
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
  const [rawPersistedFsmDraftStore, setPersistedFsmDraftStore] =
    useLocalStorage<PersistedFsmDraftStore>(
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
      api.getEffectivePipeline(repo, nextLevel === "agent" ? selectedAgentId ?? undefined : undefined),
      nextLevel === "agent" && selectedAgentId ? api.getAgentPipeline(selectedAgentId) : api.getRepoPipeline(repo),
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
    setLayers({ default: true, repo: false, agent: false });
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
    const visibleStages = filterVisibleStages(snapshot.repoStages, selectedAgentId).map(stageDraftFromApi);
    const draftPipeline = persistedDraft ? clonePipelineConfig(persistedDraft.pipeline) : snapshot.pipeline;
    const draftStageDrafts = persistedDraft ? cloneStageDrafts(persistedDraft.stageDrafts) : cloneStageDrafts(visibleStages);
    const persistedSelection = persistedDraft
      ? coerceSelectionForPipeline(draftPipeline, persistedDraft.selection)
      : null;

    setPipelineDraft(draftPipeline);
    setSavedPipeline(clonePipelineConfig(snapshot.pipeline));
    setLayers(snapshot.layers);
    setOverrideExtras(
      persistedDraft ? { ...persistedDraft.overrideExtras } : extractOverrideExtras(snapshot.rawOverride),
    );
    setOverrideExists(hasRawOverride(snapshot.rawOverride));
    setAllRepoStages(snapshot.repoStages);
    setStageDrafts(draftStageDrafts);
    setSavedStageDrafts(cloneStageDrafts(visibleStages));
    setSelection((current) => normalizeActiveSelection(current, draftPipeline, persistedSelection, isFsmVariant));
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
      if (JSON.stringify(currentEntry ?? null) === JSON.stringify(nextEntry)) {
        return normalizedStore;
      }
      return {
        version: 1,
        entries: { ...normalizedStore.entries, [scopeKey]: nextEntry },
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
    () => (pipelineDraft ? buildPipelineGraph(pipelineDraft, compactGraph && !isFsmVariant) : null),
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
  const selectedTransitionIndex = selection?.kind === "transition" ? selection.index : -1;
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
          event: fsmEdgeBindings[bindingKey]?.event ?? inferFsmEventName(transition.from, transition.to),
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
        ].filter(Boolean) as string[]),
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
        ].filter(Boolean) as string[]),
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
    if (!repo || !fsmDraftScopeKey || !pipelineDraft || loading) {
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
      if (JSON.stringify(currentEntry ?? null) === JSON.stringify(nextEntry)) {
        return normalizedStore;
      }
      return {
        version: 2,
        entries: { ...normalizedStore.entries, [fsmDraftScopeKey]: nextEntry },
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

  async function refreshAfterMutation(nextLevel: EditLevel = level) {
    const snapshot = await fetchSnapshot(nextLevel);
    const nextScopeKey = buildScopeKey(nextLevel);
    if (nextScopeKey) {
      persistSnapshot(nextScopeKey, nextLevel, snapshot);
    }
    applySnapshot(snapshot);
  }

  const actions = usePipelineVisualEditorActions({
    tr,
    repo,
    selectedAgentId,
    variant,
    isFsmVariant,
    level,
    pipelineDraft,
    allRepoStages,
    stageDrafts,
    overrideExtras,
    pipelineChanged,
    stagesChanged,
    saving,
    overrideExists,
    setLevel,
    setReloadKey,
    setCollapsed,
    setPipelineDraft,
    setStageDrafts,
    setSelection,
    setOverrideExtras,
    setSaving,
    setError,
    setSuccess,
    refreshAfterMutation,
  });

  if (!repo) {
    return null;
  }

  return (
    <PipelineVisualEditorView
      actions={actions}
      ctx={{
        activeLayers,
        agents,
        allRepoStages,
        collapsed,
        compactGraph,
        editorHelpText,
        editorTitle,
        error,
        fsmEdgeBindings,
        fsmEventOptions,
        fsmHookOptions,
        fsmQuickTransitions,
        graph,
        graphGridClass,
        graphPanelNote,
        hasVisibleChanges,
        isFsmVariant,
        level,
        loading,
        locale,
        overrideExists,
        pipelineDraft,
        preservedKeys,
        saving,
        selectedAgentDetail,
        selectedAgentId,
        selectedFsmEvent,
        selectedFsmHook,
        selectedState,
        selectedTransition,
        selectedTransitionGates,
        selectedTransitionIndex,
        selection,
        stageDrafts,
        success,
        tr,
        useScrollableMobileFsmCanvas,
      }}
    />
  );
}

function normalizeActiveSelection(
  current: Selection,
  draftPipeline: PipelineConfigFull,
  persistedSelection: Selection | null,
  isFsmVariant: boolean,
): Selection {
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
      : firstEditableSelection(draftPipeline);
  }
  if (current?.kind === "transition") {
    return draftPipeline.transitions[current.index] ? current : firstEditableSelection(draftPipeline);
  }
  if (current?.kind === "phase_gate") {
    return current;
  }
  return firstEditableSelection(draftPipeline);
}

function firstEditableSelection(pipeline: PipelineConfigFull): Selection {
  return pipeline.states[0]
    ? { kind: "state", stateId: pipeline.states[0].id }
    : { kind: "phase_gate" };
}
