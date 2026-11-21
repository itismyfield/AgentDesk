import { ArrowLeft, Cable, LayoutGrid, Maximize, MousePointer2, Plus, Spline, Undo2, X, ZoomIn, ZoomOut } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { Background, ReactFlow, getViewportForBounds, type Edge, type Node, type ReactFlowInstance } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { getCampaignDetail, saveCampaignNodes, type Campaign } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { Dialog } from "../../kit/free/dialog-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Select } from "../common/Select";
import { campaignRawEdges, rawEdgeId, type RawEdgeKey } from "./campaignGraphRouting";
import { campaignTaskTitle, groupCampaignNodes } from "./campaignModel";
import { LEDGER_STATUS_LABELS } from "./campaignOverviewModel";
import { Badge, LABELS, type Tr } from "./campaignPresentation";
import { applyTieCommand, type TieCommand } from "./campaignTieEditor";
import { definitionCleaner, editorViewScope, loadCampaignView, pruneStoredView, saveCampaignView, storedEditorScopes } from "./campaignViewStorage";
import RelationsInspector, { type ScopeOverview } from "./RelationsInspector";
import { RelationsTaskNode, RelationsTieNode, type TaskNodeData, type TieMenu, type TieNodeData } from "./RelationsTaskNode";
import { nodeMarker } from "./StatusMarker";
import {
  MAX_DEPTH, adoptLatest, applyEdits, editCounts, editField, fieldErrors, findCycles, isLocked, keepDraft, layoutTasks, parseScope, rebaseEdits, scopeKey,
  NODE_WIDTH, dropKeptDraft, keptDraft, scopeTasks, tieChanges, warnBeforeUnload, taskLabel, taskName, type Conflict, type EditField, type EditScope, type Edits, type TieDrafts,
} from "./relationsEditorModel";

const NODE_TYPES = { relationsTask: RelationsTaskNode, relationsTie: RelationsTieNode };
const TIE = "cable-tie:";
/** Ties land on the dot grid behind the canvas; a tie node is 44 units square. */
const GRID = 20;
/** How long a clicked line waits for a second click before the overlay inspector opens over it. */
const DOUBLE_CLICK_MS = 400;
const snap = (x: number, y: number) => ({ x: Math.round((x - 22) / GRID) * GRID, y: Math.round((y - 22) / GRID) * GRID });
type Line = { dep: string; id: string };
/** A 409 is sent again this many times, each on a fresh read; the ledger's revision moves every few seconds. */
const RETRIES = 3;
const UNDO_LIMIT = 50;
/** Below this the first view stops fitting every task and centres the chosen one instead: smaller text is not legible. */
const READABLE_ZOOM = 0.8;
const NODE_HEIGHT = 150;
const WIDE = "(min-width: 1280px)";
const FIELD: Record<EditField, readonly [string, string]> = { dependencies: ["선행", "dependencies"], title: ["제목", "title"], issue_url: ["이슈", "issue"] };
/** Floating canvas chrome: a card over the dots, and 40px tool buttons as in the design. */
const FLOAT = "absolute z-10 rounded-xl border border-[var(--line-soft)] bg-[var(--th-card-bg)] shadow-[var(--shadow-sm)]";
const TOOL = "!size-10 [&>span]:!size-[18px] aria-pressed:!bg-[var(--accent-soft)] aria-pressed:!text-[var(--fg)]";

/** True at 1280px and up, where the inspector stays docked beside the canvas. */
function useWide() {
  const [wide, setWide] = useState(() => window.matchMedia(WIDE).matches);
  useEffect(() => {
    const query = window.matchMedia(WIDE);
    const change = () => setWide(query.matches);
    query.addEventListener("change", change);
    return () => query.removeEventListener("change", change);
  }, []);
  return wide;
}

type Selection = { kind: "node"; id: string } | { kind: "edge"; source: string; target: string } | null;
/** Set while there are unsaved changes: asks before running `proceed`. */
export type LeaveGuard = RefObject<((proceed: () => void) => void) | null>;

