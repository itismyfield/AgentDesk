import { Position, getSmoothStepPath } from "@xyflow/react";
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

/** Points of an SVG path from React Flow's path builders, in order (M/L ends and Q corners). */
function svgPoints(d: string): Point[] {
  const numbers = d.match(/-?\d+(?:\.\d+)?(?:e-?\d+)?/gi)?.map(Number) ?? [];
  const points: Point[] = [];
  for (let index = 0; index + 1 < numbers.length; index += 2) points.push({ x: numbers[index], y: numbers[index + 1] });
  return points;
}

/**
 * The drawn path of a line: straight segments through its ties, otherwise the same smoothstep path React Flow
 * renders for an untied line, so selection follows what is on screen, backward lines included.
 */
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
  const [d] = getSmoothStepPath({ sourceX: start.x, sourceY: start.y, sourcePosition: Position.Right, targetX: end.x, targetY: end.y, targetPosition: Position.Left });
  return svgPoints(d);
}

function cross(a: Point, b: Point, c: Point) {
  return (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
}

const within = (a: Point, b: Point, point: Point) =>
  Math.min(a.x, b.x) <= point.x && point.x <= Math.max(a.x, b.x) && Math.min(a.y, b.y) <= point.y && point.y <= Math.max(a.y, b.y);

/** Proper crossings and touches both count, so a stroke through a joint between two segments selects the line. */
function segmentsIntersect(a: Point, b: Point, c: Point, d: Point): boolean {
  const d1 = cross(c, d, a), d2 = cross(c, d, b), d3 = cross(a, b, c), d4 = cross(a, b, d);
  if (((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0))) return true;
  return (d1 === 0 && within(c, d, a)) || (d2 === 0 && within(c, d, b)) || (d3 === 0 && within(a, b, c)) || (d4 === 0 && within(a, b, d));
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
