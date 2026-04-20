import type {
  PhaseGateConfig,
  PipelineConfigFull,
  PipelineOverride,
  PipelineStage,
} from "../../types";
import { MOBILE_LAYOUT_BREAKPOINT_PX } from "../../app/breakpoints";

export const PIPELINE_VISUAL_EDITOR_MOBILE_BREAKPOINT = MOBILE_LAYOUT_BREAKPOINT_PX;

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
  "events",
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
    events: Object.fromEntries(
      Object.entries(pipeline.events).map(([key, hooks]) => [key, [...hooks]]),
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
    events: Object.fromEntries(
      Object.entries(pipeline.events).map(([key, hooks]) => [key, [...hooks]]),
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
  const rowGap = compact ? 58 : 96;
  const paddingY = 20;

  const upwardEdgeInfos: { transIdx: number; fi: number; ti: number }[] = [];
  pipeline.transitions.forEach((t, transIdx) => {
    const fi = pipeline.states.findIndex((s) => s.id === t.from);
    const ti = pipeline.states.findIndex((s) => s.id === t.to);
    if (fi >= 0 && ti >= 0) {
      const fromRow = Math.floor(fi / columns);
      const toRow = Math.floor(ti / columns);
      if (toRow < fromRow) {
        upwardEdgeInfos.push({ transIdx, fi, ti });
      }
    }
  });
  const laneWidth = 28;
  const upwardLaneAssignment = new Map<number, number>();
  let leftLanes = 0;
  if (upwardEdgeInfos.length > 0) {
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

  const visibleTransitions = pipeline.transitions;

  const pairCount = new Map<string, number>();
  const pairIndex = new Map<number, number>();
  visibleTransitions.forEach((t, i) => {
    const pairKey = [t.from, t.to].sort().join("|");
    const n = pairCount.get(pairKey) ?? 0;
    pairIndex.set(i, n);
    pairCount.set(pairKey, n + 1);
  });

  const edges: GraphEdge[] = visibleTransitions.map((transition, visIdx) => {
    const index = pipeline.transitions.indexOf(transition);
    const edgePairIdx = pairIndex.get(visIdx) ?? 0;
    const edgePairTotal = pairCount.get([transition.from, transition.to].sort().join("|")) ?? 1;
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

    const pairSpread = edgePairTotal > 1 ? (edgePairIdx - (edgePairTotal - 1) / 2) * 14 : 0;

    const sameRow = fromNode.y === toNode.y;
    if (sameRow) {
      const forward = toNode.x >= fromNode.x;
      if (forward) {
        const startX = fromNode.x + fromNode.width;
        const endX = toNode.x;
        const sy = fromNode.y + fromNode.height * 0.4 + pairSpread;
        const ey = toNode.y + toNode.height * 0.4 + pairSpread;
        return {
          key: `transition-${index}`,
          index,
          from: transition.from,
          to: transition.to,
          type: transition.type,
          gates: [...(transition.gates ?? [])],
          path: `M ${startX} ${sy} L ${endX} ${ey}`,
          labelX: (startX + endX) / 2,
          labelY: Math.min(sy, ey) - 8,
        };
      }
      const startX = fromNode.x + fromNode.width * 0.4 + pairSpread;
      const endX = toNode.x + toNode.width * 0.6 + pairSpread;
      const startY = fromNode.y + fromNode.height;
      const endY = toNode.y + toNode.height;
      const loopY = startY + 24 + edgePairIdx * 16;
      return {
        key: `transition-${index}`,
        index,
        from: transition.from,
        to: transition.to,
        type: transition.type,
        gates: [...(transition.gates ?? [])],
        path: `M ${startX} ${startY} L ${startX} ${loopY} L ${endX} ${loopY} L ${endX} ${endY}`,
        labelX: (startX + endX) / 2,
        labelY: loopY - 8,
      };
    }

    const downward = toNode.y > fromNode.y;
    if (downward) {
      const startX = fromNode.x + fromNode.width / 2 + pairSpread;
      const startY = fromNode.y + fromNode.height;
      const endX = toNode.x + toNode.width / 2 + pairSpread;
      const endY = toNode.y;
      const controlDist = Math.max(36, Math.abs(endY - startY) * 0.4);
      return {
        key: `transition-${index}`,
        index,
        from: transition.from,
        to: transition.to,
        type: transition.type,
        gates: [...(transition.gates ?? [])],
        path: `M ${startX} ${startY} C ${startX} ${startY + controlDist}, ${endX} ${endY - controlDist}, ${endX} ${endY}`,
        labelX: (startX + endX) / 2,
        labelY: (startY + endY) / 2 - 10,
      };
    }

    const lane = upwardLaneAssignment.get(index) ?? 0;
    const laneX = paddingX - leftMargin + (leftLanes - 1 - lane) * laneWidth + 10;
    const startX = fromNode.x;
    const startY = fromNode.y + nodeHeight / 2 + pairSpread;
    const endX = toNode.x;
    const endY = toNode.y + nodeHeight / 2 + pairSpread;
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
  });

  const rowCount = Math.max(1, Math.ceil(nodes.length / columns));
  const rightExtra = compact ? 14 : 28;
  const backwardBottomExtra = compact ? 0 : 56;
  const width = paddingX + rightExtra + columns * nodeWidth + Math.max(0, columns - 1) * columnGap;
  const height = paddingY + 28 + rowCount * nodeHeight + Math.max(0, rowCount - 1) * rowGap + backwardBottomExtra;

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
