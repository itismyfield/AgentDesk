import type {
  PhaseGateConfig,
  PipelineConfigFull,
  PipelineOverride,
  PipelineStage,
} from "../../types";

export const PIPELINE_VISUAL_EDITOR_MOBILE_BREAKPOINT = 720;

export type StageTrigger = "ready" | "review_pass";

export interface StageDraft {
  stage_name: string;
  entry_skill: string;
  provider: string;
  agent_override_id: string;
  timeout_minutes: number;
  on_failure: "fail" | "retry" | "previous" | "goto";
  on_failure_target: string;
  max_retries: number;
  skip_condition: string;
  parallel_with: string;
  applies_to_agent_id: string;
  trigger_after: StageTrigger;
}

export interface GraphNode {
  id: string;
  label: string;
  terminal?: boolean;
  x: number;
  y: number;
  width: number;
  height: number;
  index: number;
  hookCount: number;
  hasClock: boolean;
  hasTimeout: boolean;
}

export interface GraphEdge {
  key: string;
  index: number;
  from: string;
  to: string;
  type: PipelineConfigFull["transitions"][number]["type"];
  gates: string[];
  path: string;
  labelX: number;
  labelY: number;
}

export interface PipelineGraphLayout {
  width: number;
  height: number;
  columns: number;
  nodeWidth: number;
  nodeHeight: number;
  nodes: GraphNode[];
  edges: GraphEdge[];
}

type RawOverride = PipelineOverride & Record<string, unknown>;

const VISUAL_OVERRIDE_KEYS = new Set([
  "states",
  "transitions",
  "gates",
  "hooks",
  "clocks",
  "timeouts",
  "phase_gate",
]);

export function clonePipelineConfig(pipeline: PipelineConfigFull): PipelineConfigFull {
  return {
    name: pipeline.name,
    version: pipeline.version,
    states: pipeline.states.map((state) => ({ ...state })),
    transitions: pipeline.transitions.map((transition) => ({
      ...transition,
      gates: [...(transition.gates ?? [])],
    })),
    gates: Object.fromEntries(
      Object.entries(pipeline.gates).map(([key, gate]) => [key, { ...gate }]),
    ),
    hooks: Object.fromEntries(
      Object.entries(pipeline.hooks).map(([key, hook]) => [
        key,
        {
          on_enter: [...hook.on_enter],
          on_exit: [...hook.on_exit],
        },
      ]),
    ),
    clocks: Object.fromEntries(
      Object.entries(pipeline.clocks).map(([key, clock]) => [key, { ...clock }]),
    ),
    timeouts: Object.fromEntries(
      Object.entries(pipeline.timeouts).map(([key, timeout]) => [key, { ...timeout }]),
    ),
    phase_gate: clonePhaseGate(pipeline.phase_gate),
  };
}

export function clonePhaseGate(phaseGate: PhaseGateConfig): PhaseGateConfig {
  return {
    dispatch_to: phaseGate.dispatch_to,
    dispatch_type: phaseGate.dispatch_type,
    pass_verdict: phaseGate.pass_verdict,
    checks: [...phaseGate.checks],
  };
}

export function normalizeStageTrigger(
  triggerAfter: PipelineStage["trigger_after"] | null | undefined,
): StageTrigger {
  return triggerAfter === "review_pass" ? "review_pass" : "ready";
}

export function stageDraftFromApi(stage: PipelineStage): StageDraft {
  return {
    stage_name: stage.stage_name,
    entry_skill: stage.entry_skill ?? "",
    provider: stage.provider ?? "",
    agent_override_id: stage.agent_override_id ?? "",
    timeout_minutes: stage.timeout_minutes,
    on_failure: stage.on_failure,
    on_failure_target: stage.on_failure_target ?? "",
    max_retries: stage.max_retries,
    skip_condition: stage.skip_condition ?? "",
    parallel_with: stage.parallel_with ?? "",
    applies_to_agent_id: stage.applies_to_agent_id ?? "",
    trigger_after: normalizeStageTrigger(stage.trigger_after),
  };
}

export function emptyStageDraft(): StageDraft {
  return {
    stage_name: "",
    entry_skill: "",
    provider: "",
    agent_override_id: "",
    timeout_minutes: 60,
    on_failure: "fail",
    on_failure_target: "",
    max_retries: 3,
    skip_condition: "",
    parallel_with: "",
    applies_to_agent_id: "",
    trigger_after: "ready",
  };
}

