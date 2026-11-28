"use client";
// Clean-room embeddable Gantt & Wave timeline component from public Arc UI gantt-timeline specification (2026-11-26).
// Supports topological Wave and Calendar axes, folded lane summary bars, SVG dependency arrows, and Timeline/List views without standalone app-shell chrome. No upstream Pro source code was used.

import { useCallback, useEffect, useMemo, useRef, useState, type ComponentType, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import { AlertCircle, CheckCircle2, ChevronDown, ChevronRight, CircleDashed, Clock, Diamond, Search, Users } from 'lucide-react';

export type GanttTone = 'teal' | 'sky' | 'orchid' | 'violet' | 'rose' | 'amber' | 'green';
export type GanttZoom = 'day' | 'week' | 'month';
export type GanttAxisMode = 'calendar' | 'wave';
export type GanttDerivedStatus = 'done' | 'reached' | 'conflict' | 'behind' | 'on-track' | 'not-started';

export interface GanttGroup { id: string; name: string }
export interface GanttTeam { id: string; name: string; tone: GanttTone; icon?: ComponentType<{ size?: number; strokeWidth?: number; className?: string }> }
export interface GanttTask {
  id: string;
  name: string;
  group: string;
  start?: string;
  end?: string;
  waveStart?: number;
  waveEnd?: number;
  progress?: number;
  assignee?: string;
  assigneeLabel?: string;
  subtitle?: string;
  statusOverride?: GanttDerivedStatus;
  dependsOn?: string[];
  milestone?: boolean;
  team?: string;
}

export interface GanttTimelineLabels {
  filterPlaceholder: string;
  filterAria: string;
  viewTimeline: string;
  viewList: string;
  expandAll: string;
  collapseAll: string;
  densityCompact: string;
  densityNormal: string;
  densityWide: string;
  zoomDay: string;
  zoomWeek: string;
  zoomMonth: string;
  loading: string;
  empty: string;
  colTask: string;
  colStart: string;
  colDue: string;
  colWave: string;
  colProgress: string;
  colStatus: string;
  tasksHeader: (visibleCount: number, laneCount: number) => string;
  waveHeader: (wave: number) => string;
  waveShort: (wave: number) => string;
  waveSpan: (start: number, end: number) => string;
  waveTasksCount: (count: number) => string;
  statusDone: string;
  statusReached: string;
  statusOnTrack: string;
  statusBehind: string;
  statusConflict: string;
  statusNotStarted: string;
  dependencyConflict: string;
}

export const defaultGanttTimelineLabels: GanttTimelineLabels = {
  filterPlaceholder: 'Filter tasks',
  filterAria: 'Filter tasks by name',
  viewTimeline: 'Timeline',
  viewList: 'List',
  expandAll: 'Expand all',
  collapseAll: 'Collapse all',
  densityCompact: 'Compact',
  densityNormal: 'Normal',
  densityWide: 'Wide',
  zoomDay: 'Day',
  zoomWeek: 'Week',
  zoomMonth: 'Month',
  loading: 'Loading timeline…',
  empty: 'No tasks to display.',
  colTask: 'Task',
  colStart: 'Start',
  colDue: 'Due',
  colWave: 'Wave',
  colProgress: 'Progress',
  colStatus: 'Status',
  tasksHeader: (visibleCount) => `${visibleCount} tasks`,
  waveHeader: (wave) => `Wave ${wave}`,
  waveShort: (wave) => `W${wave}`,
  waveSpan: (start, end) => (start === end ? `Wave ${start}` : `Wave ${start}–${end}`),
  waveTasksCount: (count) => `${count} tasks`,
  statusDone: 'Done',
  statusReached: 'Reached',
  statusOnTrack: 'On track',
  statusBehind: 'Behind',
  statusConflict: 'Conflict',
  statusNotStarted: 'Not started',
  dependencyConflict: 'Dependency conflict',
};

export interface GanttTimelineProps {
  title?: string;
  groups?: GanttGroup[];
  teams?: GanttTeam[];
  tasks?: GanttTask[];
  today?: string;
  zoom?: GanttZoom;
  defaultZoom?: GanttZoom;
  onZoomChange?: (zoom: GanttZoom) => void;
  collapsed?: string[];
  defaultCollapsed?: string[];
  onCollapsedChange?: (ids: string[]) => void;
  loading?: boolean;
  empty?: boolean;
  embedded?: boolean;
  readOnly?: boolean;
  axisMode?: GanttAxisMode;
  view?: 'timeline' | 'list';
  defaultView?: 'timeline' | 'list';
  onViewChange?: (view: 'timeline' | 'list') => void;
  selectedTaskId?: string | null;
  onSelectTask?: (taskId: string | null) => void;
  hideDetailsPanel?: boolean;
  toolbarExtra?: ReactNode;
  labels?: Partial<GanttTimelineLabels>;
  className?: string;
  style?: CSSProperties;
}

export interface ComputedTaskWaves {
  waves: Map<string, { start: number; end: number }>;
  maxWave: number;
  cyclicIds: Set<string>;
}

export function computeTaskWaves(tasks: ReadonlyArray<Pick<GanttTask, 'id' | 'dependsOn' | 'waveStart' | 'waveEnd'>>): ComputedTaskWaves {
  const byId = new Map(tasks.map((t) => [t.id, t]));
  const state = new Map<string, 1 | 2>();
  const waves = new Map<string, { start: number; end: number }>();
  const cyclicIds = new Set<string>();
  const visit = (id: string): { start: number; end: number } => {
    const cached = waves.get(id);
    if (cached) return cached;
    const task = byId.get(id);
    if (!task) return { start: 1, end: 1 };
    if (state.get(id) === 1) { cyclicIds.add(id); return { start: 1, end: 1 }; }
    state.set(id, 1);
    let depMax = 0;
    for (const depId of task.dependsOn ?? []) {
      if (!byId.has(depId)) continue;
      if (state.get(depId) === 1) { cyclicIds.add(id); cyclicIds.add(depId); continue; }
      const depWave = visit(depId);
      if (cyclicIds.has(depId)) cyclicIds.add(id);
      depMax = Math.max(depMax, depWave.end);
    }
    const start = task.waveStart && task.waveStart >= 1 ? Math.floor(task.waveStart) : depMax + 1;
    const end = task.waveEnd && task.waveEnd >= start ? Math.floor(task.waveEnd) : start;
    const res = { start, end };
    waves.set(id, res);
    state.set(id, 2);
    return res;
  };
  let maxWave = 1;
  for (const t of tasks) {
    const res = visit(t.id);
    if (res.end > maxWave) maxWave = res.end;
  }
  return { waves, maxWave, cyclicIds };
}

const DAY_MS = 86_400_000;
const MONTHS_SHORT = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const MONTHS_LONG = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];
const TONE_ICON: Record<GanttTone, string> = { orchid: '#8e4ec6', violet: '#6e56cf', teal: '#0d9488', sky: '#0284c7', amber: '#b45309', rose: '#e11d48', green: '#15803d' };
const AVATAR_GRADIENTS = [
  'radial-gradient(circle at 28% 28%, #93c5fd, #6ee7b7 58%, #c4b5fd)',
  'radial-gradient(circle at 30% 30%, #fda4af, #fdba74 58%, #93c5fd)',
  'radial-gradient(circle at 30% 30%, #6ee7b7, #7dd3fc 58%, #a5b4fc)',
  'radial-gradient(circle at 30% 30%, #fde047, #86efac 58%, #7dd3fc)',
];
const DEFAULT_GROUPS: GanttGroup[] = [{ id: 'core', name: 'Core' }, { id: 'ops', name: 'Ops' }];
const DEFAULT_TEAMS: GanttTeam[] = [{ id: 'done', name: 'Done', tone: 'green' }, { id: 'running', name: 'Running', tone: 'sky' }, { id: 'blocked', name: 'Blocked', tone: 'amber' }, { id: 'failed', name: 'Failed', tone: 'rose' }, { id: 'pending', name: 'Pending', tone: 'teal' }];
const DEFAULT_TASKS: GanttTask[] = [
  { id: 'w1', name: '#8148 입력 원장 스키마 확정', group: 'core', progress: 1, team: 'done', subtitle: '#8148' },
  { id: 'w2', name: '#8211 Decision 포럼 선택 주입', group: 'core', progress: 0.65, dependsOn: ['w1'], team: 'running', subtitle: '#8211' },
  { id: 'w3', name: '#8301 통합 회귀 시험 게이트', group: 'ops', progress: 0, dependsOn: ['w2'], team: 'pending', subtitle: '#8301' },
];

