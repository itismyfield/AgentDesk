import { useState, type CSSProperties } from 'react';
import { ArrowDown, CircleDot, GitBranch } from 'lucide-react';

// Clean-room state diagram from the leak-checked spec and reference renders. No Pro source.
export type MachineTone = 'normal' | 'waiting' | 'error' | 'final';
export interface MachineState { id: string; title: string; column: number; row: number; tone?: MachineTone; durationMs?: number; count?: number; detail?: string; tools?: string[] }
export interface MachineEdge { from: string; to: string; label?: string; count?: number; detail?: string }
export interface AgentStateMachineLabels {
  transitions?: (count: number) => string;
  waiting?: string;
  notReached?: string;
  approve?: string;
  reject?: string;
}
export interface AgentStateMachineProps {
  title: string;
  agent?: string;
  current: string;
  states: MachineState[];
  edges?: MachineEdge[];
  labels?: AgentStateMachineLabels;
  onApprove?: (id: string) => void;
  onReject?: (id: string) => void;
  className?: string;
  style?: CSSProperties;
}

const styles = `
.aui-agent-state-machine {
  --aui-sm-ink: var(--aui-fg, #171717);
  --aui-sm-muted: var(--aui-fg-muted, #8d8d93);
  --aui-sm-line: var(--aui-line, #ececef);
  --aui-sm-edge: #d8d8de;
  --aui-sm-edge-dim: #eeeef2;
  --aui-sm-card: var(--aui-bg-1, #fff);
  --aui-sm-wait: var(--aui-warn, #f59e0b);
  --aui-sm-wait-fg: #b86a00;
  --aui-sm-wait-bg: #fff7eb;
  width: 100%;
  min-width: 0;
  padding: 16px 18px 18px;
  border: 1px solid var(--aui-sm-line);
  border-radius: var(--aui-r-xl, 22px);
  background: var(--aui-sm-card);
  color: var(--aui-sm-ink);
  box-shadow: var(--aui-shadow-sm, 0 10px 30px rgb(15 23 42 / .04));
  font: 13px/18px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif);
  word-break: keep-all;
  overflow-wrap: anywhere;
  box-sizing: border-box;
}
.aui-agent-state-machine *, .aui-agent-state-machine *::before, .aui-agent-state-machine *::after { box-sizing: border-box; }
:where(.dark, [data-theme="dark"]) .aui-agent-state-machine {
  --aui-sm-ink: var(--aui-fg, #f3f3f4);
  --aui-sm-muted: var(--aui-fg-muted, #a1a1a8);
  --aui-sm-line: var(--aui-line, rgb(255 255 255 / .1));
  --aui-sm-edge: rgb(255 255 255 / .24);
  --aui-sm-edge-dim: rgb(255 255 255 / .09);
  --aui-sm-card: var(--aui-bg-1, #1c1c1e);
  --aui-sm-wait: var(--aui-warn, #ffb35c);
  --aui-sm-wait-fg: #ffb35c;
  --aui-sm-wait-bg: rgb(245 158 11 / .14);
  box-shadow: none;
}
.aui-agent-state-machine-head {
  display: flex;
  flex-wrap: wrap;
  gap: 6px 10px;
  align-items: center;
  min-width: 0;
  margin-bottom: 16px;
}
.aui-agent-state-machine-head strong {
  min-width: 0;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 15px;
  line-height: 20px;
}
.aui-agent-state-machine-head em { color: var(--aui-sm-muted); font-style: normal; white-space: nowrap; }
.aui-agent-state-machine-head > span { margin-left: auto; color: var(--aui-sm-muted); white-space: nowrap; font-variant-numeric: tabular-nums; }
.aui-agent-state-machine-wait {
  padding: 5px 10px;
  border-radius: 999px;
  background: var(--aui-sm-wait-bg);
  color: var(--aui-sm-wait-fg);
  font-weight: 600;
  white-space: nowrap;
}
.aui-agent-state-machine-board {
  position: relative;
  display: grid;
  grid-template-columns: repeat(5, minmax(0, 1fr));
  gap: 36px 18px;
  min-height: 168px;
}
.aui-agent-state-machine-svg {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  pointer-events: none;
}
.aui-agent-state-machine-node {
  position: relative;
  z-index: 1;
  display: flex;
  gap: 8px;
  align-items: center;
  min-width: 0;
  min-height: 52px;
  padding: 8px 10px;
  border: 1px solid var(--aui-sm-line);
  border-radius: 14px;
  background: var(--aui-sm-card);
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.aui-agent-state-machine-node:focus-visible,
.aui-agent-state-machine-chip:focus-visible,
.aui-agent-state-machine-actions button:focus-visible {
  outline: 2px solid var(--aui-accent, #3b82f6);
  outline-offset: 2px;
}
.aui-agent-state-machine-node[data-current] {
  border-color: #f5c27a;
  background: var(--aui-sm-wait-bg);
  box-shadow: 0 0 0 3px rgb(245 158 11 / .15);
}
.aui-agent-state-machine-node[data-tone="error"] { border-color: rgb(239 68 68 / .45); }
.aui-agent-state-machine-node small { display: block; color: var(--aui-sm-muted); font-size: 12px; font-variant-numeric: tabular-nums; }
.aui-agent-state-machine-node[data-current] small { color: var(--aui-sm-wait-fg); }
.aui-agent-state-machine-chip {
  position: absolute;
  z-index: 2;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  min-width: 28px;
  height: 22px;
  padding: 0 7px;
  border: 1px solid var(--aui-sm-line);
  border-radius: 999px;
  background: var(--aui-sm-card);
  color: var(--aui-sm-muted);
  font: 11px/1 var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  transform: translate(-50%, -50%);
  cursor: pointer;
}
.aui-agent-state-machine-chip[data-dim] { opacity: .45; }
.aui-agent-state-machine-chip-mobile { display: none; }
.aui-agent-state-machine-badge {
  display: grid;
  place-items: center;
  width: 18px;
  height: 18px;
  margin-left: auto;
  border-radius: 6px;
  background: #fff;
  color: #b86a00;
  font-size: 11px;
  font-weight: 700;
  flex: none;
}
.aui-agent-state-machine-note { margin: 10px 0 0; color: var(--aui-sm-muted); }
.aui-agent-state-machine-actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 10px; }
.aui-agent-state-machine-actions button {
  min-width: 44px;
  min-height: 44px;
  padding: 0 14px;
  border: 0;
  border-radius: 10px;
  background: #171717;
  color: #fff;
  font: inherit;
  cursor: pointer;
}
:where(.dark, [data-theme="dark"]) .aui-agent-state-machine-actions button {
  background: #f3f3f4;
  color: #171717;
}
@container aui-sm (max-width: 640px) {
  .aui-agent-state-machine-head > span { margin-left: 0; }
  .aui-agent-state-machine-board {
    display: flex;
    flex-direction: column;
    gap: 6px;
    min-height: 0;
  }
  .aui-agent-state-machine-svg { display: none; }
  .aui-agent-state-machine-node {
    width: 100%;
    order: var(--aui-sm-order, 0);
  }
  .aui-agent-state-machine-node b {
    white-space: normal !important;
    overflow: visible !important;
    text-overflow: clip !important;
  }
  .aui-agent-state-machine-chip {
    position: static;
    transform: none;
    align-self: flex-start;
    height: auto;
    min-height: 32px;
    margin-left: 12px;
    padding: 5px 10px;
    font-size: 12px;
    order: var(--aui-sm-order, 0);
  }
  .aui-agent-state-machine-chip-desktop { display: none; }
  .aui-agent-state-machine-chip-mobile { display: inline-flex; align-items: center; gap: 4px; }
}
@media (pointer: coarse) {
  .aui-agent-state-machine-node,
  .aui-agent-state-machine-chip {
    min-width: 44px;
    min-height: 44px;
  }
}
@media (prefers-reduced-motion: reduce) {
  .aui-agent-state-machine *, .aui-agent-state-machine *::before, .aui-agent-state-machine *::after {
    transition: none !important;
    animation: none !important;
  }
}
`;

