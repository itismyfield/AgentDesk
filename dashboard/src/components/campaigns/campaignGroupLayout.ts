import type { CableTie, DisplayEdge } from "./campaignGraphRouting";
import { NODE_WIDTH } from "./relationsEditorModel";

/** A group or task card on the relations view in graph units; its pins sit `GROUP_PIN_Y` below its top edge. */
export const GROUP_WIDTH = NODE_WIDTH;
export const GROUP_HEIGHT = 188;
export const TASK_HEIGHT = 164;
export const GROUP_PIN_Y = 82;
/** A column per dependency depth, with room between columns for outer handles, a curve and its count. */
export const LAYER = GROUP_WIDTH + 88;
const ROW_GAP = 24;
/** Cards with no line to another sit closer together, under the linked ones, in rows shaped for a portrait canvas. */
const SHELF = GROUP_WIDTH + 64;
const SHELF_ASPECT = 0.9;
const SECTION_GAP = 72;
/** A line running backwards loops under the linked cards, each in its own lane this far apart. */
const LOOP_LANE = 20;
/** A tie node is 44 units square; its lines meet at its centre. */
const TIE_HALF = 22;

export type Point = { x: number; y: number };

/**
 * Linked groups in columns by dependency depth (longest path; the line that closes a loop is left out), each column
 * ordered by the mean row of its neighbours and centred; groups without a cross-group line fill rows below them, past
 * a lane for each backward line. `below` is where those lanes start. `ids` come in display order, so a campaign lands
 * the same way however its tasks are listed.
 */
export function layoutGroups(ids: readonly string[], links: readonly (readonly [string, string])[], height = GROUP_HEIGHT): { placed: Map<string, Point>; below: number } {
  const pitch = height + ROW_GAP;
  const next = new Map(ids.map((id) => [id, [] as string[]]));
  for (const [from, to] of links) if (from !== to && !next.get(from)!.includes(to)) next.get(from)!.push(to);
  const linked = ids.filter((id) => next.get(id)!.length > 0 || links.some(([from, to]) => from !== to && to === id));
  // Depth first in display order: a line back to a group still open closes a loop and is skipped.
  const state = new Map<string, "open" | "done">(), order: string[] = [], forward: [string, string][] = [];
  const visit = (id: string) => {
    state.set(id, "open");
    for (const to of next.get(id)!) {
      if (state.get(to) === "open") continue;
      forward.push([id, to]);
      if (!state.has(to)) visit(to);
    }
    state.set(id, "done"); order.unshift(id);
  };
  for (const id of linked) if (!state.has(id)) visit(id);
  const depth = new Map(linked.map((id) => [id, 0]));
  for (const id of order) for (const [from, to] of forward) if (from === id) depth.set(to, Math.max(depth.get(to)!, depth.get(id)! + 1));

  const columns: string[][] = [];
  for (const id of linked) (columns[depth.get(id)!] ??= []).push(id);
  const row = new Map<string, number>();
  columns.forEach((column) => column.forEach((id, index) => row.set(id, index)));
  // Down, up and down again: each column sorted by the mean row of its neighbours in the column just swept.
  const sweep = (column: string[], side: 0 | 1) => {
    const near = (id: string) => forward.filter((edge) => edge[1 - side] === id).map((edge) => row.get(edge[side])!);
    const key = new Map(column.map((id) => { const rows = near(id); return [id, rows.length ? rows.reduce((sum, value) => sum + value, 0) / rows.length : row.get(id)!]; }));
    column.sort((a, b) => key.get(a)! - key.get(b)!).forEach((id, index) => row.set(id, index));
  };
  for (const [side, from, to, step] of [[0, 1, columns.length, 1], [1, columns.length - 2, -1, -1], [0, 1, columns.length, 1]] as const) {
    for (let index = from; index !== to && index >= 0 && index < columns.length; index += step) sweep(columns[index], side);
  }

  const placed = new Map<string, Point>();
  const tallest = Math.max(0, ...columns.map((column) => column.length));
  columns.forEach((column, layer) => column.forEach((id, index) => placed.set(id, { x: layer * LAYER, y: (index + (tallest - column.length) / 2) * pitch })));
  const solo = ids.filter((id) => !depth.has(id));
  const width = columns.length ? (columns.length - 1) * LAYER + GROUP_WIDTH : 0;
  const shaped = Math.ceil(Math.sqrt(SHELF_ASPECT * solo.length * pitch / SHELF));
  const perRow = Math.max(1, shaped, columns.length ? Math.round((width + SHELF - GROUP_WIDTH) / SHELF) : 0);
  const below = columns.length ? (tallest - 1) * pitch + height : 0;
  const backward = links.filter(([from, to]) => from !== to && depth.get(to)! <= depth.get(from)!).length;
  const top = columns.length ? below + SECTION_GAP + backward * LOOP_LANE : 0;
  solo.forEach((id, index) => placed.set(id, { x: (index % perRow) * SHELF, y: top + Math.floor(index / perRow) * pitch }));
  return { placed, below };
}

/** Lines over a layout: an untied backwards line gets a loop lane under the linked cards (`loopY`); each tie sits in the
 * stretch all its lines cross, at their mean height, since the position saved with it belonged to another drawing. */
export function placeLines<L extends DisplayEdge>(lines: readonly L[], ties: readonly CableTie[], positions: ReadonlyMap<string, Point>, below: number) {
  let lane = 0;
  const connections = lines.map((line) => ({ ...line, count: line.edges.length,
    loopY: !line.via.length && positions.get(line.target)!.x <= positions.get(line.source)!.x ? below + 28 + LOOP_LANE * lane++ : undefined }));
  const placed = ties.flatMap((tie) => {
    const spans = connections.filter((line) => line.via.includes(tie.id)).map((line) => {
      const from = positions.get(line.source)!, to = positions.get(line.target)!;
      return { x0: from.x + GROUP_WIDTH, y0: from.y + GROUP_PIN_Y, x1: to.x, y1: to.y + GROUP_PIN_Y };
    });
    if (!spans.length) return [];
    const lo = Math.max(...spans.map((span) => span.x0)), hi = Math.min(...spans.map((span) => span.x1));
    const x = lo <= hi ? (lo + hi) / 2 : spans.reduce((sum, span) => sum + (span.x0 + span.x1) / 2, 0) / spans.length;
    const y = spans.reduce((sum, span) => sum + span.y0 + (span.y1 - span.y0) * Math.min(1, Math.max(0, (x - span.x0) / (span.x1 - span.x0 || 1))), 0) / spans.length;
    return [{ ...tie, position: { x: x - TIE_HALF, y: y - TIE_HALF } }];
  });
  return { connections, ties: placed };
}
