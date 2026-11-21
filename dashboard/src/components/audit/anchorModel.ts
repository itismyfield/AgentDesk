import type { AgentAnchor } from "../../api/promptAudit";

type Tr = (ko: string, en: string) => string;
type Anchor = Pick<AgentAnchor, "chars" | "limit" | "overlap_candidates">;

/** An anchor at or past this share of its own limit counts as near it; anchors without a limit are never judged. */
export const ANCHOR_NEAR_LIMIT_RATIO = 0.9;
export const ANCHOR_TITLE_MAX = 60;

export type AnchorFilter = "all" | "limit" | "canon" | "mutual";
export const ANCHOR_FILTERS: AnchorFilter[] = ["all", "limit", "canon", "mutual"];

export function anchorLimitState({ chars, limit }: Anchor): "over" | "near" | null {
  if (limit == null) return null;
  if (chars > limit) return "over";
  return chars >= limit * ANCHOR_NEAR_LIMIT_RATIO ? "near" : null;
}

export const overlapsCanon = (anchor: Anchor) => anchor.overlap_candidates.some((item) => item.source === "canon_file");
export const overlapsAnchor = (anchor: Anchor) => anchor.overlap_candidates.some((item) => item.source === "anchor");

export function anchorMatches(anchor: Anchor, filter: AnchorFilter): boolean {
  if (filter === "limit") return anchorLimitState(anchor) !== null;
  if (filter === "canon") return overlapsCanon(anchor);
  if (filter === "mutual") return overlapsAnchor(anchor);
  return true;
}

export function anchorFilterCounts(anchors: Anchor[]): Record<AnchorFilter, number> {
  const count = (filter: AnchorFilter) => anchors.filter((anchor) => anchorMatches(anchor, filter)).length;
  return { all: anchors.length, limit: count("limit"), canon: count("canon"), mutual: count("mutual") };
}

/** Anchors worth a look: near or past a limit, or overlapping the canon or another anchor. */
export const anchorsToCheck = (anchors: Anchor[]) => anchors.filter((anchor) => ANCHOR_FILTERS.slice(1).some((filter) => anchorMatches(anchor, filter))).length;

export function anchorFilterLabel(filter: AnchorFilter, tr: Tr): string {
  const labels: Record<AnchorFilter, [string, string]> = {
    all: ["전체", "All"],
    limit: ["한도 근접·초과", "Near or over limit"],
    canon: ["정본과 겹침", "Overlaps canon"],
    mutual: ["서로 겹침", "Overlaps another"],
  };
  return tr(...labels[filter]);
}

/** The server sends no title, so a row shows the start of the first non-empty line. */
export function anchorTitle(text: string): string {
  const line = text.split("\n").map((part) => part.trim()).find(Boolean) ?? "";
  return line.length > ANCHOR_TITLE_MAX ? `${line.slice(0, ANCHOR_TITLE_MAX).trimEnd()}…` : line;
}

/** Rows grouped by scope in order of appearance, unscoped last; with no scopes at all one ungrouped list. */
export function anchorGroups<T extends { anchor: Pick<AgentAnchor, "scope"> }>(rows: T[]): Array<{ scope: string | null; rows: T[] }> {
  if (rows.every(({ anchor }) => !anchor.scope)) return [{ scope: null, rows }];
  const scopes = [...new Set(rows.map(({ anchor }) => anchor.scope || null))].sort((a, b) => Number(a === null) - Number(b === null));
  return scopes.map((scope) => ({ scope, rows: rows.filter(({ anchor }) => (anchor.scope || null) === scope) }));
}