const clock = (ms?: number) => {
  if (!ms) return '';
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, '0')}s`;
};

export function AgentStateMachine({
  title,
  agent,
  current,
  states,
  edges = [],
  labels,
  onApprove,
  onReject,
  className = '',
  style,
}: AgentStateMachineProps) {
  const [selected, setSelected] = useState<string | null>(null);
  const [edge, setEdge] = useState<string | null>(null);
  const currentState = states.find((state) => state.id === current);
  const passed = edges.reduce((sum, item) => sum + (item.count ?? 0), 0);
  const byId = new Map(states.map((state) => [state.id, state]));
  const orderedStates = [...states].sort((a, b) => (a.row - b.row) || (a.column - b.column));
  const transitionsLabel = labels?.transitions ? labels.transitions(passed) : `${passed} transitions`;
  const waitingLabel = labels?.waiting ?? 'Waiting for you';
  const notReachedLabel = labels?.notReached ?? 'Not reached';

  return (
    <section className={`aui-agent-state-machine ${className}`} style={{ containerType: 'inline-size', containerName: 'aui-sm', ...style }}>
      <style>{styles}</style>
      <div className="aui-agent-state-machine-head">
        <GitBranch size={16} color="#3b82f6" aria-hidden="true" style={{ flex: 'none' }} />
        <strong title={title}>{title}</strong>
        {agent && <em>{agent}</em>}
        <span>{transitionsLabel}</span>
        {currentState?.tone === 'waiting' && <em className="aui-agent-state-machine-wait">{waitingLabel}</em>}
      </div>
      {orderedStates.length > 0 && (
        <div className="aui-agent-state-machine-board">
          <svg className="aui-agent-state-machine-svg" aria-hidden="true" viewBox="0 0 100 40" preserveAspectRatio="none">
            {edges.map((item) => {
              const from = byId.get(item.from);
              const to = byId.get(item.to);
              if (!from || !to) return null;
              const x1 = from.column * 20 + 10;
              const x2 = to.column * 20 + 10;
              const y1 = from.row * 24 + 8;
              const y2 = to.row * 24 + 8;
              return <path key={`${item.from}-${item.to}`} d={`M${x1},${y1} C${(x1 + x2) / 2},${y1} ${(x1 + x2) / 2},${y2} ${x2},${y2}`} fill="none" stroke={item.count ? 'var(--aui-sm-edge)' : 'var(--aui-sm-edge-dim)'} strokeWidth="1.2" />;
            })}
          </svg>
          {orderedStates.map((state) => {
            const time = clock(state.durationMs);
            const meta = state.detail ?? ([time, state.count ? `${state.count}×` : ''].filter(Boolean).join(' · ') || notReachedLabel);
            const order = state.row * 20 + state.column * 2;
            return (
              <button
                key={state.id}
                type="button"
                className="aui-agent-state-machine-node"
                data-current={state.id === current || undefined}
                data-tone={state.tone}
                aria-pressed={selected === state.id}
                style={{ gridColumn: state.column + 1, gridRow: state.row + 1, '--aui-sm-order': order } as CSSProperties}
                onClick={() => { setSelected(state.id); setEdge(null); }}
              >
                <CircleDot size={14} aria-hidden="true" style={{ flex: 'none' }} />
                <span style={{ minWidth: 0 }}>
                  <b style={{ display: 'block', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{state.title}</b>
                  <small>{meta}</small>
                </span>
                {state.tools?.[0] && <span className="aui-agent-state-machine-badge">{state.tools[0].slice(0, 1).toUpperCase()}</span>}
              </button>
            );
          })}
          {edges.map((item) => {
            const from = byId.get(item.from);
            const to = byId.get(item.to);
            if (!from || !to || from.row !== to.row) return null;
            const order = from.row * 20 + from.column * 2 + 1;
            const countText = `${item.count ?? 0}×`;
            return (
              <button
                key={`${item.from}-${item.to}`}
                type="button"
                className="aui-agent-state-machine-chip"
                data-dim={!item.count || undefined}
                style={{ left: `calc(${(from.column + to.column + 1) * 10}%)`, top: from.row ? '78%' : '22%', '--aui-sm-order': order } as CSSProperties}
                aria-label={`${item.label ?? 'transition'} ${item.count ?? 0}`}
                onClick={() => setEdge(`${item.from}-${item.to}`)}
              >
                <span className="aui-agent-state-machine-chip-desktop">{countText}</span>
                <span className="aui-agent-state-machine-chip-mobile">
                  <ArrowDown size={12} aria-hidden="true" />
                  {item.label ? `${item.label} · ${countText}` : countText}
                </span>
              </button>
            );
          })}
        </div>
      )}
      {currentState?.tone === 'waiting' && (onApprove || onReject) && (
        <div className="aui-agent-state-machine-actions">
          {onApprove && <button type="button" onClick={() => onApprove(current)}>{labels?.approve ?? '승인'}</button>}
          {onReject && <button type="button" onClick={() => onReject(current)}>{labels?.reject ?? '거부'}</button>}
        </div>
      )}
      {edge && <p className="aui-agent-state-machine-note" role="status">{edges.find((item) => `${item.from}-${item.to}` === edge)?.detail ?? edges.find((item) => `${item.from}-${item.to}` === edge)?.label}</p>}
    </section>
  );
}

const states: MachineState[] = [
  { id: 'read', title: 'Read failure', column: 0, row: 0, durationMs: 42000, count: 1 },
  { id: 'plan', title: 'Plan the fix', column: 1, row: 0, durationMs: 38000, count: 1 },
  { id: 'edit', title: 'Edit helper', column: 2, row: 0, tone: 'waiting', durationMs: 96000, count: 1, tools: ['cargo'] },
  { id: 'verify', title: 'Verify', column: 3, row: 0, detail: 'Not reached' },
  { id: 'done', title: 'Landed', column: 4, row: 0, tone: 'final', detail: 'Not reached' },
  { id: 'blocked', title: 'Needs a person', column: 2, row: 1, tone: 'error', detail: 'Not reached' },
];
const edges: MachineEdge[] = [
  { from: 'read', to: 'plan', label: 'enough context', count: 1 },
  { from: 'plan', to: 'edit', label: 'start patch', count: 1 },
  { from: 'edit', to: 'verify', label: 'check ok', count: 0 },
  { from: 'verify', to: 'done', label: 'green', count: 0 },
  { from: 'edit', to: 'blocked', label: 'check failed', count: 1, detail: 'OOM during rustc' },
  { from: 'blocked', to: 'edit', label: 'retry', count: 1 },
];
export function AgentStateMachineDemo({ state = 'default' }: { state?: 'default' | 'error' | 'empty' | 'long' }) {
  const current = state === 'error' ? 'blocked' : 'edit';
  const next = state === 'error' ? states.map((item) => item.id === 'blocked' ? { ...item, tone: 'error' as const } : item.id === 'edit' ? { ...item, tone: 'normal' as const } : item) : states;
  return <div style={{ width: '100%', maxWidth: state === 'long' ? 360 : 922 }}><AgentStateMachine title={state === 'long' ? '아주 긴 수리 제목 issue-6082-repair-identifier' : 'Issue 6082 repair'} agent="gpt-5.1" current={current} states={state === 'empty' ? [] : next} edges={state === 'empty' ? [] : edges} /></div>;
}
