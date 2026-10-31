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

/** Largest gap between a rounded corner and the segments standing in for it. */
const CURVE_TOLERANCE = 0.1;
/** A stroke this close to a path selects it; it exceeds CURVE_TOLERANCE, so touching the drawn curve always counts. */
const HIT_DISTANCE = 0.25;

/**
 * Points of a React Flow path (M, L and Q commands). Each quadratic corner becomes enough straight pieces to stay
 * within CURVE_TOLERANCE of the curve; its control point is never a vertex.
 */
function svgPoints(d: string): Point[] {
  const points: Point[] = [];
  for (const [, command, args] of d.matchAll(/([MLQ])([^MLQ]*)/g)) {
    const n = args.match(/-?\d+(?:\.\d+)?(?:e-?\d+)?/gi)?.map(Number) ?? [];
    if (command !== "Q") { points.push({ x: n[0], y: n[1] }); continue; }
    const start = points[points.length - 1];
    const control = { x: n[0], y: n[1] }, end = { x: n[2], y: n[3] };
    // A quadratic's chord error over 1/k of its range is |p0 - 2c + p2| / (4k²).
    const bend = Math.hypot(start.x - 2 * control.x + end.x, start.y - 2 * control.y + end.y);
    const pieces = Math.max(1, Math.ceil(Math.sqrt(bend / (4 * CURVE_TOLERANCE))));
    for (let step = 1; step <= pieces; step++) {
      const t = step / pieces, u = 1 - t;
      points.push({ x: u * u * start.x + 2 * u * t * control.x + t * t * end.x, y: u * u * start.y + 2 * u * t * control.y + t * t * end.y });
    }
  }
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

function pointToSegment(point: Point, a: Point, b: Point): number {
  const dx = b.x - a.x, dy = b.y - a.y;
  const length = dx * dx + dy * dy;
  const t = length === 0 ? 0 : Math.max(0, Math.min(1, ((point.x - a.x) * dx + (point.y - a.y) * dy) / length));
  return Math.hypot(point.x - (a.x + t * dx), point.y - (a.y + t * dy));
}

function segmentGap(a: Point, b: Point, c: Point, d: Point): number {
  if (segmentsIntersect(a, b, c, d)) return 0;
  return Math.min(pointToSegment(a, c, d), pointToSegment(b, c, d), pointToSegment(c, a, b), pointToSegment(d, a, b));
}

/** Whether a stroke from `a` to `b` crosses or touches the drawn path. */
export function strokeCrosses(path: readonly Point[], a: Point, b: Point): boolean {
  return path.some((point, index) => index > 0 && segmentGap(path[index - 1], point, a, b) <= HIT_DISTANCE);
}

/** Middle of a drawn path, used to place a tie created from the connection list. */
export function pathMiddle(path: readonly Point[]): Point | null {
  if (path.length === 0) return null;
  const first = path[0], last = path[path.length - 1];
  return { x: (first.x + last.x) / 2, y: (first.y + last.y) / 2 };
}
