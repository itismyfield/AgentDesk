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
  labelRotated?: boolean;
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
  const nodeWidth = compact ? 174 : 168;
  const nodeHeight = compact ? 66 : 78;
  const columnGap = compact ? 0 : 40;
  const rowGap = compact ? 58 : 76;
  const paddingY = 20;

  const upwardEdgeInfos: { transIdx: number; fi: number; ti: number }[] = [];
  if (compact) {
    pipeline.transitions.forEach((t, transIdx) => {
      if (t.type === "force_only") return;
      const fi = pipeline.states.findIndex((s) => s.id === t.from);
      const ti = pipeline.states.findIndex((s) => s.id === t.to);
      if (fi >= 0 && ti >= 0 && ti < fi) {
        upwardEdgeInfos.push({ transIdx, fi, ti });
      }
    });
  }
  const laneWidth = 28;
  const upwardLaneAssignment = new Map<number, number>();
  let leftLanes = 0;
  if (compact && upwardEdgeInfos.length > 0) {
    const sorted = [...upwardEdgeInfos].sort((a, b) => (a.fi - a.ti) - (b.fi - b.ti));
    const lanes: { maxRow: number }[] = [];
    for (const info of sorted) {
      let assigned = -1;
      for (let l = 0; l < lanes.length; l++) {
        if (lanes[l].maxRow <= info.ti) {
          assigned = l;
          lanes[l].maxRow = info.fi;
          break;
        }
      }
      if (assigned < 0) {
        assigned = lanes.length;
        lanes.push({ maxRow: info.fi });
      }
      upwardLaneAssignment.set(info.transIdx, assigned);
    }
    leftLanes = lanes.length;
  }
  const leftMargin = leftLanes > 0 ? leftLanes * laneWidth + 14 : 0;
  const paddingX = (compact ? 14 : 28) + leftMargin;

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

  const visibleTransitions = compact
    ? pipeline.transitions.filter((t) => t.type !== "force_only")
    : pipeline.transitions;

  const edges: GraphEdge[] = visibleTransitions.map((transition) => {
    const index = pipeline.transitions.indexOf(transition);
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

    if (fromNode.id === toNode.id) {
      const startX = fromNode.x + fromNode.width;
      const startY = fromNode.y + fromNode.height / 2;
      const endX = fromNode.x + fromNode.width / 2;
      const endY = fromNode.y;
      const loopRightX = fromNode.x + fromNode.width + (compact ? 42 : 64);
      const loopTopY = fromNode.y - (compact ? 32 : 52);
      return {
        key: `transition-${index}`,
        index,
        from: transition.from,
        to: transition.to,
        type: transition.type,
        gates: [...(transition.gates ?? [])],
        path: `M ${startX} ${startY} C ${loopRightX} ${startY}, ${loopRightX} ${loopTopY}, ${endX} ${loopTopY} C ${fromNode.x + 8} ${loopTopY}, ${endX - 24} ${endY}, ${endX} ${endY}`,
        labelX: loopRightX - 12,
        labelY: loopTopY - 8,
      };
    }

    if (compact) {
      const downward = toNode.y > fromNode.y;
      if (downward) {
        const cx = fromNode.x + fromNode.width / 2;
        const startY = fromNode.y + fromNode.height;
        const endY = toNode.y;
        const midY = (startY + endY) / 2;
        return {
          key: `transition-${index}`,
          index,
          from: transition.from,
          to: transition.to,
          type: transition.type,
          gates: [...(transition.gates ?? [])],
          path: `M ${cx} ${startY} L ${cx} ${endY}`,
          labelX: cx,
          labelY: midY,
        };
      }
      const lane = upwardLaneAssignment.get(index) ?? 0;
      const laneX = paddingX - leftMargin + (leftLanes - 1 - lane) * laneWidth + 10;
      const startX = fromNode.x;
      const startY = fromNode.y + nodeHeight / 2;
      const endX = toNode.x;
      const endY = toNode.y + nodeHeight / 2;
      return {
        key: `transition-${index}`,
        index,
        from: transition.from,
        to: transition.to,
        type: transition.type,
        gates: [...(transition.gates ?? [])],
        path: `M ${startX} ${startY} L ${laneX} ${startY} L ${laneX} ${endY} L ${endX} ${endY}`,
        labelX: laneX,
        labelY: (startY + endY) / 2,
        labelRotated: true,
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

    const upward = toNode.y < fromNode.y;
    const startX = fromNode.x + fromNode.width / 2;
    const startY = upward ? fromNode.y : fromNode.y + fromNode.height;
    const endX = toNode.x + toNode.width / 2;
    const endY = upward ? toNode.y + toNode.height : toNode.y;
    const controlDistance = Math.max(42, Math.abs(endY - startY) / 2);
    const controlStartY = upward ? startY - controlDistance : startY + controlDistance;
    const controlEndY = upward ? endY + controlDistance : endY - controlDistance;
    const labelY = upward
      ? Math.min(controlStartY, controlEndY) - 10
      : (startY + endY) / 2 - 10;

    return {
      key: `transition-${index}`,
      index,
      from: transition.from,
      to: transition.to,
      type: transition.type,
      gates: [...(transition.gates ?? [])],
      path: `M ${startX} ${startY} C ${startX} ${controlStartY}, ${endX} ${controlEndY}, ${endX} ${endY}`,
      labelX: (startX + endX) / 2,
      labelY,
    };
  });

  const rowCount = Math.max(1, Math.ceil(nodes.length / columns));
  const rightExtra = compact ? 14 : 28;
  const width = paddingX + rightExtra + columns * nodeWidth + Math.max(0, columns - 1) * columnGap;
  const height = paddingY + 28 + rowCount * nodeHeight + Math.max(0, rowCount - 1) * rowGap;

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
