import type { CampaignNode } from "../../api/campaigns";
import { rawEdgeId } from "./campaignGraphRouting";
import { campaignGroup } from "./campaignModel";
import { cycleComponents } from "./relationsEditorModel";

export type CriticalPathMode = "remaining" | "structural";

export interface TaskScheduleNode {
  id: string;
  baseDuration: number;
  delta: number;
  duration: number;
  earliestStart: number;
  earliestFinish: number;
  latestStart: number;
  latestFinish: number;
  slack: number;
  waveStart: number;
  waveEnd: number;
  critical: boolean;
  cyclic: boolean;
}

export interface CampaignCriticalPath {
  mode: CriticalPathMode;
  nodes: Map<string, TaskScheduleNode>;
  totalDuration: number;
  criticalNodeIds: Set<string>;
  criticalEdgeKeys: Set<string>;
  criticalGroups: Set<string>;
  cyclicIds: Set<string>;
}

export interface CriticalPathComparison {
  baseline: CampaignCriticalPath;
  simulated: CampaignCriticalPath;
  durationDelta: number;
  pathShifted: boolean;
  newlyCriticalIds: string[];
  hasWhatIf: boolean;
}

export type SchedulableTaskNode = Pick<CampaignNode, "id" | "status"> & {
  dependencies?: readonly string[];
  group?: string | null;
};

export function computeCampaignCriticalPath(
  nodes: ReadonlyArray<SchedulableTaskNode>,
  options?: { mode?: CriticalPathMode; whatIfDeltas?: Readonly<Record<string, number>> },
): CampaignCriticalPath {
  const mode: CriticalPathMode = options?.mode ?? "remaining";
  const deltas = options?.whatIfDeltas;
  const normalized = nodes.map((node) => ({ id: node.id, dependencies: node.dependencies ?? [] }));
  const cyclicIds = new Set<string>();
  for (const part of cycleComponents(normalized)) {
    for (const id of part) cyclicIds.add(id);
  }

  const byId = new Map(nodes.map((node) => [node.id, node]));
  const acyclicIds = nodes.filter((node) => !cyclicIds.has(node.id)).map((node) => node.id);
  const acyclicSet = new Set(acyclicIds);
  const depsOf = new Map<string, string[]>();
  const succsOf = new Map<string, string[]>(acyclicIds.map((id) => [id, []]));
  const inDegree = new Map<string, number>();

  for (const id of acyclicIds) {
    const validDeps = [...new Set((byId.get(id)?.dependencies ?? []).filter((dep) => acyclicSet.has(dep)))];
    depsOf.set(id, validDeps);
    inDegree.set(id, validDeps.length);
    for (const dep of validDeps) succsOf.get(dep)!.push(id);
  }

  const topo: string[] = [];
  const queue = acyclicIds.filter((id) => inDegree.get(id) === 0);
  for (let head = 0; head < queue.length; head++) {
    const u = queue[head];
    topo.push(u);
    for (const v of succsOf.get(u)!) {
      const nextDeg = inDegree.get(v)! - 1;
      inDegree.set(v, nextDeg);
      if (nextDeg === 0) queue.push(v);
    }
  }

  const baseDur = new Map<string, number>();
  const deltaOf = new Map<string, number>();
  const durOf = new Map<string, number>();
  for (const node of nodes) {
    const base = mode === "remaining" && (node.status === "completed" || node.status === "skipped") ? 0 : 1;
    const rawDelta = deltas?.[node.id] ?? 0;
    const d = Number.isFinite(rawDelta) ? Math.trunc(rawDelta) : 0;
    baseDur.set(node.id, base);
    deltaOf.set(node.id, d);
    durOf.set(node.id, Math.max(0, base + d));
  }

  const es = new Map<string, number>();
  const ef = new Map<string, number>();
  for (const id of topo) {
    const start = depsOf.get(id)!.reduce((acc, dep) => Math.max(acc, ef.get(dep)!), 0);
    es.set(id, start);
    ef.set(id, start + durOf.get(id)!);
  }

  const totalDuration = topo.reduce((acc, id) => Math.max(acc, ef.get(id)!), 0);
  const ls = new Map<string, number>();
  const lf = new Map<string, number>();
  const slackMap = new Map<string, number>();

  for (let i = topo.length - 1; i >= 0; i--) {
    const id = topo[i];
    const succs = succsOf.get(id)!;
    const finish = succs.length === 0 ? totalDuration : Math.min(...succs.map((s) => ls.get(s)!));
    const start = finish - durOf.get(id)!;
    lf.set(id, finish);
    ls.set(id, start);
    slackMap.set(id, Math.max(0, start - es.get(id)!));
  }

  const scheduleNodes = new Map<string, TaskScheduleNode>();
  const criticalNodeIds = new Set<string>();
  const criticalGroups = new Set<string>();

  for (const node of nodes) {
    const id = node.id;
    const baseDuration = baseDur.get(id)!;
    const delta = deltaOf.get(id)!;
    const duration = durOf.get(id)!;
    if (cyclicIds.has(id)) {
      scheduleNodes.set(id, {
        id,
        baseDuration,
        delta,
        duration,
        earliestStart: 0,
        earliestFinish: duration,
        latestStart: 0,
        latestFinish: duration,
        slack: 0,
        waveStart: 1,
        waveEnd: Math.max(1, duration),
        critical: false,
        cyclic: true,
      });
      continue;
    }
    const earliestStart = es.get(id)!;
    const earliestFinish = ef.get(id)!;
    const latestStart = ls.get(id)!;
    const latestFinish = lf.get(id)!;
    const slack = slackMap.get(id)!;
    const waveStart = earliestStart + 1;
    const waveEnd = Math.max(waveStart, earliestFinish);
    const critical = totalDuration > 0 && duration > 0 && slack === 0;
    if (critical) {
      criticalNodeIds.add(id);
      criticalGroups.add(campaignGroup(node as CampaignNode));
    }
    scheduleNodes.set(id, {
      id,
      baseDuration,
      delta,
      duration,
      earliestStart,
      earliestFinish,
      latestStart,
      latestFinish,
      slack,
      waveStart,
      waveEnd,
      critical,
      cyclic: false,
    });
  }

  // Propagate critical reachability across zero-duration slack-0 bridges so active chains never break across completed steps.
  const fromCritical = new Map<string, boolean>();
  for (const id of topo) {
    const node = scheduleNodes.get(id)!;
    const bridged =
      node.duration === 0 &&
      node.slack === 0 &&
      depsOf.get(id)!.some((dep) => ef.get(dep) === es.get(id) && fromCritical.get(dep));
    fromCritical.set(id, node.critical || bridged);
  }

  const toCritical = new Map<string, boolean>();
  for (let i = topo.length - 1; i >= 0; i--) {
    const id = topo[i];
    const node = scheduleNodes.get(id)!;
    const bridged =
      node.duration === 0 &&
      node.slack === 0 &&
      succsOf.get(id)!.some((succ) => ef.get(id) === es.get(succ) && toCritical.get(succ));
    toCritical.set(id, node.critical || bridged);
  }

  const criticalEdgeKeys = new Set<string>();
  if (totalDuration > 0) {
    for (const id of topo) {
      if (!toCritical.get(id)) continue;
      for (const dep of depsOf.get(id)!) {
        if (fromCritical.get(dep) && ef.get(dep) === es.get(id)) {
          criticalEdgeKeys.add(rawEdgeId([dep, id]));
        }
      }
    }
  }

  return {
    mode,
    nodes: scheduleNodes,
    totalDuration,
    criticalNodeIds,
    criticalEdgeKeys,
    criticalGroups,
    cyclicIds,
  };
}

