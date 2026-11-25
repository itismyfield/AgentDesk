import { useState, type CSSProperties } from 'react';

// Clean-room metro map from the leak-checked spec and reference renders. No Pro source.
export interface MetroLine { id: string; name: string; stations: string[]; here: string; status?: 'running' | 'idle'; model?: string; color: string }
export interface MetroStation { id: string; name: string; column: number; row: number; waiting?: boolean; note?: string }
export interface AgentMetroMapProps { title: string; lines: MetroLine[]; stations: MetroStation[]; className?: string; style?: CSSProperties }
const styles = `
.aui-agent-metro-map { --aui-metro-ink:#171717; --aui-metro-muted:#8d8d93; --aui-metro-line:#ececef; width:100%; min-width:0; padding:16px 18px 8px; border:1px solid rgb(0 0 0 / .05); border-radius:22px; background:#fff; color:var(--aui-metro-ink); box-shadow:0 10px 30px rgb(15 23 42 / .04); font:13px/18px Inter, ui-sans-serif, system-ui, sans-serif; }
:where(.dark,[data-theme="dark"]) .aui-agent-metro-map { --aui-metro-ink:#f3f3f4; --aui-metro-muted:#a1a1a8; --aui-metro-line:rgb(255 255 255 / .1); background:#1c1c1e; }
.aui-agent-metro-map-head { display:flex; gap:8px; align-items:center; justify-content:space-between; min-width:0; }
.aui-agent-metro-map-head strong { font-size:16px; }
.aui-agent-metro-map-lines { display:flex; flex-wrap:wrap; gap:6px; justify-content:flex-end; }
.aui-agent-metro-map-lines span { display:inline-flex; gap:6px; align-items:center; min-height:28px; padding:0 10px; border:1px solid var(--aui-metro-line); border-radius:999px; color:var(--aui-metro-muted); font-size:12px; }
.aui-agent-metro-map-lines i { width:14px; height:3px; border-radius:99px; }
.aui-agent-metro-map-canvas { position:relative; height:340px; }
.aui-agent-metro-map-canvas svg { position:absolute; inset:0; width:100%; height:100%; }
.aui-agent-metro-map button { position:absolute; display:flex; flex-direction:column; align-items:center; justify-content:center; min-width:44px; min-height:44px; padding:0; border:0; background:transparent; color:inherit; font:12px/16px Inter, sans-serif; transform:translate(-50%, -40%); }
.aui-agent-metro-map button b { display:block; width:10px; height:10px; border:3px solid currentColor; border-radius:99px; background:#fff; }
.aui-agent-metro-map button[data-here] b { width:22px; height:22px; border-width:0; background:currentColor; }
.aui-agent-metro-map button[data-wait] { color:#e08a00; }
.aui-agent-metro-map-note { min-height:20px; margin:0; color:var(--aui-metro-muted); }
@container aui-metro (max-width:640px) {
  .aui-agent-metro-map-canvas { height:auto; }
  .aui-agent-metro-map-canvas svg { display:none; }
  .aui-agent-metro-map button { position:static; transform:none; flex-direction:row; gap:8px; justify-content:flex-start; width:100%; }
}
`;
const spot: Record<string, { x: number; y: number }> = {
  brief: { x: 16, y: 46 },
  edit: { x: 38, y: 24 },
  check: { x: 58, y: 24 },
  push: { x: 84, y: 24 },
  move: { x: 40, y: 64 },
  wsl: { x: 64, y: 68 },
  held: { x: 16, y: 88 },
};
export function AgentMetroMap({ title, lines, stations, className = '', style }: AgentMetroMapProps) {
  const [selected, setSelected] = useState<string | null>(null);
  const at = (id: string) => spot[id] ?? { x: 50, y: 50 };
  const curve = (line: MetroLine) => {
    const points = line.stations.map(at);
    return points.map((point, index) => `${index ? 'L' : 'M'}${point.x},${point.y}`).join(' ');
  };
  const active = stations.find((station) => station.id === selected);
  return (
    <section className={`aui-agent-metro-map ${className}`} style={{ containerType: 'inline-size', containerName: 'aui-metro', ...style }}>
      <style>{styles}</style>
      <div className="aui-agent-metro-map-head">
        <strong>{title}</strong>
        <div className="aui-agent-metro-map-lines">{lines.map((line) => <span key={line.id}><i style={{ background: line.color }} />{line.name}</span>)}</div>
      </div>
      <div className="aui-agent-metro-map-canvas">
        <svg viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
          {lines.map((line) => <path key={line.id} d={curve(line)} fill="none" stroke={line.color} strokeWidth="1.6" strokeLinecap="round" vectorEffect="non-scaling-stroke" />)}
        </svg>
        {stations.map((station) => {
          const here = lines.some((line) => line.here === station.id && line.status === 'running');
          const color = lines.find((line) => line.stations.includes(station.id))?.color ?? '#8b8b92';
          const point = at(station.id);
          return <button key={station.id} type="button" data-here={here || undefined} data-wait={station.waiting || undefined} aria-pressed={selected === station.id} style={{ left: `${point.x}%`, top: `${point.y}%`, color: station.waiting ? undefined : color }} onClick={() => setSelected(station.id)}><b /><span>{station.name}</span></button>;
        })}
      </div>
      <p className="aui-agent-metro-map-note" role="status">{active?.note ?? ''}</p>
    </section>
  );
}
const stations: MetroStation[] = [
  { id: 'brief', name: 'Brief', column: 0, row: 1 },
  { id: 'edit', name: 'Edit G7', column: 1, row: 0 },
  { id: 'check', name: 'Mac check', column: 2, row: 0, waiting: true, note: 'Waiting on cargo check' },
  { id: 'push', name: 'Push', column: 3, row: 0 },
  { id: 'move', name: 'Move tests', column: 1, row: 2 },
  { id: 'wsl', name: 'WSL tests', column: 2, row: 2 },
  { id: 'held', name: 'Held', column: 0, row: 3, waiting: true, note: 'Blocked on C10e-2 overlap' },
];
const lines: MetroLine[] = [
  { id: 'c10e', name: 'KR-C10e', stations: ['brief', 'edit', 'check', 'push'], here: 'check', status: 'running', model: 'Fable', color: '#0190fe' },
  { id: 't9b', name: 'KRX-T9b', stations: ['brief', 'move', 'wsl'], here: 'move', status: 'running', model: 'Fable', color: '#a651f0' },
  { id: 't12', name: 'T12 hold', stations: ['held'], here: 'held', status: 'idle', color: '#8b8b92' },
];
export function AgentMetroMapDemo({ state = 'default' }: { state?: 'default' | 'selected' | 'empty' | 'long' }) {
  return <div style={{ width: '100%', maxWidth: state === 'long' ? 360 : 922 }}><AgentMetroMap title={state === 'long' ? '아주 긴 노선 제목 crate-split-lanes-identifier' : 'Crate split lanes'} lines={state === 'empty' ? [] : lines} stations={state === 'empty' ? [] : stations} /></div>;
}
