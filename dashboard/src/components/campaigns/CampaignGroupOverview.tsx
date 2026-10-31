import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Background, Controls, MarkerType, Position, ReactFlow, type Edge, type Node, type ReactFlowInstance } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { CampaignNode } from "../../api/campaigns";
import { NO_TIES, campaignRawEdges, rawEdgeId, routeEdges, type CableTie, type GraphViewSettings, type RawEdgeKey } from "./campaignGraphRouting";
import { campaignGroup, groupCampaignNodes } from "./campaignModel";
import { TIE_WIDTH, linePath, pathMiddle, strokeCrosses, tieHeight, tieSlots, type Box, type Point } from "./campaignTieGeometry";
import { CampaignTieNode, CrossingOverlay } from "./CampaignTieGraph";
import CampaignTiePanel, { type TieStrand } from "./CampaignTiePanel";
import { useCampaignTieView } from "./useCampaignTieView";
import { groupViewScope } from "./campaignViewStorage";

type Props = {
  campaignId: string;
  nodes: CampaignNode[];
  onSelectGroup: (group: string) => void;
  tr: (ko: string, en: string) => string;
};

/** The group key, not its sort position, names a group, so adding a group renames nothing. */
const groupNodeId = (key: string) => `campaign-group:${encodeURIComponent(key)}`;
const tieNodeId = (id: string) => `campaign-tie:${id}`;
const GROUP_WIDTH = 244;
const GROUP_HEIGHT = 145;
const NODE_TYPES = { campaignTie: CampaignTieNode };
const NONE: ReadonlySet<string> = new Set();

/** A group projection can contain cycles even when the underlying task graph is a DAG. */
export function buildCampaignGroupOverview(nodes: CampaignNode[], settings: GraphViewSettings = NO_TIES) {
  const groups = groupCampaignNodes(nodes).map((group, index) => ({
    ...group,
    id: groupNodeId(group.key),
    position: { x: (index % 3) * 310, y: Math.floor(index / 3) * 210 },
    internalDependencies: 0,
  }));
  const groupsById = new Map(groups.map((group) => [group.id, group]));
  const displayOf = new Map(nodes.map((node) => [node.id, groupNodeId(campaignGroup(node))]));
  const routed = routeEdges({ edges: campaignRawEdges(nodes), displayOf }, settings);
  for (const [, target] of routed.internal) groupsById.get(displayOf.get(target)!)!.internalDependencies++;
  return {
    groups,
    connections: routed.display
      .map((line) => ({ ...line, count: line.edges.length }))
      .sort((a, b) => a.source.localeCompare(b.source) || a.target.localeCompare(b.target)),
    internalDependencies: routed.internal.length,
    /** The stored ties without members that left the definition. */
    ties: routed.settings.ties,
  };
}

