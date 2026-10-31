/** A dependency of the full definition, pointing from the dependency to its dependent. */
export type RawEdgeKey = readonly [sourceNodeId: string, targetNodeId: string];

export type CableTie = {
  id: string;
  position: { x: number; y: number };
  /** Strand order through the tie as the user left it; never re-derived from positions. */
  strandOrder: RawEdgeKey[];
};

/** The ties one raw edge passes through, in order. */
export type EdgeRoute = {
  edge: RawEdgeKey;
  via: string[];
};

/** Which drawing a view belongs to: tie positions only mean something inside that drawing. */
export type ViewScope = { mode: "legacy" | "server"; campaignId: string; projection: "groups"; taskScope: string };

export type CampaignGraphView = {
  schemaVersion: 1;
  scope: ViewScope;
  ties: CableTie[];
  routes: EdgeRoute[];
  showOriginal: boolean;
  /** Definition revision that judged the tie membership; absent in values saved before it was recorded. */
  revision?: number;
};

export type GraphViewSettings = Pick<CampaignGraphView, "ties" | "routes" | "showOriginal">;
export const NO_TIES: GraphViewSettings = { ties: [], routes: [], showOriginal: false };

/** Every raw edge of the full definition and the drawn node each task sits in. */
export type GraphProjection = {
  edges: readonly RawEdgeKey[];
  /** A task missing here is hidden by a filter, a collapse or a screen limit, not deleted. */
  displayOf: ReadonlyMap<string, string>;
};

/** One drawn line and the raw edges behind it; it carries no status of its own. */
export type DisplayEdge = {
  id: string;
  source: string;
  target: string;
  via: string[];
  edges: RawEdgeKey[];
};

export type RoutedGraph = {
  /** Every raw edge of the definition with its tie path ([] when untied). */
  routes: EdgeRoute[];
  display: DisplayEdge[];
  /** Both ends in one drawn node: summarized there, not drawn. */
  internal: RawEdgeKey[];
  /** Still in the definition but not drawn; their ties and routes are kept. */
  hidden: RawEdgeKey[];
  /** Stored edges that left the definition, and the settings without them. */
  removed: RawEdgeKey[];
  settings: GraphViewSettings;
};

/** Collision-free identity of a raw edge; "a:b" + "c" and "a" + "b:c" stay distinct. */
export function rawEdgeId(edge: RawEdgeKey): string {
  return JSON.stringify([edge[0], edge[1]]);
}

/** Each dependency between two existing tasks once, dependency first. */
export function campaignRawEdges(nodes: ReadonlyArray<{ id: string; dependencies: readonly string[] }>): RawEdgeKey[] {
  const ids = new Set(nodes.map((node) => node.id));
  const edges = new Map<string, RawEdgeKey>();
  for (const node of nodes) {
    for (const dependency of node.dependencies) {
      const edge: RawEdgeKey = [dependency, node.id];
      if (ids.has(dependency)) edges.set(rawEdgeId(edge), edges.get(rawEdgeId(edge)) ?? edge);
    }
  }
  return [...edges.values()];
}

/**
 * Routes every raw edge through its stored ties and groups edges that share drawn ends and path into one line.
 * Pure: the definition and the stored settings are read, never written.
 */
export function routeEdges(projection: GraphProjection, settings: GraphViewSettings): RoutedGraph {
  const present = new Map(projection.edges.map((edge) => [rawEdgeId(edge), edge]));
  const stored = new Map(settings.routes.map((route) => [rawEdgeId(route.edge), route.via]));
  const removed = new Map<string, RawEdgeKey>();
  const keep = (edge: RawEdgeKey) => present.has(rawEdgeId(edge)) || (removed.set(rawEdgeId(edge), edge), false);
  const kept: GraphViewSettings = {
    ties: settings.ties.map((tie) => ({ ...tie, strandOrder: tie.strandOrder.filter(keep) })),
    routes: settings.routes.filter((route) => keep(route.edge)),
    showOriginal: settings.showOriginal,
  };
  const routes = [...present.values()].map((edge) => ({ edge, via: [...(stored.get(rawEdgeId(edge)) ?? [])] }));
  const display = new Map<string, DisplayEdge>();
  const internal: RawEdgeKey[] = [];
  const hidden: RawEdgeKey[] = [];
  for (const { edge, via } of routes) {
    const source = projection.displayOf.get(edge[0]);
    const target = projection.displayOf.get(edge[1]);
    if (source === undefined || target === undefined) hidden.push(edge);
    else if (source === target) internal.push(edge);
    else {
      const id = JSON.stringify(settings.showOriginal ? [source, target, via, rawEdgeId(edge)] : [source, target, via]);
      const line = display.get(id) ?? { id, source, target, via, edges: [] };
      line.edges.push(edge);
      display.set(id, line);
    }
  }
  return { routes, display: [...display.values()], internal, hidden, removed: [...removed.values()], settings: kept };
}
