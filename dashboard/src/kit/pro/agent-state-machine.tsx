import { useState, type CSSProperties } from 'react';
import { CircleDot, GitBranch } from 'lucide-react';

// Clean-room state diagram from the leak-checked spec and reference renders. No Pro source.
export type MachineTone = 'normal' | 'waiting' | 'error' | 'final';
export interface MachineState { id: string; title: string; column: number; row: number; tone?: MachineTone; durationMs?: number; count?: number; detail?: string; tools?: string[] }
export interface MachineEdge { from: string; to: string; label?: string; count?: number; detail?: string }
export interface AgentStateMachineProps { title: string; agent?: string; current: string; states: MachineState[]; edges?: MachineEdge[]; onApprove?: (id: string) => void; onReject?: (id: string) => void; className?: string; style?: CSSProperties }
const styles = `
.aui-agent-state-machine { --aui-sm-ink:#171717; --aui-sm-muted:#8d8d93; --aui-sm-line:#ececef; --aui-sm-card:#fff; --aui-sm-wait:#f59e0b; --aui-sm-wait-bg:#fff7eb; width:100%; min-width:0; padding:16px 18px 18px; border:1px solid rgb(0 0 0 / .05); border-radius:22px; background:var(--aui-sm-card); color:var(--aui-sm-ink); box-shadow:0 10px 30px rgb(15 23 42 / .04); font:13px/18px Inter, ui-sans-serif, system-ui, sans-serif; }
:where(.dark,[data-theme="dark"]) .aui-agent-state-machine { --aui-sm-ink:#f3f3f4; --aui-sm-muted:#a1a1a8; --aui-sm-line:rgb(255 255 255 / .1); --aui-sm-card:#1c1c1e; --aui-sm-wait:#ffb35c; --aui-sm-wait-bg:rgb(245 158 11 / .14); box-shadow:none; }
.aui-agent-state-machine-head { display:flex; gap:10px; align-items:center; min-width:0; margin-bottom:18px; }
.aui-agent-state-machine-head strong { font-size:16px; }
.aui-agent-state-machine-head em { color:var(--aui-sm-muted); font-style:normal; }
.aui-agent-state-machine-head span { margin-left:auto; color:var(--aui-sm-muted); white-space:nowrap; }
.aui-agent-state-machine-wait { margin-left:8px; padding:6px 10px; border-radius:999px; background:var(--aui-sm-wait-bg); color:#b86a00; font-weight:650; }
.aui-agent-state-machine-board { position:relative; display:grid; grid-template-columns:repeat(5, minmax(0,1fr)); gap:36px 18px; min-height:168px; }
.aui-agent-state-machine-node { position:relative; z-index:1; display:flex; gap:8px; align-items:center; min-width:0; min-height:52px; padding:8px 10px; border:1px solid var(--aui-sm-line); border-radius:14px; background:var(--aui-sm-card); color:inherit; font:inherit; text-align:left; }
.aui-agent-state-machine-node[data-current] { border-color:#f5c27a; background:var(--aui-sm-wait-bg); box-shadow:0 0 0 3px rgb(245 158 11 / .15); }
.aui-agent-state-machine-node[data-tone="error"] { border-color:#fecaca; }
.aui-agent-state-machine-node small { display:block; color:var(--aui-sm-muted); font-size:12px; }
.aui-agent-state-machine-node[data-current] small { color:#b86a00; }
.aui-agent-state-machine-chip { position:absolute; z-index:2; display:grid; place-items:center; width:28px; height:22px; border:1px solid var(--aui-sm-line); border-radius:999px; background:var(--aui-sm-card); color:var(--aui-sm-muted); font-size:11px; }
.aui-agent-state-machine-chip[data-dim] { opacity:.45; }
.aui-agent-state-machine-badge { display:grid; place-items:center; width:18px; height:18px; margin-left:auto; border-radius:6px; background:#fff; color:#b86a00; font-size:11px; font-weight:700; }
.aui-agent-state-machine-note { margin:8px 0 0; color:var(--aui-sm-muted); }
.aui-agent-state-machine-actions { display:flex; gap:8px; margin-top:8px; }
.aui-agent-state-machine-actions button { min-width:44px; min-height:44px; padding:0 12px; border:0; border-radius:10px; background:#171717; color:#fff; font:inherit; }
@container aui-sm (max-width:640px) {
  .aui-agent-state-machine-board { display:flex; gap:8px; overflow-x:auto; padding-bottom:8px; }
  .aui-agent-state-machine-node { flex:0 0 148px; }
}
@media (pointer: coarse) { .aui-agent-state-machine-node, .aui-agent-state-machine-chip { min-width:44px; min-height:44px; } }
`;
const clock = (ms?: number) => {
  if (!ms) return '';
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, '0')}s`;
};
export function AgentStateMachine({ title, agent, current, states, edges = [], onApprove, onReject, className = '', style }: AgentStateMachineProps) {
  const [selected, setSelected] = useState<string | null>(null);
  const [edge, setEdge] = useState<string | null>(null);
  const currentState = states.find((state) => state.id === current);
  const passed = edges.reduce((sum, item) => sum + (item.count ?? 0), 0);
  const byId = new Map(states.map((state) => [state.id, state]));
  return (
    <section className={`aui-agent-state-machine ${className}`} style={{ containerType: 'inline-size', containerName: 'aui-sm', ...style }}>
      <style>{styles}</style>
      <div className="aui-agent-state-machine-head">
        <GitBranch size={16} color="#3b82f6" aria-hidden="true" />
        <strong>{title}</strong>
        {agent && <em>{agent}</em>}
        <span>{passed} transitions</span>
        {currentState?.tone === 'waiting' && <em className="aui-agent-state-machine-wait">Waiting for you</em>}
      </div>
      <div className="aui-agent-state-machine-board">
        <svg aria-hidden="true" viewBox="0 0 100 40" preserveAspectRatio="none" style={{ position: 'absolute', inset: 0, width: '100%', height: '100%' }}>
          {edges.map((item) => {
            const from = byId.get(item.from);
            const to = byId.get(item.to);
            if (!from || !to) return null;
            const x1 = from.column * 20 + 10;
            const x2 = to.column * 20 + 10;
            const y1 = from.row * 24 + 8;
            const y2 = to.row * 24 + 8;
            return <path key={`${item.from}-${item.to}`} d={`M${x1},${y1} C${(x1 + x2) / 2},${y1} ${(x1 + x2) / 2},${y2} ${x2},${y2}`} fill="none" stroke={item.count ? '#d8d8de' : '#eeeef2'} strokeWidth="1.2" />;
          })}
        </svg>
        {states.map((state) => {
          const time = clock(state.durationMs);
          const meta = state.detail ?? ([time, state.count ? `${state.count}×` : ''].filter(Boolean).join(' · ') || 'Not reached');
          return (
            <button key={state.id} type="button" className="aui-agent-state-machine-node" data-current={state.id === current || undefined} data-tone={state.tone} aria-pressed={selected === state.id} style={{ gridColumn: state.column + 1, gridRow: state.row + 1 }} onClick={() => { setSelected(state.id); setEdge(null); }}>
              <CircleDot size={14} aria-hidden="true" />
              <span style={{ minWidth: 0 }}><b style={{ display: 'block', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{state.title}</b><small>{meta}</small></span>
              {state.tools?.[0] && <span className="aui-agent-state-machine-badge">{state.tools[0].slice(0, 1).toUpperCase()}</span>}
            </button>
          );
        })}
        {edges.map((item) => {
          const from = byId.get(item.from);
          const to = byId.get(item.to);
          if (!from || !to || from.row !== to.row) return null;
          return <button key={`${item.from}-${item.to}`} type="button" className="aui-agent-state-machine-chip" data-dim={!item.count || undefined} style={{ left: `calc(${(from.column + to.column + 1) * 10}% )`, top: from.row ? '78%' : '22%' }} aria-label={`${item.label ?? 'transition'} ${item.count ?? 0}`} onClick={() => setEdge(`${item.from}-${item.to}`)}>{item.count ?? 0}×</button>;
        })}
      </div>
      {currentState?.tone === 'waiting' && (onApprove || onReject) && (
        <div className="aui-agent-state-machine-actions">
          {onApprove && <button type="button" onClick={() => onApprove(current)}>승인</button>}
          {onReject && <button type="button" onClick={() => onReject(current)}>거부</button>}
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