function parseDay(iso: string): number { const [y, m, d] = iso.split('-').map(Number); return Date.UTC(y || 2026, (m || 1) - 1, d || 1); }
function diffDays(a: string, b: string): number { return Math.round((parseDay(b) - parseDay(a)) / DAY_MS); }
function shortDate(iso: string): string { const dt = new Date(parseDay(iso)); return `${MONTHS_SHORT[dt.getUTCMonth()]} ${dt.getUTCDate()}`; }

function assigneeDisplay(task: GanttTask): { name: string; gradient: string } | null {
  const label = task.assigneeLabel ?? task.assignee;
  if (!label) return null;
  let h = 0;
  for (let i = 0; i < label.length; i++) h = (h * 31 + label.charCodeAt(i)) | 0;
  return { name: label, gradient: AVATAR_GRADIENTS[Math.abs(h) % AVATAR_GRADIENTS.length] };
}

function deriveTaskStatus(task: GanttTask, byId: Map<string, GanttTask>, today: string, axisMode: GanttAxisMode, waveData: ComputedTaskWaves): GanttDerivedStatus {
  if (axisMode === 'wave') {
    if (waveData.cyclicIds.has(task.id)) return 'conflict';
    const curW = waveData.waves.get(task.id)?.start ?? 1;
    if ((task.dependsOn ?? []).some((id) => byId.has(id) && curW <= (waveData.waves.get(id)?.end ?? 0))) return 'conflict';
    if (task.statusOverride) return task.statusOverride;
    const prog = task.progress ?? 0;
    return prog >= 1 ? (task.milestone ? 'reached' : 'done') : prog > 0 ? 'on-track' : 'not-started';
  }
  if (task.statusOverride) return task.statusOverride;
  const sIso = task.start ?? today, eIso = task.end ?? sIso;
  const depConflict = (task.dependsOn ?? []).some((id) => { const dep = byId.get(id); return dep && parseDay(sIso) < parseDay(dep.end ?? dep.start ?? today); });
  if (task.milestone) return depConflict ? 'conflict' : parseDay(sIso) <= parseDay(today) ? 'reached' : 'not-started';
  const prog = task.progress ?? 0;
  if (prog >= 1) return 'done';
  if (depConflict) return 'conflict';
  if (parseDay(today) < parseDay(sIso)) return 'not-started';
  const elapsed = Math.min(1, Math.max(0, (diffDays(sIso, today) + 1) / Math.max(1, diffDays(sIso, eIso) + 1)));
  return elapsed - prog > 0.15 ? 'behind' : 'on-track';
}

