import { useState, type CSSProperties } from 'react';

// Clean-room metro map from the leak-checked spec and reference renders. No Pro source.
export interface MetroLine { id: string; name: string; stations: string[]; here: string; status?: 'running' | 'idle'; model?: string; color: string }
export interface MetroStation { id: string; name: string; column: number; row: number; waiting?: boolean; note?: string }
export interface AgentMetroMapProps { title: string; lines: MetroLine[]; stations: MetroStation[]; className?: string; style?: CSSProperties }
const styles = `
.aui-agent-metro-map {
  --aui-metro-ink: var(--aui-fg, #171717);
  --aui-metro-muted: var(--aui-fg-muted, #8d8d93);
  --aui-metro-line: var(--aui-line, #ececef);
  --aui-metro-card: var(--aui-bg-1, #fff);
  width: 100%;
  min-width: 0;
  padding: 16px 18px 12px;
  border: 1px solid var(--aui-metro-line);
  border-radius: var(--aui-r-xl, 22px);
  background: var(--aui-metro-card);
  color: var(--aui-metro-ink);
  box-shadow: var(--aui-shadow-sm, 0 10px 30px rgb(15 23 42 / .04));
  font: 13px/18px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif);
  word-break: keep-all;
  overflow-wrap: anywhere;
  box-sizing: border-box;
}
.aui-agent-metro-map *, .aui-agent-metro-map *::before, .aui-agent-metro-map *::after { box-sizing: border-box; }
:where(.dark, [data-theme="dark"]) .aui-agent-metro-map {
  --aui-metro-ink: var(--aui-fg, #f3f3f4);
  --aui-metro-muted: var(--aui-fg-muted, #a1a1a8);
  --aui-metro-line: var(--aui-line, rgb(255 255 255 / .1));
  --aui-metro-card: var(--aui-bg-1, #1c1c1e);
  box-shadow: none;
}
.aui-agent-metro-map-head {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 12px;
  align-items: center;
  justify-content: space-between;
  min-width: 0;
}
.aui-agent-metro-map-head strong {
  min-width: 0;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 15px;
  line-height: 20px;
}
.aui-agent-metro-map-lines { display: flex; flex-wrap: wrap; gap: 6px; justify-content: flex-end; min-width: 0; }
.aui-agent-metro-map-lines span {
  display: inline-flex;
  gap: 6px;
  align-items: center;
  min-height: 28px;
  max-width: 100%;
  padding: 0 10px;
  border: 1px solid var(--aui-metro-line);
  border-radius: 999px;
  color: var(--aui-metro-muted);
  font-size: 12px;
}
.aui-agent-metro-map-lines i { flex: none; width: 14px; height: 3px; border-radius: 99px; }
.aui-agent-metro-map-canvas { position: relative; height: 340px; }
.aui-agent-metro-map-canvas svg { position: absolute; inset: 0; width: 100%; height: 100%; pointer-events: none; }
.aui-agent-metro-map button {
  position: absolute;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  min-width: 44px;
  min-height: 44px;
  padding: 0;
  border: 0;
  border-radius: 10px;
  background: transparent;
  color: inherit;
  font: 12px/16px var(--aui-font-sans, Inter, sans-serif);
  transform: translate(-50%, -40%);
  cursor: pointer;
}
.aui-agent-metro-map button:focus-visible { outline: 2px solid var(--aui-accent, #3b82f6); outline-offset: 2px; }
.aui-agent-metro-map button b { flex: none; display: block; width: 10px; height: 10px; border: 3px solid currentColor; border-radius: 99px; background: var(--aui-metro-card); }
.aui-agent-metro-map button[data-here] b { width: 18px; height: 18px; border-width: 0; background: currentColor; box-shadow: 0 0 0 4px color-mix(in srgb, currentColor 20%, transparent); }
.aui-agent-metro-map button[data-wait] { color: #e08a00; }
.aui-agent-metro-map-mobile-tags { display: none; }
.aui-agent-metro-map-note { min-height: 20px; margin: 4px 0 0; color: var(--aui-metro-muted); font-size: 12px; }
@container aui-metro (max-width: 640px) {
  .aui-agent-metro-map-lines { justify-content: flex-start; }
  .aui-agent-metro-map-canvas {
    height: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin-top: 12px;
    padding-left: 10px;
  }
  .aui-agent-metro-map-canvas::before {
    content: '';
    position: absolute;
    left: 25px;
    top: 22px;
    bottom: 22px;
    width: 2px;
    background: var(--aui-metro-line);
    border-radius: 99px;
    pointer-events: none;
  }
  .aui-agent-metro-map-canvas svg { display: none; }
  .aui-agent-metro-map button {
    position: relative;
    left: auto !important;
    top: auto !important;
    transform: none;
    flex-direction: row;
    gap: 10px;
    justify-content: flex-start;
    width: 100%;
    padding: 6px 8px;
    text-align: left;
  }
  .aui-agent-metro-map button[aria-pressed="true"] { background: color-mix(in srgb, var(--aui-metro-ink) 6%, transparent); }
  .aui-agent-metro-map button b {
    width: 12px;
    height: 12px;
    margin-left: 2px;
  }
  .aui-agent-metro-map button[data-here] b {
    width: 16px;
    height: 16px;
    margin-left: 0;
  }
  .aui-agent-metro-map-mobile-tags {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-left: auto;
  }
  .aui-agent-metro-map-mobile-tags i {
    display: block;
    width: 8px;
    height: 8px;
    border-radius: 99px;
  }
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
        <strong title={title}>{title}</strong>
        <div className="aui-agent-metro-map-lines">{lines.map((line) => <span key={line.id}><i style={{ background: line.color }} />{line.name}</span>)}</div>
      </div>
      {stations.length > 0 && (
        <div className="aui-agent-metro-map-canvas">
          <svg viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
            {lines.map((line) => <path key={line.id} d={curve(line)} fill="none" stroke={line.color} strokeWidth="1.6" strokeLinecap="round" vectorEffect="non-scaling-stroke" />)}
          </svg>
          {stations.map((station) => {
            const hereLines = lines.filter((line) => line.here === station.id);
            const here = hereLines.some((line) => line.status === 'running');
            const color = lines.find((line) => line.stations.includes(station.id))?.color ?? '#8b8b92';
            const point = at(station.id);
            return (
              <button
                key={station.id}
                type="button"
                data-here={here || undefined}
                data-wait={station.waiting || undefined}
                aria-pressed={selected === station.id}
                style={{ left: `${point.x}%`, top: `${point.y}%`, color: station.waiting ? undefined : color }}
                onClick={() => setSelected(station.id)}
              >
                <b />
                <span>{station.name}</span>
                {hereLines.length > 0 && (
                  <span className="aui-agent-metro-map-mobile-tags" aria-hidden="true">
                    {hereLines.map((line) => <i key={line.id} style={{ background: line.color }} title={line.name} />)}
                  </span>
                )}
              </button>
            );
          })}
        </div>
      )}
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
