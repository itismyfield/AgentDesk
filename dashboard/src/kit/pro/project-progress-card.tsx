"use client";
// Clean-room implementation derived from public Arc UI project-board overview specification (2026-11-26).
// Provides an embedded completion ring, 4-cell live KPI strip, and interactive group/milestone progress track. No upstream Pro source code was used.

import { useRef, useState, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import { ChevronDown, ChevronRight, X } from 'lucide-react';

export type ProjectProgressHealth = 'on-track' | 'at-risk' | 'paused';

export interface ProjectProgressGroup {
  id: string;
  name: string;
  total: number;
  done: number;
  running?: number;
  attention?: number;
  meta?: string;
}

export interface ProjectProgressLabels {
  regionAria: string;
  complete: string;
  openTasks: string;
  attention: string;
  groupsStat: string;
  groupsHeading: string;
  groupsAria: string;
  groupsSummary: (doneGroups: number, totalGroups: number) => string;
  clearGroup: string;
  showAllGroups: (total: number) => string;
  showFewerGroups: string;
  groupDone: string;
  groupTasks: (done: number, total: number) => string;
  groupRunning: (count: number) => string;
  groupAttention: (count: number) => string;
  noAttention: string;
  singleTrack: string;
  activeGroupsSub: (count: number) => string;
}

export const defaultProjectProgressLabels: ProjectProgressLabels = {
  regionAria: 'Project progress summary',
  complete: 'Complete',
  openTasks: 'Open tasks',
  attention: 'Needs attention',
  groupsStat: 'Tracks done',
  groupsHeading: 'Progress by group',
  groupsAria: 'Groups',
  groupsSummary: (done, total) => `${done} of ${total} done`,
  clearGroup: 'Clear filter',
  showAllGroups: (total) => `Show all ${total} groups`,
  showFewerGroups: 'Show fewer',
  groupDone: 'Done',
  groupTasks: (done, total) => `${done}/${total}`,
  groupRunning: (n) => `${n} running`,
  groupAttention: (n) => `${n} blocked`,
  noAttention: 'All clear',
  singleTrack: 'Single track',
  activeGroupsSub: (n) => (n > 0 ? `${n} active` : 'Idle'),
};

export interface ProjectProgressCardProps {
  totalCount: number;
  doneCount: number;
  doneSub?: string;
  openCount: number;
  runningCount?: number;
  openSub?: string;
  attentionCount?: number;
  attentionSub?: string;
  groups?: ProjectProgressGroup[];
  selectedGroupId?: string | null;
  defaultSelectedGroupId?: string | null;
  onSelectGroup?: (groupId: string | null) => void;
  maxPreviewGroups?: number;
  health?: ProjectProgressHealth;
  labels?: Partial<ProjectProgressLabels>;
  children?: ReactNode;
  className?: string;
  style?: CSSProperties;
}

const PROGRESS_STYLES = `
.aui-project-progress-root { --pp-card:var(--bg-1,var(--aui-bg-1,#fff)); --pp-subtle:var(--bg-2,var(--aui-bg-2,#f5f5f6)); --pp-fg:var(--fg,var(--aui-fg,#141414)); --pp-fg-dim:var(--fg-dim,var(--aui-fg-dim,#525252)); --pp-fg-muted:var(--fg-muted,var(--aui-fg-muted,#6e6e73)); --pp-line:var(--line,var(--aui-line,#e5e5e7)); --pp-line-soft:var(--line-soft,var(--aui-line-soft,#ededef)); --pp-ok:var(--ok,#059669); --pp-info:var(--info,#0284c7); --pp-warn:var(--warn,#b45309); --pp-sel-bg:color-mix(in srgb,var(--accent,#0284c7) 10%,var(--pp-subtle)); --pp-sel-bdr:var(--accent,#0284c7); container-type:inline-size; display:flex; flex-direction:column; gap:12px; min-width:0; color:var(--pp-fg); font-family:Inter,var(--aui-font-sans,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif); word-break:keep-all; }
:where(.dark,[data-theme="dark"]) .aui-project-progress-root { --pp-card:var(--bg-1,#17181c); --pp-subtle:var(--bg-2,#202228); --pp-fg:var(--fg,#f1f1f3); --pp-fg-dim:var(--fg-dim,#c2c4cc); --pp-fg-muted:var(--fg-muted,#9497a1); --pp-line:var(--line,rgba(255,255,255,.1)); --pp-line-soft:var(--line-soft,rgba(255,255,255,.07)); --pp-ok:var(--ok,#34d399); --pp-info:var(--info,#38bdf8); --pp-warn:var(--warn,#fbbf24); }
.aui-project-progress-root *, .aui-project-progress-root *::before, .aui-project-progress-root *::after { box-sizing:border-box; }
.aui-project-progress-stats { margin:0; border:1px solid var(--pp-line); border-radius:12px; background:var(--pp-card); display:grid; grid-template-columns:repeat(4,minmax(0,1fr)); overflow:hidden; }
@container (max-width: 560px) { .aui-project-progress-stats { grid-template-columns:repeat(2,minmax(0,1fr)); } .aui-project-progress-cell:nth-child(2) { border-right:0; } .aui-project-progress-cell:nth-child(-n+2) { border-bottom:1px solid var(--pp-line-soft); } }
.aui-project-progress-cell { padding:13px 15px 14px; border-right:1px solid var(--pp-line-soft); display:flex; flex-direction:column; gap:5px; min-width:0; }
.aui-project-progress-cell:last-child { border-right:0; }
.aui-project-progress-cell dt { font-size:11.5px; font-weight:500; color:var(--pp-fg-muted); white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
.aui-project-progress-cell dd { margin:0; display:flex; align-items:center; gap:7px; font-size:20px; font-weight:650; line-height:1.2; letter-spacing:-.02em; font-variant-numeric:tabular-nums; color:var(--pp-fg); }
.aui-project-progress-cell dd small { font-size:14px; font-weight:500; color:var(--pp-fg-muted); }
.aui-project-progress-ring { flex-shrink:0; }
.aui-project-progress-bar { margin-top:3px; height:5px; border-radius:999px; background:var(--pp-subtle); overflow:hidden; }
.aui-project-progress-bar > div { height:100%; border-radius:999px; background:var(--pp-ok); transition:width 180ms ease; }
.aui-project-progress-sub { font-size:11.5px; color:var(--pp-fg-muted); font-variant-numeric:tabular-nums; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
.aui-project-progress-sub.is-warn { color:var(--pp-warn); font-weight:500; }
.aui-project-progress-groups-wrap { border:1px solid var(--pp-line); border-radius:12px; background:var(--pp-card); padding:12px 14px 14px; display:flex; flex-direction:column; gap:10px; min-width:0; }
.aui-project-progress-subhead { display:flex; align-items:center; justify-content:space-between; flex-wrap:wrap; gap:8px; }
.aui-project-progress-subhead-left { display:flex; align-items:baseline; gap:8px; min-width:0; }
.aui-project-progress-subhead-title { margin:0; font-size:13px; font-weight:600; color:var(--pp-fg); }
.aui-project-progress-subhead-meta { font-size:12px; color:var(--pp-fg-muted); font-variant-numeric:tabular-nums; }
.aui-project-progress-actions { display:inline-flex; align-items:center; gap:6px; }
.aui-project-progress-link-btn { border:1px solid var(--pp-line); border-radius:7px; background:var(--pp-subtle); color:var(--pp-fg-dim); padding:4px 9px; min-height:28px; font-size:12px; font-weight:500; display:inline-flex; align-items:center; gap:4px; white-space:nowrap; cursor:pointer; }
.aui-project-progress-link-btn:hover { color:var(--pp-fg); border-color:var(--pp-fg-muted); }
.aui-project-progress-track { display:grid; grid-template-columns:repeat(auto-fill,minmax(148px,1fr)); gap:8px; min-width:0; }
@container (max-width: 440px) { .aui-project-progress-track { grid-template-columns:repeat(2,minmax(0,1fr)); } }
.aui-project-progress-card { position:relative; padding:9px 11px 10px; border:1px solid var(--pp-line-soft); border-radius:10px; background:var(--pp-subtle); color:var(--pp-fg); display:flex; flex-direction:column; gap:6px; cursor:pointer; text-align:left; min-width:0; }
.aui-project-progress-card:hover { border-color:var(--pp-line); }
.aui-project-progress-card[aria-checked="true"] { background:var(--pp-sel-bg); border-color:var(--pp-sel-bdr); }
.aui-project-progress-card:focus-visible, .aui-project-progress-link-btn:focus-visible { outline:2px solid var(--pp-sel-bdr); outline-offset:2px; }
.aui-project-progress-line { width:100%; height:3.5px; border-radius:999px; background:var(--pp-line); overflow:hidden; }
.aui-project-progress-line > div { height:100%; border-radius:999px; }
.aui-project-progress-name { font-size:12.5px; font-weight:600; line-height:1.3; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.aui-project-progress-card-meta { display:flex; align-items:center; justify-content:space-between; gap:6px; font-size:11.5px; color:var(--pp-fg-muted); font-variant-numeric:tabular-nums; min-width:0; }
.aui-project-progress-card-meta span { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.aui-project-progress-card-flag { flex-shrink:0; font-weight:500; }
.aui-project-progress-card-flag.is-warn { color:var(--pp-warn); }
.aui-project-progress-card-flag.is-info { color:var(--pp-info); }
.aui-project-progress-card-flag.is-ok { color:var(--pp-ok); }
@media (pointer: coarse), (max-width: 899px) { .aui-project-progress-card, .aui-project-progress-link-btn { min-height:44px; } }
@media (prefers-reduced-motion: reduce) { .aui-project-progress-root * { transition:none !important; } }
`;

function CompletionRing({ pct, health }: { pct: number; health: ProjectProgressHealth }) {
  const clamped = Math.max(0, Math.min(100, Number.isFinite(pct) ? pct : 0));
  const r = 7.5;
  const c = 2 * Math.PI * r;
  const stroke = health === 'at-risk' ? 'var(--pp-warn)' : health === 'paused' ? 'var(--pp-fg-muted)' : 'var(--pp-ok)';
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" aria-hidden="true" className="aui-project-progress-ring">
      <circle cx="10" cy="10" r={r} fill="none" stroke="currentColor" strokeOpacity="0.16" strokeWidth="2.2" />
      <circle cx="10" cy="10" r={r} fill="none" stroke={stroke} strokeWidth="2.2" strokeDasharray={`${c}`} strokeDashoffset={`${c * (1 - clamped / 100)}`} strokeLinecap="round" transform="rotate(-90 10 10)" />
    </svg>
  );
}