const GANTT_STYLES = `
.aui-gantt-timeline-root { --gt-bg:#f9f9f9; --gt-card:#ffffff; --gt-subtle:#f6f6f6; --gt-weekend:#f9f9f9; --gt-fg:var(--aui-fg,#141414); --gt-fg-dim:var(--aui-fg-dim,#525252); --gt-fg-muted:var(--aui-fg-muted,#737373); --gt-line:var(--aui-line,#e6e6e6); --gt-line-soft:var(--aui-line-soft,#f1f1f1); --gt-lane-track:#e7e7e7; --gt-lane-fill:#848484; --gt-conflict:#b3274a; --gt-track-orchid:#f2e5f4; --gt-fill-orchid:#dcbee0; --gt-bdr-orchid:#cfabd5; --gt-track-violet:#ebe7f8; --gt-fill-violet:#cbc2ef; --gt-bdr-violet:#b5a7e6; --gt-track-teal:#dde9e7; --gt-fill-teal:#b2cec8; --gt-bdr-teal:#96c0bb; --gt-track-sky:#dde8f1; --gt-fill-sky:#b4ccde; --gt-bdr-sky:#9bc0db; --gt-track-amber:#f1e7dc; --gt-fill-amber:#dfc6ab; --gt-bdr-amber:#d3b998; --gt-track-rose:#f7e4e8; --gt-fill-rose:#eabcc6; --gt-bdr-rose:#dc9cac; --gt-track-green:#dff0e4; --gt-fill-green:#b5dec1; --gt-bdr-green:#92cda4; position:relative; display:flex; width:100%; color:var(--gt-fg); font-family:Inter,var(--aui-font-sans,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif); font-size:13px; line-height:1.4; box-sizing:border-box; user-select:none; word-break:keep-all; }
:where(.dark,[data-theme="dark"]) .aui-gantt-timeline-root { --gt-bg:#141414; --gt-card:#181818; --gt-subtle:#202020; --gt-weekend:#1d1d1d; --gt-fg:#ededed; --gt-fg-dim:#b4b4b4; --gt-fg-muted:#8a8a8a; --gt-line:#262626; --gt-line-soft:#1f1f1f; --gt-lane-track:#2b2b2b; --gt-lane-fill:#787878; --gt-conflict:#ee6884; --gt-track-orchid:#2a202d; --gt-fill-orchid:#47354a; --gt-bdr-orchid:#67456d; --gt-track-violet:#242034; --gt-fill-violet:#3b3358; --gt-bdr-violet:#584b84; --gt-track-teal:#202c29; --gt-fill-teal:#274540; --gt-bdr-teal:#2d6a5f; --gt-track-sky:#232a2e; --gt-fill-sky:#31414c; --gt-bdr-sky:#3c5a6e; --gt-track-amber:#2b251d; --gt-fill-amber:#483b28; --gt-bdr-amber:#705936; --gt-track-rose:#2f1f24; --gt-fill-rose:#4c2c36; --gt-bdr-rose:#7d3c4d; --gt-track-green:#1e2c23; --gt-fill-green:#294633; --gt-bdr-green:#386b4a; }
.aui-gantt-timeline-root *, .aui-gantt-timeline-root *::before, .aui-gantt-timeline-root *::after { box-sizing:border-box; }
:where(.aui-gantt-timeline-root) button, :where(.aui-gantt-timeline-root) input { font:inherit; color:inherit; }
.aui-gantt-timeline-card { flex:1; min-width:0; background:var(--gt-card); border:1px solid var(--gt-line); border-radius:10px; display:flex; flex-direction:column; overflow:hidden; }
.aui-gantt-timeline-toolbar { padding:8px 14px 10px; border-bottom:1px solid var(--gt-line-soft); display:flex; align-items:center; justify-content:space-between; gap:8px; flex-wrap:wrap; flex-shrink:0; }
.aui-gantt-timeline-tool-left, .aui-gantt-timeline-tool-right { display:flex; align-items:center; gap:8px; min-width:0; flex-wrap:wrap; }
.aui-gantt-timeline-filter-box { position:relative; width:196px; min-height:32px; padding:0 8px 0 10px; border-radius:8px; border:1px solid var(--gt-line); background:var(--gt-card); display:flex; align-items:center; gap:6px; min-width:0; }
.aui-gantt-timeline-filter-box input { flex:1; min-width:0; min-height:30px; border:0; outline:none; background:transparent; font-size:12.5px; }
.aui-gantt-timeline-btn-outline { position:relative; min-height:32px; padding:0 12px; border-radius:8px; border:1px solid var(--gt-line); background:var(--gt-card); color:var(--gt-fg); font-size:12.5px; font-weight:500; display:inline-flex; align-items:center; gap:6px; white-space:nowrap; flex-shrink:0; cursor:pointer; }
.aui-gantt-timeline-seg { min-height:32px; padding:2px; border-radius:8px; background:var(--gt-subtle); border:1px solid var(--gt-line-soft); display:inline-flex; align-items:center; }
.aui-gantt-timeline-seg button { position:relative; min-height:26px; padding:0 11px; border:0; border-radius:6px; background:transparent; color:var(--gt-fg-muted); font-size:12.5px; font-weight:500; white-space:nowrap; cursor:pointer; }
.aui-gantt-timeline-seg button.is-active { background:var(--gt-card); color:var(--gt-fg); box-shadow:0 1px 2px rgba(0,0,0,.06); }
.aui-gantt-timeline-scroll { flex:1; min-height:160px; max-height:min(68vh,620px); overflow:auto; position:relative; scrollbar-width:thin; }
.aui-gantt-timeline-canvas { position:relative; min-height:100%; display:flex; flex-direction:column; }
.aui-gantt-timeline-cal-head { position:sticky; top:0; z-index:15; height:52px; background:var(--gt-card); border-bottom:1px solid var(--gt-line-soft); display:flex; flex-shrink:0; }
.aui-gantt-timeline-sticky-col { position:sticky; left:0; z-index:12; width:252px; flex:0 0 252px; background:var(--gt-card); border-right:1px solid var(--gt-line-soft); }
.aui-gantt-timeline-cal-head .aui-gantt-timeline-sticky-col { z-index:18; display:flex; align-items:flex-end; padding:0 14px 8px; color:var(--gt-fg-muted); font-size:12px; }
.aui-gantt-timeline-cal-track { position:relative; height:52px; flex-shrink:0; }
.aui-gantt-timeline-month-label { position:absolute; top:7px; font-size:12px; font-weight:500; color:var(--gt-fg); padding-left:8px; pointer-events:none; }
.aui-gantt-timeline-week-tick { position:absolute; bottom:7px; transform:translateX(-50%); font-size:11.5px; color:var(--gt-fg-muted); font-variant-numeric:tabular-nums; }
.aui-gantt-timeline-wave-head { position:absolute; top:0; bottom:0; padding:6px 10px 5px; display:flex; flex-direction:column; justify-content:space-between; border-right:1px solid var(--gt-line-soft); pointer-events:none; }
.aui-gantt-timeline-wave-head strong { font-size:12px; font-weight:600; color:var(--gt-fg); white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
.aui-gantt-timeline-wave-head small { font-size:11px; color:var(--gt-fg-muted); font-variant-numeric:tabular-nums; white-space:nowrap; }
.aui-gantt-timeline-body-wrap { position:relative; flex:1; }
.aui-gantt-timeline-grid-bg { position:absolute; top:0; bottom:0; left:252px; pointer-events:none; z-index:0; }
.aui-gantt-timeline-dep-svg { position:absolute; top:0; left:252px; pointer-events:none; z-index:2; }
.aui-gantt-timeline-row { display:flex; height:var(--gt-row-h,36px); border-bottom:1px solid var(--gt-line-soft); }
.aui-gantt-timeline-lane-btn { position:relative; width:100%; height:100%; padding:0 14px; border:0; background:var(--gt-card); display:flex; align-items:center; gap:6px; font-size:12.5px; font-weight:600; color:var(--gt-fg); cursor:pointer; text-align:left; min-width:0; }
.aui-gantt-timeline-lane-count { margin-left:auto; font-weight:400; font-size:12px; color:var(--gt-fg-muted); flex-shrink:0; }
.aui-gantt-timeline-task-cell { width:100%; height:100%; padding:0 12px 0 24px; border:0; background:var(--gt-card); display:flex; align-items:center; gap:8px; cursor:pointer; text-align:left; min-width:0; }
.aui-gantt-timeline-task-cell.is-selected { background:var(--gt-subtle); }
.aui-gantt-timeline-avatar-sm { width:18px; height:18px; border-radius:999px; flex-shrink:0; display:inline-block; }
.aui-gantt-timeline-task-title { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; font-size:12.5px; color:var(--gt-fg-dim); }
.aui-gantt-timeline-err-dot { color:var(--gt-conflict); display:inline-flex; flex-shrink:0; }
.aui-gantt-timeline-task-due { font-size:11.5px; color:var(--gt-fg-muted); font-variant-numeric:tabular-nums; flex-shrink:0; max-width:68px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.aui-gantt-timeline-row-track { position:relative; height:calc(var(--gt-row-h,36px) - 1px); flex-shrink:0; }
.aui-gantt-timeline-lane-summary { position:absolute; top:calc((var(--gt-row-h,36px) - 4px) / 2); height:4px; border-radius:999px; background:var(--gt-lane-track); overflow:hidden; }
.aui-gantt-timeline-lane-summary > div { height:100%; border-radius:999px; background:var(--gt-lane-fill); }
.aui-gantt-timeline-lane-summary.is-folded { top:calc((var(--gt-row-h,36px) - 16px) / 2); height:16px; border-radius:5px; border:1px solid var(--gt-line); padding:0; display:flex; align-items:center; cursor:pointer; overflow:visible; z-index:3; }
.aui-gantt-timeline-lane-summary-clip { position:absolute; inset:0; overflow:hidden; border-radius:4px; pointer-events:none; }
.aui-gantt-timeline-lane-summary-clip > div { position:absolute; inset:0 auto 0 0; border-radius:4px 0 0 4px; background:var(--gt-fill-teal); }
.aui-gantt-timeline-lane-summary-pct { position:relative; z-index:1; margin-left:auto; padding:0 6px; font-size:10.5px; font-weight:600; color:var(--gt-fg); font-variant-numeric:tabular-nums; white-space:nowrap; }
.aui-gantt-timeline-bar { position:absolute; top:calc((var(--gt-row-h,36px) - 22px) / 2); height:22px; border-radius:6px; border:1px solid transparent; padding:0 8px; display:flex; align-items:center; justify-content:space-between; gap:6px; font-size:11.5px; font-weight:500; cursor:pointer; overflow:visible; z-index:3; }
.aui-gantt-timeline-bar:focus-visible, .aui-gantt-timeline-lane-btn:focus-visible, .aui-gantt-timeline-lane-summary.is-folded:focus-visible { outline:2px solid var(--gt-fg); outline-offset:1px; }
.aui-gantt-timeline-bar-clip { position:absolute; inset:0; overflow:hidden; border-radius:5px; pointer-events:none; }
.aui-gantt-timeline-bar-fill { position:absolute; inset:0 auto 0 0; border-radius:5px 0 0 5px; pointer-events:none; }
.aui-gantt-timeline-bar-label { position:relative; z-index:1; min-width:0; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
.aui-gantt-timeline-bar-pct { position:relative; z-index:1; flex-shrink:0; white-space:nowrap; margin-left:auto; font-size:11px; opacity:.82; font-variant-numeric:tabular-nums; }
.aui-gantt-timeline-ms-node { position:absolute; top:calc((var(--gt-row-h,36px) - 24px) / 2); height:24px; transform:translateX(-7px); border:0; background:transparent; display:inline-flex; align-items:center; gap:6px; padding:0 4px; font-size:12px; font-weight:500; color:var(--gt-fg); cursor:pointer; white-space:nowrap; z-index:3; }
.aui-gantt-timeline-sort-btn { background:none; border:0; padding:0; cursor:pointer; font-weight:500; display:inline-flex; align-items:center; gap:4px; }
.aui-gantt-timeline-badge { display:inline-flex; align-items:center; gap:5px; height:22px; padding:0 8px; border-radius:999px; font-size:11.5px; font-weight:500; white-space:nowrap; }
.aui-gantt-timeline-badge.is-ok { background:rgba(16,185,129,.12); color:#059669; }
.aui-gantt-timeline-badge.is-done { background:var(--gt-subtle); color:var(--gt-fg-dim); }
.aui-gantt-timeline-badge.is-warn { background:rgba(245,158,11,.14); color:#b45309; }
.aui-gantt-timeline-badge.is-err { background:rgba(225,29,72,.12); color:var(--gt-conflict); }
.aui-gantt-timeline-badge.is-idle { background:var(--gt-subtle); color:var(--gt-fg-muted); }
:where(.dark,[data-theme="dark"]) .aui-gantt-timeline-badge.is-ok { background:rgba(16,185,129,.16); color:#34d399; }
:where(.dark,[data-theme="dark"]) .aui-gantt-timeline-badge.is-warn { background:rgba(245,158,11,.16); color:#fbbf24; }
.aui-gantt-timeline-table { width:100%; border-collapse:collapse; font-size:12.5px; }
.aui-gantt-timeline-table th { text-align:left; padding:10px 14px; color:var(--gt-fg-muted); font-weight:500; font-size:12px; border-bottom:1px solid var(--gt-line-soft); }
.aui-gantt-timeline-table td { padding:9px 14px; border-bottom:1px solid var(--gt-line-soft); }
.aui-gantt-timeline-table tr.is-lane td { background:var(--gt-subtle); font-weight:600; }
.aui-gantt-timeline-root.is-phone .aui-gantt-timeline-sticky-col { width:136px; flex:0 0 136px; }
.aui-gantt-timeline-root.is-phone .aui-gantt-timeline-task-cell { padding:0 10px; }
.aui-gantt-timeline-root.is-phone .aui-gantt-timeline-grid-bg, .aui-gantt-timeline-root.is-phone .aui-gantt-timeline-dep-svg { left:136px; }
.aui-gantt-timeline-root.is-phone .aui-gantt-timeline-tool-left, .aui-gantt-timeline-root.is-phone .aui-gantt-timeline-tool-right { width:100%; justify-content:space-between; }
.aui-gantt-timeline-root.is-phone .aui-gantt-timeline-filter-box { flex:1; width:auto; }
.aui-gantt-timeline-root.is-phone .aui-gantt-timeline-seg { flex:1; }
.aui-gantt-timeline-root.is-phone .aui-gantt-timeline-seg button { flex:1; }
@media (pointer: coarse), (max-width: 899px) { .aui-gantt-timeline-root input { font-size:16px !important; } .aui-gantt-timeline-btn-outline, .aui-gantt-timeline-filter-box, .aui-gantt-timeline-seg { min-height:44px; } .aui-gantt-timeline-seg button, .aui-gantt-timeline-sort-btn { min-height:44px; min-width:44px; } .aui-gantt-timeline-lane-summary.is-folded::after { content:''; position:absolute; inset:-14px; } .aui-gantt-timeline-bar::after { content:''; position:absolute; inset:-11px; } .aui-gantt-timeline-ms-node::after { content:''; position:absolute; inset:-10px; } }
@media (prefers-reduced-motion: reduce) { .aui-gantt-timeline-root * { transition:none !important; animation:none !important; } }
`;

