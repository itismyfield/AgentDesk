import type { Dispatch, SetStateAction } from "react";

import * as api from "../../api";
import type { PipelineConfigFull, PipelineStage } from "../../types";
import {
  buildFsmEdgeBindingKey,
  buildOverridePayload,
  buildStageSavePayload,
  clonePipelineConfig,
  createNewStateId,
  createNewStateLabel,
  emptyStageDraft,
  inferFsmEventName,
  type Selection,
  type StageDraft,
} from "./pipeline-visual-editor-model";
import type { EditLevel } from "./pipeline-visual-editor-types";
import {
  downloadTextFile,
  FSM_EDGE_BINDINGS_KEY,
  normalizeFsmEdgeBindings,
  parseCommaSeparated,
} from "./pipeline-visual-editor-ui";

interface Params {
  tr: (ko: string, en: string) => string;
  repo?: string;
  selectedAgentId?: string | null;
  variant: "advanced" | "fsm";
  isFsmVariant: boolean;
  level: EditLevel;
  pipelineDraft: PipelineConfigFull | null;
  allRepoStages: PipelineStage[];
  stageDrafts: StageDraft[];
  overrideExtras: Record<string, unknown>;
  pipelineChanged: boolean;
  stagesChanged: boolean;
  saving: boolean;
  overrideExists: boolean;
  setLevel: Dispatch<SetStateAction<EditLevel>>;
  setReloadKey: Dispatch<SetStateAction<number>>;
  setCollapsed: Dispatch<SetStateAction<boolean>>;
  setPipelineDraft: Dispatch<SetStateAction<PipelineConfigFull | null>>;
  setStageDrafts: Dispatch<SetStateAction<StageDraft[]>>;
  setSelection: Dispatch<SetStateAction<Selection>>;
  setOverrideExtras: Dispatch<SetStateAction<Record<string, unknown>>>;
  setSaving: Dispatch<SetStateAction<boolean>>;
  setError: Dispatch<SetStateAction<string | null>>;
  setSuccess: Dispatch<SetStateAction<string | null>>;
  refreshAfterMutation: (nextLevel?: EditLevel) => Promise<void>;
}

export function usePipelineVisualEditorActions(params: Params) {
  const {
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
  } = params;

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

  function updateStateHooks(stateId: string, hookType: "on_enter" | "on_exit", value: string) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      const existing = next.hooks[stateId] ?? { on_enter: [], on_exit: [] };
      const updated = { ...existing, [hookType]: parseCommaSeparated(value) };
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

  function updateStateClock(stateId: string, patch: Partial<PipelineConfigFull["clocks"][string]>) {
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
        max_retries: patch.max_retries ?? next.timeouts[stateId]?.max_retries,
        on_exhaust: patch.on_exhaust ?? next.timeouts[stateId]?.on_exhaust,
        condition: patch.condition ?? next.timeouts[stateId]?.condition,
      };
      if (next.timeouts[stateId].max_retries === undefined) delete next.timeouts[stateId].max_retries;
      if (!next.timeouts[stateId].on_exhaust) delete next.timeouts[stateId].on_exhaust;
      if (!next.timeouts[stateId].condition) delete next.timeouts[stateId].condition;
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
      next.states.push({ id: nextStateId, label: createNewStateLabel(next.states), terminal: false });
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
      next.transitions.push({ from: next.states[0].id, to: next.states[1].id, type: "free", gates: [] });
      nextIndex = next.transitions.length - 1;
      return next;
    });
    if (nextIndex >= 0) {
      setSelection({ kind: "transition", index: nextIndex });
    }
  }

  function addTransitionBetween(fromId: string, toId: string) {
    if (!pipelineDraft || fromId === toId) return;
    if (pipelineDraft.transitions.some((transition) => transition.from === fromId && transition.to === toId)) {
      return;
    }
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

  function updateTransition(index: number, patch: Partial<PipelineConfigFull["transitions"][number]>) {
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
        gates: patch.type && patch.type !== "gated" ? [] : patch.gates ?? target.gates ?? [],
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
          next.gates[gateName] = { type: "builtin" };
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
    const base = `${transition.from}_${transition.to}_gate`.replace(/[^a-zA-Z0-9_]/g, "_").toLowerCase();
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
      next.gates[gateName] = { type: "builtin" };
      if (target.type !== "gated") {
        target.type = "gated";
      }
      return next;
    });
  }

  function updateGate(gateName: string, patch: Partial<PipelineConfigFull["gates"][string]>) {
    setPipelineDraft((current) => {
      if (!current) {
        return current;
      }
      const next = clonePipelineConfig(current);
      next.gates[gateName] = {
        type: patch.type ?? next.gates[gateName]?.type ?? "builtin",
        check: patch.check !== undefined ? patch.check : next.gates[gateName]?.check,
        description: patch.description !== undefined ? patch.description : next.gates[gateName]?.description,
      };
      if (!next.gates[gateName].check) delete next.gates[gateName].check;
      if (!next.gates[gateName].description) delete next.gates[gateName].description;
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
      next.phase_gate = { ...next.phase_gate, ...patch };
      return next;
    });
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
        await api.savePipelineStages(repo, buildStageSavePayload(allRepoStages, stageDrafts, selectedAgentId));
      }
      await refreshAfterMutation(level);
      setSuccess(tr("비주얼 파이프라인 편집 내용을 저장했습니다.", "Saved visual pipeline editor changes."));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : tr("파이프라인 저장에 실패했습니다.", "Failed to save pipeline."));
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
      setError(cause instanceof Error ? cause.message : tr("오버라이드 초기화에 실패했습니다.", "Failed to clear override."));
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
        await api.savePipelineStages(repo, buildStageSavePayload(allRepoStages, [], selectedAgentId));
      } else {
        await api.deletePipelineStages(repo);
      }
      await refreshAfterMutation(level);
      setSuccess(tr("보이는 파이프라인 스테이지를 정리했습니다.", "Cleared visible pipeline stages."));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : tr("스테이지 정리에 실패했습니다.", "Failed to clear stages."));
    } finally {
      setSaving(false);
    }
  }

  return {
    addGate,
    addStage,
    addState,
    addTransition,
    addTransitionBetween,
    clearStateClock,
    clearStateHooks,
    clearStateTimeout,
    handleClearOverride,
    handleClearStages,
    handleExportJson,
    handleSave,
    moveStage,
    removeStage,
    removeState,
    removeTransition,
    setCollapsed,
    setLevel,
    setReloadKey,
    setSelection,
    updateFsmEventHook,
    updateFsmTransitionEvent,
    updateGate,
    updatePhaseGate,
    updateStage,
    updateState,
    updateStateClock,
    updateStateHooks,
    updateStateTimeout,
    updateTransition,
    updateTransitionGates,
  };
}
