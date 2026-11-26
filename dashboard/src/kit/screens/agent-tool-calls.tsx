import { useEffect, useState, type CSSProperties } from 'react';
import { Activity, Bot, Folder, Pause, Play, Plus, Search, Settings, SquareStack, Wrench } from 'lucide-react';

// Clean-room screen. Layout follows the reference renders; names are AgentDesk's, not the marketing sample.
export type ToolMode = 'auto' | 'ask';
export type CallStatus = 'running' | 'done' | 'failed' | 'waiting';
export interface DeskTool { id: string; name: string; calls: number; latency?: string; mode?: ToolMode; connected?: boolean }
export interface DeskCall { id: string; toolId: string; at: string; status: CallStatus; input: string; tokens?: number; duration?: string; run: string }
export interface AgentToolCallsLabels {
  navAria: string;
  searchAria: string;
  liveBadge: string;
  connectTool: string;
  resume: string;
  pause: string;
  loading: string;
  empty: string;
  callsCount: (count: number) => string;
  noCalls: string;
  askFirst: string;
  colTime: string;
  colTool: string;
  colInput: string;
  colStatus: string;
  colDuration: string;
  colTokens: string;
  colRun: string;
  status: Record<CallStatus, string>;
  open: string;
  close: string;
  approve: string;
  reject: string;
  failedOnly: (count: number) => string;
  detailAria: string;
}
export const agentToolCallsDefaultLabels: AgentToolCallsLabels = {
  navAria: '탐색',
  searchAria: '검색',
  liveBadge: 'LIVE',
  connectTool: 'Connect tool',
  resume: '재개',
  pause: '에이전트 멈추기',
  loading: '불러오는 중',
  empty: '연결된 도구와 호출이 없습니다.',
  callsCount: (count) => `${count} calls`,
  noCalls: 'No calls',
  askFirst: 'Ask first',
  colTime: 'TIME',
  colTool: 'TOOL',
  colInput: 'INPUT',
  colStatus: 'STATUS',
  colDuration: 'DURATION',
  colTokens: 'TOKENS',
  colRun: 'RUN',
  status: { running: 'Running', done: 'Done', failed: 'Failed', waiting: 'Needs you' },
  open: '열기',
  close: '닫기',
  approve: '승인',
  reject: '거절',
  failedOnly: (count) => `실패만 ${count}`,
  detailAria: '호출 상세',
};
export interface AgentToolCallsProps { workspace?: string; agent?: string; model?: string; tools?: DeskTool[]; calls?: DeskCall[]; loading?: boolean; empty?: boolean; live?: boolean; embedded?: boolean; labels?: Partial<AgentToolCallsLabels>; onApprove?: (id: string) => void; onReject?: (id: string) => void; onPause?: (paused: boolean) => void; className?: string; style?: CSSProperties }
const tools: DeskTool[] = [
  { id: 'web', name: 'Search', calls: 4, latency: '6.4s' },
  { id: 'files', name: 'Files', calls: 2, latency: '300ms' },
  { id: 'notes', name: 'Notes', calls: 2, latency: '900ms' },
  { id: 'pay', name: 'Billing', calls: 3, latency: '553ms' },
  { id: 'issues', name: 'Issues', calls: 2, latency: '790ms' },
  { id: 'channel', name: 'Channel', calls: 2, latency: '790ms', mode: 'ask' },
  { id: 'mail', name: 'Mail', calls: 0 },
];
const calls: DeskCall[] = [
  { id: 'c1', toolId: 'web', at: '09:40:58', status: 'running', input: 'query:"session panel empty state"', duration: '3.0s', run: 'run-desk-1' },
  { id: 'c2', toolId: 'channel', at: '09:40:22', status: 'waiting', input: 'channel:#ops text:"배포 확인"', run: 'run-desk-1' },
  { id: 'c3', toolId: 'files', at: '09:40:08', status: 'done', input: 'path:dashboard/session-panel', duration: '420ms', tokens: 2100, run: 'run-desk-1' },
  { id: 'c4', toolId: 'issues', at: '09:39:50', status: 'done', input: 'title:"Card payments queue"', duration: '820ms', tokens: 140, run: 'run-desk-1' },
  { id: 'c5', toolId: 'pay', at: '09:39:20', status: 'done', input: 'invoice:weekly', duration: '640ms', tokens: 410, run: 'run-desk-1' },
  { id: 'c6', toolId: 'pay', at: '09:38:55', status: 'failed', input: 'invoice:declined', duration: '380ms', tokens: 60, run: 'run-desk-1' },
  { id: 'c7', toolId: 'notes', at: '09:38:20', status: 'done', input: 'query:"card declined do not honor"', duration: '920ms', tokens: 1200, run: 'run-desk-1' },
];
const styles = `
.aui-agent-tool-calls { --ink:var(--aui-fg,#171717); --muted:var(--aui-fg-muted,#8d8d93); --line:var(--aui-line,#ececef); --card:var(--aui-bg-1,#fff); --canvas:var(--aui-bg-2,#f7f7f8); display:grid; grid-template-columns:216px minmax(0,1fr); width:100%; max-width:100%; min-height:100dvh; overflow-x:clip; background:var(--card); color:var(--ink); font:13px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls { --ink:var(--aui-fg,#f3f3f4); --muted:var(--aui-fg-muted,#a1a1a8); --line:var(--aui-line,rgb(255 255 255 / .08)); --card:var(--aui-bg-1,#141416); --canvas:var(--aui-bg-2,#101114); }
.aui-agent-tool-calls[data-embedded="true"] { grid-template-columns:minmax(0,1fr); min-height:0; border-radius:16px; border:1px solid var(--line); }
.aui-agent-tool-calls nav { display:flex; flex-direction:column; gap:4px; min-width:0; padding:16px 12px; border-right:1px solid var(--line); background:#fbfbfc; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls nav { background:#18181b; }
.aui-agent-tool-calls nav strong { padding:8px; font-size:14px; }
.aui-agent-tool-calls nav button, .aui-agent-tool-calls header button, .aui-agent-tool-calls-table button, .aui-agent-tool-calls-table-wrap > button, .aui-agent-tool-calls-detail button { display:inline-flex; gap:8px; align-items:center; min-width:32px; min-height:32px; padding:0 10px; border:0; border-radius:10px; background:transparent; color:inherit; font:inherit; text-align:left; cursor:pointer; }
@media (pointer: coarse), (max-width: 900px) {
  .aui-agent-tool-calls nav button, .aui-agent-tool-calls header button, .aui-agent-tool-calls-table button, .aui-agent-tool-calls-table-wrap > button, .aui-agent-tool-calls-detail button, .aui-agent-tool-calls-tools button { min-width:44px; min-height:44px; }
}
.aui-agent-tool-calls nav button[aria-current="page"] { background:#ececef; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls nav button[aria-current="page"] { background:#202022; }
.aui-agent-tool-calls-main { min-width:0; background:var(--canvas); }
.aui-agent-tool-calls header { display:flex; flex-wrap:wrap; gap:8px; align-items:center; padding:10px 16px; }
.aui-agent-tool-calls h1 { margin:0; font-size:16px; font-weight:650; }
.aui-agent-tool-calls h3 { margin:0; font-size:14px; font-weight:650; }
.aui-agent-tool-calls-live { padding:1px 6px; border-radius:999px; background:#e7f6ee; color:#178a4b; font-size:11px; font-weight:700; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls-live { background:#2a2a2e; color:#c6c6cb; }
.aui-agent-tool-calls-canvas { position:relative; min-height:300px; margin:0 16px; padding:18px 16px 20px; border:1px solid var(--line); border-radius:16px; background:radial-gradient(circle, rgb(0 0 0 / .14) 1px, transparent 1.2px) 0 0 / 16px 16px, var(--card); }
.aui-agent-tool-calls-agent { position:relative; z-index:1; display:flex; gap:8px; align-items:center; width:max-content; max-width:100%; margin:0 auto 28px; padding:10px 14px; border:1px solid var(--line); border-radius:14px; background:var(--card); }
.aui-agent-tool-calls-links { position:absolute; left:8%; right:8%; top:62px; height:78px; width:84%; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls-links path { stroke:#6ac0f3; }
.aui-agent-tool-calls-tools { position:relative; z-index:1; display:flex; flex-direction:column; align-items:center; gap:16px; }
.aui-agent-tool-calls-row { display:flex; flex-wrap:wrap; justify-content:center; gap:14px; width:min(760px, 100%); }
.aui-agent-tool-calls-tools button { width:176px; min-height:58px; padding:8px 10px; border:1px solid var(--line); border-radius:14px; background:var(--card); color:inherit; text-align:left; font:inherit; }
.aui-agent-tool-calls-tools small { display:block; color:var(--muted); }
.aui-agent-tool-calls-table-wrap { margin:16px; overflow-x:auto; border:1px solid var(--line); border-radius:16px; background:var(--card); }
.aui-agent-tool-calls-table { width:100%; border-collapse:collapse; }
.aui-agent-tool-calls-table th { height:34px; padding:0 10px; color:var(--muted); font-size:11px; font-weight:600; letter-spacing:.04em; text-align:left; }
.aui-agent-tool-calls-table td { height:34px; padding:0 10px; border-top:1px solid var(--line); overflow-wrap:anywhere; vertical-align:middle; }
.aui-agent-tool-calls-table button { min-height:0; height:22px; padding:0 8px; }
@media (pointer: coarse), (max-width: 900px) {
  .aui-agent-tool-calls-table button, .aui-agent-tool-calls-table-wrap > button { height:auto; min-height:44px; min-width:44px; }
}
.aui-agent-tool-calls-status { display:inline-flex; align-items:center; min-height:24px; padding:0 8px; border-radius:999px; font-weight:650; }
.aui-agent-tool-calls-status[data-status="running"] { background:#e8f3ff; color:#006dcb; }
.aui-agent-tool-calls-status[data-status="done"] { background:#e7f6ee; color:#178a4b; }
.aui-agent-tool-calls-status[data-status="failed"] { background:#fdecec; color:#d14343; }
.aui-agent-tool-calls-status[data-status="waiting"] { background:#fff4e5; color:#b86a00; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls-status[data-status="running"] { background:#0077ff3a; color:#4fb0fe; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls-status[data-status="done"] { background:#00f04a1f; color:#5fc88a; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls-status[data-status="failed"] { background:#ff35232b; color:#ff8a7a; }
:where(.dark,[data-theme="dark"]) .aui-agent-tool-calls-status[data-status="waiting"] { background:#fb6a0025; color:#ffb35c; }
.aui-agent-tool-calls-detail { position:fixed; right:16px; bottom:16px; z-index:3; width:min(388px, calc(100vw - 32px)); padding:16px; border-radius:16px; background:var(--card); box-shadow:0 16px 40px rgb(0 0 0 / .16); }
.aui-agent-tool-calls[data-embedded="true"] .aui-agent-tool-calls-detail { position:static; width:auto; margin:0 16px 16px; border:1px solid var(--line); box-shadow:none; }
@container aui-tools (max-width:720px) {
  .aui-agent-tool-calls { grid-template-columns:1fr; }
  .aui-agent-tool-calls nav { flex-direction:row; overflow:auto; border-right:0; border-bottom:1px solid var(--line); }
  .aui-agent-tool-calls-tools { grid-template-columns:1fr 1fr; }
  .aui-agent-tool-calls-table, .aui-agent-tool-calls-table tr, .aui-agent-tool-calls-table td, .aui-agent-tool-calls-table th { display:block; }
}
@media (max-width: 900px) {
  .aui-agent-tool-calls { grid-template-columns:1fr; }
  .aui-agent-tool-calls nav { flex-direction:row; overflow:auto; border-right:0; border-bottom:1px solid var(--line); }
  .aui-agent-tool-calls-tools { grid-template-columns:1fr 1fr; }
}
`;
export function AgentToolCalls({ workspace = 'AgentDesk', agent = '운영 에이전트', model = 'Sonnet 4.5', tools: toolList = tools, calls: callList = calls, loading = false, empty = false, live = true, embedded = false, labels, onApprove, onReject, onPause, className = '', style }: AgentToolCallsProps) {
  const text: AgentToolCallsLabels = { ...agentToolCallsDefaultLabels, ...labels, status: { ...agentToolCallsDefaultLabels.status, ...labels?.status } };
  const [failedOnly, setFailedOnly] = useState(false);
  const [paused, setPaused] = useState(false);
  const [open, setOpen] = useState<string | null>(null);
  const [nav, setNav] = useState(false);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === 'Escape') { setOpen(null); setNav(false); } };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
  const visible = empty ? [] : callList.filter((call) => !failedOnly || call.status === 'failed');
  const chosen = callList.find((call) => call.id === open);
  return (
    <div className={`aui-agent-tool-calls ${className}`} data-embedded={embedded ? 'true' : undefined} style={{ containerType: 'inline-size', containerName: 'aui-tools', ...style }}>
      <style>{styles}</style>
      {!embedded && (
        <nav aria-label={text.navAria}>
          <strong>{workspace}</strong>
          <button type="button" aria-label={text.searchAria}><Search size={16} aria-hidden="true" />Search</button>
          <button type="button"><Activity size={16} aria-hidden="true" />Runs</button>
          <button type="button" aria-current="page"><Bot size={16} aria-hidden="true" />Agents</button>
          <button type="button"><SquareStack size={16} aria-hidden="true" />Traces</button>
          <button type="button" aria-expanded={nav} onClick={() => setNav((value) => !value)}><Wrench size={16} aria-hidden="true" />Tools</button>
          <button type="button"><Settings size={16} aria-hidden="true" />Settings</button>
        </nav>
      )}
      <main className="aui-agent-tool-calls-main">
        <header>
          {embedded ? <h3>{agent}</h3> : <h1>{agent}</h1>}
          {live && <span className="aui-agent-tool-calls-live">{text.liveBadge}</span>}
          <span>{model}</span>
          {!embedded && <button type="button" style={{ marginLeft: 'auto' }}><Plus size={16} aria-hidden="true" />{text.connectTool}</button>}
          {(!embedded || onPause) && <button type="button" style={embedded ? { marginLeft: 'auto' } : undefined} onClick={() => { setPaused((value) => !value); onPause?.(!paused); }}>{paused ? <Play size={16} aria-hidden="true" /> : <Pause size={16} aria-hidden="true" />}{paused ? text.resume : text.pause}</button>}
        </header>
        {loading && <p aria-busy="true">{text.loading}</p>}
        {!loading && empty && <p>{text.empty}</p>}
        {!loading && !empty && (
          <div className="aui-agent-tool-calls-canvas">
            <div className="aui-agent-tool-calls-agent"><Bot size={16} aria-hidden="true" /><span><b>{agent}</b><small style={{ display: 'block', color: 'var(--muted)' }}>{model}</small></span>{visible.some((call) => call.status === 'waiting') && <span className="aui-agent-tool-calls-status" data-status="waiting">{text.status.waiting}</span>}</div>
            <svg className="aui-agent-tool-calls-links" viewBox="0 0 800 80" aria-hidden="true"><path d="M400 0 V18 H70 V48 M400 18 H250 V48 M400 18 H550 V48 M400 18 H730 V48 M160 48 V80 M400 48 V80 M640 48 V80" fill="none" stroke="#7ec8f8" strokeWidth="2" /></svg>
            <div className="aui-agent-tool-calls-tools">
              <div className="aui-agent-tool-calls-row">{toolList.slice(0, 4).map((tool) => <button key={tool.id} type="button"><Folder size={14} aria-hidden="true" /> {tool.name}<small>{tool.calls ? text.callsCount(tool.calls) : text.noCalls} {tool.latency} {tool.mode === 'ask' ? text.askFirst : ''}</small></button>)}</div>
              {toolList.length > 4 && <div className="aui-agent-tool-calls-row">{toolList.slice(4).map((tool) => <button key={tool.id} type="button"><Folder size={14} aria-hidden="true" /> {tool.name}<small>{tool.calls ? text.callsCount(tool.calls) : text.noCalls} {tool.latency} {tool.mode === 'ask' ? text.askFirst : ''}</small></button>)}</div>}
            </div>
          </div>
        )}
        {!loading && !empty && (
          <div className="aui-agent-tool-calls-table-wrap">
            <table className="aui-agent-tool-calls-table">
              <thead><tr><th>{text.colTime}</th><th>{text.colTool}</th><th>{text.colInput}</th><th>{text.colStatus}</th><th>{text.colDuration}</th><th>{text.colTokens}</th><th>{text.colRun}</th><th /></tr></thead>
              <tbody>{visible.map((call) => <tr key={call.id}><td>{call.at}</td><td>{toolList.find((tool) => tool.id === call.toolId)?.name ?? call.toolId}</td><td>{call.input}</td><td><span className="aui-agent-tool-calls-status" data-status={call.status}>{text.status[call.status]}</span></td><td>{call.duration ?? '—'}</td><td>{call.tokens ?? '—'}</td><td>{call.run}</td><td><button type="button" onClick={() => setOpen(open === call.id ? null : call.id)}>{text.open}</button>{call.status === 'waiting' && (onApprove || onReject) && <>{onApprove && <button type="button" onClick={() => onApprove(call.id)}>{text.approve}</button>}{onReject && <button type="button" onClick={() => onReject(call.id)}>{text.reject}</button>}</>}</td></tr>)}</tbody>
            </table>
            <button type="button" aria-pressed={failedOnly} onClick={() => setFailedOnly((value) => !value)}>{text.failedOnly(callList.filter((call) => call.status === 'failed').length)}</button>
          </div>
        )}
        {chosen && (
          <aside className="aui-agent-tool-calls-detail" role="dialog" aria-label={text.detailAria}>
            <p>{text.status[chosen.status]} · {chosen.at}</p>
            <p>{chosen.input}</p>
            <button type="button" onClick={() => setOpen(null)}>{text.close}</button>
            {chosen.status === 'waiting' && (onApprove || onReject) && <>{onApprove && <button type="button" onClick={() => onApprove(chosen.id)}>{text.approve}</button>}{onReject && <button type="button" onClick={() => onReject(chosen.id)}>{text.reject}</button>}</>}
          </aside>
        )}
      </main>
    </div>
  );
}
export function AgentToolCallsDemo({ variant = 'default' }: { variant?: 'default' | 'loading' | 'empty' | 'long' }) {
  return <AgentToolCalls loading={variant === 'loading'} empty={variant === 'empty'} tools={variant === 'long' ? tools.map((tool) => ({ ...tool, name: tool.name + ' 아주 긴 도구 이름' })) : tools} calls={variant === 'long' ? calls.map((call) => ({ ...call, input: call.input + ' 긴 인자 문자열 '.repeat(8) })) : calls} onApprove={() => undefined} onReject={() => undefined} onPause={() => undefined} />;
}