export function ProjectProgressCard({
  totalCount, doneCount, doneSub, openCount, runningCount = 0, openSub, attentionCount = 0, attentionSub,
  groups = [], selectedGroupId: controlledGroupId, defaultSelectedGroupId = null, onSelectGroup,
  maxPreviewGroups = 8, health: explicitHealth, labels: labelOverrides, children, className, style,
}: ProjectProgressCardProps) {
  const L: ProjectProgressLabels = { ...defaultProjectProgressLabels, ...labelOverrides };
  const [internalGroupId, setInternalGroupId] = useState<string | null>(defaultSelectedGroupId);
  const [expandedAll, setExpandedAll] = useState(false);
  const radioRefs = useRef<Array<HTMLButtonElement | null>>([]);

  const selectedGroupId = controlledGroupId !== undefined ? controlledGroupId : internalGroupId;
  const selectGroup = (next: string | null) => {
    if (controlledGroupId === undefined) setInternalGroupId(next);
    onSelectGroup?.(next);
  };

  const safeTotal = Math.max(0, totalCount);
  const safeDone = Math.max(0, Math.min(safeTotal, doneCount));
  const pct = safeTotal > 0 ? Math.round((safeDone / safeTotal) * 100) : 0;
  const derivedHealth: ProjectProgressHealth = explicitHealth ?? (attentionCount > 0 ? 'at-risk' : runningCount === 0 && safeDone < safeTotal ? 'paused' : 'on-track');
  const doneGroupsCount = groups.filter((g) => g.total > 0 && g.done >= g.total).length;
  const activeGroupsCount = groups.filter((g) => (g.running ?? 0) > 0).length;
  const limit = Math.max(1, maxPreviewGroups);
  const visibleGroups = expandedAll ? groups : groups.slice(0, limit);
  const hasMoreGroups = groups.length > limit;
  const activeRadioIdx = visibleGroups.findIndex((g) => g.id === selectedGroupId);

  const handleGroupKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!visibleGroups.length || !['ArrowRight', 'ArrowDown', 'ArrowLeft', 'ArrowUp', 'Home', 'End'].includes(e.key)) return;
    e.preventDefault();
    const focusedIdx = radioRefs.current.indexOf(document.activeElement as HTMLButtonElement);
    const curIdx = activeRadioIdx >= 0 ? activeRadioIdx : focusedIdx;
    let nextIdx = 0;
    if (e.key === 'Home') nextIdx = 0;
    else if (e.key === 'End') nextIdx = visibleGroups.length - 1;
    else {
      const delta = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : -1;
      nextIdx = curIdx < 0 ? (delta > 0 ? 0 : visibleGroups.length - 1) : (curIdx + delta + visibleGroups.length) % visibleGroups.length;
    }
    if (visibleGroups[nextIdx]) {
      selectGroup(visibleGroups[nextIdx].id);
      radioRefs.current[nextIdx]?.focus();
    }
  };

  return (
    <section aria-label={L.regionAria} className={`aui-project-progress-root ${className ?? ''}`} style={style}>
      <style>{PROGRESS_STYLES}</style>
      <dl className="aui-project-progress-stats">
        <div className="aui-project-progress-cell">
          <dt>{L.complete}</dt>
          <dd><CompletionRing pct={pct} health={derivedHealth} /><span>{pct}%</span></dd>
          <div className="aui-project-progress-bar" aria-hidden="true"><div style={{ width: `${pct}%` }} /></div>
          {doneSub && <span className="aui-project-progress-sub">{doneSub}</span>}
        </div>
        <div className="aui-project-progress-cell">
          <dt>{L.openTasks}</dt>
          <dd>{openCount}</dd>
          <span className="aui-project-progress-sub">{openSub ?? L.groupRunning(runningCount)}</span>
        </div>
        <div className="aui-project-progress-cell">
          <dt>{L.attention}</dt>
          <dd>{attentionCount}</dd>
          <span className={`aui-project-progress-sub ${attentionCount > 0 ? 'is-warn' : ''}`}>{attentionSub ?? (attentionCount > 0 ? L.groupAttention(attentionCount) : L.noAttention)}</span>
        </div>
        <div className="aui-project-progress-cell">
          <dt>{L.groupsStat}</dt>
          <dd>{groups.length > 0 ? <><span>{doneGroupsCount}</span><small>/ {groups.length}</small></> : <span>—</span>}</dd>
          <span className="aui-project-progress-sub">{groups.length > 0 ? L.activeGroupsSub(activeGroupsCount) : L.singleTrack}</span>
        </div>
      </dl>

      {groups.length > 0 && (
        <div className="aui-project-progress-groups-wrap">
          <div className="aui-project-progress-subhead">
            <div className="aui-project-progress-subhead-left">
              <strong className="aui-project-progress-subhead-title">{L.groupsHeading}</strong>
              <span className="aui-project-progress-subhead-meta">{L.groupsSummary(doneGroupsCount, groups.length)}</span>
            </div>
            <div className="aui-project-progress-actions">
              {selectedGroupId !== null && (
                <button type="button" className="aui-project-progress-link-btn" onClick={() => selectGroup(null)}>
                  <X size={12} aria-hidden="true" /><span>{L.clearGroup}</span>
                </button>
              )}
              {hasMoreGroups && (
                <button type="button" className="aui-project-progress-link-btn" aria-expanded={expandedAll} onClick={() => setExpandedAll((v) => !v)}>
                  <span>{expandedAll ? L.showFewerGroups : L.showAllGroups(groups.length)}</span>
                  {expandedAll ? <ChevronDown size={13} aria-hidden="true" /> : <ChevronRight size={13} aria-hidden="true" />}
                </button>
              )}
            </div>
          </div>

          <div className="aui-project-progress-track" role="radiogroup" aria-label={L.groupsAria} onKeyDown={handleGroupKeyDown}>
            {visibleGroups.map((gr, idx) => {
              const gPct = gr.total > 0 ? Math.round((gr.done / gr.total) * 100) : 0;
              const isDone = gr.total > 0 && gr.done >= gr.total;
              const hasAtt = (gr.attention ?? 0) > 0;
              const hasRun = (gr.running ?? 0) > 0;
              const checked = selectedGroupId === gr.id;
              const tabStop = activeRadioIdx >= 0 ? idx === activeRadioIdx : idx === 0;
              const barColor = hasAtt ? 'var(--pp-warn)' : isDone ? 'var(--pp-ok)' : hasRun ? 'var(--pp-info)' : 'var(--pp-fg-muted)';
              return (
                <button key={gr.id} ref={(el) => { radioRefs.current[idx] = el; }} type="button" role="radio" aria-checked={checked} tabIndex={tabStop ? 0 : -1} data-group-card={gr.id} className="aui-project-progress-card" onClick={() => selectGroup(checked ? null : gr.id)}>
                  <span className="aui-project-progress-line" aria-hidden="true"><div style={{ width: `${gPct}%`, background: barColor }} /></span>
                  <strong className="aui-project-progress-name" title={gr.name}>{gr.name}</strong>
                  <span className="aui-project-progress-card-meta">
                    <span>{gr.meta ?? `${L.groupTasks(gr.done, gr.total)} · ${gPct}%`}</span>
                    {hasAtt ? <span className="aui-project-progress-card-flag is-warn">{L.groupAttention(gr.attention!)}</span>
                      : hasRun ? <span className="aui-project-progress-card-flag is-info">{L.groupRunning(gr.running!)}</span>
                      : isDone ? <span className="aui-project-progress-card-flag is-ok">{L.groupDone}</span> : null}
                  </span>
                </button>
              );
            })}
          </div>
          {children}
        </div>
      )}
    </section>
  );
}

