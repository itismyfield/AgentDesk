import { Map as MapIcon, Maximize, ZoomIn, ZoomOut } from "lucide-react";
import type { CSSProperties, HTMLAttributes, RefObject } from "react";
import { MiniMap, type FitViewOptions, type Node, type ReactFlowInstance } from "@xyflow/react";
import { IconButton } from "../../../kit/free/icon-button-clean";
import type { ChipTone } from "../KitPrimitives";

/** Opacity applied to nodes and edges outside the selected/hovered causal chain. */
export const GRAPH_DIM_OPACITY = 0.24;

/** Floating canvas chrome container and 40px icon tool button styles. */
export const GRAPH_FLOAT_CLASS = "absolute z-10 rounded-xl border border-[var(--line-soft)] bg-[var(--th-card-bg)] shadow-[var(--shadow-sm)]";
export const GRAPH_TOOL_CLASS = "min-[900px]:!size-10 max-[899px]:!size-11 [&>span]:!size-[18px] aria-pressed:!bg-[var(--accent-soft)] aria-pressed:!text-[var(--fg)]";

export interface GraphStatusTone {
  accent: string;
  headerBg: string;
  border: string;
  pinColor: string;
  chipTone: ChipTone;
  quiet: boolean;
}

export const GRAPH_STATUS_TONES: Record<string, GraphStatusTone> = {
  running: {
    accent: "var(--info)",
    headerBg: "color-mix(in srgb, var(--info) 9%, transparent)",
    border: "color-mix(in srgb, var(--info) 45%, var(--line))",
    pinColor: "var(--info)",
    chipTone: "info",
    quiet: false,
  },
  blocked: {
    accent: "var(--warn)",
    headerBg: "color-mix(in srgb, var(--warn) 11%, transparent)",
    border: "color-mix(in srgb, var(--warn) 48%, var(--line))",
    pinColor: "var(--warn)",
    chipTone: "warn",
    quiet: false,
  },
  waiting: {
    accent: "var(--warn)",
    headerBg: "color-mix(in srgb, var(--warn) 11%, transparent)",
    border: "color-mix(in srgb, var(--warn) 48%, var(--line))",
    pinColor: "var(--warn)",
    chipTone: "warn",
    quiet: false,
  },
  failed: {
    accent: "var(--err)",
    headerBg: "color-mix(in srgb, var(--err) 11%, transparent)",
    border: "var(--err)",
    pinColor: "var(--err)",
    chipTone: "err",
    quiet: false,
  },
  completed: {
    accent: "color-mix(in srgb, var(--ok) 58%, var(--line))",
    headerBg: "transparent",
    border: "var(--line-soft)",
    pinColor: "var(--fg-faint)",
    chipTone: "neutral",
    quiet: true,
  },
  skipped: {
    accent: "var(--line)",
    headerBg: "transparent",
    border: "var(--line-soft)",
    pinColor: "var(--fg-faint)",
    chipTone: "neutral",
    quiet: true,
  },
  pending: {
    accent: "var(--fg-faint)",
    headerBg: "transparent",
    border: "var(--line)",
    pinColor: "var(--fg-dim)",
    chipTone: "neutral",
    quiet: false,
  },
};

const TONE_ALIASES: Record<string, keyof typeof GRAPH_STATUS_TONES> = {
  active: "running",
  progress: "running",
  assigned: "running",
  verifying: "running",
  warn: "blocked",
  paused: "waiting",
  err: "failed",
  done: "completed",
  succeeded: "completed",
  complete: "completed",
  plan: "skipped",
  cancelled: "skipped",
  open: "pending",
};

/** Resolves any task status or marker tone to shared DAG card surface/accent tokens. */
export function getGraphStatusTone(status?: string | null): GraphStatusTone {
  if (!status) return GRAPH_STATUS_TONES.pending;
  const key = GRAPH_STATUS_TONES[status] ? status : TONE_ALIASES[status];
  return (key && GRAPH_STATUS_TONES[key]) || GRAPH_STATUS_TONES.pending;
}

/** Circular 10x10 React Flow handle dot with 2px border matching PipelineCanvas blueprint cards. */
export function graphHandleDot(color = "var(--fg-dim)"): CSSProperties {
  return {
    width: 10,
    height: 10,
    minWidth: 0,
    minHeight: 0,
    borderRadius: 999,
    border: `2px solid ${color}`,
    background: "var(--th-card-bg)",
  };
}