/** The full-width relations editor over one scope of a ledger campaign: dependencies, titles and issue links. */
export default function CampaignRelationsEditor({ campaign, scope, onScope, onExit, onSaved, leave, tr }: {
  campaign: Campaign; scope: EditScope; onScope: (scope: EditScope) => void; onExit: () => void;
  onSaved: (campaign: Campaign) => void; leave: LeaveGuard; tr: Tr;
}) {
  // Read here, dropped after mount: a kept draft is used once and comes back only if this editor leaves undecided again.
  const [kept] = useState(() => keptDraft(campaign.id));
  const [base, setBase] = useState(kept?.base ?? campaign);
  const [edits, setEdits] = useState<Edits>(kept?.edits ?? {});
  const [restored, setRestored] = useState(kept !== null);
  const [ties, setTies] = useState<TieDrafts>(kept?.ties ?? {});
  /** Field edits and tie changes share one undo stack, so undo walks back through both in the order they were made. */
  const [undo, setUndo] = useState<{ edits: Edits; ties: TieDrafts }[]>([]);
  /** The field being typed into, so one run of typing is one undo step. */
  const typing = useRef<string | null>(null);
  const wide = useWide();
  // Docked, the chosen task starts selected; as an overlay the inspector waits for a choice.
  const [selection, setSelection] = useState<Selection>(scope.kind === "task" && wide ? { kind: "node", id: scope.id } : null);
  const root = useRef<HTMLElement>(null);
  const canvas = useRef<HTMLDivElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const scopeBar = useRef<HTMLDivElement>(null);
  const flow = useRef<ReactFlowInstance | null>(null);
  const [tool, setTool] = useState<"select" | "connect" | "tie">("select");
  const [from, setFrom] = useState<string | null>(null);
  /** Lines chosen with the tie tool, as raw edge ids; the chosen tie, its original-line preview and its drag. */
  const [bundle, setBundle] = useState<string[]>([]);
  const [chosenTie, setChosenTie] = useState<string | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [drag, setDrag] = useState<{ id: string; position: { x: number; y: number } } | null>(null);
  const [tieReads, setTieReads] = useState(0);
  // Sizes React Flow measured, handed back on each node: without them every render re-measures and redraws the lines,
  // which drops the focus from a line just chosen with the keyboard or the mouse.
  const [measured, setMeasured] = useState<Record<string, { width: number; height: number }>>({});
  const [arranged, setArranged] = useState<Campaign["nodes"] | null>(null);
  const [waiting, setWaiting] = useState(false);
  const clickWait = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(clickWait.current), []);
  const [saving, setSaving] = useState(false);
  const [problem, setProblem] = useState<{ text: string; conflicts?: Conflict[]; latest?: Campaign } | null>(null);
  const [leaving, setLeaving] = useState<(() => void) | null>(null);

  const nodes = useMemo(() => applyEdits(base.nodes, edits), [base, edits]);
  const byId = useMemo(() => new Map(nodes.map((node) => [node.id, node])), [nodes]);
  const baseById = useMemo(() => new Map(base.nodes.map((node) => [node.id, node])), [base]);
  const groups = useMemo(() => groupCampaignNodes(base.nodes), [base]);
  const { ids, total } = useMemo(() => scopeTasks(nodes, scope), [nodes, scope]);
  const inScope = new Set(ids);
  const positions = useMemo(() => layoutTasks(arranged ?? base.nodes, ids), [arranged, base, ids]);
  const cycles = useMemo(() => findCycles(nodes), [nodes]);
  const counts = editCounts(edits);
  const changed = counts.dependencies + counts.title + counts.issue_url;
  // Ties are saved per scope in this browser, apart from the ledger; a draft holds the scopes touched so far.
  const key = scopeKey(scope);
  const stored = useMemo(() => loadCampaignView(editorViewScope(campaign.id, key)), [campaign.id, key, tieReads]); // eslint-disable-line react-hooks/exhaustive-deps -- read again after each save
  const tieView = ties[key]?.view ?? stored.view;
  const tieCount = tieChanges(ties);
  const pending = changed + tieCount;
  const dirty = pending > 0;
  const invalid = Object.keys(edits).flatMap((id) => [...fieldErrors(byId.get(id)!, edits[id])].map((field) => ({ id, field })));
  const errors = cycles.length + invalid.length;
  const label = (id: string) => byId.get(id) ? taskLabel(byId.get(id)!) : id;
  const cycleText = (path: string[]) => tr(`순환: ${path.map(label).join(" → ")}`, `Cycle: ${path.map(label).join(" → ")}`);
  const lockOf = (id: string) => !isLocked(baseById.get(id)) ? null
    : baseById.get(id)!.status === "completed" ? tr("완료 · 바꿀 수 없음", "Completed · cannot change") : tr("건너뜀 · 바꿀 수 없음", "Skipped · cannot change");
  const parts = [[counts.dependencies, "선행", "dependencies"], [counts.title, "제목", "titles"], [counts.issue_url, "이슈", "issues"], [tieCount, "케이블타이", "cable ties"]] as const;
  const summary = parts.filter(([count]) => count > 0).map(([count, ko, en]) => tr(`${ko} ${count}`, `${count} ${en}`)).join(" · ");

  /** A new base takes the undo steps with it: they were drafted against the old one and would undo the server's changes. */
  const reset = (next: Campaign, nextEdits: Edits) => { setBase(next); setEdits(nextEdits); setUndo([]); typing.current = null; };
  // With nothing unsaved, a newer copy from the page's refresh becomes the base, so the screen stays current.
  useEffect(() => { if (!changed && campaign.revision > base.revision) { setBase(campaign); setUndo([]); typing.current = null; } }, [campaign, base.revision, changed]);
  /** Tasks of a cycle the merge brought in: once drawn, the view moves so both ends show. */
  const [reveal, setReveal] = useState<string[] | null>(null);
  useEffect(() => {
    const shown = reveal?.filter((id) => inScope.has(id)) ?? [];
    if (shown.length && flow.current) void flow.current.fitView({ nodes: shown.map((id) => ({ id })), padding: 0.3, minZoom: 0.1, maxZoom: 1, duration: 200 });
    if (reveal) setReveal(null);
  }, [reveal]); // eslint-disable-line react-hooks/exhaustive-deps -- runs once per merge, on the tasks drawn after it

  /** Every change goes through here: one undo step, and the save problem it may have answered is cleared. Nothing
   * changes while a save is out, so what it sent is what the screen shows when it lands. */
  const commit = (update: (current: Edits) => Edits, run: string | null = null) => {
    if (saving) return;
    const next = update(edits);
    if (next === edits) return;
    if (run === null || run !== typing.current) setUndo((stack) => [...stack, { edits, ties }].slice(-UNDO_LIMIT));
    typing.current = run;
    setEdits(next); setProblem(null); setRestored(false);
  };
  /** Every tie change, under the same rules: one undo step, and nothing while a save is out. */
  const tieCommit = (command: TieCommand) => {
    if (saving) return null;
    const next = applyTieCommand(tieView, command);
    if (next === tieView) return null;
    setUndo((stack) => [...stack, { edits, ties }].slice(-UNDO_LIMIT)); typing.current = null;
    setTies({ ...ties, [key]: { base: ties[key]?.base ?? stored.view, view: next } }); setProblem(null); setRestored(false);
    return next;
  };
  /** The one place dependencies change, so a locked task refuses the canvas, the inspector and line removal alike. */
  const withDeps = (current: Edits, id: string, change: (deps: string[]) => string[]) => {
    const node = baseById.get(id);
    if (!node || isLocked(node)) return current;
    const deps = current[id]?.dependencies ?? node.dependencies;
    const next = change(deps);
    return next.join("\n") === deps.join("\n") ? current : editField(current, node, id, "dependencies", next);
  };
  const link = (dependency: string, id: string) => commit((current) => withDeps(current, id, (deps) => deps.includes(dependency) ? deps : [...deps, dependency]));
  const unlink = (pairs: [dependency: string, id: string][]) => commit((current) => pairs.reduce((acc, [dependency, id]) => withDeps(acc, id, (deps) => deps.filter((dep) => dep !== dependency)), current));
  const setText = (id: string, field: "title" | "issue_url", value: string) => {
    const node = baseById.get(id);
    if (node && !isLocked(node)) commit((current) => editField(current, node, id, field, field === "issue_url" ? value.trim() || null : value), `${id}:${field}`);
  };
  const stepBack = () => {
    const previous = undo.at(-1);
    if (!previous || saving) return;
    // A conflict notice judged the draft before this step, so its list and its adopt button go with it.
    setUndo((stack) => stack.slice(0, -1)); setEdits(previous.edits); setTies(previous.ties); setProblem(null); typing.current = null;
  };

  /** Reads the newest copy, puts the edits on it and sends it; a 409 starts over from a fresh read. Null: nothing sent. */
  const saveNodes = async (): Promise<Campaign | null> => {
    for (let attempt = 0; ; attempt++) {
      const { campaign: latest } = await getCampaignDetail(base.id);
      const merged = rebaseEdits(base.nodes, edits, latest.nodes);
      if ("conflicts" in merged) {
        const what = { ...FIELD, missing: ["사라짐", "gone"], locked: ["잠김", "locked"] } as const;
        const list = merged.conflicts.map(({ id, field }) => `${label(id)} ${tr(what[field][0], what[field][1])}`).join(" · ");
        setProblem({ text: tr(`다른 곳에서도 바뀌어 저장하지 않았어요: ${list}`, `Changed elsewhere too, so nothing was saved: ${list}`), conflicts: merged.conflicts, latest });
        return null;
      }
      // A cycle only the merge makes is brought onto the screen: the newest copy becomes the base with the draft on top.
      const loops = findCycles(merged.nodes);
      if (loops.length) {
        reset(latest, adoptLatest(latest, edits, [])); setReveal(loops[0]);
        setProblem({ text: tr(`최신본과 합치면 순환이 생겨 저장하지 않았어요. 최신본을 불러오고 고친 내용은 그대로 두었어요. ${cycleText(loops[0])}`, `Merged with the newest copy this makes a cycle, so nothing was saved. The newest copy is loaded and your edits are kept. ${cycleText(loops[0])}`) });
        return null;
      }
      try {
        const saved = await saveCampaignNodes(latest, merged.nodes);
        onSaved(saved); reset(saved, {});
        return saved;
      } catch (cause) {
        if (!(cause instanceof ApiRequestError && cause.status === 409 && attempt < RETRIES)) throw cause;
      }
    }
  };
  /** Ties are written only once the ledger holds the definition they are judged by, members it lacks dropped; saved
   * ties of scopes without a draft are cleaned against it too. A draft the browser refuses stays unsaved. */
  const writeTies = (definition: Campaign) => {
    const present = campaignRawEdges(definition.nodes);
    const failed = Object.entries(ties).filter(([name, draft]) => {
      const target = editorViewScope(campaign.id, name);
      const recorded = loadCampaignView(target).revision;
      const clean = definitionCleaner(present, definition.revision, recorded);
      return !saveCampaignView(target, clean ? clean(draft.view) : draft.view, Math.max(definition.revision, recorded ?? 0)).ok;
    });
    for (const target of storedEditorScopes(campaign.id)) if (!ties[target.taskScope]) pruneStoredView(target, present, definition.revision);
    // Undo steps lose only the scopes now saved, and steps that then change nothing; a refused draft keeps its steps.
    const left = Object.fromEntries(failed), saved = (name: string) => !(name in left) && name in ties;
    const now = JSON.stringify({ edits, ties: left });
    setUndo((stack) => stack.map((step) => ({ edits: step.edits, ties: Object.fromEntries(Object.entries(step.ties).filter(([name]) => !saved(name))) }))
      .filter((step, index, all) => JSON.stringify(step) !== (index + 1 < all.length ? JSON.stringify(all[index + 1]) : now)));
    setTies(left); typing.current = null; setTieReads((count) => count + 1);
    if (failed.length) setProblem({ text: tr("케이블타이를 저장하지 못했어요. 다시 저장해 보세요.", "Could not save the cable ties. Try saving again.") });
    return failed.length === 0;
  };
  /** The ledger first (only when a field changed), then the ties; a refused or conflicting write leaves the ties unsaved. */
  const save = async (): Promise<boolean> => {
    setSaving(true); setProblem(null);
    try {
      const saved = changed ? await saveNodes() : base;
      return saved !== null && writeTies(saved);
    } catch (cause) {
      const busy = cause instanceof ApiRequestError && cause.status === 409;
      setProblem({ text: busy ? tr("캠페인이 계속 바뀌고 있어요. 잠시 뒤 다시 저장해 보세요.", "The campaign keeps changing. Try saving again shortly.")
        : cause instanceof Error ? cause.message : tr("저장하지 못했어요.", "Could not save.") });
      return false;
    } finally { setSaving(false); }
  };
  const adopt = (latest: Campaign, conflicts: Conflict[]) => { reset(latest, adoptLatest(latest, edits, conflicts)); setProblem(null); };
  const discard = () => { reset(campaign, {}); setTies({}); setRestored(false); };

  // Leaving with unsaved changes: the parent asks through `leave`, a reload or close through beforeunload, and an
  // unmount without a decision (palette, shortcut) keeps the draft, ties included, for this tab's session.
  const keep = useRef<{ base: Campaign; edits: Edits; ties: TieDrafts } | null>(null);
  keep.current = dirty ? { base, edits, ties } : null;
  const request = useCallback((proceed: () => void) => keep.current ? setLeaving(() => proceed) : proceed(), []);
  useEffect(() => {
    leave.current = dirty ? request : null;
    return () => { leave.current = null; };
  }, [dirty, request, leave]);
  useEffect(() => {
    if (!dirty) return;
    window.addEventListener("beforeunload", warnBeforeUnload);
    return () => window.removeEventListener("beforeunload", warnBeforeUnload);
  }, [dirty]);
  useEffect(() => { dropKeptDraft(campaign.id); return () => { if (keep.current) keepDraft(keep.current); }; }, [campaign.id]);
  const decide = async (choice: "discard" | "save") => {
    const proceed = leaving;
    if (choice === "save" && !(await save())) { setLeaving(null); return; }
    keep.current = null; setLeaving(null); proceed?.();
  };

  const pick = (id: string) => {
    if (id.startsWith(TIE)) { setChosenTie(id.slice(TIE.length)); setSelection(null); return; }
    setChosenTie(null); setPreviewing(false);
    if (tool !== "connect") return setSelection({ kind: "node", id });
    if (from && from !== id) link(from, id);
    setFrom(from ? null : id);
  };
  const cycleEdges = new Map<string, string[]>();
  for (const path of cycles) path.slice(1).forEach((to, index) => cycleEdges.set(`${path[index]}->${to}`, path));
  const labelled = new Set<string[]>();
  const graphNodes: Node[] = ids.map((id) => {
    const node = byId.get(id)!;
    const outside = node.dependencies.filter((dep) => !inScope.has(dep)).length;
    const looped = cycles.some((path) => path.includes(id));
    const bad = invalid.some((entry) => entry.id === id);
    const lock = lockOf(id);
    const data: TaskNodeData = {
      title: campaignTaskTitle(node), meta: `${taskLabel(node)} · ${node.group || tr("미분류", "Ungrouped")}`, ...nodeMarker(node.status, node.stage),
      pins: [tr("선행", "Needs"), tr("다음", "Next")],
      summary: [tr(`수용조건 ${node.acceptance.length}`, `${node.acceptance.length} acceptance`), outside > 0 && tr(`범위 밖 선행 ${outside}`, `${outside} outside the scope`)].filter(Boolean).join(" · "),
      note: looped ? { tone: "err", text: tr("순환 오류", "Cycle") } : bad ? { tone: "err", text: tr("입력 오류", "Invalid field") } : lock ? { tone: "lock", text: lock } : null,
      locked: lock !== null, error: looped || bad, picked: from === id,
    };
    return { id, type: "relationsTask", position: positions.get(id) ?? { x: 0, y: 0 }, measured: measured[id], data, draggable: false, deletable: false,
      selected: tool === "select" && selection?.kind === "node" && selection.id === id, ariaLabel: `${taskName(node)}, ${tr(...LABELS[node.status])}` };
  });
  // A tied line runs through its ties in stored order; its first piece keeps the plain id, so focus and checks find it.
  const routeOf = new Map(tieView.routes.map((route) => [rawEdgeId(route.edge), route.via]));
  const tieById = new Map(tieView.ties.map((tie) => [tie.id, tie]));
  const through = new Map<string, RawEdgeKey[]>();
  const edges: Edge[] = ids.flatMap((id) => byId.get(id)!.dependencies.filter((dep) => inScope.has(dep)).flatMap((dep) => {
    const via = (routeOf.get(rawEdgeId([dep, id])) ?? []).filter((tie) => tieById.has(tie));
    via.forEach((tie) => through.set(tie, [...through.get(tie) ?? [], [dep, id]]));
    const path = cycleEdges.get(`${dep}->${id}`);
    const chosen = tool !== "tie" && selection?.kind === "edge" && selection.source === dep && selection.target === id;
    const lit = chosen || (tool === "tie" && bundle.includes(rawEdgeId([dep, id]))) || (chosenTie !== null && via.includes(chosenTie));
    const first = path && !labelled.has(path);
    if (path) labelled.add(path);
    const chain = [dep, ...via.map((tie) => TIE + tie), id];
    return chain.slice(1).map((target, index) => ({
      id: index ? `${dep}->${id}#${index}` : `${dep}->${id}`, source: chain[index], target, data: { dep, id } satisfies Line, selected: chosen, deletable: lockOf(id) === null,
      ariaLabel: tr(`선행 ${label(dep)} → ${label(id)}`, `Dependency ${label(dep)} → ${label(id)}`),
      style: path ? { stroke: "var(--err)", strokeWidth: 2, strokeDasharray: "6 4" } : { stroke: lit ? "var(--info)" : "var(--fg-muted)", strokeWidth: lit ? 2.5 : 1.5 },
      ...first && !index ? { label: cycleText(path), labelStyle: { fill: "var(--err)", fontSize: 12 }, labelBgStyle: { fill: "var(--th-card-bg)" }, labelBgPadding: [6, 3] as [number, number], labelBgBorderRadius: 6 } : {},
    }));
  }));
  const chosenLines = bundle.map((edge) => JSON.parse(edge) as RawEdgeKey).filter(([dep, id]) => inScope.has(dep) && !!byId.get(id)?.dependencies.includes(dep) && inScope.has(id));
  const activeTie = chosenTie && through.has(chosenTie) ? chosenTie : null;
  const untie = (tieId: string) => { tieCommit({ kind: "remove", tieId }); setChosenTie(null); setPreviewing(false); };
  const made = (view: ReturnType<typeof tieCommit>) => {
    if (!view) return;
    const id = view.ties.at(-1)!.id;
    setChosenTie(id); setSelection(null); setBundle([]);
    // The new tie takes the focus so its menu keys answer at once; it is drawn a frame or two after this change.
    requestAnimationFrame(() => requestAnimationFrame(() => root.current?.querySelector<HTMLElement>(`.react-flow__node[data-id="${TIE + id}"]`)?.focus()));
  };
  /** Ties the chosen lines where their middles meet; a line's middle is halfway between its two pins. */
  const tieBundle = () => {
    const middles = chosenLines.map(([dep, id]) => ({ x: (positions.get(dep)!.x + NODE_WIDTH + positions.get(id)!.x) / 2, y: (positions.get(dep)!.y + positions.get(id)!.y + NODE_HEIGHT) / 2 }));
    if (!middles.length) return;
    made(tieCommit({ kind: "create", edges: chosenLines, position: snap(middles.reduce((sum, point) => sum + point.x, 0) / middles.length, middles.reduce((sum, point) => sum + point.y, 0) / middles.length) }));
    setTool("select");
  };
  // Original-line preview: the chosen tie's lines also drawn on their own paths, dashed.
  if (activeTie && previewing) edges.push(...through.get(activeTie)!.map(([dep, id]) => ({ id: `preview:${dep}->${id}`, source: dep, target: id, selectable: false, focusable: false, deletable: false, style: { stroke: "var(--fg-muted)", strokeDasharray: "4 4" } })));
  for (const [tieId, lines] of through) {
    const menu: TieMenu | null = tieId !== activeTie || drag ? null : {
      labels: { menu: tr("케이블타이 메뉴", "Cable tie menu"), remove: tr("타이 풀기", "Untie"), detach: tr("선 하나만 빼기", "Take one line out"), preview: tr("원래 선 미리 보기", "Preview original lines") },
      strands: lines.map(([dep, id]) => ({ key: rawEdgeId([dep, id]), label: `${label(dep)} → ${label(id)}` })), previewing,
      act: { remove: () => untie(tieId), detach: (edge) => tieCommit({ kind: "detach", tieId, edge: JSON.parse(edge) as RawEdgeKey }), preview: () => setPreviewing(!previewing) },
    };
    graphNodes.push({ id: TIE + tieId, type: "relationsTie", position: drag?.id === tieId ? drag.position : tieById.get(tieId)!.position, measured: measured[TIE + tieId], draggable: !saving, deletable: false,
      selected: tieId === activeTie, ariaLabel: tr(`케이블타이, 선 ${lines.length}개`, `Cable tie, ${lines.length} lines`), data: { count: lines.length, menu } satisfies TieNodeData });
  }
  const chosen = selection?.kind === "node" ? byId.get(selection.id) ?? null : null;
  const problems = [...cycles.map(cycleText), ...invalid.map(({ id, field }) => `${label(id)} ${field === "title" ? tr("제목", "title") : tr("이슈 주소", "issue link")}`)];
  const done = ids.filter((id) => byId.get(id)!.status === "completed").length;
  const finished = nodes.filter((node) => node.status === "completed").length;
  /** First view: every task while that stays legible, else the chosen task (or the scope's start) at a legible zoom. */
  const firstView = (instance: ReactFlowInstance) => {
    flow.current = instance;
    const box = canvas.current?.getBoundingClientRect();
    const placed = ids.flatMap((id) => positions.get(id) ?? []);
    if (!box || placed.length === 0) return;
    const left = Math.min(...placed.map((point) => point.x)), top = Math.min(...placed.map((point) => point.y));
    const bounds = { x: left, y: top, width: Math.max(...placed.map((point) => point.x)) + NODE_WIDTH - left, height: Math.max(...placed.map((point) => point.y)) + NODE_HEIGHT - top };
    // The scope bar and the tool row float over the canvas, so tasks are fitted into the band between them.
    const above = (scopeBar.current?.offsetHeight ?? 0) + 12, band = Math.max(box.height - above - 64, 1);
    const all = getViewportForBounds(bounds, box.width, band, 0.1, 1, 0.08);
    const focus = scope.kind === "task" ? positions.get(scope.id) : undefined;
    if (all.zoom >= READABLE_ZOOM) void instance.setViewport({ ...all, y: all.y + above });
    else if (focus) void instance.setViewport({ x: box.width / 2 - (focus.x + NODE_WIDTH / 2) * READABLE_ZOOM, y: above + band / 2 - (focus.y + NODE_HEIGHT / 2) * READABLE_ZOOM, zoom: READABLE_ZOOM });
    else void instance.setViewport({ x: 24 - left * READABLE_ZOOM, y: above + 12 - top * READABLE_ZOOM, zoom: READABLE_ZOOM });
  };
  /** Closing the overlay inspector drops the choice and puts the focus back on the task or line it showed. */
  const closeInspector = () => {
    const target = selection?.kind === "node" ? `.react-flow__node[data-id="${selection.id}"]`
      : selection ? `.react-flow__edge[data-id="${selection.source}->${selection.target}"]` : null;
    setSelection(null);
    const element = target ? root.current?.querySelector<HTMLElement>(target) : null;
    // React Flow blurs a task on the frame after Escape unselects it, so the focus is set again after that.
    element?.focus();
    requestAnimationFrame(() => requestAnimationFrame(() => element?.focus()));
  };
  // The overlay covers the canvas's right side, so a chosen task under it slides left into view.
  useEffect(() => {
    if (wide || selection?.kind !== "node" || !flow.current) return;
    const task = root.current?.querySelector(`.react-flow__node[data-id="${selection.id}"]`)?.getBoundingClientRect();
    const hidden = task && panel.current ? task.right + 16 - panel.current.getBoundingClientRect().left : 0;
    const view = flow.current.getViewport();
    if (hidden > 0) void flow.current.setViewport({ ...view, x: view.x - hidden }, { duration: 200 });
  }, [selection, wide]);
  // With nothing chosen the docked column sums up the scope: its tasks, the checks and every unsaved change.
  const overview: ScopeOverview = {
    name: scope.kind === "group" ? tr(`그룹 · ${scope.key || "미분류"}`, `Group · ${scope.key || "Ungrouped"}`) : tr(`작업 주변 · ${label(scope.id)} · ${scope.depth}단계`, `Around ${label(scope.id)} · ${scope.depth} step${scope.depth > 1 ? "s" : ""}`),
    meta: tr(`작업 ${ids.length}개 · 완료 ${done}`, `${ids.length} tasks · ${done} done`),
    changes: Object.entries(edits).map(([id, fields]) => ({ id, name: byId.get(id) ? taskName(byId.get(id)!) : id,
      fields: (Object.keys(fields) as EditField[]).map((field) => tr(...FIELD[field])).join(" · "), open: inScope.has(id) ? () => setSelection({ kind: "node", id }) : undefined })),
    ties: tieCount,
  };
  const inspector = <RelationsInspector node={chosen} edge={selection?.kind === "edge" ? selection : null} lock={selection ? lockOf(selection.kind === "node" ? selection.id : selection.target) : null}
    nodes={byId} cycles={cycles} errors={new Set(invalid.filter((entry) => entry.id === chosen?.id).map((entry) => entry.field))} problems={problems} overview={overview} cycleText={cycleText} busy={saving} tr={tr}
    act={{
      field: (field, value) => chosen && setText(chosen.id, field, value),
      add: (dependency) => chosen && link(dependency, chosen.id),
      remove: (dependency) => chosen && unlink([[dependency, chosen.id]]),
      unlink: () => { if (selection?.kind === "edge") { unlink([[selection.source, selection.target]]); setSelection(null); } },
      focus: () => chosen && onScope({ kind: "task", id: chosen.id, depth: 1 }),
    }} />;

  return <section ref={root} aria-label={tr("작업 관계도 편집", "Edit task relations")}
    className="campaign-relations-editor grid h-full min-h-0 min-w-0 grid-rows-[auto_auto_minmax(0,1fr)] overflow-hidden bg-[var(--th-bg-primary)]"
    onKeyDown={(event) => {
      if (!event.currentTarget.contains(event.target as HTMLElement)) return;
      // The tie keys answer only on the canvas and its chrome, never while typing; Escape closes whatever is open.
      const typing = !!(event.target as HTMLElement).closest("input, textarea, select") || event.metaKey || event.ctrlKey || event.altKey;
      const letter = event.key.toLowerCase();
      if (!typing && activeTie && (event.key === "Delete" || event.key === "Backspace")) { event.preventDefault(); untie(activeTie); }
      else if (!typing && activeTie && letter === "h") setPreviewing(!previewing);
      else if (!typing && tool === "tie" && letter === "b") tieBundle();
      else if (event.key === "Escape" && activeTie) { setChosenTie(null); setPreviewing(false); }
      else if (event.key === "Escape" && !wide && selection) { event.stopPropagation(); closeInspector(); }
    }}>
    <header className="flex min-h-14 min-w-0 flex-wrap items-center gap-x-4 gap-y-2 border-b border-[var(--line-soft)] px-6 py-2">
      <Button variant="ghost" icon={<ArrowLeft aria-hidden className="size-4" />} onClick={() => request(onExit)}>{tr("캠페인", "Campaign")}</Button>
      <div className="grid min-w-0 flex-[0_1_auto] gap-0.5">
        <span className="flex min-w-0 items-center gap-2"><strong className="truncate text-[15px] text-[var(--fg)]">{base.title}</strong><Badge status={base.status} label={tr(...LEDGER_STATUS_LABELS[base.status])} tr={tr} /></span>
        <span className="truncate text-xs text-[var(--fg-muted)] tabular-nums">{tr(`원장 · rev ${base.revision} · 작업 ${nodes.length}개 · 완료 ${finished}`, `Ledger · rev ${base.revision} · ${nodes.length} tasks · ${finished} done`)}</span>
      </div>
      <div className="w-60 shrink-0"><SegmentedControl kind="tabs" variant="tray" aria-label={tr("편집할 관계", "Relation to edit")} value="relations"
        options={[{ value: "relations", label: tr("작업 관계", "Task relations") }, { value: "pipeline", label: tr("파이프라인", "Pipeline"), badge: tr("준비 중", "Coming later"), disabled: true }]} /></div>
      <div className="ml-auto flex flex-wrap items-center gap-2">
        <KitBadge size="md" tone={errors ? "danger" : "success"}>{errors ? tr(`오류 ${errors}`, `${errors} errors`) : tr("검사 통과", "Checks pass")}</KitBadge>
        {dirty && <KitBadge size="md" tone="warning">{tr(`저장 안 됨 · ${summary}`, `Unsaved · ${summary}`)}</KitBadge>}
        <Button variant="secondary" size="lg" icon={<Undo2 aria-hidden className="size-4" />} disabled={!undo.length || saving} onClick={stepBack}>{tr("되돌리기", "Undo")}</Button>
        <Button variant="primary" size="lg" loading={saving} disabled={!dirty || errors > 0}
          title={errors ? tr("오류를 고쳐야 저장돼요", "Fix the errors to save") : undefined} onClick={() => void save()}>{tr("저장", "Save")}</Button>
      </div>
    </header>
    <div>
      {problem && <div role="alert" className="flex flex-wrap items-center gap-2 border-b border-[var(--line)] bg-[color-mix(in_oklch,var(--err)_12%,transparent)] px-4 py-2 text-sm text-[var(--fg)]">
        <span className="min-w-0 flex-1">{problem.text}</span>
        {problem.latest && problem.conflicts && <Button variant="secondary" size="sm" onClick={() => adopt(problem.latest!, problem.conflicts!)}>{tr("바뀐 칸은 최신값으로 두기", "Keep the newest values there")}</Button>}
      </div>}
      {restored && <div role="status" className="flex flex-wrap items-center gap-2 border-b border-[var(--line)] px-4 py-2 text-sm text-[var(--fg)]">
        <span className="min-w-0 flex-1">{tr("저장하지 않은 편집을 다시 불러왔어요.", "Your unsaved edits are back.")}</span>
        <Button variant="ghost" size="sm" disabled={saving} onClick={discard}>{tr("버리기", "Discard")}</Button>
      </div>}
    </div>
    <div className={`relative grid min-h-0 ${wide ? "grid-cols-[minmax(0,1fr)_22.5rem]" : ""}`}>
      <div ref={canvas} data-editor-canvas className="relative min-h-0 min-w-0">
        <ReactFlow key={JSON.stringify(scope)} nodes={graphNodes} edges={edges} nodeTypes={NODE_TYPES} onInit={firstView} proOptions={{ hideAttribution: true }}
          minZoom={0.1} maxZoom={1.5} nodesDraggable={false} panOnScroll zoomOnScroll={false} zoomOnDoubleClick={false} snapToGrid snapGrid={[GRID, GRID]}
          nodesConnectable={!saving} deleteKeyCode={saving ? null : ["Delete", "Backspace"]}
          onConnect={({ source, target }) => source && target && link(source, target)}
          isValidConnection={({ target }) => lockOf(target) === null}
          // Selection arrives as a change for a click and for Enter on a focused task, tie or line alike.
          onNodesChange={(changes) => {
            const sized = changes.flatMap((change) => change.type === "dimensions" && change.dimensions ? [[change.id, change.dimensions] as const] : []);
            if (sized.length) setMeasured((current) => ({ ...current, ...Object.fromEntries(sized) }));
            const picked = changes.find((change) => change.type === "select" && change.selected); if (picked && "id" in picked) pick(picked.id);
          }}
          onEdgesChange={(changes) => {
            const line = edges.find((edge) => changes.some((change) => change.type === "select" && change.selected && change.id === edge.id))?.data as Line | undefined;
            if (!line) return;
            const raw = rawEdgeId([line.dep, line.id]);
            if (tool === "tie") setBundle(bundle.includes(raw) ? bundle.filter((edge) => edge !== raw) : [...bundle, raw]);
            else { setSelection({ kind: "edge", source: line.dep, target: line.id }); setChosenTie(null); }
          }}
          // Narrower than 1280px a clicked line is lit at once, but the overlay waits out a double-click that would land on it.
          onEdgeClick={() => { if (wide || tool === "tie") return; clearTimeout(clickWait.current); setWaiting(true); clickWait.current = setTimeout(() => setWaiting(false), DOUBLE_CLICK_MS); }}
          onEdgeDoubleClick={(event, edge) => {
            clearTimeout(clickWait.current); setWaiting(false);
            const line = edge.data as Line | undefined, point = flow.current?.screenToFlowPosition({ x: event.clientX, y: event.clientY });
            if (line && point) made(tieCommit({ kind: "create", edges: [[line.dep, line.id]], position: snap(point.x, point.y) }));
          }}
          onNodeDrag={(_, node) => { if (node.id.startsWith(TIE)) setDrag({ id: node.id.slice(TIE.length), position: node.position }); }}
          onNodeDragStop={(_, node) => {
            if (!node.id.startsWith(TIE)) return;
            tieCommit({ kind: "move", tieId: node.id.slice(TIE.length), position: snap(node.position.x + 22, node.position.y + 22) });
            setDrag(null); setChosenTie(node.id.slice(TIE.length));
          }}
          onPaneClick={() => { setSelection(null); setFrom(null); setChosenTie(null); setPreviewing(false); }}
          onEdgesDelete={(deleted) => { unlink(deleted.flatMap((edge) => edge.data ? [[(edge.data as Line).dep, (edge.data as Line).id] as [string, string]] : [])); setSelection(null); }}>
          <Background />
        </ReactFlow>
        <div ref={scopeBar} data-editor-chrome="scope" className={`${FLOAT} top-3 left-3 flex max-w-[calc(100%-1.5rem)] flex-wrap items-center gap-x-3 gap-y-1.5 p-1.5`}>
          <Select aria-label={tr("편집 범위", "Scope")} className="!min-h-9 !py-1.5" wrapperClassName="w-64 max-w-full" value={scope.kind === "task" ? `task:${scope.id}` : `group:${scope.key}`}
            onChange={(event) => { const next = parseScope(event.target.value, "1"); if (next) onScope(next); }}>
            {scope.kind === "task" && <option value={`task:${scope.id}`}>{tr(`작업 주변 · ${label(scope.id)}`, `Around ${label(scope.id)}`)}</option>}
            {groups.map((group) => <option key={group.key} value={`group:${group.key}`}>{tr(`그룹 · ${group.key || "미분류"} (${group.nodes.length})`, `Group · ${group.key || "Ungrouped"} (${group.nodes.length})`)}</option>)}
          </Select>
          {scope.kind === "task" && <Select aria-label={tr("단계", "Steps")} className="!min-h-9 !py-1.5" wrapperClassName="w-28 shrink-0" value={scope.depth} onChange={(event) => onScope({ ...scope, depth: Number(event.target.value) })}>
            {Array.from({ length: MAX_DEPTH }, (_, index) => <option key={index} value={index + 1}>{tr(`${index + 1}단계`, `${index + 1} step${index ? "s" : ""}`)}</option>)}
          </Select>}
          {total > ids.length && <span role="status" className="text-xs text-[var(--warn)]">{tr(`작업 ${total}개 중 ${ids.length}개만 그려요. 작업 주변으로 좁혀 보세요.`, `Drawing ${ids.length} of ${total} tasks. Narrow it to one task's neighbors.`)}</span>}
          {ids.length === 0 && <span role="status" className="text-xs text-[var(--fg-muted)]">{tr("이 범위에 작업이 없어요.", "No tasks in this scope.")}</span>}
          {tool === "connect" && <span role="status" className="text-xs text-[var(--fg)]">{from ? tr(`${label(from)} 다음에 올 작업을 누르세요.`, `Choose the task that comes after ${label(from)}.`) : tr("선행이 될 작업을 먼저 누르세요.", "First choose the task to depend on.")}</span>}
          {tool === "tie" && <span role="status" className="text-xs text-[var(--fg)]">{chosenLines.length ? tr(`선 ${chosenLines.length}개를 골랐어요.`, `${chosenLines.length} lines chosen.`) : tr("묶을 선을 누르세요. 선 하나는 두 번 눌러도 그 자리에 묶여요.", "Choose the lines to tie. Double-click one line to tie it where it is.")}</span>}
          {tool === "tie" && chosenLines.length > 0 && <Button variant="primary" size="sm" disabled={saving} onClick={tieBundle}>{tr("묶기", "Tie")}<kbd className="ml-1.5 rounded border border-current/40 px-1 font-mono text-[11px]">B</kbd></Button>}
        </div>
        <div data-editor-chrome="legend" className={`${FLOAT} bottom-4 left-4 flex max-w-[calc(50%-9rem)] flex-wrap items-center gap-x-3.5 gap-y-1 px-3 py-2 text-xs text-[var(--fg-muted)]`}>
          <span className="flex items-center gap-1.5"><i aria-hidden className="w-5 border-t border-[var(--fg-muted)]" />{tr("선행", "Dependency")}</span>
          <span className="flex items-center gap-1.5"><i aria-hidden className="w-5 border-t-2 border-dashed border-[var(--err)]" />{tr("순환 오류", "Cycle")}</span>
          <span className="flex items-center gap-1.5"><i aria-hidden className="size-2.5 rounded-full border-2 border-[var(--fg-muted)]" />{tr("케이블타이 · 실행에는 영향 없음", "Cable tie · no effect on runs")}</span>
        </div>
        <div data-editor-chrome="tools" role="toolbar" aria-label={tr("편집 도구", "Editing tools")} className={`${FLOAT} bottom-4 left-1/2 flex -translate-x-1/2 gap-0.5 p-1`}>
          <IconButton icon={<MousePointer2 aria-hidden />} className={TOOL} label={tr("선택", "Select")} pressed={tool === "select"} onClick={() => { setTool("select"); setFrom(null); setBundle([]); }} />
          <IconButton icon={<Plus aria-hidden />} className={TOOL} label={tr("작업 추가 · 준비 중", "Add a task · coming later")} disabled />
          <IconButton icon={<Spline aria-hidden />} className={TOOL} label={tr("잇기", "Connect")} pressed={tool === "connect"} onClick={() => { setTool("connect"); setBundle([]); }} />
          <IconButton icon={<Cable aria-hidden />} className={TOOL} label={tr("케이블타이", "Cable ties")} pressed={tool === "tie"} onClick={() => { setTool("tie"); setFrom(null); setBundle([]); }} />
          <IconButton icon={<LayoutGrid aria-hidden />} className={TOOL} label={tr("정렬", "Arrange")} onClick={() => setArranged(nodes)} />
        </div>
        <div data-editor-chrome="zoom" role="group" aria-label={tr("보기 크기", "Zoom")} className={`${FLOAT} right-4 bottom-4 flex gap-0.5 p-1`}>
          <IconButton icon={<ZoomOut aria-hidden />} className={TOOL} label={tr("축소", "Zoom out")} onClick={() => void flow.current?.zoomOut()} />
          <IconButton icon={<ZoomIn aria-hidden />} className={TOOL} label={tr("확대", "Zoom in")} onClick={() => void flow.current?.zoomIn()} />
          <IconButton icon={<Maximize aria-hidden />} className={TOOL} label={tr("전체 보기", "Show every task")} onClick={() => void flow.current?.fitView({ padding: 0.08, minZoom: 0.1, maxZoom: 1 })} />
        </div>
      </div>
      {/* Docked from 1280px; narrower, it opens over the canvas's right side only while a task or line is chosen. */}
      {wide ? <div className="flex min-h-0 flex-col bg-[var(--bg-1)]">{inspector}</div> : selection && !waiting && <div ref={panel} className="absolute inset-y-0 right-0 z-20 flex w-80 max-w-[85%] flex-col bg-[var(--th-card-bg)] shadow-xl">
        <div className="flex justify-end border-b border-l border-[var(--line)] px-2 py-1"><IconButton icon={<X aria-hidden />} label={tr("검사기 닫기", "Close the inspector")} onClick={closeInspector} /></div>
        {inspector}
      </div>}
    </div>
    <Dialog open={leaving !== null} onOpenChange={(open) => { if (!open) setLeaving(null); }} size="md"
      title={tr(`저장하지 않은 변경이 ${pending}건 있어요`, `${pending} unsaved changes`)}
      description={tr(`${summary}. 나가면 이 변경은 사라져요.`, `${summary}. Leaving drops them.`)}
      footer={<>
        <Button variant="secondary" disabled={saving} onClick={() => void decide("discard")}>{tr("버리고 나가기", "Discard and leave")}</Button>
        <Button variant="secondary" onClick={() => setLeaving(null)}>{tr("계속 편집", "Keep editing")}</Button>
        <Button variant="primary" loading={saving} disabled={saving || errors > 0} onClick={() => void decide("save")}>{tr("저장하고 나가기", "Save and leave")}</Button>
      </>} />
  </section>;
}
