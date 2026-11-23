import type { Campaign, CampaignNode } from "../../api/campaigns";
import { campaignGroup, campaignTaskNumber, campaignTaskTitle, safeCampaignLink } from "./campaignModel";
import type { GraphViewSettings } from "./campaignGraphRouting";

/** What the editor draws: one group, or a task with its dependencies and dependents up to `depth` steps out. */
export type EditScope = { kind: "group"; key: string } | { kind: "task"; id: string; depth: number };
export const SCOPE_LIMIT = 120;
export const MAX_DEPTH = 3;

/** The ledger fields the editor changes; status, stage and the rest stay as the server has them. */
export type EditField = "dependencies" | "title" | "issue_url";
export type Edits = Record<string, Partial<Pick<CampaignNode, EditField>>>;
export type Conflict = { id: string; field: EditField | "missing" | "locked" };

const clampDepth = (value: number) => Number.isInteger(value) ? Math.min(MAX_DEPTH, Math.max(1, value)) : 1;

export function parseScope(scope: string | null, depth: string | null): EditScope | null {
  if (scope?.startsWith("group:")) return { kind: "group", key: scope.slice("group:".length) };
  if (scope?.startsWith("task:") && scope.length > "task:".length) return { kind: "task", id: scope.slice("task:".length), depth: clampDepth(Number(depth)) };
  return null;
}

export const scopeParams = (scope: EditScope): Record<string, string> =>
  scope.kind === "group" ? { scope: `group:${scope.key}` } : { scope: `task:${scope.id}`, depth: String(scope.depth) };
/** Names a scope for its saved ties: tie positions belong to the drawing of exactly this scope. */
export const scopeKey = (scope: EditScope) => scope.kind === "group" ? `group:${scope.key}` : `task:${scope.id}:${scope.depth}`;

/** Unsaved ties per scope key: the stored view the draft started from, and the draft. */
export type TieDrafts = Record<string, { base: GraphViewSettings; view: GraphViewSettings }>;

/** Ties added, removed, or moved or changed in membership, counted against the stored view each draft started from. */
export function tieChanges(drafts: TieDrafts): number {
  let count = 0;
  for (const { base, view } of Object.values(drafts)) {
    const before = new Map(base.ties.map((tie) => [tie.id, JSON.stringify(tie)]));
    const after = new Map(view.ties.map((tie) => [tie.id, JSON.stringify(tie)]));
    for (const id of new Set([...before.keys(), ...after.keys()])) if (before.get(id) !== after.get(id)) count++;
  }
  return count;
}

/** Completed and skipped tasks keep their own fields and dependencies; a line out of them may still change. */
export const isLocked = (node: CampaignNode | undefined) => node?.status === "completed" || node?.status === "skipped";

export const taskLabel = (node: CampaignNode) => campaignTaskNumber(node) ?? node.id;
export const taskName = (node: CampaignNode) => `${taskLabel(node)} ${campaignTaskTitle(node)}`;

/** The scope's tasks in draw order, the chosen task first, at most SCOPE_LIMIT; `total` counts them all. */
export function scopeTasks(nodes: CampaignNode[], scope: EditScope): { ids: string[]; total: number } {
  let ids: string[] = [];
  if (scope.kind === "group") ids = nodes.filter((node) => campaignGroup(node) === scope.key).map((node) => node.id);
  else {
    const byId = new Map(nodes.map((node) => [node.id, node]));
    const dependents = new Map<string, string[]>();
    for (const node of nodes) for (const id of node.dependencies) dependents.set(id, [...dependents.get(id) ?? [], node.id]);
    // Upstream and downstream each keep their own visited set, so a dependency's other dependents (siblings) stay out
    // and a task reached upstream through a cycle is still walked downstream.
    const walk = (next: (id: string) => string[]) => {
      const seen = new Set([scope.id]);
      for (let step = 0, frontier = [scope.id]; step < scope.depth; step++) frontier = frontier.flatMap(next).filter((id) => byId.has(id) && !seen.has(id) && !!seen.add(id));
      return seen;
    };
    if (byId.has(scope.id)) ids = [...new Set([...walk((id) => byId.get(id)?.dependencies ?? []), ...walk((id) => dependents.get(id) ?? [])])];
  }
  return { ids: ids.slice(0, SCOPE_LIMIT), total: ids.length };
}

export const NODE_WIDTH = 240;
const COLUMN = NODE_WIDTH + 90;
const ROW = 160;

