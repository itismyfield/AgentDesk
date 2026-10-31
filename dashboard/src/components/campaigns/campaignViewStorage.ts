import { subscribeToLocalStorageKey } from "../../lib/useLocalStorage";
import { NO_TIES, rawEdgeId, type CampaignGraphView, type GraphViewSettings, type RawEdgeKey, type ViewScope } from "./campaignGraphRouting";
import { pruneTies } from "./campaignTieEditor";

/**
 * Tie layout per browser profile, apart from the campaign definition: nothing here reaches the server.
 * useLocalStorage swallows write failures, so this wrapper reports them to the caller.
 */
const KEY_PREFIX = "agentdesk.campaign-graph-view.v1:";

/** `revision` is the definition revision recorded with the value, or null when none was (the lowest). */
export type StoredView = { view: GraphViewSettings; raw: string | null; recovered: boolean; revision: number | null };
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
function parseView(raw: string, scope: ViewScope): { view: GraphViewSettings; revision: number | null } | null {
  let value: Partial<CampaignGraphView>;
  try {
    value = JSON.parse(raw) as Partial<CampaignGraphView>;
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null || value.schemaVersion !== 1 || typeof value.showOriginal !== "boolean") return null;
  if (value.revision !== undefined && !(Number.isSafeInteger(value.revision) && value.revision > 0)) return null;
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
  if (!consistent(value.ties, value.routes)) return null;
  return { view: { ties: value.ties, routes: value.routes, showOriginal: value.showOriginal }, revision: value.revision ?? null };
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
  if (raw === null) return { view: NO_TIES, raw, recovered: false, revision: null };
  const parsed = parseView(raw, scope);
  return parsed ? { ...parsed, raw, recovered: false } : { view: NO_TIES, raw, recovered: true, revision: null };
}

/**
 * The one rule for every persistent cleanup: a definition at `own` may clean a view only when it is not older than
 * the definition (`stored`) that judged the view's membership. Returns the cleaner, or null to hold the view as it
 * is; routeEdges still leaves members this definition lacks out of the drawing.
 */
export function definitionCleaner(present: readonly RawEdgeKey[], own: number, stored: number | null): ((view: GraphViewSettings) => GraphViewSettings) | null {
  return stored === null || own >= stored ? (view) => pruneTies(view, present) : null;
}

/** Saves `view` with the definition revision that judged it; null records none. */
export function saveCampaignView(scope: ViewScope, view: GraphViewSettings, revision: number | null): SaveResult {
  const target = storage();
  if (!target) return { ok: false, reason: "unavailable" };
  const value: CampaignGraphView = { schemaVersion: 1, scope, ties: view.ties, routes: view.routes, showOriginal: view.showOriginal, ...(revision === null ? {} : { revision }) };
  const raw = JSON.stringify(value);
  try {
    target.setItem(campaignViewKey(scope), raw);
    return { ok: true, raw };
  } catch (error) {
    return { ok: false, reason: error instanceof Error ? error.name : "write failed" };
  }
}

/**
 * Removes members that left `present` (the full definition at `revision`) from the stored view and saves it, so a
 * dependency deleted while the graph was closed is not tied again when it comes back. Only for a definition this tab
 * loaded from the server. A null revision means the campaign is gone: no stored view outranks that, and the recorded
 * revision is dropped so an id created again, starting from a lower revision, can clean its own ties.
 */
export function pruneStoredView(scope: ViewScope, present: readonly RawEdgeKey[], revision: number | null): void {
  const raw = readStoredRaw(scope);
  const stored = loadCampaignView(scope, raw);
  if (raw === null || stored.recovered) return;
  const clean = definitionCleaner(present, revision ?? Infinity, stored.revision);
  if (!clean) return;
  const view = clean(stored.view);
  if (JSON.stringify(view) !== JSON.stringify(stored.view) || (revision === null && stored.revision !== null)) saveCampaignView(scope, view, revision);
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
