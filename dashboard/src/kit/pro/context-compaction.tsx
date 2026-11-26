import { useState, type CSSProperties, type ReactNode } from 'react';

// Clean-room context meter from the leak-checked spec. No Pro source.
export type ContextKind = 'system' | 'tools' | 'user' | 'assistant' | 'tool' | 'file' | 'summary';
export interface ContextItem { id: string; kind: ContextKind; label: string; tokens: number; detail?: string; compacting?: boolean }
export interface ContextCompactionLabels {
  usedOfLimit: (used: string, limit: string) => ReactNode;
  leftCompactsAt: (left: string, compactAt: string) => string;
  empty: string;
  compactingPrefix: string;
}
export const contextCompactionDefaultLabels: ContextCompactionLabels = {
  usedOfLimit: (used, limit) => <><b>{used}</b> / {limit}</>,
  leftCompactsAt: (left, compactAt) => `남음 ${left} · 압축 기준 ${compactAt}`,
  empty: '압축할 항목이 없습니다.',
  compactingPrefix: '압축 중 ',
};
export interface ContextCompactionProps { title: string; model?: string; limit: number; threshold?: number; items?: ContextItem[]; visible?: number; labels?: Partial<ContextCompactionLabels>; className?: string; style?: CSSProperties }
const colors: Record<ContextKind, string> = { system: '#8f8f8f', tools: '#e8b000', user: '#0190fe', assistant: '#a651f0', tool: '#13b3a0', file: '#ee3e9e', summary: '#5fc88a' };
const styles = `
.aui-context-compaction { --aui-ctx-ink:var(--aui-fg,#171717); --aui-ctx-muted:var(--aui-fg-muted,#8d8d93); --aui-ctx-card:var(--aui-bg-1,#fff); --aui-ctx-tray:var(--aui-bg-2,#f7f7f8); --aui-ctx-track:var(--aui-line,#ececef); --aui-ctx-focus:var(--aui-focus,rgba(0,96,226,.7)); width:100%; min-width:0; padding:14px; border-radius:22px; background:var(--aui-ctx-tray); color:var(--aui-ctx-ink); font:13px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); font-variant-numeric:tabular-nums; }
:where(.dark,[data-theme="dark"]) .aui-context-compaction { --aui-ctx-ink:var(--aui-fg,#f3f3f4); --aui-ctx-muted:var(--aui-fg-muted,#a1a1a8); --aui-ctx-card:var(--aui-bg-1,#1c1c1e); --aui-ctx-tray:var(--aui-bg-2,#141416); --aui-ctx-track:var(--aui-line,rgb(255 255 255 / .08)); --aui-ctx-focus:var(--aui-focus,rgba(77,141,255,.8)); }
.aui-context-compaction-head { display:flex; gap:8px; align-items:center; margin-bottom:10px; }
.aui-context-compaction-head b { font-size:16px; }
.aui-context-compaction-head small { padding:3px 8px; border-radius:999px; background:var(--aui-ctx-card); color:var(--aui-ctx-muted); }
:where(.dark,[data-theme="dark"]) .aui-context-compaction-head small { background:var(--aui-bg-3,#2a2a2e); }
.aui-context-compaction-head span { margin-left:auto; font-size:18px; font-weight:650; }
.aui-context-compaction-panel { padding:12px 12px 4px; border-radius:18px; background:var(--aui-ctx-card); }
.aui-context-compaction-track { position:relative; height:8px; overflow:hidden; border-radius:99px; background:var(--aui-ctx-track); }
.aui-context-compaction-bar { display:flex; height:100%; }
.aui-context-compaction-mark { position:absolute; top:-3px; bottom:-3px; border-left:1px dashed #e08a00; }
.aui-context-compaction-meta { display:flex; flex-wrap:wrap; justify-content:space-between; gap:8px; margin:8px 0; color:var(--aui-ctx-muted); font-size:12px; }
.aui-context-compaction-meta b { color:var(--aui-ctx-ink); font-weight:650; }
.aui-context-compaction-row { display:flex; gap:8px; align-items:center; width:100%; min-height:44px; padding:0; border:0; border-radius:8px; background:transparent; color:inherit; text-align:left; cursor:pointer; }
.aui-context-compaction-row:focus-visible { outline:2px solid var(--aui-ctx-focus); outline-offset:2px; }
.aui-context-compaction-row[aria-current="true"] { font-weight:650; }
.aui-context-compaction-row span { min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.aui-context-compaction-row em { margin-left:6px; color:var(--aui-ctx-muted); font-style:normal; font-weight:450; }
.aui-context-compaction-row b { margin-left:auto; font-weight:500; }
`;
export function contextTotals(items: ContextItem[], limit: number) {
  const used = items.reduce((sum, item) => sum + Math.max(0, item.tokens), 0);
  return { used, left: Math.max(0, limit - used), ratio: limit ? used / limit : 0 };
}
export function ContextCompaction({ title, model, limit, threshold = 0.8, items = [], visible = 7, labels, className = '', style }: ContextCompactionProps) {
  const text: ContextCompactionLabels = { ...contextCompactionDefaultLabels, ...labels };
  const [selected, setSelected] = useState<string | null>(null);
  const totals = contextTotals(items, limit);
  const shown = items.slice(-visible);
  const compact = (value: number) => {
    if (value < 1000) return String(value);
    const kilo = value / 1000;
    if (kilo >= 100) return `${Math.round(kilo)}K`;
    const text = kilo.toFixed(1);
    return `${text.endsWith('.0') ? text.slice(0, -2) : text}K`;
  };
  return (
    <section className={`aui-context-compaction ${className}`} style={style}>
      <style>{styles}</style>
      <div className="aui-context-compaction-head"><b>{title}</b>{model && <small>{model}</small>}<span>{Math.round(totals.ratio * 100)}%</span></div>
      <div className="aui-context-compaction-panel">
        <div className="aui-context-compaction-track" aria-hidden="true">
          <div className="aui-context-compaction-bar" style={{ width: `${Math.min(100, totals.ratio * 100)}%` }}>{items.map((item) => <i key={item.id} style={{ width: `${item.tokens / Math.max(1, totals.used) * 100}%`, background: colors[item.kind] }} />)}</div>
          <i className="aui-context-compaction-mark" style={{ left: `${threshold * 100}%` }} />
        </div>
        <div className="aui-context-compaction-meta"><span>{text.usedOfLimit(compact(totals.used), compact(limit))}</span><span>{text.leftCompactsAt(compact(totals.left), compact(limit * threshold))}</span></div>
        {shown.length === 0 && <p>{text.empty}</p>}
        {shown.map((item) => <button key={item.id} type="button" className="aui-context-compaction-row" aria-current={selected === item.id ? 'true' : undefined} onClick={() => setSelected(item.id)}><span style={{ color: colors[item.kind] }} aria-hidden="true">●</span><span>{item.compacting ? text.compactingPrefix : ''}{item.label}{item.detail ? <em>{item.detail}</em> : null}</span><b>{compact(item.tokens)}</b></button>)}
      </div>
    </section>
  );
}
const sample: ContextItem[] = [
  { id: 'sys', kind: 'system', label: 'Orchestrator rules', tokens: 6200 },
  { id: 'tools', kind: 'tools', label: 'Tool schemas', tokens: 14800 },
  { id: 'u1', kind: 'user', label: 'Restart the adk-cc session', tokens: 420 },
  { id: 'a1', kind: 'assistant', label: 'Clear on Discord, not herdr', tokens: 1860 },
  { id: 't1', kind: 'tool', label: 'dcserver log', tokens: 9400, detail: '2' },
  { id: 'f1', kind: 'file', label: 'handoff-main-20261122.md', tokens: 5100 },
  { id: 's1', kind: 'summary', label: 'Earlier turns folded', tokens: 2400, detail: '2 entries · 18K' },
  { id: 'u2', kind: 'user', label: 'Quota reset in 4 hours', tokens: 280 },
  { id: 'a2', kind: 'assistant', label: 'Dashboard-only pass plan', tokens: 2200 },
];
export function ContextCompactionDemo({ state = 'default' }: { state?: 'default' | 'empty' | 'compacting' | 'long' }) {
  const items = state === 'empty' ? [] : state === 'compacting' ? sample.map((item) => item.id === 's1' ? { ...item, compacting: true } : item) : sample;
  return <div style={{ width: '100%', maxWidth: state === 'long' ? 360 : 552 }}><ContextCompaction title={state === 'long' ? '아주 긴 세션 제목 main-session-identifier' : 'Main session'} model="Sonnet 4.5" limit={200000} items={items} /></div>;
}