export function compareCampaignCriticalPath(
  nodes: ReadonlyArray<SchedulableTaskNode>,
  mode: CriticalPathMode = "remaining",
  whatIfDeltas?: Readonly<Record<string, number>>,
): CriticalPathComparison {
  const normalized: Record<string, number> = {};
  for (const [id, raw] of Object.entries(whatIfDeltas ?? {})) {
    const d = Number.isFinite(raw) ? Math.trunc(raw) : 0;
    if (d !== 0) normalized[id] = d;
  }
  const hasWhatIf = Object.keys(normalized).length > 0;
  const baseline = computeCampaignCriticalPath(nodes, { mode });
  const simulated = hasWhatIf
    ? computeCampaignCriticalPath(nodes, { mode, whatIfDeltas: normalized })
    : computeCampaignCriticalPath(nodes, { mode });
  const durationDelta = simulated.totalDuration - baseline.totalDuration;
  const newlyCriticalIds = [...simulated.criticalNodeIds].filter((id) => !baseline.criticalNodeIds.has(id));
  const pathShifted =
    hasWhatIf &&
    (newlyCriticalIds.length > 0 || [...baseline.criticalNodeIds].some((id) => !simulated.criticalNodeIds.has(id)));
  return {
    baseline,
    simulated,
    durationDelta,
    pathShifted,
    newlyCriticalIds,
    hasWhatIf,
  };
}