export function stageInputFromDraft(stage: StageDraft) {
  return {
    stage_name: stage.stage_name.trim(),
    entry_skill: stage.entry_skill.trim() || null,
    provider: stage.provider.trim() || null,
    agent_override_id: stage.agent_override_id || null,
    timeout_minutes: stage.timeout_minutes,
    on_failure: stage.on_failure,
    on_failure_target: stage.on_failure_target.trim() || null,
    max_retries: stage.max_retries,
    skip_condition: stage.skip_condition.trim() || null,
    parallel_with: stage.parallel_with.trim() || null,
    applies_to_agent_id: stage.applies_to_agent_id || null,
    trigger_after: normalizeStageTrigger(stage.trigger_after),
  };
}

export function filterVisibleStages(stages: PipelineStage[], selectedAgentId?: string | null) {
  if (!selectedAgentId) {
    return stages;
  }
  return stages.filter(
    (stage) => !stage.applies_to_agent_id || stage.applies_to_agent_id === selectedAgentId,
  );
}

export function buildStageSavePayload(
  repoStages: PipelineStage[],
  stageDrafts: StageDraft[],
  selectedAgentId?: string | null,
) {
  const editedStages = stageDrafts
    .filter((stage) => stage.stage_name.trim())
    .map((stage) => stageInputFromDraft(stage));

  if (!selectedAgentId) {
    return editedStages;
  }

  const otherAgentStages = repoStages
    .filter((stage) => stage.applies_to_agent_id && stage.applies_to_agent_id !== selectedAgentId)
    .map((stage) => ({
      stage_name: stage.stage_name,
      entry_skill: stage.entry_skill ?? null,
      provider: stage.provider ?? null,
      agent_override_id: stage.agent_override_id ?? null,
      timeout_minutes: stage.timeout_minutes,
      on_failure: stage.on_failure,
      on_failure_target: stage.on_failure_target ?? null,
      max_retries: stage.max_retries,
      skip_condition: stage.skip_condition ?? null,
      parallel_with: stage.parallel_with ?? null,
      applies_to_agent_id: stage.applies_to_agent_id ?? null,
      trigger_after: normalizeStageTrigger(stage.trigger_after),
    }));

  return [...editedStages, ...otherAgentStages];
}

export function extractOverrideExtras(rawConfig: unknown): Record<string, unknown> {
  if (!rawConfig || typeof rawConfig !== "object" || Array.isArray(rawConfig)) {
    return {};
  }
  const extras: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(rawConfig as Record<string, unknown>)) {
    if (!VISUAL_OVERRIDE_KEYS.has(key)) {
      extras[key] = value;
    }
  }
  return extras;
}

export function hasRawOverride(rawConfig: unknown) {
  return !!rawConfig && typeof rawConfig === "object" && !Array.isArray(rawConfig);
}

export function buildOverridePayload(
  pipeline: PipelineConfigFull,
  extras: Record<string, unknown> = {},
): RawOverride {
  return {
    ...extras,
    states: pipeline.states.map((state) => ({ ...state })),
    transitions: pipeline.transitions.map((transition) => ({
      from: transition.from,
      to: transition.to,
      type: transition.type,
      gates: [...(transition.gates ?? [])],
    })),
    gates: Object.fromEntries(
      Object.entries(pipeline.gates).map(([key, gate]) => [key, { ...gate }]),
    ),
    hooks: Object.fromEntries(
      Object.entries(pipeline.hooks).map(([key, hook]) => [
        key,
        {
          on_enter: [...hook.on_enter],
          on_exit: [...hook.on_exit],
        },
      ]),
    ),
    clocks: Object.fromEntries(
      Object.entries(pipeline.clocks).map(([key, clock]) => [key, { ...clock }]),
    ),
    timeouts: Object.fromEntries(
      Object.entries(pipeline.timeouts).map(([key, timeout]) => [key, { ...timeout }]),
    ),
    phase_gate: clonePhaseGate(pipeline.phase_gate),
  };
}

export function createNewStateId(states: PipelineConfigFull["states"]) {
  let nextIndex = states.length + 1;
  while (states.some((state) => state.id === `state_${nextIndex}`)) {
    nextIndex += 1;
  }
  return `state_${nextIndex}`;
}

export function createNewStateLabel(states: PipelineConfigFull["states"]) {
  return `State ${states.length + 1}`;
}

export function getGraphColumnCount(stateCount: number, compact: boolean) {
  if (compact) {
    return 1;
  }
  if (stateCount <= 4) {
    return stateCount;
  }
  if (stateCount <= 6) {
    return 3;
  }
  return 4;
}