export default function CampaignGroupOverview({ campaignId, nodes, onSelectGroup, tr }: Props) {
  const scope = useMemo(() => groupViewScope(campaignId), [campaignId]);
  const rawEdges = useMemo(() => campaignRawEdges(nodes), [nodes]);
  const ties = useCampaignTieView(scope, rawEdges);
  const { setBusy } = ties;
  const [tieMode, setTieMode] = useState(false);
  const [draft, setDraft] = useState<ReadonlySet<string>>(NONE);
  const [stroke, setStroke] = useState<{ ids: ReadonlySet<string>; anchor: Point | null }>({ ids: NONE, anchor: null });
  /** A dragged tie's position until release; only the release becomes an undo step. */
  const [drag, setDrag] = useState<{ tieId: string; position: Point } | null>(null);
  const [stroking, setStroking] = useState(false);
  /** The lines as drawn when a stroke began; preview and release both judge against them. */
  const strokePaths = useRef<{ line: { edges: RawEdgeKey[] }; path: Point[] }[] | null>(null);
  const [selectedTie, setSelectedTie] = useState<string | null>(null);
  const [focused, setFocused] = useState<string | null>(null);
  const flow = useRef<ReactFlowInstance | null>(null);
  useEffect(() => setBusy(drag !== null || stroking || draft.size > 0), [drag, stroking, draft, setBusy]);
  const view = useMemo(() => drag ? { ...ties.view, ties: ties.view.ties.map((tie) => tie.id === drag.tieId ? { ...tie, position: drag.position } : tie) } : ties.view, [ties.view, drag]);
  const overview = useMemo(() => buildCampaignGroupOverview(nodes, view), [nodes, view]);
  const showOriginal = view.showOriginal;
  const shownTies = useMemo(() => showOriginal ? [] : overview.ties.filter((tie) => tie.strandOrder.length > 0), [overview.ties, showOriginal]);
  const selected = shownTies.find((tie) => tie.id === selectedTie);
  /** The chosen original, only while it is still a member of the selected tie. */
  const focusedEdge = selected && focused && selected.strandOrder.some((edge) => rawEdgeId(edge) === focused) ? focused : null;
  /** What the graph lights: lines being chosen in tie mode, else the one chosen original, else the selected tie's members. */
  const highlight = useMemo<ReadonlySet<string>>(() => tieMode ? new Set([...draft, ...stroke.ids])
    : focusedEdge ? new Set([focusedEdge])
      : selected ? new Set(selected.strandOrder.map(rawEdgeId)) : NONE, [tieMode, draft, stroke.ids, focusedEdge, selected]);
  /**
   * A lit original sharing a line with unlit ones is drawn as its own line, so only it lights. In tie mode only
   * the confirmed draft splits lines: a stroke in progress colours lines but never moves them under the pointer.
   */
  const separated = tieMode ? draft : highlight;
  const lines = useMemo(() => overview.connections.flatMap((connection) => {
    const line = showOriginal ? { ...connection, via: [] } : connection;
    const lit = (edges: RawEdgeKey[]) => edges.some((edge) => highlight.has(rawEdgeId(edge)));
    const apart = line.edges.filter((edge) => separated.has(rawEdgeId(edge)));
    if (apart.length === 0 || apart.length === line.edges.length) return [{ ...line, lit: lit(line.edges) }];
    const rest = line.edges.filter((edge) => !separated.has(rawEdgeId(edge)));
    return [{ ...line, id: `${line.id}:lit`, edges: apart, count: apart.length, lit: lit(apart) }, { ...line, edges: rest, count: rest.length, lit: lit(rest) }];
  }), [overview.connections, showOriginal, highlight, separated]);
  const tieById = useMemo(() => new Map<string, CableTie>(shownTies.map((tie) => [tie.id, tie])), [shownTies]);
  const slots = useMemo(() => tieSlots(shownTies, lines), [shownTies, lines]);
  const paths = useCallback(() => {
    const boxes = new Map<string, Box>(overview.groups.map((group) => [group.id, { ...group.position, width: GROUP_WIDTH, height: flow.current?.getNode(group.id)?.measured?.height ?? GROUP_HEIGHT }]));
    return lines.map((line) => ({ line, path: linePath(line, boxes, tieById, slots) }));
  }, [lines, overview.groups, tieById, slots]);

  const titles = useMemo(() => new Map(nodes.map((node) => [node.id, node.title])), [nodes]);
  const tiedCount = useMemo(() => {
    const counts = new Map<string, number>();
    for (const route of view.routes) counts.set(rawEdgeId(route.edge), route.via.length);
    return counts;
  }, [view.routes]);
  const strandOf = useCallback((edge: readonly [string, string]): TieStrand => ({ id: rawEdgeId(edge), from: titles.get(edge[0]) ?? edge[0], to: titles.get(edge[1]) ?? edge[1], ties: tiedCount.get(rawEdgeId(edge)) ?? 0 }), [titles, tiedCount]);
  const strands = useMemo(() => overview.connections.flatMap((line) => line.edges.map(strandOf)), [overview.connections, strandOf]);
  const tieSummaries = useMemo(() => shownTies.map((tie) => {
    const number = view.ties.findIndex((candidate) => candidate.id === tie.id) + 1;
    return { id: tie.id, label: tr(`타이 ${number}`, `Tie ${number}`), strands: tie.strandOrder.map(strandOf) };
  }), [shownTies, view.ties, strandOf, tr]);

  const focusedEnds = new Map<string, string>();
  if (focusedEdge) {
    const [from, to] = (JSON.parse(focusedEdge) as string[]).map((id) => nodes.find((node) => node.id === id));
    if (from) focusedEnds.set(groupNodeId(campaignGroup(from)), tr(`선행 · ${from.title}`, `Dependency · ${from.title}`));
    if (to) focusedEnds.set(groupNodeId(campaignGroup(to)), tr(`후속 · ${to.title}`, `Dependent · ${to.title}`));
  }
  const graphNodes: Node[] = overview.groups.map((group) => {
    const label = group.key || tr("미분류", "Ungrouped");
    const { counts, percent, total } = group.progress;
    return {
      id: group.id,
      position: group.position,
      sourcePosition: Position.Right,
      targetPosition: Position.Left,
      ariaLabel: tr(`${label}, 작업 ${total}개, 완료 ${percent}%, 진행 ${counts.running}, 막힘 ${counts.blocked}`, `${label}, ${total} tasks, ${percent}% completed, ${counts.running} running, ${counts.blocked} blocked`),
      data: {
        group: group.key,
        label: <div style={{ display: "grid", gap: 8, textAlign: "left", fontSize: 12 }}>
          <strong title={label} style={{ overflow: "hidden", whiteSpace: "nowrap", textOverflow: "ellipsis", fontSize: 14 }}>{label}</strong>
          <div className="campaign-row"><span>{tr(`작업 ${total}개`, `${total} tasks`)}</span><span>{counts.completed}/{total} · {percent}%</span></div>
          <progress value={percent} max={100} aria-label={tr(`${label} 완료율`, `${label} completion`)} style={{ width: "100%", height: 6, accentColor: "var(--th-accent-info)" }} />
          <div className="campaign-row"><span>{tr(`진행 ${counts.running}`, `${counts.running} running`)}</span><span>{tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)}</span></div>
          <small className="campaign-muted">{tr(`그룹 내부 연결 ${group.internalDependencies}개`, `${group.internalDependencies} internal dependencies`)}</small>
          {focusedEnds.has(group.id) && <small className="campaign-tie-end">{focusedEnds.get(group.id)}</small>}
        </div>,
      },
      style: { width: GROUP_WIDTH, minHeight: GROUP_HEIGHT, padding: 14, border: `1px solid ${counts.blocked ? "#d97706" : "var(--th-border)"}`, borderRadius: 10, background: "var(--th-card-bg)", color: "var(--th-text-primary)", cursor: "pointer", boxShadow: focusedEnds.has(group.id) ? "0 0 0 2px var(--th-accent-info)" : undefined },
    };
  });
  for (const tie of shownTies) {
    const summary = tieSummaries.find((candidate) => candidate.id === tie.id)!;
    graphNodes.push({
      id: tieNodeId(tie.id), type: "campaignTie", position: tie.position, draggable: true,
      ariaLabel: tr(`${summary.label}, 원본 선 ${tie.strandOrder.length}개. 선택하면 이동·해제할 수 있습니다`, `${summary.label}, ${tie.strandOrder.length} original lines. Select to move or remove`),
      data: { tieId: tie.id, slots: slots.get(tie.id)?.length ?? 0, selected: tie.id === selectedTie },
      style: { width: TIE_WIDTH, height: tieHeight(slots.get(tie.id)?.length ?? 0) },
    });
  }
  const edges: Edge[] = lines.flatMap((line) => {
    const style = { stroke: line.lit ? "var(--th-accent-info)" : "var(--th-text-muted)", strokeWidth: line.lit ? 3 : 1.5 };
    const chain = [line.source, ...line.via.map(tieNodeId), line.target];
    const slotOf = (index: number) => slots.get(line.via[index - 1])?.indexOf(line.id) ?? 0;
    return chain.slice(1).map((target, index) => {
      const last = index === chain.length - 2;
      return {
        id: chain.length > 2 ? `${line.id}#${index}` : line.id,
        source: chain[index],
        target,
        sourceHandle: index > 0 ? `out-${slotOf(index)}` : undefined,
        targetHandle: last ? undefined : `in-${slotOf(index + 1)}`,
        data: { edges: line.edges },
        type: chain.length > 2 ? "straight" : "smoothstep",
        style,
        ...(last ? {
          label: String(line.count),
          ariaLabel: tr(`그룹 간 의존성 ${line.count}개`, `${line.count} dependencies between groups`),
          markerEnd: { type: MarkerType.ArrowClosed, color: "var(--th-text-muted)" },
          labelStyle: { fill: "var(--th-text-primary)", fontWeight: 600, fontSize: 12 },
          labelBgStyle: { fill: "var(--th-card-bg)" },
          labelBgPadding: [6, 3] as [number, number],
          labelBgBorderRadius: 4,
        } : {}),
      };
    });
  });

  const finishDraft = () => { setDraft(NONE); setStroke({ ids: NONE, anchor: null }); };
  const crossed = (from: Point, to: Point) => new Set((strokePaths.current ?? paths()).filter(({ path }) => strokeCrosses(path, from, to)).flatMap(({ line }) => line.edges.map(rawEdgeId)));
  const confirm = () => {
    const chosen = rawEdges.filter((edge) => draft.has(rawEdgeId(edge)));
    const middles = paths().filter(({ line }) => line.edges.some((edge) => draft.has(rawEdgeId(edge)))).map(({ path }) => pathMiddle(path)).filter((point): point is Point => point !== null);
    const center = stroke.anchor ?? (middles.length ? { x: middles.reduce((sum, point) => sum + point.x, 0) / middles.length, y: middles.reduce((sum, point) => sum + point.y, 0) / middles.length } : { x: 0, y: 0 });
    const created = ties.commit({ kind: "create", edges: chosen, position: { x: Math.round(center.x - TIE_WIDTH / 2), y: Math.round(center.y - tieHeight(chosen.length) / 2) } });
    finishDraft();
    setTieMode(false);
    if (created) setSelectedTie(created.ties.at(-1)?.id ?? null);
  };
  const tieOf = (node: Node) => node.type === "campaignTie" ? String(node.data.tieId) : null;

  const crossDependencies = overview.connections.reduce((total, connection) => total + connection.count, 0);
  return <section className="campaign-neighborhood" aria-label={tr("그룹별 전체 관계", "All campaign group relationships")}>
    <div className="campaign-row"><strong>{tr("그룹별 전체 관계", "All campaign group relationships")}</strong><span>{tr(`그룹 ${overview.groups.length}개 · 작업 ${nodes.length}개`, `${overview.groups.length} groups · ${nodes.length} tasks`)}</span></div>
    <p className="campaign-muted">{tr("전체 작업의 그룹 관계입니다. 화살표는 선행 그룹 → 후속 그룹, 숫자는 연결된 의존성 수입니다. 그룹을 선택하면 해당 작업 목록으로 이동합니다.", "Groups across all tasks. Arrows point from dependency to dependent; numbers count the connecting dependencies. Select a group to open its task list.")}</p>
    {nodes.length === 0 ? <p>{tr("표시할 작업이 없습니다.", "No tasks to display.")}</p> : <>
      <CampaignTiePanel tr={tr} strands={strands} ties={tieSummaries} tieMode={tieMode} draft={draft} selectedTie={selected ? selectedTie : null} focused={focused}
        canUndo={ties.canUndo} showOriginal={showOriginal} dragging={drag !== null} status={ties}
        act={{
          toggleMode: () => { finishDraft(); setTieMode(!tieMode); },
          toggleDraft: (id) => setDraft((current) => { const next = new Set(current); if (next.has(id)) next.delete(id); else next.add(id); return next; }),
          confirm, cancel: () => { finishDraft(); setTieMode(false); },
          selectTie: (id) => { setSelectedTie(id); setFocused(null); },
          move: (dx, dy) => selected && ties.commit({ kind: "move", tieId: selected.id, position: { x: selected.position.x + dx, y: selected.position.y + dy } }),
          remove: () => { if (selected) ties.commit({ kind: "remove", tieId: selected.id }); setSelectedTie(null); setFocused(null); },
          clear: () => { ties.commit({ kind: "clear" }); setSelectedTie(null); setFocused(null); },
          undo: ties.undo, showOriginal: ties.setShowOriginal, focus: setFocused, retry: ties.retrySave, adoptPending: ties.adoptPending,
        }} />
      <div className="campaign-graph-scroll"><div className="campaign-graph [&_.react-flow\_\_controls]:shadow-none [&_.react-flow\_\_controls-button]:!bg-th-bg-surface [&_.react-flow\_\_controls-button]:!text-th-text-primary [&_.react-flow\_\_controls-button]:!border-th-border-subtle [&_.react-flow\_\_attribution]:!bg-th-bg-surface [&_.react-flow\_\_attribution_a]:!text-th-text-muted" style={{ height: Math.min(580, Math.max(300, Math.ceil(overview.groups.length / 3) * 210)) }}>
        <ReactFlow nodes={graphNodes} edges={edges} nodeTypes={NODE_TYPES} fitView fitViewOptions={{ padding: 0.18, minZoom: 0.65, maxZoom: 1 }} minZoom={0.65} maxZoom={1.4}
          nodesDraggable={false} nodesConnectable={false} panOnScroll panOnDrag={!tieMode} zoomOnScroll={false}
          onInit={(instance) => { flow.current = instance; }}
          onNodeClick={(_, node) => { const tie = tieOf(node); if (tie) { setSelectedTie(tie); setFocused(null); } else onSelectGroup(String(node.data.group)); }}
          onNodeDragStart={(_, node) => { const tie = tieOf(node); if (tie) setDrag({ tieId: tie, position: node.position }); }}
          onNodeDrag={(_, node) => { const tie = tieOf(node); if (tie) setDrag({ tieId: tie, position: node.position }); }}
          onNodeDragStop={(_, node) => { const tie = tieOf(node); if (!tie) return; ties.commit({ kind: "move", tieId: tie, position: { x: Math.round(node.position.x), y: Math.round(node.position.y) } }); setDrag(null); setSelectedTie(tie); }}>
          <Background /><Controls showInteractive={false} />
          {tieMode && <CrossingOverlay label={tr("선을 가로질러 드래그해 묶을 선 고르기", "Drag across lines to choose them")}
            toFlow={(point) => flow.current?.screenToFlowPosition(point) ?? point} onActive={(active) => { strokePaths.current = active ? paths() : null; setStroking(active); }}
            onStroke={(line, done) => {
              const ids = line ? crossed(line.from, line.to) : NONE;
              if (!done) setStroke({ ids, anchor: null });
              else {
                setDraft((current) => new Set([...current, ...ids]));
                setStroke({ ids: NONE, anchor: line && ids.size ? { x: (line.from.x + line.to.x) / 2, y: (line.from.y + line.to.y) / 2 } : stroke.anchor });
              }
            }} />}
        </ReactFlow>
      </div></div>
      <p className="campaign-muted">{tr(`그룹 간 연결 ${crossDependencies}개 · 내부 연결 ${overview.internalDependencies}개는 각 그룹에 요약됩니다. 드래그·스크롤로 이동하고 확대 버튼으로 조절하세요.`, `${crossDependencies} cross-group dependencies · ${overview.internalDependencies} internal dependencies summarized in their groups. Drag or scroll to pan; use the zoom controls to adjust.`)}</p>
    </>}
  </section>;
}
