import { rawEdgeId, type CableTie, type DisplayEdge } from "./campaignGraphRouting";

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