const DEMO_GROUPS: ProjectProgressGroup[] = [
  { id: 'input', name: '입력 기록부', total: 57, done: 54, running: 2, attention: 1 },
  { id: 'herdr', name: 'herdr 레인', total: 46, done: 46 },
  { id: 'ci', name: 'CI · 시험', total: 42, done: 39, running: 3 },
  { id: 'tui', name: '출력 · tui_o', total: 24, done: 18, running: 2 },
  { id: 'ops', name: '운영 자동화', total: 27, done: 27 },
];

export function ProjectProgressCardDemo({ state = 'default' }: { state?: 'default' | 'empty' | 'many' | 'selected' }) {
  if (state === 'empty') return <ProjectProgressCard totalCount={0} doneCount={0} openCount={0} />;
  if (state === 'many') {
    const many = Array.from({ length: 14 }, (_, i) => ({ id: `g-${i + 1}`, name: `트랙 ${i + 1} · 다중 게이트웨이 검증`, total: 12, done: i % 3 === 0 ? 12 : 4 + (i % 7), running: i % 4 === 1 ? 2 : 0, attention: i === 2 ? 1 : 0 }));
    return <ProjectProgressCard totalCount={168} doneCount={118} doneSub="118/168 완료" openCount={50} runningCount={6} attentionCount={1} attentionSub="막힘 1" groups={many} />;
  }
  return <ProjectProgressCard totalCount={196} doneCount={184} doneSub="184/196 done" openCount={12} runningCount={7} attentionCount={1} attentionSub="1 blocked" groups={DEMO_GROUPS} defaultSelectedGroupId={state === 'selected' ? 'input' : null} />;
}

export default ProjectProgressCard;