export function GanttTimeline({
  title = 'Wave Timeline', groups = DEFAULT_GROUPS, teams = DEFAULT_TEAMS, tasks: propTasks, today = '2026-11-12',
  zoom: controlledZoom, defaultZoom = 'week', onZoomChange, collapsed: controlledCollapsed, defaultCollapsed = [], onCollapsedChange,
  loading = false, empty = false, embedded = true, readOnly = true, axisMode = 'wave',
  view: controlledView, defaultView = 'timeline', onViewChange, selectedTaskId: controlledSelectedId, onSelectTask,
  toolbarExtra, labels: labelOverrides, className, style,
}: GanttTimelineProps) {
  const L: GanttTimelineLabels = useMemo(() => ({ ...defaultGanttTimelineLabels, ...labelOverrides }), [labelOverrides]);
  const rootRef = useRef<HTMLDivElement>(null);
  const barRefs = useRef<Map<string, HTMLButtonElement>>(new Map());
  const [frameWidth, setFrameWidth] = useState(1440);
  const [isCoarse, setIsCoarse] = useState(false);
  useEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const update = () => setFrameWidth(el.clientWidth || window.innerWidth || 1440);
    update();
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  useEffect(() => {
    if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return;
    const mq = window.matchMedia('(pointer: coarse)');
    const sync = () => setIsCoarse(mq.matches);
    sync();
    mq.addEventListener?.('change', sync);
    return () => mq.removeEventListener?.('change', sync);
  }, []);

  const isMobileShell = frameWidth < 900 || isCoarse;
  const isPhone = frameWidth < 600;
  const rowH = isMobileShell ? 44 : 36;
  const activeTasks = empty ? [] : (propTasks ?? DEFAULT_TASKS);
  const taskStart = useCallback((t: GanttTask) => t.start ?? today, [today]);
  const taskEnd = useCallback((t: GanttTask) => t.end ?? t.start ?? today, [today]);

  const [internalZoom, setInternalZoom] = useState<GanttZoom>(defaultZoom);
  const zoom = controlledZoom ?? internalZoom;
  const setZoom = (next: GanttZoom) => { if (controlledZoom === undefined) setInternalZoom(next); onZoomChange?.(next); };

  const [internalCollapsed, setInternalCollapsed] = useState<string[]>(defaultCollapsed);
  const collapsed = controlledCollapsed ?? internalCollapsed;
  const setCollapsedIds = (next: string[]) => { if (controlledCollapsed === undefined) setInternalCollapsed(next); onCollapsedChange?.(next); };
  const toggleLane = (id: string) => setCollapsedIds(collapsed.includes(id) ? collapsed.filter((x) => x !== id) : [...collapsed, id]);

  const [internalView, setInternalView] = useState<'timeline' | 'list'>(defaultView);
  const view = controlledView ?? internalView;
  const setView = (next: 'timeline' | 'list') => { if (controlledView === undefined) setInternalView(next); onViewChange?.(next); };

  const [internalSelectedId, setInternalSelectedId] = useState<string | null>(null);
  const selectedTaskId = controlledSelectedId !== undefined ? controlledSelectedId : internalSelectedId;
  const selectTask = (id: string | null) => { if (controlledSelectedId === undefined) setInternalSelectedId(id); onSelectTask?.(id); };

  const [query, setQuery] = useState('');
  const [focusedTaskId, setFocusedTaskId] = useState<string | null>(null);
  const [sortCol, setSortCol] = useState<'name' | 'start'>('start');
  const [sortAsc, setSortAsc] = useState(true);

  const dayPx = zoom === 'day' ? 34 : zoom === 'week' ? 15 : 5;
  const wavePx = zoom === 'day' ? (isPhone ? 156 : 204) : zoom === 'week' ? (isPhone ? 124 : 156) : (isPhone ? 96 : 114);
  const waveData = useMemo(() => computeTaskWaves(activeTasks), [activeTasks]);
  const byId = useMemo(() => new Map(activeTasks.map((t) => [t.id, t])), [activeTasks]);
  const teamMap = useMemo(() => new Map(teams.map((t) => [t.id, t])), [teams]);

  const calendarRange = useMemo(() => {
    let minMs = parseDay(today) - 14 * DAY_MS, maxMs = parseDay(today) + 21 * DAY_MS;
    for (const t of activeTasks) { minMs = Math.min(minMs, parseDay(taskStart(t))); maxMs = Math.max(maxMs, parseDay(taskEnd(t))); }
    const minDow = (new Date(minMs).getUTCDay() + 6) % 7;
    const rangeStartMs = minMs - (minDow + 7) * DAY_MS, rangeEndMs = maxMs + 21 * DAY_MS;
    return { rangeStartMs, totalDays: Math.max(14, Math.round((rangeEndMs - rangeStartMs) / DAY_MS)) };
  }, [activeTasks, taskEnd, taskStart, today]);

  const filteredTasks = useMemo(() => {
    const q = query.trim().toLowerCase();
    return q ? activeTasks.filter((t) => t.name.toLowerCase().includes(q) || Boolean(t.subtitle?.toLowerCase().includes(q))) : activeTasks;
  }, [activeTasks, query]);

  const lanes = useMemo(() => groups.map((g) => ({ id: g.id, name: g.name, tasks: filteredTasks.filter((t) => t.group === g.id) })).filter((l) => l.tasks.length > 0 || !query.trim()), [filteredTasks, groups, query]);
  const visibleTasks = useMemo(() => lanes.flatMap((l) => (collapsed.includes(l.id) ? [] : l.tasks)), [collapsed, lanes]);
  const chartRows = useMemo(() => lanes.flatMap((lane) => [{ kind: 'lane' as const, lane }, ...(collapsed.includes(lane.id) ? [] : lane.tasks.map((task) => ({ kind: 'task' as const, task })))]), [collapsed, lanes]);
  const rowIndexMap = useMemo(() => { const m = new Map<string, number>(); chartRows.forEach((r, idx) => { if (r.kind === 'task') m.set(r.task.id, idx); }); return m; }, [chartRows]);

  const timelineWidth = axisMode === 'wave' ? Math.max(waveData.maxWave * wavePx + 24, 300) : calendarRange.totalDays * dayPx;
  const getTaskGeom = useCallback((task: GanttTask) => {
    if (axisMode === 'wave') {
      const w = waveData.waves.get(task.id) ?? { start: 1, end: 1 };
      const startX = (w.start - 1) * wavePx + 8;
      const barW = Math.max(40, Math.max(1, w.end - w.start + 1) * wavePx - 16);
      return { startX, barW, endEdgeX: startX + barW, startLabel: L.waveShort(w.start), endLabel: L.waveShort(w.end), rangeAria: L.waveSpan(w.start, w.end) };
    }
    const sIso = taskStart(task), eIso = taskEnd(task);
    const startX = Math.round(((parseDay(sIso) - calendarRange.rangeStartMs) / DAY_MS) * dayPx);
    const spanDays = Math.max(1, diffDays(sIso, eIso) + 1);
    const barW = Math.max(28, Math.round(spanDays * dayPx));
    return { startX, barW, endEdgeX: Math.round(((parseDay(eIso) - calendarRange.rangeStartMs) / DAY_MS + (task.milestone ? 0.5 : 1)) * dayPx), startLabel: shortDate(sIso), endLabel: shortDate(eIso), rangeAria: `${shortDate(sIso)} to ${shortDate(eIso)}` };
  }, [L, axisMode, calendarRange.rangeStartMs, dayPx, taskEnd, taskStart, waveData.waves, wavePx]);

  const waveColumns = useMemo(() => {
    if (axisMode !== 'wave') return [];
    const counts = new Array<number>(waveData.maxWave + 1).fill(0);
    for (const t of filteredTasks) { const w = waveData.waves.get(t.id)?.start ?? 1; if (w >= 1 && w <= waveData.maxWave) counts[w] += 1; }
    return Array.from({ length: waveData.maxWave }, (_, i) => ({ wave: i + 1, x: i * wavePx, width: wavePx, count: counts[i + 1] ?? 0 }));
  }, [axisMode, filteredTasks, waveData.maxWave, waveData.waves, wavePx]);

  const calendarTicks = useMemo(() => {
    if (axisMode === 'wave') return { weeks: [], months: [] };
    const weeks: { x: number; dayNum: number; key: number }[] = [], months: { x: number; label: string }[] = [];
    let curM = -1;
    for (let d = 0; d < calendarRange.totalDays; d++) {
      const dt = new Date(calendarRange.rangeStartMs + d * DAY_MS), x = d * dayPx;
      if (dt.getUTCMonth() !== curM) { curM = dt.getUTCMonth(); months.push({ x, label: MONTHS_LONG[curM] }); }
      if (dt.getUTCDay() === 1) weeks.push({ x, dayNum: dt.getUTCDate(), key: d });
    }
    return { weeks, months };
  }, [axisMode, calendarRange, dayPx]);

  const activeTabStopId = focusedTaskId && visibleTasks.some((t) => t.id === focusedTaskId) ? focusedTaskId : visibleTasks[0]?.id ?? null;
  const handleBarKeyDown = (e: ReactKeyboardEvent, task: GanttTask) => {
    const idx = visibleTasks.findIndex((t) => t.id === task.id);
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const next = visibleTasks[e.key === 'ArrowDown' ? Math.min(visibleTasks.length - 1, idx + 1) : Math.max(0, idx - 1)];
      if (next) { setFocusedTaskId(next.id); barRefs.current.get(next.id)?.focus(); }
    } else if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      selectTask(task.id);
    }
  };

  const renderBadge = (st: GanttDerivedStatus) => {
    const map: Record<GanttDerivedStatus, { label: string; cls: string; ic: ReactNode }> = {
      done: { label: L.statusDone, cls: 'done', ic: <CheckCircle2 size={12} /> },
      reached: { label: L.statusReached, cls: 'done', ic: <CheckCircle2 size={12} /> },
      'on-track': { label: L.statusOnTrack, cls: 'ok', ic: <Clock size={12} /> },
      behind: { label: L.statusBehind, cls: 'warn', ic: <Clock size={12} /> },
      conflict: { label: L.statusConflict, cls: 'err', ic: <AlertCircle size={12} /> },
      'not-started': { label: L.statusNotStarted, cls: 'idle', ic: <CircleDashed size={12} /> },
    };
    const item = map[st];
    return <span className={`aui-gantt-timeline-badge is-${item.cls}`}>{item.ic}<span>{item.label}</span></span>;
  };

  return (
    <div ref={rootRef} data-embedded={embedded ? 'true' : undefined} data-readonly={readOnly ? 'true' : undefined} data-axis={axisMode} style={{ '--gt-row-h': `${rowH}px`, ...style } as CSSProperties} className={`aui-gantt-timeline-root ${isPhone ? 'is-phone' : ''} ${isMobileShell ? 'is-mobile-shell' : ''} ${className ?? ''}`}>
      <style>{GANTT_STYLES}</style>
      <section className="aui-gantt-timeline-card" aria-label={title}>
        <div className="aui-gantt-timeline-toolbar">
          <div className="aui-gantt-timeline-tool-left">
            <label className="aui-gantt-timeline-filter-box">
              <Search size={13} style={{ color: 'var(--gt-fg-muted)', flexShrink: 0 }} />
              <input type="search" placeholder={L.filterPlaceholder} aria-label={L.filterAria} value={query} onChange={(e) => setQuery(e.target.value)} />
            </label>
            {lanes.length > 1 && (
              <button type="button" className="aui-gantt-timeline-btn-outline" onClick={() => { const ids = lanes.map((l) => l.id); setCollapsedIds(ids.every((id) => collapsed.includes(id)) ? [] : ids); }}>
                <span>{lanes.every((l) => collapsed.includes(l.id)) ? L.expandAll : L.collapseAll}</span>
              </button>
            )}
            {toolbarExtra}
          </div>
          <div className="aui-gantt-timeline-tool-right">
            <div className="aui-gantt-timeline-seg" role="group" aria-label={title}>
              <button type="button" aria-pressed={view === 'timeline'} className={view === 'timeline' ? 'is-active' : ''} onClick={() => setView('timeline')}>{L.viewTimeline}</button>
              <button type="button" aria-pressed={view === 'list'} className={view === 'list' ? 'is-active' : ''} onClick={() => setView('list')}>{L.viewList}</button>
            </div>
            {view === 'timeline' && (
              <div className="aui-gantt-timeline-seg" role="group" aria-label="Zoom">
                {(axisMode === 'wave' ? (['month', 'week', 'day'] as const) : (['day', 'week', 'month'] as const)).map((z) => (
                  <button key={z} type="button" aria-pressed={zoom === z} className={zoom === z ? 'is-active' : ''} onClick={() => setZoom(z)}>
                    {axisMode === 'wave' ? (z === 'month' ? L.densityCompact : z === 'week' ? L.densityNormal : L.densityWide) : (z === 'day' ? L.zoomDay : z === 'week' ? L.zoomWeek : L.zoomMonth)}
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>

        {loading ? (
          <div style={{ padding: 24, color: 'var(--gt-fg-muted)' }} role="status">{L.loading}</div>
        ) : activeTasks.length === 0 ? (
          <div style={{ padding: 36, textAlign: 'center', color: 'var(--gt-fg-muted)' }}>{L.empty}</div>
        ) : view === 'list' ? (
          <div className="aui-gantt-timeline-scroll">
            <table className="aui-gantt-timeline-table">
              <thead>
                <tr>
                  <th aria-sort={sortCol === 'name' ? (sortAsc ? 'ascending' : 'descending') : 'none'}>
                    <button type="button" className="aui-gantt-timeline-sort-btn" onClick={() => { setSortCol('name'); setSortAsc(sortCol === 'name' ? !sortAsc : true); }}>{L.colTask}</button>
                  </th>
                  <th aria-sort={sortCol === 'start' ? (sortAsc ? 'ascending' : 'descending') : 'none'}>
                    <button type="button" className="aui-gantt-timeline-sort-btn" onClick={() => { setSortCol('start'); setSortAsc(sortCol === 'start' ? !sortAsc : true); }}>
                      {axisMode === 'wave' ? L.colWave : L.colStart} {sortCol === 'start' ? (sortAsc ? '↑' : '↓') : ''}
                    </button>
                  </th>
                  {axisMode === 'calendar' && <th>{L.colDue}</th>}
                  {frameWidth >= 600 && <th>{L.colProgress}</th>}
                  <th>{L.colStatus}</th>
                </tr>
              </thead>
              <tbody>
                {lanes.map((lane) => {
                  const listColCount = 3 + (axisMode === 'calendar' ? 1 : 0) + (frameWidth >= 600 ? 1 : 0);
                  const sorted = [...lane.tasks].sort((a, b) => {
                    const cmp = sortCol === 'name' ? a.name.localeCompare(b.name) : axisMode === 'wave' ? (waveData.waves.get(a.id)?.start ?? 1) - (waveData.waves.get(b.id)?.start ?? 1) : parseDay(taskStart(a)) - parseDay(taskStart(b));
                    return sortAsc ? cmp : -cmp;
                  });
                  return [
                    <tr key={`lane-${lane.id}`} className="is-lane"><td colSpan={listColCount}>{lane.name} <span style={{ color: 'var(--gt-fg-muted)', fontWeight: 400 }}>{lane.tasks.length}</span></td></tr>,
                    ...sorted.map((t) => {
                      const st = deriveTaskStatus(t, byId, today, axisMode, waveData);
                      const tm = t.team ? teamMap.get(t.team) : teams[0];
                      const Ic = tm?.icon ?? Users;
                      const iconColor = TONE_ICON[tm?.tone ?? 'teal'];
                      const w = waveData.waves.get(t.id) ?? { start: 1, end: 1 };
                      return (
                        <tr key={t.id} data-task-id={t.id} onClick={() => selectTask(t.id)} style={{ cursor: 'pointer' }}>
                          <td><span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>{t.milestone ? <Diamond size={12} style={{ color: iconColor }} /> : <Ic size={13} style={{ color: iconColor }} />}<span>{t.name}</span></span></td>
                          <td>{axisMode === 'wave' ? L.waveSpan(w.start, w.end) : shortDate(taskStart(t))}</td>
                          {axisMode === 'calendar' && <td>{shortDate(taskEnd(t))}</td>}
                          {frameWidth >= 600 && <td>{!t.milestone && `${Math.round((t.progress ?? 0) * 100)}%`}</td>}
                          <td>{renderBadge(st)}</td>
                        </tr>
                      );
                    }),
                  ];
                })}
              </tbody>
            </table>
          </div>
        ) : (
          <div className="aui-gantt-timeline-scroll">
            <div className="aui-gantt-timeline-canvas" style={{ width: (isPhone ? 136 : 252) + timelineWidth }}>
              <div className="aui-gantt-timeline-cal-head">
                <div className="aui-gantt-timeline-sticky-col"><span>{L.tasksHeader(visibleTasks.length, lanes.length)}</span></div>
                <div className="aui-gantt-timeline-cal-track" style={{ width: timelineWidth }}>
                  {axisMode === 'wave'
                    ? waveColumns.map((col) => <div key={col.wave} className="aui-gantt-timeline-wave-head" style={{ left: col.x, width: col.width }}><strong>{L.waveHeader(col.wave)}</strong><small>{L.waveTasksCount(col.count)}</small></div>)
                    : <>
                        {calendarTicks.months.map((m) => <span key={`${m.label}-${m.x}`} className="aui-gantt-timeline-month-label" style={{ left: m.x }}>{m.label}</span>)}
                        {calendarTicks.weeks.map((w) => <span key={w.key} className="aui-gantt-timeline-week-tick" style={{ left: w.x }}>{w.dayNum}</span>)}
                      </>}
                </div>
              </div>

              <div className="aui-gantt-timeline-body-wrap">
                <div className="aui-gantt-timeline-grid-bg" style={{ width: timelineWidth }}>
                  {axisMode === 'wave'
                    ? waveColumns.map((col) => <div key={col.wave} style={{ position: 'absolute', top: 0, bottom: 0, left: col.x, width: col.width, background: col.wave % 2 === 0 ? 'var(--gt-weekend)' : 'transparent', borderRight: '1px solid var(--gt-line-soft)' }} />)
                    : calendarTicks.weeks.map((w) => <div key={w.key} style={{ position: 'absolute', top: 0, bottom: 0, left: w.x, width: 1, background: 'var(--gt-line-soft)' }} />)}
                </div>

                <svg className="aui-gantt-timeline-dep-svg" width={timelineWidth} height={Math.max(chartRows.length * rowH, 1)} aria-hidden="true">
                  {chartRows.map((row, toIdx) => {
                    if (row.kind !== 'task' || !row.task.dependsOn?.length) return null;
                    const toTask = row.task, toStartPx = getTaskGeom(toTask).startX, toY = toIdx * rowH + Math.round(rowH / 2);
                    return toTask.dependsOn!.map((depId) => {
                      const fromIdx = rowIndexMap.get(depId), fromTask = byId.get(depId);
                      if (fromIdx === undefined || !fromTask) return null;
                      const fromX = getTaskGeom(fromTask).endEdgeX, fromY = fromIdx * rowH + Math.round(rowH / 2);
                      const isConflict = axisMode === 'wave' ? waveData.cyclicIds.has(toTask.id) || (waveData.waves.get(toTask.id)?.start ?? 1) <= (waveData.waves.get(fromTask.id)?.end ?? 0) : parseDay(taskStart(toTask)) < parseDay(taskEnd(fromTask));
                      const stroke = isConflict ? 'var(--gt-conflict)' : '#949494', endX = toTask.milestone ? toStartPx - 6 : toStartPx - 2;
                      const d = endX >= fromX + 14
                        ? `M ${fromX} ${fromY} L ${fromX + 10} ${fromY} Q ${fromX + 14} ${fromY} ${fromX + 14} ${fromY + (toY > fromY ? 4 : -4)} L ${fromX + 14} ${toY - (toY > fromY ? 4 : -4)} Q ${fromX + 14} ${toY} ${fromX + 18} ${toY} L ${endX} ${toY}`
                        : `M ${fromX} ${fromY} L ${fromX + 8} ${fromY} L ${fromX + 8} ${fromY + (toY > fromY ? 16 : -16)} L ${endX - 10} ${fromY + (toY > fromY ? 16 : -16)} L ${endX - 10} ${toY} L ${endX} ${toY}`;
                      return <g key={`${depId}->${toTask.id}`}><path d={d} fill="none" stroke={stroke} strokeWidth="1.2" /><path d={`M ${endX - 4} ${toY - 3} L ${endX} ${toY} L ${endX - 4} ${toY + 3}`} fill="none" stroke={stroke} strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" /></g>;
                    });
                  })}
                </svg>

                {chartRows.map((row) => {
                  if (row.kind === 'lane') {
                    const isFolded = collapsed.includes(row.lane.id), laneTasks = row.lane.tasks;
                    let minX = Infinity, maxX = -Infinity, sumProg = 0, countNonMs = 0;
                    for (const t of laneTasks) { const g = getTaskGeom(t); minX = Math.min(minX, g.startX); maxX = Math.max(maxX, g.endEdgeX); if (!t.milestone) { sumProg += t.progress ?? 0; countNonMs++; } }
                    const laneLeft = Number.isFinite(minX) ? minX : 0, laneW = Number.isFinite(minX) ? Math.max(24, maxX - minX) : 0, lanePct = countNonMs ? Math.round((sumProg / countNonMs) * 100) : 0;
                    return (
                      <div key={`lane-${row.lane.id}`} className="aui-gantt-timeline-row">
                        <div className="aui-gantt-timeline-sticky-col">
                          <button type="button" className="aui-gantt-timeline-lane-btn" aria-expanded={!isFolded} onClick={() => toggleLane(row.lane.id)}>
                            {isFolded ? <ChevronRight size={13} style={{ color: 'var(--gt-fg-muted)', flexShrink: 0 }} /> : <ChevronDown size={13} style={{ color: 'var(--gt-fg-muted)', flexShrink: 0 }} />}
                            <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{row.lane.name}</span>
                            <span className="aui-gantt-timeline-lane-count">{laneTasks.length}</span>
                          </button>
                        </div>
                        <div className="aui-gantt-timeline-row-track" style={{ width: timelineWidth }}>
                          {laneW > 0 && (isFolded
                            ? <button type="button" data-hit-slop="14" className="aui-gantt-timeline-lane-summary is-folded" aria-label={`${row.lane.name}, ${lanePct}%`} style={{ left: laneLeft, width: laneW }} onClick={() => toggleLane(row.lane.id)}><span className="aui-gantt-timeline-lane-summary-clip"><div style={{ width: `${lanePct}%` }} /></span><span className="aui-gantt-timeline-lane-summary-pct">{lanePct}%</span></button>
                            : <div className="aui-gantt-timeline-lane-summary" style={{ left: laneLeft, width: laneW }}><div style={{ width: `${lanePct}%` }} /></div>)}
                        </div>
                      </div>
                    );
                  }
                  const { task } = row;
                  const person = assigneeDisplay(task), team = task.team ? teamMap.get(task.team) : teams[0], toneKey = team?.tone ?? 'teal';
                  const st = deriveTaskStatus(task, byId, today, axisMode, waveData), geom = getTaskGeom(task), pct = Math.round((task.progress ?? 0) * 100);
                  return (
                    <div key={task.id} className="aui-gantt-timeline-row">
                      <div className="aui-gantt-timeline-sticky-col">
                        <div className={`aui-gantt-timeline-task-cell ${selectedTaskId === task.id ? 'is-selected' : ''}`} onClick={() => selectTask(task.id)}>
                          {!isPhone && person && <span className="aui-gantt-timeline-avatar-sm" title={person.name} style={{ background: person.gradient }} />}
                          <span className="aui-gantt-timeline-task-title" title={task.name}>{task.name}</span>
                          {st === 'conflict' && <span className="aui-gantt-timeline-err-dot" title={L.dependencyConflict}><AlertCircle size={13} fill="currentColor" stroke="var(--gt-card)" /></span>}
                          {!isPhone && <span className="aui-gantt-timeline-task-due">{task.subtitle ?? geom.endLabel}</span>}
                        </div>
                      </div>
                      <div className="aui-gantt-timeline-row-track" style={{ width: timelineWidth }}>
                        {task.milestone ? (
                          <button ref={(el) => { if (el) barRefs.current.set(task.id, el); else barRefs.current.delete(task.id); }} type="button" data-task-id={task.id} data-hit-slop="10" aria-pressed={selectedTaskId === task.id} aria-current={selectedTaskId === task.id || undefined} aria-label={`${task.name}, ${geom.startLabel}`} tabIndex={activeTabStopId === task.id ? 0 : -1} className="aui-gantt-timeline-ms-node" style={{ left: geom.startX + Math.round((axisMode === 'wave' ? wavePx : dayPx) * 0.5) }} onClick={() => selectTask(task.id)} onKeyDown={(e) => handleBarKeyDown(e, task)}>
                            <span style={{ color: TONE_ICON[toneKey], fontSize: 13 }}>◆</span><span>{task.name}</span>
                          </button>
                        ) : (
                          <button ref={(el) => { if (el) barRefs.current.set(task.id, el); else barRefs.current.delete(task.id); }} type="button" data-task-id={task.id} data-hit-slop="11" aria-pressed={selectedTaskId === task.id} aria-current={selectedTaskId === task.id || undefined} aria-label={`${task.name}, ${geom.rangeAria}, ${pct}% done`} tabIndex={activeTabStopId === task.id ? 0 : -1} className="aui-gantt-timeline-bar" style={{ left: geom.startX, width: geom.barW, background: `var(--gt-track-${toneKey})`, borderColor: st === 'conflict' ? 'var(--gt-conflict)' : `var(--gt-bdr-${toneKey})` }} onClick={() => selectTask(task.id)} onKeyDown={(e) => handleBarKeyDown(e, task)}>
                            <span className="aui-gantt-timeline-bar-clip"><span className="aui-gantt-timeline-bar-fill" style={{ width: `${pct}%`, background: `var(--gt-fill-${toneKey})` }} /></span>
                            {geom.barW >= 86 && <span className="aui-gantt-timeline-bar-label">{task.name}</span>}
                            <span className="aui-gantt-timeline-bar-pct">{pct}%</span>
                          </button>
                        )}
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          </div>
        )}
      </section>
    </div>
  );
}

export function EmbeddedGanttDemo({ state = 'default' }: { state?: 'default' | 'loading' | 'empty' | 'folded' }) {
  if (state === 'loading') return <GanttTimeline loading />;
  if (state === 'empty') return <GanttTimeline empty />;
  return <GanttTimeline defaultCollapsed={state === 'folded' ? ['ops'] : []} />;
}

export default GanttTimeline;