export function buildPipelineGraph(
  pipeline: PipelineConfigFull,
  compact: boolean,
): PipelineGraphLayout {
  const columns = Math.max(1, getGraphColumnCount(pipeline.states.length, compact));
  const nodeWidth = compact ? 256 : 168;
  const nodeHeight = compact ? 86 : 78;
  const columnGap = compact ? 0 : 40;
  const rowGap = compact ? 54 : 76;
  const paddingX = compact ? 18 : 28;
  const paddingY = 24;

  const nodes: GraphNode[] = pipeline.states.map((state, index) => {
    const column = index % columns;
    const row = Math.floor(index / columns);
    const x = paddingX + column * (nodeWidth + columnGap);
    const y = paddingY + row * (nodeHeight + rowGap);
    const hooks = pipeline.hooks[state.id];

    return {
      id: state.id,
      label: state.label,
      terminal: state.terminal,
      x,
      y,
      width: nodeWidth,
      height: nodeHeight,
      index,
      hookCount: (hooks?.on_enter.length ?? 0) + (hooks?.on_exit.length ?? 0),
      hasClock: !!pipeline.clocks[state.id],
      hasTimeout: !!pipeline.timeouts[state.id],
    };
  });

  const nodeMap = new Map(nodes.map((node) => [node.id, node]));

  const edges: GraphEdge[] = pipeline.transitions.map((transition, index) => {
    const fromNode = nodeMap.get(transition.from);
    const toNode = nodeMap.get(transition.to);
    if (!fromNode || !toNode) {
      return {
        key: `transition-${index}`,
        index,
        from: transition.from,
        to: transition.to,
        type: transition.type,
        gates: [...(transition.gates ?? [])],
        path: "",
        labelX: 0,
        labelY: 0,
      };
    }

    if (compact) {
      const startX = fromNode.x + fromNode.width / 2;
      const startY = fromNode.y + fromNode.height;
      const endX = toNode.x + toNode.width / 2;
      const endY = toNode.y;
      const midY = startY + (endY - startY) / 2;
      return {
        key: `transition-${index}`,
        index,
        from: transition.from,
        to: transition.to,
        type: transition.type,
        gates: [...(transition.gates ?? [])],
        path: `M ${startX} ${startY} C ${startX} ${midY}, ${endX} ${midY}, ${endX} ${endY}`,
        labelX: (startX + endX) / 2,
        labelY: midY - 8,
      };
    }

    const sameRow = fromNode.y === toNode.y;
    if (sameRow) {
      const forward = toNode.x >= fromNode.x;
      const startX = forward ? fromNode.x + fromNode.width : fromNode.x;
      const endX = forward ? toNode.x : toNode.x + toNode.width;
      const startY = fromNode.y + fromNode.height / 2;
      const endY = toNode.y + toNode.height / 2;
      const delta = Math.max(42, Math.abs(endX - startX) / 2);
      return {
        key: `transition-${index}`,
        index,
        from: transition.from,
        to: transition.to,
        type: transition.type,
        gates: [...(transition.gates ?? [])],
        path: `M ${startX} ${startY} C ${startX + (forward ? delta : -delta)} ${startY}, ${endX - (forward ? delta : -delta)} ${endY}, ${endX} ${endY}`,
        labelX: (startX + endX) / 2,
        labelY: startY - 10,
      };
    }

    const startX = fromNode.x + fromNode.width / 2;
    const startY = fromNode.y + fromNode.height;
    const endX = toNode.x + toNode.width / 2;
    const endY = toNode.y;
    const midY = startY + (endY - startY) / 2;

    return {
      key: `transition-${index}`,
      index,
      from: transition.from,
      to: transition.to,
      type: transition.type,
      gates: [...(transition.gates ?? [])],
      path: `M ${startX} ${startY} C ${startX} ${midY}, ${endX} ${midY}, ${endX} ${endY}`,
      labelX: (startX + endX) / 2,
      labelY: midY - 10,
    };
  });

  const rowCount = Math.max(1, Math.ceil(nodes.length / columns));
  const width = paddingX * 2 + columns * nodeWidth + Math.max(0, columns - 1) * columnGap;
  const height = paddingY * 2 + rowCount * nodeHeight + Math.max(0, rowCount - 1) * rowGap;

  return {
    width,
    height,
    columns,
    nodeWidth,
    nodeHeight,
    nodes,
    edges,
  };
}
