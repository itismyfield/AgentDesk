import type { HTMLAttributes, RefObject } from "react";
import { BaseEdge, EdgeLabelRenderer, getBezierPath, useStore, type EdgeProps, type FitViewOptions, type ReactFlowInstance } from "@xyflow/react";
import { GRAPH_DIM_OPACITY, GRAPH_FLOAT_CLASS, GRAPH_TOOL_CLASS, GraphZoomTools, type MinimapToggleProps } from "../common/graph/GraphChrome";
import type { Tr } from "./campaignPresentation";

/** Floating canvas chrome: a card over the dots, and 40px tool buttons as in the design. */
export const FLOAT = GRAPH_FLOAT_CLASS;
export const TOOL = GRAPH_TOOL_CLASS;
/** Zoomed out past 75%, a group card shows only its name and state, sized to stay legible. */
export const FAR = 1 / 0.75;

/** Below 100%, the factor that keeps canvas words and lines at their own size on screen; 1 from 100% up. */
export const useLegible = () => useStore((state) => Math.max(1, 1 / state.transform[2]));

/** The key at the canvas's bottom left: a dependency line, a cycle line and a cable tie. */
export function RelationsLegend({ className = "", tr, ...rest }: HTMLAttributes<HTMLDivElement> & { tr: Tr }) {
  return <div {...rest} className={`${FLOAT} bottom-4 left-4 flex flex-wrap items-center gap-x-3.5 gap-y-1 px-3 py-2 text-xs text-[var(--fg-muted)] ${className}`}>
    <span className="flex items-center gap-1.5"><i aria-hidden className="w-5 border-t border-[var(--fg-muted)]" />{tr("선행", "Dependency")}</span>
    <span className="flex items-center gap-1.5"><i aria-hidden className="w-5 border-t-2 border-dashed border-[var(--err)]" />{tr("순환 오류", "Cycle")}</span>
    <span className="flex items-center gap-1.5"><i aria-hidden className="size-2.5 rounded-full border-2 border-[var(--fg-muted)]" />{tr("케이블타이 · 실행에는 영향 없음", "Cable tie · no effect on runs")}</span>
  </div>;
}

/** Zoom out, zoom in, fit everything, and optional minimap toggle floating at the canvas's bottom right. */
export function ZoomTools({ flow, fit = { padding: 0.08, minZoom: 0.1, maxZoom: 1 }, fitLabel, minimap, className = "", tr, ...rest }: HTMLAttributes<HTMLDivElement> & {
  flow: RefObject<ReactFlowInstance | null>; fit?: FitViewOptions; fitLabel: string; minimap?: MinimapToggleProps; tr: Tr;
}) {
  return <GraphZoomTools {...rest} flow={flow} fit={fit} fitLabel={fitLabel} minimap={minimap} className={className}
    labels={{ zoomOut: tr("축소", "Zoom out"), zoomIn: tr("확대", "Zoom in"), minimap: tr("미니맵", "Minimap") }} />;
}

/** `count` rides the line as a small round badge; `cycle` is the loop's label; `error` draws it red and dashed; `faint`
 * dims a line that leaves the scope; `dim` dims unrelated lines when inspecting a causal path. */
export type LineData = { count?: number; cycle?: string; error: boolean; loopY?: number; faint?: boolean; dim?: boolean; active?: boolean };
const TURN = 40;
/** Half the gap between two columns of groups. */
const GAP_HALF = 32;

/** Where a forward curve (React Flow's bezier, pins facing) crosses into the last gap before its target, so a line that
 * spans several columns keeps its badge clear of the cards it passes. */
function nearTarget(sx: number, sy: number, tx: number, ty: number): [number, number] {
  const reach = (t: number) => sx + (tx - sx) * (1.5 * t * (1 - t) + t ** 3);
  let lo = 0.5;
  for (let step = 0.25; step > 0.001; step /= 2) if (reach(lo + step) < tx - GAP_HALF) lo += step;
  return [reach(lo), sy + (ty - sy) * lo * lo * (3 - 2 * lo)];
}

/** A view-only curve from a 다음 pin to a 선행 pin; its badge and line keep their size on screen when zoomed out. */
export function RelationsLine({ id, sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition, markerEnd, data }: EdgeProps) {
  const { count, cycle, error, loopY, faint, dim, active } = data as LineData;
  const scale = useLegible();
  const [curve, midX, midY] = getBezierPath({ sourceX, sourceY, sourcePosition, targetX, targetY, targetPosition });
  const [path, x, y] = loopY === undefined ? [curve, ...targetX - sourceX > 2 * GAP_HALF ? nearTarget(sourceX, sourceY, targetX, targetY) : [midX, midY]]
    : [`M${sourceX},${sourceY} C${sourceX + TURN},${sourceY} ${sourceX + TURN},${loopY} ${sourceX},${loopY} L${targetX},${loopY} C${targetX - TURN},${loopY} ${targetX - TURN},${targetY} ${targetX},${targetY}`, (sourceX + targetX) / 2, loopY];
  const width = (active && !error ? 2.25 : 1.75) * Math.min(scale, 2.5);
  const stroke = error ? "var(--err)" : active ? "var(--info)" : "var(--fg-muted)";
  return <>
    <BaseEdge id={id} path={path} markerEnd={markerEnd} style={{ stroke, strokeWidth: width, strokeLinecap: "round", strokeDasharray: error ? `${3 * width} ${2.5 * width}` : undefined, opacity: dim ? GRAPH_DIM_OPACITY : faint ? 0.5 : undefined }} />
    {(count !== undefined || cycle) && <EdgeLabelRenderer>
      <div className="pointer-events-none absolute z-[1] grid justify-items-center gap-1" style={{ transform: `translate(-50%, -50%) translate(${x}px, ${y}px) scale(${scale})`, opacity: dim ? GRAPH_DIM_OPACITY : 1 }}>
        {count !== undefined && <span className="h-[18px] min-w-[18px] rounded-full border border-[var(--line)] bg-[var(--bg-3)] px-1.5 text-center text-[11px] leading-4 font-semibold text-[var(--fg-dim)] tabular-nums">{count}</span>}
        {cycle && <span className="rounded-full bg-[var(--err-soft)] px-2 text-xs leading-[22px] font-medium whitespace-nowrap text-[var(--err)] shadow-[0_0_0_4px_var(--bg)]">{cycle}</span>}
      </div>
    </EdgeLabelRenderer>}
  </>;
}
