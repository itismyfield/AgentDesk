import { subscribeToLocalStorageKey } from "../../lib/useLocalStorage";
import { NO_TIES, rawEdgeId, type CampaignGraphView, type GraphViewSettings, type RawEdgeKey, type ViewScope } from "./campaignGraphRouting";
import { pruneTies } from "./campaignTieEditor";

/**
 * Tie layout per browser profile, apart from the campaign definition: nothing here reaches the server.
 * useLocalStorage swallows write failures, so this wrapper reports them to the caller.
 */
const KEY_PREFIX = "agentdesk.campaign-graph-view.v1:";

export type StoredView = { view: GraphViewSettings; raw: string | null; recovered: boolean };
export type SaveResult = { ok: true; raw: string } | { ok: false; reason: string };

/** The legacy campaign's all-task group graph, the one drawing that has ties today. */
export const groupViewScope = (campaignId: string): ViewScope => ({ mode: "legacy", campaignId, projection: "groups", taskScope: "all" });

export function campaignViewKey(scope: ViewScope): string {
  return KEY_PREFIX + JSON.stringify([scope.mode, scope.campaignId, scope.projection, scope.taskScope]);
}

function storage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

const isText = (value: unknown): value is string => typeof value === "string";
const isEdge = (value: unknown) => Array.isArray(value) && value.length === 2 && value.every(isText);
const isPoint = (value: unknown) => {
  const point = value as { x?: unknown; y?: unknown } | null;
  return typeof point === "object" && point !== null && Number.isFinite(point.x) && Number.isFinite(point.y);
};

/** Each tie's members and the routes through it name the same edges, once each, and no tie is empty. */
function consistent(ties: CampaignGraphView["ties"], routes: CampaignGraphView["routes"]): boolean {
  const members = new Set<string>();
  for (const tie of ties) {
    if (tie.strandOrder.length === 0) return false;
    for (const edge of tie.strandOrder) {
      const pair = JSON.stringify([tie.id, rawEdgeId(edge)]);
      if (members.has(pair)) return false;
      members.add(pair);
    }
  }
  const routed = new Set<string>();
  const passes = new Set<string>();
  for (const route of routes) {
    if (route.via.length === 0 || routed.has(rawEdgeId(route.edge)) || new Set(route.via).size !== route.via.length) return false;
    routed.add(rawEdgeId(route.edge));
    for (const id of route.via) passes.add(JSON.stringify([id, rawEdgeId(route.edge)]));
  }
  return passes.size === members.size && [...passes].every((pair) => members.has(pair));
}

/** A stored view is used only when it matches this schema and scope and its routes agree with tie membership. */
function parseView(raw: string, scope: ViewScope): GraphViewSettings | null {
  let value: Partial<CampaignGraphView>;
  try {
    value = JSON.parse(raw) as Partial<CampaignGraphView>;
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null || value.schemaVersion !== 1 || typeof value.showOriginal !== "boolean") return null;
  const stored = value.scope as Partial<ViewScope> | undefined;
  if (!stored || campaignViewKey(stored as ViewScope) !== campaignViewKey(scope)) return null;
  if (!Array.isArray(value.ties) || !Array.isArray(value.routes)) return null;
  const ids = new Set<string>();
  for (const tie of value.ties) {
    if (!tie || !isText(tie.id) || ids.has(tie.id) || !isPoint(tie.position) || !Array.isArray(tie.strandOrder) || !tie.strandOrder.every(isEdge)) return null;
    ids.add(tie.id);
  }
  for (const route of value.routes) {
    if (!route || !isEdge(route.edge) || !Array.isArray(route.via) || !route.via.every((id) => isText(id) && ids.has(id))) return null;
  }
  return consistent(value.ties, value.routes) ? { ties: value.ties, routes: value.routes, showOriginal: value.showOriginal } : null;
}

export function readStoredRaw(scope: ViewScope): string | null {
  try {
    return storage()?.getItem(campaignViewKey(scope)) ?? null;
  } catch {
    return null;
  }
}

/** A damaged or other-version value falls back to the original lines and is left in place until the next save. */
export function loadCampaignView(scope: ViewScope, raw = readStoredRaw(scope)): StoredView {
  if (raw === null) return { view: NO_TIES, raw, recovered: false };
  const view = parseView(raw, scope);
  return view ? { view, raw, recovered: false } : { view: NO_TIES, raw, recovered: true };
}

export function saveCampaignView(scope: ViewScope, view: GraphViewSettings): SaveResult {
  const target = storage();
  if (!target) return { ok: false, reason: "unavailable" };
  const value: CampaignGraphView = { schemaVersion: 1, scope, ties: view.ties, routes: view.routes, showOriginal: view.showOriginal };
  const raw = JSON.stringify(value);
  try {
    target.setItem(campaignViewKey(scope), raw);
    return { ok: true, raw };
  } catch (error) {
    return { ok: false, reason: error instanceof Error ? error.name : "write failed" };
  }
}

/**
 * Removes members that left `present` (the full definition) from the stored view and saves it, so a dependency
 * deleted while the graph was closed is not tied again when it comes back. Only for a definition this tab just
 * loaded from the server, never for a value another tab wrote.
 */
export function pruneStoredView(scope: ViewScope, present: readonly RawEdgeKey[]): void {
  const raw = readStoredRaw(scope);
  const stored = loadCampaignView(scope, raw);
  if (raw === null || stored.recovered) return;
  const view = pruneTies(stored.view, present);
  if (JSON.stringify(view) !== JSON.stringify(stored.view)) saveCampaignView(scope, view);
}

/** Campaign ids that have a saved group-graph layout in this browser. */
export function storedGroupViewCampaigns(): string[] {
  const target = storage();
  const ids: string[] = [];
  try {
    for (let index = 0; target && index < target.length; index++) {
      const name = target.key(index);
      if (!name?.startsWith(KEY_PREFIX)) continue;
      const [mode, campaignId, projection, taskScope] = JSON.parse(name.slice(KEY_PREFIX.length)) as unknown[];
      if (typeof campaignId === "string" && campaignViewKey(groupViewScope(campaignId)) === campaignViewKey({ mode, campaignId, projection, taskScope } as ViewScope)) ids.push(campaignId);
    }
  } catch {
    // An unreadable storage or a foreign key under the prefix leaves the list as found so far.
  }
  return ids;
}

/** Fires when another tab changes this scope's value. */
export function subscribeCampaignView(scope: ViewScope, listener: () => void): () => void {
  return subscribeToLocalStorageKey(campaignViewKey(scope), listener);
}