/** Returns all transitive upstream ancestors + `selectedId` + transitive downstream descendants. */
export function computeConnectedSubgraph(
  selectedId: string | null,
  edges: readonly (readonly [string, string])[],
): Set<string> {
  if (!selectedId) return new Set();
  const upstream = new Map<string, string[]>();
  const downstream = new Map<string, string[]>();
  for (const [source, target] of edges) {
    if (source === target) continue;
    const prev = upstream.get(target);
    if (prev) prev.push(source);
    else upstream.set(target, [source]);
    const next = downstream.get(source);
    if (next) next.push(target);
    else downstream.set(source, [target]);
  }

  const walk = (adj: Map<string, string[]>) => {
    const visited = new Set<string>([selectedId]);
    const queue = [selectedId];
    for (let i = 0; i < queue.length; i++) {
      const current = queue[i];
      for (const neighbor of adj.get(current) ?? []) {
        if (!visited.has(neighbor)) {
          visited.add(neighbor);
          queue.push(neighbor);
        }
      }
    }
    return visited;
  };

  const ancestors = walk(upstream);
  const descendants = walk(downstream);
  return new Set([...ancestors, ...descendants]);
}

/** Toggleable React Flow minimap styled uniformly across Pipeline and Relations canvases. */
export function GraphMiniMap({
  open,
  ready = true,
  ariaLabel,
  chosenId,
  nodeColor,
  style,
}: {
  open: boolean;
  ready?: boolean;
  ariaLabel: string;
  chosenId?: string | null;
  nodeColor?: (node: Node) => string;
  style?: CSSProperties;
}) {
  if (!open || !ready || !MiniMap) return null;
  return (
    <MiniMap
      pannable
      zoomable
      ariaLabel={ariaLabel}
      bgColor="var(--th-card-bg)"
      maskColor="color-mix(in srgb, var(--bg) 55%, transparent)"
      nodeColor={
        nodeColor ??
        ((node) => {
          if (node.selected || (chosenId && node.id === chosenId)) return "var(--accent)";
          const status = (node.data as { status?: string; tone?: string } | undefined)?.status ?? (node.data as { tone?: string } | undefined)?.tone;
          const tone = getGraphStatusTone(status);
          return tone.quiet ? "var(--line)" : tone.accent;
        })
      }
      nodeBorderRadius={6}
      style={{
        marginBottom: 76,
        width: 168,
        height: 104,
        border: "1px solid var(--line-soft)",
        borderRadius: 12,
        ...style,
      }}
    />
  );
}

export interface MinimapToggleProps {
  open: boolean;
  onToggle: () => void;
  label?: string;
}

/** Shared bottom-right zoom + fit + optional minimap toggle toolbar for React Flow canvases. */
export function GraphZoomTools({
  flow,
  fit = { padding: 0.08, minZoom: 0.1, maxZoom: 1 },
  fitLabel,
  minimap,
  labels,
  className = "",
  ...rest
}: HTMLAttributes<HTMLDivElement> & {
  flow: RefObject<ReactFlowInstance | null>;
  fit?: FitViewOptions;
  fitLabel: string;
  minimap?: MinimapToggleProps;
  labels: { zoomOut: string; zoomIn: string; minimap: string };
}) {
  return (
    <div {...rest} className={`${GRAPH_FLOAT_CLASS} right-4 bottom-4 flex gap-0.5 p-1 ${className}`}>
      <IconButton icon={<ZoomOut aria-hidden />} className={GRAPH_TOOL_CLASS} label={labels.zoomOut} onClick={() => void flow.current?.zoomOut()} />
      <IconButton icon={<ZoomIn aria-hidden />} className={GRAPH_TOOL_CLASS} label={labels.zoomIn} onClick={() => void flow.current?.zoomIn()} />
      <IconButton icon={<Maximize aria-hidden />} className={GRAPH_TOOL_CLASS} label={fitLabel} onClick={() => void flow.current?.fitView(fit)} />
      {minimap && (
        <IconButton
          icon={<MapIcon aria-hidden />}
          className={GRAPH_TOOL_CLASS}
          label={minimap.label ?? labels.minimap}
          pressed={minimap.open}
          onClick={minimap.onToggle}
        />
      )}
    </div>
  );
}
