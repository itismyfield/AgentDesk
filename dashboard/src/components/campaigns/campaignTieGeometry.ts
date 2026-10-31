import { rawEdgeId, type CableTie, type DisplayEdge } from "./campaignGraphRouting";

export type Point = { x: number; y: number };
export type Box = Point & { width: number; height: number };

/** Tie handle size in graph units; the box stays at least 44 units for touch. */
export const TIE_WIDTH = 44;
const TIE_PAD = 10;
const TIE_SLOT = 12;
export const tieHeight = (slots: number) => Math.max(44, TIE_PAD * 2 + slots * TIE_SLOT);
export const tieSlotTop = (slot: number) => TIE_PAD + slot * TIE_SLOT + TIE_SLOT / 2;

/** Slot of every drawn line through each tie, ordered by the tie's stored strand order. */
export function tieSlots(ties: readonly CableTie[], lines: readonly DisplayEdge[]): Map<string, string[]> {
  const slots = new Map<string, string[]>();
  for (const tie of ties) {
    const order = new Map(tie.strandOrder.map((edge, index) => [rawEdgeId(edge), index]));
    const rank = (line: DisplayEdge) => Math.min(...line.edges.map((edge) => order.get(rawEdgeId(edge)) ?? Infinity));
    slots.set(tie.id, lines.filter((line) => line.via.includes(tie.id)).sort((a, b) => rank(a) - rank(b)).map((line) => line.id));
  }
  return slots;
}

/** The drawn path of a line: straight through its ties, an orthogonal step when untied. */
export function linePath(line: DisplayEdge, boxes: ReadonlyMap<string, Box>, ties: ReadonlyMap<string, CableTie>, slots: ReadonlyMap<string, string[]>): Point[] {
  const source = boxes.get(line.source);
  const target = boxes.get(line.target);
  if (!source || !target) return [];
  const start = { x: source.x + source.width, y: source.y + source.height / 2 };
  const end = { x: target.x, y: target.y + target.height / 2 };
  const via = line.via.flatMap((id) => {
    const tie = ties.get(id);
    const slot = slots.get(id)?.indexOf(line.id) ?? -1;
    if (!tie || slot < 0) return [];
    const y = tie.position.y + tieSlotTop(slot);
    return [{ x: tie.position.x, y }, { x: tie.position.x + TIE_WIDTH, y }];
  });
  if (via.length > 0) return [start, ...via, end];
  const middle = (start.x + end.x) / 2;
  return [start, { x: middle, y: start.y }, { x: middle, y: end.y }, end];
}

function cross(a: Point, b: Point, c: Point) {
  return (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
}

function segmentsIntersect(a: Point, b: Point, c: Point, d: Point): boolean {
  const d1 = cross(c, d, a), d2 = cross(c, d, b), d3 = cross(a, b, c), d4 = cross(a, b, d);
  return ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0));
}

/** Whether a stroke from `a` to `b` crosses the drawn path. */
export function strokeCrosses(path: readonly Point[], a: Point, b: Point): boolean {
  return path.some((point, index) => index > 0 && segmentsIntersect(path[index - 1], point, a, b));
}

/** Middle of a drawn path, used to place a tie created from the connection list. */
export function pathMiddle(path: readonly Point[]): Point | null {
  if (path.length === 0) return null;
  const first = path[0], last = path[path.length - 1];
  return { x: (first.x + last.x) / 2, y: (first.y + last.y) / 2 };
}
