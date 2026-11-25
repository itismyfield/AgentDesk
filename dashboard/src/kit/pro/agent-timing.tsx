import { useState, type CSSProperties } from 'react';

// Clean-room timing lanes from the leak-checked spec. No Pro source.
export type TimingKind = 'thinking' | 'tool' | 'waiting';
export interface TimingActor { id: string; name: string; kind: 'human' | 'agent'; color?: string }
export interface TimingSpan { actor: string; kind: TimingKind; start: number; end?: number; label?: string }
export interface TimingHandoff { from: string; to: string; at: number; label?: string }
export interface AgentTimingProps { actors: TimingActor[]; spans?: TimingSpan[]; handoffs?: TimingHandoff[]; now?: number; duration?: number; title?: string; className?: string; style?: CSSProperties }
const palette = ['rgba(166,81,240,.85)', 'rgba(20,179,240,.85)', 'rgba(95,200,138,.9)', 'rgba(238,62,158,.8)'];
const clock = (seconds: number) => `${Math.floor(seconds / 60)}:${String(Math.floor(seconds % 60)).padStart(2, '0')}`;
const styles = `
.aui-agent-timing, .aui-agent-timing * { box-sizing:border-box; }
.aui-agent-timing { --aui-time-surface:#fff; --aui-time-ink:#000; --aui-time-muted:rgb(0 0 0 / .55); --aui-time-grid:rgb(0 0 0 / .035); --aui-time-now:rgba(0,96,226,.78); --aui-time-wait:#e08a00; width:100%; max-width:100%; min-width:0; overflow-x:clip; margin-top:12px; padding:16px 18px 14px; border:1px solid rgb(0 0 0 / .06); border-radius:18px; background:#fff; color:var(--aui-time-ink); box-shadow:0 8px 24px rgb(15 23 42 / .04); font:13px/20px Inter, ui-sans-serif, system-ui, sans-serif; }
:where(.dark,[data-theme="dark"]) .aui-agent-timing { --aui-time-surface:#1c1c1e; --aui-time-ink:#fff; --aui-time-muted:rgb(255 255 255 / .62); --aui-time-grid:rgb(255 255 255 / .05); --aui-time-now:rgba(77,141,255,.85); --aui-time-wait:#ffb35c; background:#1c1c1e; border-color:rgb(255 255 255 / .08); }
.aui-agent-timing-head, .aui-agent-timing-row { display:grid; grid-template-columns:120px minmax(0,1fr); gap:8px; align-items:center; }
.aui-agent-timing-head { margin-bottom:8px; font-weight:600; }
.aui-agent-timing-head span:last-child, .aui-agent-timing-axis { color:var(--aui-time-muted); font-size:12px; font-weight:400; text-align:right; }
.aui-agent-timing-name { display:flex; gap:8px; align-items:center; min-width:0; font-weight:500; }
.aui-agent-timing-name small { display:block; color:var(--aui-time-muted); font-size:11px; font-weight:400; }
.aui-agent-timing-lane { position:relative; height:64px; width:100%; padding:0; border:0; background:repeating-linear-gradient(90deg, transparent, transparent calc(14.28% - 1px), var(--aui-time-grid) calc(14.28% - 1px), var(--aui-time-grid) 14.28%); }
.aui-agent-timing-plot { position:relative; min-width:0; }
.aui-agent-timing-span { position:absolute; top:21px; height:22px; overflow:hidden; padding:0 8px; border:1px solid var(--lane, #8b8b92); border-radius:8px; background:color-mix(in srgb, var(--lane, #8b8b92) 16%, transparent); color:var(--lane, #8b8b92); font:12px/20px Inter, sans-serif; text-overflow:ellipsis; white-space:nowrap; pointer-events:none; }
.aui-agent-timing-span[data-kind="tool"] { background:transparent; }
.aui-agent-timing-span[data-kind="waiting"] { --lane: var(--aui-time-wait); border-style:dashed; background:transparent; }
.aui-agent-timing-now { position:absolute; top:-8px; bottom:-28px; width:2px; background:var(--aui-time-now); }
.aui-agent-timing-axis { position:relative; display:grid; grid-template-columns:120px minmax(0,1fr); }
.aui-agent-timing-ticks { position:relative; display:flex; justify-content:space-between; min-width:0; }
.aui-agent-timing-pill { position:absolute; top:-14px; padding:2px 8px; border-radius:999px; background:var(--aui-time-now); color:#fff; font-size:12px; transform:translateX(-50%); }
@container aui-time (max-width:520px) { .aui-agent-timing-head, .aui-agent-timing-row, .aui-agent-timing-axis { grid-template-columns:72px minmax(0,1fr); } }
`;
export function AgentTiming({ actors, spans = [], handoffs = [], now, duration, title = '실행', className = '', style }: AgentTimingProps) {
  const end = duration ?? Math.max(60, ...spans.map((span) => span.end ?? span.start), ...handoffs.map((item) => item.at), now ?? 0) * 1.05;
  const [open, setOpen] = useState<string | null>(null);
  const left = (value: number) => `${Math.max(0, Math.min(100, value / end * 100))}%`;
  return (
    <section className={`aui-agent-timing ${className}`} style={{ containerType: 'inline-size', containerName: 'aui-time', ...style }} aria-label={title}>
      <style>{styles}</style>
      <div className="aui-agent-timing-head"><span>{title}</span><span>{clock(now ?? end)}</span></div>
      {actors.map((actor, index) => (
        <div className="aui-agent-timing-row" key={actor.id}>
          <div className="aui-agent-timing-name"><span aria-hidden="true">{actor.kind === 'agent' ? '✦' : '▦'}</span><span>{actor.name}<small>Idle</small></span></div>
          <button type="button" className="aui-agent-timing-lane" aria-label={actor.name} onClick={() => setOpen(open === actor.id ? null : actor.id)}>
            {spans.filter((span) => span.actor === actor.id || span.actor === actor.name).map((span) => {
              const stop = span.end ?? now ?? end;
              const id = `${actor.id}-${span.start}`;
              return <span key={id} className="aui-agent-timing-span" data-kind={span.kind} style={{ left: left(span.start), width: `calc(${left(stop)} - ${left(span.start)})`, ['--lane' as string]: actor.color ?? palette[index % palette.length] }}>{span.label}</span>;
            })}
            {now !== undefined && <span className="aui-agent-timing-now" style={{ left: left(now) }} />}
          </button>
        </div>
      ))}
      <div className="aui-agent-timing-axis" aria-hidden="true"><span /><div className="aui-agent-timing-ticks">{[0, 1, 2, 3, 4, 5, 6, 7].map((minute) => <span key={minute}>{minute}:00</span>)}{now !== undefined && <span className="aui-agent-timing-pill" style={{ left: left(now) }}>{clock(now)}</span>}</div></div>
      {open && <p role="status">{spans.find((span) => `${actors.find((actor) => actor.id === span.actor || actor.name === span.actor)?.id}-${span.start}` === open)?.label}</p>}
    </section>
  );
}
export function AgentTimingDemo({ state = 'default' }: { state?: 'default' | 'empty' | 'long' }) {
  const actors: TimingActor[] = [{ id: 'minseo', name: 'Minseo', kind: 'human' }, { id: 'cc', name: 'adk-cc', kind: 'agent', color: 'rgba(20,179,240,.85)' }, { id: 'codex', name: 'Codex', kind: 'agent', color: 'rgba(166,81,240,.85)' }];
  const spans: TimingSpan[] = state === 'empty' ? [] : [
    { actor: 'cc', kind: 'thinking', start: 0, end: 45, label: 'Read the failing test' },
    { actor: 'cc', kind: 'tool', start: 45, end: 110, label: 'Open issue 6082' },
    { actor: 'cc', kind: 'waiting', start: 110, end: 150, label: 'Handoff' },
    { actor: 'minseo', kind: 'waiting', start: 80, end: 150, label: 'Review the plan' },
    { actor: 'codex', kind: 'thinking', start: 150, end: 210, label: 'Plan the clock fix' },
    { actor: 'codex', kind: 'tool', start: 210, end: 320, label: 'Edit the test helper' },
    { actor: 'codex', kind: 'thinking', start: 320, end: 360, label: 'Check the remaining assertions' },
  ];
  return <div style={{ width: '100%', maxWidth: state === 'long' ? 360 : 872 }}><AgentTiming title={state === 'long' ? '아주 긴 야간 수리 nightly-repair-session-identifier' : 'Nightly repair'} actors={actors} spans={spans} handoffs={[{ from: 'cc', to: 'codex', at: 150, label: 'Fix the deadline flake' }]} now={state === 'empty' ? undefined : 360} duration={420} /></div>;
}
