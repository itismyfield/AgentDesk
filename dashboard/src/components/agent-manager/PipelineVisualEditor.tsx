import { useCallback, useEffect, useMemo, useRef, useState } from "react";

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
  buildPipelineGraph,
  buildStageSavePayload,
  clonePipelineConfig,
  createNewStateId,
  createNewStateLabel,
  emptyStageDraft,
  extractOverrideExtras,
  filterVisibleStages,
  hasRawOverride,
  stageDraftFromApi,
  type StageDraft,
} from "./pipeline-visual-editor-model";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repo?: string;
  agents: Agent[];
  selectedAgentId?: string | null;
  variant?: "advanced" | "fsm";
}

type EditLevel = "repo" | "agent";

type Selection =
  | { kind: "state"; stateId: string }
  | { kind: "transition"; index: number }
  | { kind: "phase_gate" }
  | null;

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

interface FsmEdgeBinding {
  event: string;
}

const INPUT_CLASS =
  "w-full rounded-xl border bg-transparent px-3 py-2 text-sm outline-none";
const TEXTAREA_CLASS =
  "w-full rounded-xl border bg-transparent px-3 py-2 text-sm outline-none resize-y";

const INPUT_STYLE = {
  borderColor: "rgba(148,163,184,0.24)",
  color: "var(--th-text-primary)",
} as const;

const MUTED_TEXT_STYLE = {
  color: "var(--th-text-muted)",
} as const;

const EMPTY_FSM_DRAFT_STORE: PersistedFsmDraftStore = {
  version: 2,
  entries: {},
};

const FSM_VIEWBOX = {
  width: 1100,
  height: 420,
} as const;

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