/** Columns by dependency depth inside the scope, wrapped into sub-columns so a flat group stays near square. Each sub-column
 * is centred on one line, so a chosen task alone in its column sits level with the middle of its neighbours. */
export function layoutTasks(nodes: CampaignNode[], ids: string[]): Map<string, { x: number; y: number }> {
  const inScope = new Set(ids);
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const depth = new Map(ids.map((id) => [id, 0]));
  // Longest path; the depth cap ends the passes when the draft holds a cycle.
  for (let pass = 0, moved = true; moved && pass < ids.length; pass++) {
    moved = false;
    for (const id of ids) for (const dep of byId.get(id)?.dependencies ?? []) {
      const next = Math.min(ids.length, (depth.get(dep) ?? -1) + 1);
      if (inScope.has(dep) && dep !== id && next > depth.get(id)!) { depth.set(id, next); moved = true; }
    }
  }
  const rows = Math.max(5, Math.ceil(Math.sqrt(ids.length)));
  const layers = [...new Set(depth.values())].sort((a, b) => a - b);
  const placed = new Map<string, { x: number; y: number }>();
  let column = 0;
  for (const layer of layers) {
    const members = ids.filter((id) => depth.get(id) === layer);
    members.forEach((id, index) => {
      const sub = Math.floor(index / rows), height = Math.min(rows, members.length - sub * rows);
      placed.set(id, { x: (column + sub) * COLUMN, y: (index % rows - (height - 1) / 2) * ROW });
    });
    column += Math.ceil(members.length / rows);
  }
  return placed;
}

type DependencyGraph = ReadonlyArray<{ id: string; dependencies: readonly string[] }>;

/** Tarjan's strongly connected components that hold a loop (a self-dependency counts), each with the task it closed on. */
function loopedParts(nodes: DependencyGraph) {
  const next = new Map<string, string[]>(nodes.map((node) => [node.id, []]));
  for (const node of nodes) for (const id of node.dependencies) next.get(id)?.push(node.id);
  let counter = 0;
  const index = new Map<string, number>(), low = new Map<string, number>(), stack: string[] = [], open = new Set<string>(), parts: { root: string; members: Set<string> }[] = [];
  const visit = (id: string) => {
    index.set(id, counter); low.set(id, counter++); stack.push(id); open.add(id);
    for (const to of next.get(id)!) {
      if (!index.has(to)) { visit(to); low.set(id, Math.min(low.get(id)!, low.get(to)!)); }
      else if (open.has(to)) low.set(id, Math.min(low.get(id)!, index.get(to)!));
    }
    if (low.get(id) !== index.get(id)) return;
    const component = new Set<string>();
    for (let top = ""; top !== id;) { top = stack.pop()!; open.delete(top); component.add(top); }
    if (component.size > 1 || next.get(id)!.includes(id)) parts.push({ root: id, members: component });
  };
  for (const node of nodes) if (!index.has(node.id)) visit(node.id);
  return { next, parts };
}

/** Each dependency cycle once, as ids from a task back to itself in dependency → dependent order; a self-dependency counts. */
export function findCycles(nodes: DependencyGraph): string[][] {
  const { next, parts } = loopedParts(nodes);
  return parts.map(({ root, members }) => shortestLoop(root, members, next));
}

/** The members of each part `findCycles` takes a loop from, in the same order: every line between two of them lies on a cycle. */
export const cycleComponents = (nodes: DependencyGraph): Set<string>[] => loopedParts(nodes).parts.map((part) => part.members);

function shortestLoop(start: string, component: Set<string>, next: Map<string, string[]>): string[] {
  const parent = new Map<string, string>();
  const queue = [start];
  for (let head = 0; head < queue.length; head++) {
    const id = queue[head];
    for (const to of next.get(id)!) {
      if (to === start) {
        const path = [start];
        for (let at = id; at !== start; at = parent.get(at)!) path.splice(1, 0, at);
        return [...path, start];
      }
      if (component.has(to) && !parent.has(to)) { parent.set(to, id); queue.push(to); }
    }
  }
  return [start, start];
}

const same = (field: EditField, a: unknown, b: unknown) => field === "dependencies"
  ? [...a as string[]].sort().join("\n") === [...b as string[]].sort().join("\n")
  : (a || null) === (b || null);

