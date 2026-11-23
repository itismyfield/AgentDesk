import { NODE_WIDTH } from "./relationsEditorModel";

/** A group card on the relations view in graph units; its pins sit `GROUP_PIN_Y` below its top edge. */
export const GROUP_WIDTH = NODE_WIDTH;
export const GROUP_HEIGHT = 164;
export const GROUP_PIN_Y = 71;
/** A column per dependency depth, with room between columns for a curve and its count. */
const LAYER = GROUP_WIDTH + 64;
const ROW = GROUP_HEIGHT + 16;
/** Groups with no line to another group sit closer together, under the linked ones. */
const SHELF = GROUP_WIDTH + 24;
const SECTION_GAP = 64;
/** A line running backwards loops under the linked groups, each in its own lane this far apart. */
export const LOOP_LANE = 18;

type Point = { x: number; y: number };

/**
 * Linked groups in columns by dependency depth (longest path; the line that closes a loop is left out), each column
 * ordered by the mean row of its neighbours and centred; groups without a cross-group line fill rows below them, past
 * a lane for each backward line. `below` is where those lanes start. `ids` come in display order, so a campaign lands
 * the same way however its tasks are listed.
 */
export function layoutGroups(ids: readonly string[], links: readonly (readonly [string, string])[]): { placed: Map<string, Point>; below: number } {
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
  columns.forEach((column, layer) => column.forEach((id, index) => placed.set(id, { x: layer * LAYER, y: (index + (tallest - column.length) / 2) * ROW })));
  const solo = ids.filter((id) => !depth.has(id));
  const width = columns.length ? (columns.length - 1) * LAYER + GROUP_WIDTH : 0;
  const perRow = Math.max(1, columns.length ? Math.round((width + SHELF - GROUP_WIDTH) / SHELF) : Math.ceil(Math.sqrt(solo.length)));
  const below = columns.length ? (tallest - 1) * ROW + GROUP_HEIGHT : 0;
  const backward = links.filter(([from, to]) => from !== to && depth.get(to)! <= depth.get(from)!).length;
  const top = columns.length ? below + SECTION_GAP + backward * LOOP_LANE : 0;
  solo.forEach((id, index) => placed.set(id, { x: (index % perRow) * SHELF, y: top + Math.floor(index / perRow) * ROW }));
  return { placed, below };
}
