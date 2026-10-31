import { NO_TIES, rawEdgeId, type GraphViewSettings, type RawEdgeKey } from "./campaignGraphRouting";

/** A user's tie operation; each committed command is one undo unit. */
export type TieCommand =
  | { kind: "create"; edges: readonly RawEdgeKey[]; position: { x: number; y: number } }
  | { kind: "move"; tieId: string; position: { x: number; y: number } }
  | { kind: "remove"; tieId: string }
  | { kind: "clear" };

/** The committed view and the views before each committed command, oldest first. */
export type TieHistory = { view: GraphViewSettings; undo: GraphViewSettings[] };

export const UNDO_LIMIT = 50;

/**
 * A fresh random id per tie, never derived from the ties present: a removed tie's id coming back would let
 * a stale move from another tab land on a different tie. getRandomValues also works outside secure contexts.
 */
function newTieId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(8));
  return `tie-${[...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

/** Drops members that left the definition and ties left without members; hidden edges stay because `present` is the full definition. */
export function pruneTies(view: GraphViewSettings, present: readonly RawEdgeKey[]): GraphViewSettings {
  const ids = new Set(present.map(rawEdgeId));
  const ties = view.ties
    .map((tie) => ({ ...tie, strandOrder: tie.strandOrder.filter((edge) => ids.has(rawEdgeId(edge))) }))
    .filter((tie) => tie.strandOrder.length > 0);
  const tieIds = new Set(ties.map((tie) => tie.id));
  const routes = view.routes
    .filter((route) => ids.has(rawEdgeId(route.edge)))
    .map((route) => ({ edge: route.edge, via: route.via.filter((id) => tieIds.has(id)) }))
    .filter((route) => route.via.length > 0);
  return { ties, routes, showOriginal: view.showOriginal };
}

/** Applies one command to a view; returns the same view when the command does not apply. */
export function applyTieCommand(view: GraphViewSettings, command: TieCommand): GraphViewSettings {
  switch (command.kind) {
    case "create": {
      const edges = [...new Map(command.edges.map((edge) => [rawEdgeId(edge), [edge[0], edge[1]] as RawEdgeKey])).values()];
      if (edges.length === 0) return view;
      const id = newTieId();
      const selected = new Set(edges.map(rawEdgeId));
      const routes = view.routes.map((route) => selected.has(rawEdgeId(route.edge)) ? { ...route, via: [...route.via, id] } : route);
      const routed = new Set(view.routes.map((route) => rawEdgeId(route.edge)));
      for (const edge of edges) if (!routed.has(rawEdgeId(edge))) routes.push({ edge, via: [id] });
      return { ...view, ties: [...view.ties, { id, position: { ...command.position }, strandOrder: edges }], routes };
    }
    case "move": {
      const tie = view.ties.find((candidate) => candidate.id === command.tieId);
      if (!tie || (tie.position.x === command.position.x && tie.position.y === command.position.y)) return view;
      return { ...view, ties: view.ties.map((candidate) => candidate === tie ? { ...tie, position: { ...command.position } } : candidate) };
    }
    case "remove":
      if (!view.ties.some((tie) => tie.id === command.tieId)) return view;
      return {
        ...view,
        ties: view.ties.filter((tie) => tie.id !== command.tieId),
        routes: view.routes
          .map((route) => ({ ...route, via: route.via.filter((id) => id !== command.tieId) }))
          .filter((route) => route.via.length > 0),
      };
    case "clear":
      return view.ties.length === 0 && view.routes.length === 0 ? view : { ...NO_TIES, showOriginal: view.showOriginal };
  }
}

/** Commits a command on `base` (the latest stored view) and keeps the shown view as the undo point. */
export function commitTie(history: TieHistory, command: TieCommand, base: GraphViewSettings = history.view): TieHistory {
  const view = applyTieCommand(base, command);
  if (view === base) return history;
  return { view, undo: [...history.undo, history.view].slice(-UNDO_LIMIT) };
}

export function undoTie(history: TieHistory): TieHistory {
  const previous = history.undo.at(-1);
  if (!previous) return history;
  return { view: { ...previous, showOriginal: history.view.showOriginal }, undo: history.undo.slice(0, -1) };
}