export const applyEdits = (nodes: CampaignNode[], edits: Edits) => nodes.map((node) => edits[node.id] ? { ...node, ...edits[node.id] } : node);

/** Sets one field of one task; a value back at the base drops the edit, so the unsaved count stays true. */
export function editField(edits: Edits, base: CampaignNode | undefined, id: string, field: EditField, value: CampaignNode[EditField]): Edits {
  const node: Edits[string] = { ...edits[id], [field]: value };
  if (base && same(field, base[field], value)) delete node[field];
  const next = { ...edits, [id]: node };
  if (Object.keys(node).length === 0) delete next[id];
  return next;
}

export function editCounts(edits: Edits): Record<EditField, number> {
  const counts = { dependencies: 0, title: 0, issue_url: 0 };
  for (const changes of Object.values(edits)) for (const field of Object.keys(changes) as EditField[]) counts[field]++;
  return counts;
}

/** Changed fields the server would refuse or the screen cannot open; an untouched field stays as the ledger keeps it. */
export function fieldErrors(node: CampaignNode, changed: Edits[string] = {}): Set<"title" | "issue_url"> {
  const errors = new Set<"title" | "issue_url">();
  if ("title" in changed && (!node.title.trim() || new TextEncoder().encode(node.title).length > 512)) errors.add("title");
  if ("issue_url" in changed && node.issue_url && !safeCampaignLink(node.issue_url)) errors.add("issue_url");
  return errors;
}

/**
 * Puts the edits on the newest copy, field by field: a field moves only while the server still holds the base value
 * (or already holds the edited one). A task that is gone or now locked, a field the server changed too, or a
 * dependency on a task that is gone is a conflict, and nothing is sent.
 */
export function rebaseEdits(base: CampaignNode[], edits: Edits, latest: CampaignNode[]): { nodes: CampaignNode[] } | { conflicts: Conflict[] } {
  const before = new Map(base.map((node) => [node.id, node]));
  const now = new Map(latest.map((node) => [node.id, node]));
  const conflicts: Conflict[] = [];
  for (const [id, changes] of Object.entries(edits)) {
    const from = before.get(id), to = now.get(id);
    if (!from || !to) { conflicts.push({ id, field: "missing" }); continue; }
    if (isLocked(to)) { conflicts.push({ id, field: "locked" }); continue; }
    for (const field of Object.keys(changes) as EditField[]) {
      const moved = !same(field, from[field], to[field]) && !same(field, to[field], changes[field]);
      if (moved || (field === "dependencies" && changes.dependencies!.some((dep) => !now.has(dep)))) conflicts.push({ id, field });
    }
  }
  return conflicts.length ? { conflicts } : { nodes: applyEdits(latest, edits) };
}

/** Takes the newest copy as the base: conflicting edits are dropped, the rest stay as unsaved changes on top of it. */
export function adoptLatest(latest: Campaign, edits: Edits, conflicts: Conflict[]): Edits {
  const now = new Map(latest.nodes.map((node) => [node.id, node]));
  let next: Edits = {};
  for (const [id, changes] of Object.entries(edits)) {
    for (const [field, value] of Object.entries(changes) as [EditField, CampaignNode[EditField]][]) {
      if (now.has(id) && !conflicts.some((conflict) => conflict.id === id && [field, "missing", "locked"].includes(conflict.field))) next = editField(next, now.get(id), id, field, value);
    }
  }
  return next;
}

export const warnBeforeUnload = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; };

/** Unsaved edits and ties kept for this tab's session when the editor goes away without a decision (palette, shortcut,
 * another page). They live only in memory, so while any is kept a reload or close asks first, whatever page the tab shows. */
type KeptDraft = { base: Campaign; edits: Edits; ties: TieDrafts };
const kept = new Map<string, KeptDraft>();
// Its own listener, not the editor's: a listener is one per function, so sharing one would let either side's removal
// drop the other's warning (a restored draft, or another campaign's editor saving while a draft is kept).
const keptGuard = (event: BeforeUnloadEvent) => warnBeforeUnload(event);
const guard = () => kept.size ? window.addEventListener("beforeunload", keptGuard) : window.removeEventListener("beforeunload", keptGuard);
export const keepDraft = (draft: KeptDraft) => { kept.set(draft.base.id, draft); guard(); };
export const keptDraft = (campaignId: string) => kept.get(campaignId) ?? null;
export const dropKeptDraft = (campaignId: string) => { kept.delete(campaignId); guard(); };