function buildFsmEdgeBindingKey(from: string, to: string) {
  return `${from}->${to}`;
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

function inferFsmEventName(from: string, to: string) {
  const key = `${from}->${to}`;
  switch (key) {
    case "backlog->ready":
      return "on_enqueue";
    case "ready->requested":
    case "ready->in_progress":
    case "requested->in_progress":
      return "on_dispatch";
    case "in_progress->review":
      return "on_submit";
    case "review->done":
      return "on_approve";
    case "review->in_progress":
      return "on_changes_request";
    default:
      if (to === "failed") {
        return "on_error";
      }
      if (from === "failed") {
        return "on_recover";
      }
      return `on_${from}_to_${to}`.replace(/[^a-zA-Z0-9_]/g, "_");
  }
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

function transitionAccent(type: PipelineConfigFull["transitions"][number]["type"]) {
  if (type === "free") {
    return {
      stroke: "#22c55e",
      background: "rgba(34,197,94,0.14)",
      text: "#4ade80",
    };
  }
  if (type === "gated") {
    return {
      stroke: "#f59e0b",
      background: "rgba(245,158,11,0.16)",
      text: "#fbbf24",
    };
  }
  return {
    stroke: "#ef4444",
    background: "rgba(239,68,68,0.14)",
    text: "#f87171",
  };
}

function selectedAgentLabel(
  agents: Agent[],
  locale: UiLanguage,
  selectedAgentId?: string | null,
) {
  const agent = selectedAgentId
    ? agents.find((candidate) => candidate.id === selectedAgentId)
    : null;
  if (!agent) {
    return null;
  }
  return `${agent.avatar_emoji} ${localeName(locale, agent)}`;
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

  const svgRef = useRef<SVGSVGElement>(null);
  const persistedFsmDraftStore = useMemo(
    () => normalizePersistedFsmDraftStore(rawPersistedFsmDraftStore),
    [rawPersistedFsmDraftStore],
  );
  const persistedFsmDraftStoreRef = useRef(persistedFsmDraftStore);
  const [dragConnect, setDragConnect] = useState<{
    fromId: string;
    fromCx: number;
    fromCy: number;
    cursorX: number;
    cursorY: number;
    hoverId: string | null;
  } | null>(null);
  const fsmDraftScopeKey = useMemo(
    () => (repo ? buildFsmDraftScopeKey(repo, level, selectedAgentId) : null),
    [level, repo, selectedAgentId],
  );

  useEffect(() => {
    persistedFsmDraftStoreRef.current = persistedFsmDraftStore;
  }, [persistedFsmDraftStore]);

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

  useEffect(() => {
    if (!repo) {
      setPipelineDraft(null);
      setSavedPipeline(null);
      setStageDrafts([]);
      setSavedStageDrafts([]);
      setLoading(false);
      return;
    }

    let cancelled = false;
    setLoading(true);
    setError(null);

    void (async () => {
      try {
        const snapshot = await fetchSnapshot(level);
        if (cancelled) {
          return;
        }
        const persistedDraft = fsmDraftScopeKey
          ? persistedFsmDraftStoreRef.current.entries[fsmDraftScopeKey] ?? null
          : null;
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
  }, [fsmDraftScopeKey, level, reloadKey, repo, selectedAgentId]);

  const selectedAgentName = selectedAgentLabel(agents, locale, selectedAgentId);
  const graph = useMemo(
    () => (pipelineDraft ? buildPipelineGraph(pipelineDraft, compactGraph) : null),
    [compactGraph, pipelineDraft],
  );
  const graphTransform = useMemo(() => {
    if (!graph || !isFsmVariant) {
      return null;
    }
    const scale = Math.min(
      FSM_VIEWBOX.width / Math.max(graph.width, 1),
      FSM_VIEWBOX.height / Math.max(graph.height, 1),
    );
    const scaledWidth = graph.width * scale;
    const scaledHeight = graph.height * scale;
    return {
      scale,
      translateX: (FSM_VIEWBOX.width - scaledWidth) / 2,
      translateY: (FSM_VIEWBOX.height - scaledHeight) / 2,
    };
  }, [graph, isFsmVariant]);
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
        "FSM 캔버스는 1100×420 viewBox로 고정되고, 좁은 화면에서는 패널이 아래로 떨어집니다.",
        "The FSM canvas uses a fixed 1100×420 viewBox, and the side panel drops below on narrow screens.",
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

  const svgPointFromEvent = useCallback(
    (event: React.MouseEvent | MouseEvent): { x: number; y: number } | null => {
      const svg = svgRef.current;
      if (!svg) return null;
      const pt = svg.createSVGPoint();
      pt.x = event.clientX;
      pt.y = event.clientY;
      const ctm = svg.getScreenCTM();
      if (!ctm) return null;
      const svgPt = pt.matrixTransform(ctm.inverse());
      return { x: svgPt.x, y: svgPt.y };
    },
    [],
  );

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
      className="min-w-0 overflow-hidden rounded-2xl border p-3 sm:p-4 space-y-4"
      style={{
        borderColor: "rgba(99,102,241,0.35)",
        backgroundColor: "var(--th-bg-surface)",
      }}
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
                className="rounded-full px-2 py-0.5 text-xs"
                style={{
                  backgroundColor: "rgba(99,102,241,0.18)",
                  color: "#818cf8",
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
                className="rounded-full px-2 py-0.5 text-xs"
                style={{
                  backgroundColor: "rgba(251,191,36,0.15)",
                  color: "#fbbf24",
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
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="space-y-2">
          <p className="text-xs" style={MUTED_TEXT_STYLE}>
            {editorHelpText}
          </p>
          {selectedAgentName && (
            <p className="text-xs" style={MUTED_TEXT_STYLE}>
              {tr("현재 선택된 에이전트", "Selected agent")}: {selectedAgentName}
            </p>
          )}
        </div>

        <div className="flex flex-wrap items-center justify-end gap-2">
          <div
            className="inline-flex rounded-full p-1"
            style={{ backgroundColor: "var(--th-overlay-medium)" }}
          >
            <button
              onClick={() => setLevel("repo")}
              className="rounded-full px-3 py-1.5 text-xs font-medium"
              style={{
                backgroundColor:
                  level === "repo" ? "rgba(99,102,241,0.24)" : "transparent",
                color: level === "repo" ? "#c7d2fe" : "var(--th-text-muted)",
              }}
            >
              {tr("레포 레벨", "Repo level")}
            </button>
            <button
              onClick={() => setLevel("agent")}
              disabled={!selectedAgentId}
              className="rounded-full px-3 py-1.5 text-xs font-medium"
              style={{
                backgroundColor:
                  level === "agent" ? "rgba(99,102,241,0.24)" : "transparent",
                color: level === "agent" ? "#c7d2fe" : "var(--th-text-muted)",
                opacity: selectedAgentId ? 1 : 0.45,
              }}
            >
              {tr("에이전트 레벨", "Agent level")}
            </button>
          </div>

          <button
            onClick={() => setReloadKey((current) => current + 1)}
            className="rounded-xl border px-3 py-1.5 text-xs"
            style={{
              borderColor: "rgba(148,163,184,0.2)",
              color: "var(--th-text-secondary)",
            }}
          >
            {tr("새로고침", "Refresh")}
          </button>

          <button
            onClick={() => void handleClearOverride()}
            disabled={saving || !overrideExists}
            className="rounded-xl border px-3 py-1.5 text-xs"
            style={{
              borderColor: "rgba(245,158,11,0.3)",
              color: "#fbbf24",
              opacity: saving || !overrideExists ? 0.45 : 1,
            }}
          >
            {isFsmVariant ? tr("기본값 복원", "Reset default") : tr("오버라이드 상속", "Clear override")}
          </button>

          {isFsmVariant && (
            <button
              onClick={handleExportJson}
              disabled={!pipelineDraft}
              className="rounded-xl border px-3 py-1.5 text-xs"
              style={{
                borderColor: "rgba(56,189,248,0.3)",
                color: "#38bdf8",
                opacity: pipelineDraft ? 1 : 0.45,
              }}
            >
              {tr("JSON 내보내기", "Export JSON")}
            </button>
          )}

          <button
            onClick={() => void handleSave()}
            disabled={saving || !hasVisibleChanges}
            className="rounded-xl px-3 py-1.5 text-xs font-medium text-white disabled:opacity-50"
            style={{ backgroundColor: "#4f46e5" }}
          >
            {saving
              ? tr("저장 중…", "Saving…")
              : hasVisibleChanges
                ? tr("변경 저장", "Save changes")
                : tr("변경 없음", "No changes")}
          </button>
        </div>
      </div>

      {(error || success || preservedKeys.length > 0) && (
        <div className="space-y-2">
          {error && (
            <div
              className="rounded-xl border px-3 py-2 text-xs"
              style={{
                borderColor: "rgba(248,113,113,0.35)",
                backgroundColor: "rgba(127,29,29,0.2)",
                color: "#fecaca",
              }}
            >
              {error}
            </div>
          )}
          {success && (
            <div
              className="rounded-xl border px-3 py-2 text-xs"
              style={{
                borderColor: "rgba(74,222,128,0.35)",
                backgroundColor: "rgba(34,197,94,0.12)",
                color: "#86efac",
              }}
            >
              {success}
            </div>
          )}
          {preservedKeys.length > 0 && (
            <div
              className="rounded-xl border px-3 py-2 text-xs"
              style={{
                borderColor: "rgba(148,163,184,0.22)",
                backgroundColor: "var(--th-overlay-subtle)",
                color: "var(--th-text-secondary)",
              }}
            >
              {tr("시각 편집기 밖의 override 키는 저장 시 유지됩니다.", "Non-visual override keys are preserved on save.")}{" "}
              <span style={{ color: "var(--th-text-primary)" }}>
                {preservedKeys.join(", ")}
              </span>
            </div>
          )}
        </div>
      )}

      {loading || !pipelineDraft || !graph ? (
        <div className="rounded-2xl border px-4 py-8 text-sm text-center" style={INPUT_STYLE}>
          {tr("비주얼 파이프라인을 불러오는 중…", "Loading visual pipeline…")}
        </div>
      ) : (
        <>
          <div className={graphGridClass}>
            <div className="min-w-0 rounded-2xl border p-3 sm:p-4 space-y-3" style={INPUT_STYLE}>
              {!isFsmVariant && (
                <div className="flex flex-wrap items-center gap-2">
                  <button
                    onClick={addState}
                    className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                    style={{
                      borderColor: "rgba(56,189,248,0.35)",
                      color: "#38bdf8",
                      backgroundColor: "rgba(56,189,248,0.08)",
                    }}
                  >
                    + {tr("상태", "State")}
                  </button>
                  <button
                    onClick={addTransition}
                    className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                    style={{
                      borderColor: "rgba(129,140,248,0.35)",
                      color: "#a5b4fc",
                      backgroundColor: "rgba(129,140,248,0.08)",
                    }}
                  >
                    + {tr("전환", "Transition")}
                  </button>
                  <button
                    onClick={() => setSelection({ kind: "phase_gate" })}
                    className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                    style={{
                      borderColor: "rgba(245,158,11,0.35)",
                      color: "#fbbf24",
                      backgroundColor: "rgba(245,158,11,0.08)",
                    }}
                  >
                    {tr("Phase Gate", "Phase Gate")}
                  </button>
                </div>
              )}

              <div
                className="overflow-hidden rounded-2xl border p-2 sm:p-3"
                style={{
                  borderColor: "rgba(148,163,184,0.18)",
                  background:
                    "radial-gradient(circle at top left, rgba(79,70,229,0.16), transparent 38%), radial-gradient(circle at bottom right, rgba(14,165,233,0.14), transparent 34%), var(--th-overlay-subtle)",
                }}
              >
                <svg
                  ref={svgRef}
                  viewBox={
                    isFsmVariant
                      ? `0 0 ${FSM_VIEWBOX.width} ${FSM_VIEWBOX.height}`
                      : `0 0 ${graph.width} ${graph.height}`
                  }
                  className="h-auto w-full select-none"
                  preserveAspectRatio={isFsmVariant ? "xMidYMid meet" : undefined}
                  role="img"
                  aria-label={tr(
                    "파이프라인 상태와 전환 그래프",
                    "Pipeline state and transition graph",
                  )}
                  onMouseDown={(event) => { if (event.target === svgRef.current) event.preventDefault(); }}
                  onMouseMove={(event) => {
                    if (isFsmVariant) return;
                    if (!dragConnect) return;
                    const pt = svgPointFromEvent(event);
                    if (!pt) return;
                    const hovered = graph.nodes.find(
                      (n) => n.id !== dragConnect.fromId
                        && pt.x >= n.x && pt.x <= n.x + n.width
                        && pt.y >= n.y && pt.y <= n.y + n.height,
                    );
                    setDragConnect((prev) => prev ? { ...prev, cursorX: pt.x, cursorY: pt.y, hoverId: hovered?.id ?? null } : null);
                  }}
                  onMouseUp={() => {
                    if (isFsmVariant) return;
                    if (dragConnect?.hoverId) {
                      addTransitionBetween(dragConnect.fromId, dragConnect.hoverId);
                    }
                    setDragConnect(null);
                  }}
                  onMouseLeave={() => {
                    if (!isFsmVariant) {
                      setDragConnect(null);
                    }
                  }}
                >
                  <defs>
                    <marker
                      id="pipeline-arrow"
                      viewBox="0 0 12 12"
                      refX="9"
                      refY="6"
                      markerWidth="8"
                      markerHeight="8"
                      orient="auto"
                    >
                      <path d="M 0 0 L 12 6 L 0 12 z" fill="currentColor" />
                    </marker>
                  </defs>

                  <g
                    transform={
                      graphTransform
                        ? `translate(${graphTransform.translateX} ${graphTransform.translateY}) scale(${graphTransform.scale})`
                        : undefined
                    }
                  >
                  {graph.edges.map((edge) => {
                    const accent = transitionAccent(edge.type);
                    const isSelected =
                      selection?.kind === "transition" && selection.index === edge.index;
                    return (
                      <g key={edge.key}>
                        <path
                          d={edge.path}
                          fill="none"
                          stroke={accent.stroke}
                          strokeOpacity={isSelected ? 0.95 : 0.65}
                          strokeWidth={isSelected ? 3.5 : 2.25}
                          markerEnd="url(#pipeline-arrow)"
                          style={{ color: accent.stroke }}
                        />
                        <path
                          d={edge.path}
                          fill="none"
                          stroke="transparent"
                          strokeWidth={16}
                          onClick={() => setSelection({ kind: "transition", index: edge.index })}
                          className="cursor-pointer"
                        />
                        {(() => {
                          const typeLabel = edge.type === "free"
                            ? tr("자동", "auto")
                            : edge.type === "gated"
                              ? edge.gates.length > 0 ? tr(`조건${edge.gates.length}`, `cond${edge.gates.length}`) : tr("조건부", "cond")
                              : String(edge.type);
                          const label = typeLabel;
                          if (edge.labelRotated) {
                            const labelLen = Math.max(44, label.length * 7 + 14);
                            return (
                              <g
                                transform={`translate(${edge.labelX}, ${edge.labelY}) rotate(-90)`}
                                onClick={() => setSelection({ kind: "transition", index: edge.index })}
                                className="cursor-pointer"
                              >
                                <rect
                                  x={-labelLen / 2}
                                  y={-11}
                                  width={labelLen}
                                  height={22}
                                  rx={11}
                                  fill={isSelected ? "rgba(15,23,42,0.96)" : "rgba(15,23,42,0.92)"}
                                  stroke={accent.stroke}
                                  strokeOpacity={isSelected ? 1 : 0.5}
                                  strokeWidth={1.5}
                                />
                                <text
                                  x="0"
                                  y="4"
                                  textAnchor="middle"
                                  fontSize="10"
                                  fontWeight="700"
                                  fill={accent.text}
                                >
                                  {label}
                                </text>
                              </g>
                            );
                          }
                          const labelWidth = Math.max(48, label.length * 7 + 16);
                          return (
                            <g
                              transform={`translate(${edge.labelX}, ${edge.labelY})`}
                              onClick={() => setSelection({ kind: "transition", index: edge.index })}
                              className="cursor-pointer"
                            >
                              <rect
                                x={-labelWidth / 2}
                                y={-11}
                                width={labelWidth}
                                height={22}
                                rx={11}
                                fill={isSelected ? "rgba(15,23,42,0.95)" : "rgba(15,23,42,0.88)"}
                                stroke={accent.stroke}
                                strokeOpacity={isSelected ? 1 : 0.55}
                              />
                              <text
                                x="0"
                                y="4"
                                textAnchor="middle"
                                fontSize="10"
                                fontWeight="600"
                                fill={accent.text}
                              >
                                {label}
                              </text>
                            </g>
                          );
                        })()}
                      </g>
                    );
                  })}

                  {graph.nodes.map((node) => {
                    const isSelected =
                      selection?.kind === "state" && selection.stateId === node.id;
                    const isDropTarget = dragConnect?.hoverId === node.id;
                    const isDragSource = dragConnect?.fromId === node.id;
                    return (
                      <g
                        key={node.id}
                        transform={`translate(${node.x}, ${node.y})`}
                        onClick={() => {
                          if (!dragConnect && !isFsmVariant) {
                            setSelection({ kind: "state", stateId: node.id });
                          }
                        }}
                        onMouseDown={(event) => {
                          if (isFsmVariant) return;
                          if (event.button !== 0) return;
                          event.preventDefault();
                          const pt = svgPointFromEvent(event);
                          if (!pt) return;
                          event.stopPropagation();
                          setDragConnect({
                            fromId: node.id,
                            fromCx: node.x + node.width / 2,
                            fromCy: node.y + node.height / 2,
                            cursorX: pt.x,
                            cursorY: pt.y,
                            hoverId: null,
                          });
                        }}
                        className={dragConnect ? "cursor-crosshair" : "cursor-pointer"}
                      >
                        <rect
                          width={node.width}
                          height={node.height}
                          rx={18}
                          fill={isDropTarget ? "rgba(165,180,252,0.18)" : isDragSource ? "rgba(129,140,248,0.12)" : node.terminal ? "rgba(22,163,74,0.14)" : "rgba(15,23,42,0.82)"}
                          stroke={isDropTarget ? "#a5b4fc" : isDragSource ? "#818cf8" : isSelected ? "#c4b5fd" : node.terminal ? "#4ade80" : "#64748b"}
                          strokeOpacity={isDropTarget ? 0.95 : isDragSource ? 0.8 : isSelected ? 0.95 : 0.55}
                          strokeWidth={isDropTarget ? 3 : isSelected ? 2.5 : 1.5}
                        />
                        <text
                          x="12"
                          y={compactGraph ? 20 : 24}
                          fontSize={compactGraph ? 10 : 11}
                          fontFamily="ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace"
                          fill={isSelected ? "#e9d5ff" : "#cbd5f5"}
                        >
                          {node.id}
                        </text>
                        <text
                          x="12"
                          y={compactGraph ? 38 : 45}
                          fontSize={compactGraph ? 13 : 14}
                          fontWeight="700"
                          fill={node.terminal ? "#86efac" : "#f8fafc"}
                        >
                          {node.label}
                        </text>
                        <text x="12" y={compactGraph ? 54 : 66} fontSize={compactGraph ? 9 : 11} fill="#94a3b8">
                          {[
                            node.hookCount > 0 ? `${node.hookCount}h` : null,
                            node.hasClock ? "clock" : null,
                            node.hasTimeout ? "timeout" : null,
                          ]
                            .filter(Boolean)
                            .join(" · ") || tr("속성 없음", "No extras")}
                        </text>
                      </g>
                    );
                  })}

                  {dragConnect && (
                    <line
                      x1={dragConnect.fromCx}
                      y1={dragConnect.fromCy}
                      x2={dragConnect.cursorX}
                      y2={dragConnect.cursorY}
                      stroke={dragConnect.hoverId ? "#a5b4fc" : "#818cf8"}
                      strokeWidth={2.5}
                      strokeDasharray={dragConnect.hoverId ? "none" : "6 4"}
                      strokeOpacity={0.8}
                      markerEnd="url(#pipeline-arrow)"
                      style={{ color: dragConnect.hoverId ? "#a5b4fc" : "#818cf8", pointerEvents: "none" }}
                    />
                  )}
                  </g>
                </svg>
              </div>

              <div className="flex flex-wrap gap-2 text-xs" style={MUTED_TEXT_STYLE}>
                <span>{graphPanelNote}</span>
              </div>
            </div>

            <div className="min-w-0 rounded-2xl border p-3 sm:p-4 space-y-3" style={INPUT_STYLE}>
              <div className="flex flex-wrap items-center justify-between gap-2">
                <h4 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {formatSelectionTitle(tr, selection, pipelineDraft)}
                </h4>
                {selection?.kind === "state" && (
                  <span className="text-xs" style={MUTED_TEXT_STYLE}>
                    {tr("노드 클릭으로 선택됨", "Selected from graph")}
                  </span>
                )}
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
                        style={{
                          borderColor: "rgba(148,163,184,0.18)",
                          color: "var(--th-text-primary)",
                          backgroundColor: "var(--th-overlay-subtle)",
                        }}
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
                      style={{
                        borderColor: "rgba(148,163,184,0.18)",
                        color: "var(--th-text-secondary)",
                      }}
                    >
                      {tr("훅 비우기", "Clear hooks")}
                    </button>
                    <button
                      onClick={() => clearStateClock(selectedState.id)}
                      className="rounded-xl border px-3 py-1.5 text-xs"
                      style={{
                        borderColor: "rgba(148,163,184,0.18)",
                        color: "var(--th-text-secondary)",
                      }}
                    >
                      {tr("클록 비우기", "Clear clock")}
                    </button>
                    <button
                      onClick={() => clearStateTimeout(selectedState.id)}
                      className="rounded-xl border px-3 py-1.5 text-xs"
                      style={{
                        borderColor: "rgba(148,163,184,0.18)",
                        color: "var(--th-text-secondary)",
                      }}
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

                  <div className="rounded-2xl border p-3 space-y-3" style={INPUT_STYLE}>
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
                    style={{
                      borderColor: "rgba(248,113,113,0.28)",
                      color: "#f87171",
                      backgroundColor: "rgba(248,113,113,0.08)",
                    }}
                  >
                    {tr("이 상태 삭제", "Delete state")}
                  </button>
                </div>
              )}

              {selectedTransition && (
                <div className="space-y-3">
                  {isFsmVariant ? (
                    <>
                      <div className="rounded-2xl border p-3 space-y-3" style={INPUT_STYLE}>
                        <div className="flex items-start justify-between gap-3">
                          <div>
                            <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
                              {tr("선택된 전환", "Selected transition")}
                            </h5>
                            <p className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                              {selectedTransition.from} → {selectedTransition.to}
                            </p>
                          </div>
                          <label className="inline-flex items-center gap-2 text-xs" style={{ color: "var(--th-text-secondary)" }}>
                            <span>{tr("사용", "Enabled")}</span>
                            <input
                              type="checkbox"
                              checked
                              onChange={(event) => {
                                if (!event.target.checked) {
                                  removeTransition(selectedTransitionIndex);
                                }
                              }}
                            />
                          </label>
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
                              style={INPUT_STYLE}
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
                              style={INPUT_STYLE}
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
                              style={INPUT_STYLE}
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
                                      style={{
                                        borderColor: active ? "rgba(245,158,11,0.5)" : "rgba(148,163,184,0.2)",
                                        backgroundColor: active ? "rgba(245,158,11,0.14)" : "transparent",
                                        color: active ? "#fbbf24" : "var(--th-text-muted)",
                                      }}
                                    >
                                      {name}
                                    </button>
                                  );
                                })}
                              </div>
                            </div>
                          )}
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
                                  style={{
                                    borderColor: active ? "rgba(245,158,11,0.5)" : "rgba(148,163,184,0.2)",
                                    backgroundColor: active ? "rgba(245,158,11,0.14)" : "transparent",
                                    color: active ? "#fbbf24" : "var(--th-text-muted)",
                                  }}
                                >
                                  {name}
                                </button>
                              );
                            })}
                          </div>
                        </div>
                      </div>

                      <div className="rounded-2xl border p-3 space-y-3" style={INPUT_STYLE}>
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
                            style={{
                              borderColor: "rgba(245,158,11,0.35)",
                              color: "#fbbf24",
                            }}
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
                              style={{
                                borderColor: "rgba(148,163,184,0.18)",
                                backgroundColor: "var(--th-overlay-subtle)",
                              }}
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
                        style={{
                          borderColor: "rgba(248,113,113,0.28)",
                          color: "#f87171",
                          backgroundColor: "rgba(248,113,113,0.08)",
                        }}
                      >
                        {tr("이 전환 삭제", "Delete transition")}
                      </button>
                    </>
                  )}
                </div>
              )}

              {isFsmVariant && !selectedTransition && (
                <div
                  className="rounded-2xl border px-4 py-6 text-sm"
                  style={{
                    borderColor: "rgba(148,163,184,0.18)",
                    backgroundColor: "var(--th-overlay-subtle)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {tr(
                    "전환선을 선택하면 우측 280px 패널에서 event, hook, policy를 바로 편집할 수 있습니다.",
                    "Select an edge to edit its event, hook, and policy in the 280px side panel.",
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
                            style={{
                              borderColor: active ? "rgba(96,165,250,0.5)" : "rgba(148,163,184,0.2)",
                              backgroundColor: active ? "rgba(96,165,250,0.14)" : "transparent",
                              color: active ? "#60a5fa" : "var(--th-text-muted)",
                            }}
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
          <div className="min-w-0 rounded-2xl border p-3 sm:p-4 space-y-3" style={INPUT_STYLE}>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <h4 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {tr("파이프라인 스테이지", "Pipeline Stages")}
                </h4>
                <p className="text-xs" style={MUTED_TEXT_STYLE}>
                  {selectedAgentName
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
                  style={{
                    borderColor: "rgba(56,189,248,0.35)",
                    color: "#38bdf8",
                  }}
                >
                  + {tr("스테이지", "Stage")}
                </button>
                <button
                  onClick={() => void handleClearStages()}
                  disabled={saving || (stageDrafts.length === 0 && allRepoStages.length === 0)}
                  className="rounded-xl border px-3 py-1.5 text-xs"
                  style={{
                    borderColor: "rgba(248,113,113,0.28)",
                    color: "#f87171",
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
                className="rounded-2xl border px-4 py-6 text-center text-sm"
                style={{
                  borderColor: "rgba(148,163,184,0.18)",
                  backgroundColor: "var(--th-overlay-subtle)",
                  color: "var(--th-text-muted)",
                }}
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
                    className="min-w-0 rounded-2xl border p-3 space-y-3"
                    style={{
                      borderColor: "rgba(148,163,184,0.18)",
                      backgroundColor: "var(--th-overlay-subtle)",
                    }}
                  >
                    <div className="flex items-center gap-2">
                      <span
                        className="inline-flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold"
                        style={{
                          backgroundColor: "rgba(99,102,241,0.18)",
                          color: "#c7d2fe",
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
                              {agent.avatar_emoji} {localeName(locale, agent)}
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
                              {agent.avatar_emoji} {localeName(locale, agent)}
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
                          style={{
                            borderColor: "rgba(148,163,184,0.18)",
                            color: "var(--th-text-secondary)",
                          }}
                        >
                          ↑ {tr("앞으로", "Earlier")}
                        </button>
                      )}
                      {index < stageDrafts.length - 1 && (
                        <button
                          onClick={() => moveStage(index, 1)}
                          className="rounded-xl border px-3 py-1.5 text-xs"
                          style={{
                            borderColor: "rgba(148,163,184,0.18)",
                            color: "var(--th-text-secondary)",
                          }}
                        >
                          ↓ {tr("뒤로", "Later")}
                        </button>
                      )}
                      <button
                        onClick={() => removeStage(index)}
                        className="rounded-xl border px-3 py-1.5 text-xs"
                        style={{
                          borderColor: "rgba(248,113,113,0.28)",
                          color: "#f87171",
                        }}
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
