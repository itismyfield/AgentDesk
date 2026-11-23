import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from 'react';
import { Bot, ChartNoAxesColumn, Check, CircleHelp, CornerDownLeft, FileText, Hash, House, Inbox, Info, Layers, MessageSquare, Moon, Rocket, Search, Settings, ShieldCheck, Sparkles, Terminal, Upload, UserPlus, X } from 'lucide-react';

// Clean-room implementation from clean-specs/command-menu/spec-v2.md and delta-d1.md.
export interface CommandMenuCommand {
  id: string;
  label: string;
  hint?: string;
  icon?: ReactNode;
  /** A neutral mark rendered in a tile; takes precedence over icon. */
  mark?: ReactNode;
  keywords?: string | string[];
  shortcut?: string[];
  /** Kit extension: unavailable commands remain visible but cannot run. */
  disabled?: boolean;
  /** Runs this command instead of `onRun`. A command with neither stays visible as text and cannot be chosen. */
  onSelect?: () => void;
}
export interface CommandMenuGroup { id: string; label: string; commands: CommandMenuCommand[]; recommended?: boolean }
export interface CommandMenuProps {
  groups?: CommandMenuGroup[];
  placeholder?: string;
  /**
   * General commands provide id only; assistant requests provide ('ask', trimmedQuery). Without it only commands with their
   * own `onSelect` can run, the assistant row is not offered and nothing is announced as run.
   */
  onRun?: (id: string, query?: string) => void;
  onClose?: () => void;
  autoFocus?: boolean;
  /** Optional host-owned opening extension. */
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  assistantFallback?: boolean;
  /** Desktop modal extension; the default is an embedded, non-modal panel. */
  modal?: boolean;
  /** Preserve the kit full-screen mobile overlay. Set false for embedded use. */
  mobileOverlay?: boolean;
  label?: string;
  /** Every other visible and spoken text. Missing keys keep the Korean defaults. */
  labels?: Partial<CommandMenuLabels>;
  className?: string;
}
export interface CommandMenuLabels {
  /** Launcher shown while closed. */
  launch: string;
  /** Accessible name of the search field. */
  search: string;
  enter: string;
  detailsShow: string;
  detailsHide: string;
  close: string;
  details: string;
  list: string;
  /** Assistant row; `query` is the trimmed search. */
  ask: (query: string) => string;
  askGroup: string;
  /** Footer line after the assistant request reached `onRun`. */
  asked: (query: string) => string;
  empty: string;
  /** Follows `empty` when the assistant row can take a search. */
  emptyAsk: string;
  noMatch: string;
  groupMove: string;
  searchKey: string;
}
export const commandMenuDefaultLabels: CommandMenuLabels = {
  launch: '명령 메뉴 열기',
  search: '명령 검색',
  enter: 'ENTER',
  detailsShow: '작업 설명 보기 (F2)',
  detailsHide: '작업 설명 숨기기 (F2)',
  close: '명령 메뉴 닫기',
  details: '작업 설명',
  list: '사용할 수 있는 명령',
  ask: (query) => `도우미에게 “${query}” 질문하기`,
  askGroup: '도우미 질의',
  asked: (query) => `도우미에게 질문 전달: ${query}`,
  empty: '등록된 작업이 없습니다.',
  emptyAsk: '검색하면 도우미에게 질문할 수 있습니다.',
  noMatch: '찾는 작업이 없어요.',
  groupMove: '범주 이동',
  searchKey: '검색',
};
const css = `
.aui-command-menu{--aui-command-menu-bg:var(--aui-bg-1,#fff);--aui-command-menu-tray:var(--aui-bg-2,#f0f0f0);--aui-command-menu-line:var(--aui-line,#ececec);--aui-command-menu-ink:var(--aui-fg,#171717);--aui-command-menu-fg:var(--aui-fg-dim,#404040);--aui-command-menu-muted:var(--aui-fg-muted,#8f8f8f);--aui-command-menu-faint:var(--aui-fg-faint,#b5b5b5);--aui-command-menu-active:var(--aui-bg-3,#f0f0f1);--aui-command-menu-tile:var(--aui-command-menu-mark-bg,#fff);--aui-command-menu-tileline:var(--aui-command-menu-mark-line,#00000017);--aui-command-menu-key:var(--aui-command-menu-key-bg,#fff);--aui-command-menu-highlight:var(--aui-accent-soft,#0190fe24);--aui-command-menu-ok:var(--aui-ok,#1e8a4f);--aui-command-menu-focus:var(--aui-command-menu-focus-color,#17171738);--aui-command-menu-ease:var(--aui-ease,cubic-bezier(.22,1,.36,1));--aui-command-menu-r-lg:var(--aui-r-lg,16px);--aui-command-menu-r-sm:var(--aui-r-sm,7px);width:100%;max-width:600px;min-width:0;container-type:inline-size;font:13.5px var(--aui-font-sans,Inter,ui-sans-serif,system-ui,sans-serif);color:var(--aui-command-menu-fg);word-break:keep-all;overflow-wrap:anywhere}
:where(.dark,[data-theme="dark"]) .aui-command-menu{--aui-command-menu-bg:var(--aui-bg-1,#19191b);--aui-command-menu-tray:var(--aui-bg-2,#111113);--aui-command-menu-line:var(--aui-line,#2a2a2e);--aui-command-menu-ink:var(--aui-fg,#ededed);--aui-command-menu-fg:var(--aui-fg-dim,#c8c8cc);--aui-command-menu-muted:var(--aui-fg-muted,#8b8b92);--aui-command-menu-faint:var(--aui-fg-faint,#5c5c63);--aui-command-menu-active:var(--aui-bg-3,#26262a);--aui-command-menu-tile:var(--aui-command-menu-mark-bg,#26262a);--aui-command-menu-tileline:var(--aui-command-menu-mark-line,#ffffff1a);--aui-command-menu-key:var(--aui-command-menu-key-bg,#232326);--aui-command-menu-highlight:var(--aui-accent-soft,#4fb0fe3d);--aui-command-menu-ok:var(--aui-ok,#5fc88a);--aui-command-menu-focus:var(--aui-command-menu-focus-color,#ffffff4d)}
.aui-command-menu *,.aui-command-menu *::before,.aui-command-menu *::after{box-sizing:border-box}.aui-command-menu button,.aui-command-menu input{font:inherit;color:inherit;touch-action:manipulation}.aui-command-menu button{cursor:pointer}.aui-command-menu button:disabled{opacity:.45;cursor:default}.aui-command-menu .aui-command-menu-row[data-static]:disabled{opacity:1}.aui-command-menu button:focus-visible,.aui-command-menu input:focus-visible{outline:2px solid var(--aui-command-menu-focus);outline-offset:2px}.aui-command-menu svg{display:block;flex:none}.aui-command-menu[data-modal=true]{container-type:normal}.aui-command-menu-launch{display:flex;align-items:center;gap:10px;width:100%;min-height:44px;padding:12px 16px;border:1px solid var(--aui-command-menu-line);border-radius:12px;background:var(--aui-command-menu-bg);text-align:left}.aui-command-menu-launch span{flex:1}.aui-command-menu-backdrop{position:fixed;inset:0;z-index:70;background:var(--aui-overlay,#10142133)}
.aui-command-menu-panel{position:relative;z-index:71;width:100%;padding:4px 4px 0;border-radius:calc(var(--aui-command-menu-r-lg) + 4px);background:var(--aui-command-menu-tray)}.aui-command-menu[data-modal=true] .aui-command-menu-panel{container-type:inline-size}.aui-command-menu-card{overflow:clip;border-radius:var(--aui-command-menu-r-lg);background:var(--aui-command-menu-bg);box-shadow:0 0 0 1px var(--aui-command-menu-line),var(--aui-shadow-sm,0 1px 2px #0000000a)}.aui-command-menu-search{display:flex;align-items:center;gap:10px;height:52px;padding:0 16px;border-bottom:1px solid var(--aui-command-menu-line);color:var(--aui-command-menu-muted)}.aui-command-menu-search:focus-within{box-shadow:inset 0 0 0 2px var(--aui-command-menu-focus)}.aui-command-menu-search input{flex:1;width:100%;min-width:0;height:44px;padding:0;border:0;background:transparent;color:var(--aui-command-menu-ink);font-size:15px;outline:none}.aui-command-menu-search input::placeholder{color:var(--aui-command-menu-faint)}.aui-command-menu-enter{display:flex;align-items:center;gap:4px;color:var(--aui-command-menu-faint);font-size:11.5px;font-weight:500;letter-spacing:.04em}.aui-command-menu-close,.aui-command-menu-info{display:grid;place-items:center;width:44px;height:44px;min-width:44px;padding:0;border:0;border-radius:8px;background:transparent;color:var(--aui-command-menu-muted)}.aui-command-menu-info{display:none}.aui-command-menu-details{max-height:160px;overflow-y:auto;overscroll-behavior:contain;padding:12px 16px;border-bottom:1px solid var(--aui-command-menu-line);line-height:1.5;color:var(--aui-command-menu-fg)}.aui-command-menu-details p{margin:0 0 8px;overflow-wrap:anywhere}.aui-command-menu-details strong{font-weight:500;color:var(--aui-command-menu-ink)}
.aui-command-menu-list{position:relative;max-height:340px;overflow-y:auto;overscroll-behavior:contain;scrollbar-width:thin;transition:height 320ms var(--aui-command-menu-ease)}.aui-command-menu-list-content{position:relative;padding:4px 6px 8px}.aui-command-menu-group-label{position:relative;padding:10px 10px 4px;font-size:12px;font-weight:500;color:var(--aui-command-menu-faint);text-align:start;overflow-wrap:anywhere}.aui-command-menu-highlight{position:absolute;left:6px;right:6px;background:var(--aui-command-menu-active);border-radius:calc(var(--aui-command-menu-r-sm) + 3px);pointer-events:none;transition:top 220ms var(--aui-command-menu-ease),height 220ms var(--aui-command-menu-ease)}.aui-command-menu-highlight[data-first=true]{transition:none}.aui-command-menu-row{position:relative;display:flex;align-items:center;gap:12px;width:100%;height:40px;min-width:0;padding:0 10px;border:0;border-radius:calc(var(--aui-command-menu-r-sm) + 3px);background:transparent;text-align:start;color:var(--aui-command-menu-ink)!important}.aui-command-menu-row[data-pulse=true]{animation:aui-command-menu-press 600ms var(--aui-command-menu-ease)}.aui-command-menu-icon{display:grid;place-items:center;flex:none;width:26px;height:26px;color:var(--aui-command-menu-muted)}.aui-command-menu-icon svg{width:17px;height:17px;stroke-width:1.7px}.aui-command-menu-icon img{display:block;width:17px;height:17px;object-fit:cover;overflow:clip}.aui-command-menu-icon[data-mark=true] img{width:14px;height:14px;object-fit:contain}.aui-command-menu-fallback{font-size:11px;font-weight:600;line-height:1;color:var(--aui-command-menu-fg)}.aui-command-menu-row[aria-selected=true] .aui-command-menu-icon:not([data-mark=true]){color:var(--aui-command-menu-ink)}.aui-command-menu-icon[data-mark=true]{border-radius:7px;background:var(--aui-command-menu-tile);box-shadow:inset 0 0 0 1px var(--aui-command-menu-tileline)}.aui-command-menu-icon[data-mark=true] svg{width:14px;height:14px}.aui-command-menu-copy{display:flex;align-items:baseline;gap:8px;flex:1;min-width:0}.aui-command-menu-name{min-width:0;flex:0 1 auto;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.aui-command-menu-hint{min-width:0;flex:1 100000 auto;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--aui-command-menu-muted)}.aui-command-menu-row mark{color:inherit;font-weight:inherit;background:var(--aui-command-menu-highlight);border-radius:3px}.aui-command-menu-shortcut{display:flex;gap:3px;flex:none;opacity:.85;color:var(--aui-command-menu-muted)}.aui-command-menu-row[aria-selected=true] .aui-command-menu-shortcut{opacity:1;color:var(--aui-command-menu-ink)}.aui-command-menu kbd{display:inline-grid;place-items:center;min-width:20px;height:20px;padding:0 5px;border:0;border-radius:calc(var(--aui-command-menu-r-sm) - 2px);background:var(--aui-command-menu-key);box-shadow:inset 0 0 0 1px var(--aui-command-menu-line),0 1px var(--aui-command-menu-line);font:500 11px var(--aui-font-sans,system-ui)}
.aui-command-menu-footer{display:flex;align-items:center;justify-content:space-between;gap:12px;min-height:38px;padding:0 12px;color:var(--aui-command-menu-muted);font-size:12.5px;line-height:1.4}.aui-command-menu-footer-left{min-width:0;flex:1}.aui-command-menu-wait,.aui-command-menu-global{display:flex;align-items:center;gap:6px}.aui-command-menu-global{flex:none}.aui-command-menu-result{display:flex;align-items:center;gap:6px;min-width:0;padding-block:9px;color:var(--aui-command-menu-ink);animation:aui-command-menu-confirm 260ms var(--aui-command-menu-ease)}.aui-command-menu-result svg{width:14px;height:14px;stroke-width:2.4px;color:var(--aui-command-menu-ok)}.aui-command-menu-result span{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.aui-command-menu-empty{padding:14px 10px;color:var(--aui-command-menu-muted);font-size:12px;line-height:1.5}.aui-command-menu-sr{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap;border:0}
.aui-command-menu-demo{width:100%;max-width:600px;min-width:0;font:13px/1.5 var(--aui-font-sans,system-ui);word-break:keep-all;overflow-wrap:anywhere}.aui-command-menu-caption{font-size:12px;color:var(--aui-fg-muted,#8f8f8f);margin:10px 0 0}.aui-command-menu-demo-controls{display:flex;flex-wrap:wrap;gap:6px;margin:10px 0}.aui-command-menu-demo-controls button{min-width:44px;min-height:44px;border:1px solid var(--aui-line,#ececec);border-radius:8px;padding:6px 10px;background:var(--aui-bg-1,#fff);color:var(--aui-fg,#171717);font:12px var(--aui-font-sans,system-ui);touch-action:manipulation}:where(.dark,[data-theme="dark"]) .aui-command-menu-demo-controls button{background:var(--aui-bg-1,#19191b);color:var(--aui-fg,#ededed);border-color:var(--aui-line,#2a2a2e)}
@container(max-width:480px){.aui-command-menu-shortcut,.aui-command-menu-hint,.aui-command-menu-enter,.aui-command-menu-global,.aui-command-menu-wait{display:none}.aui-command-menu-info{display:grid}.aui-command-menu-row{height:44px}.aui-command-menu-list{max-height:360px}.aui-command-menu-search{padding-inline:12px}.aui-command-menu-footer{min-height:38px}}
@media(pointer:coarse){.aui-command-menu-row{height:44px;min-height:44px}.aui-command-menu-search input{height:44px;min-height:44px;font-size:16px}.aui-command-menu-list{max-height:360px}}
@media(max-width:899.98px){.aui-command-menu[data-modal=true] .aui-command-menu-panel{position:fixed;top:var(--aui-command-menu-vtop,0px);left:0;right:0;display:flex;flex-direction:column;height:var(--aui-command-menu-vh,100dvh);max-height:none;border-radius:0;padding:env(safe-area-inset-top) max(4px,env(safe-area-inset-right)) env(safe-area-inset-bottom) max(4px,env(safe-area-inset-left));background:var(--aui-command-menu-bg)}.aui-command-menu[data-modal=true] .aui-command-menu-card{flex:1;min-height:0;display:flex;flex-direction:column;border-radius:0;box-shadow:none}.aui-command-menu[data-modal=true] .aui-command-menu-search{flex:none;height:auto;min-height:56px}.aui-command-menu[data-modal=true] .aui-command-menu-search input{font-size:16px}.aui-command-menu[data-modal=true] .aui-command-menu-list{flex:1;min-height:0;height:auto!important;max-height:none}.aui-command-menu[data-modal=true] .aui-command-menu-list-content{padding-top:4px}.aui-command-menu[data-modal=true] .aui-command-menu-footer{flex:none;min-height:44px}.aui-command-menu[data-modal=true] .aui-command-menu-global,.aui-command-menu[data-modal=true] .aui-command-menu-wait,.aui-command-menu[data-modal=true] .aui-command-menu-shortcut,.aui-command-menu[data-modal=true] .aui-command-menu-hint,.aui-command-menu[data-modal=true] .aui-command-menu-enter{display:none}.aui-command-menu[data-modal=true] .aui-command-menu-info{display:grid}.aui-command-menu[data-modal=true] .aui-command-menu-row{min-height:44px;height:auto;padding-block:10px}.aui-command-menu[data-modal=true] .aui-command-menu-name{white-space:normal;overflow:visible;overflow-wrap:anywhere}.aui-command-menu[data-modal=true] .aui-command-menu-details{flex:none}}
@keyframes aui-command-menu-press{0%,100%{transform:scale(1)}30%{transform:scale(.985)}}@keyframes aui-command-menu-confirm{from{opacity:0;transform:translateY(3px)}to{opacity:1;transform:none}}@media(prefers-reduced-motion:reduce){.aui-command-menu *{animation:none!important;transition:none!important}}
`;
function defaultGroups(): CommandMenuGroup[] { return [
  { id: 'suggested', label: '추천 작업', recommended: true, commands: [
    { id: 'summarize', label: '실패한 턴 요약', hint: '배포 배포 기록 검토', mark: <Layers />, keywords: 'failure review', shortcut: ['X'] },
    { id: 'research', label: '조사 실행 만들기', hint: 'Research run', icon: <Sparkles />, keywords: 'research 배포' },
    { id: 'deploy', label: '배포 검증 시작', hint: 'Release checks', mark: <Hash />, keywords: 'release' },
    { id: 'agent', label: '에이전트 배정', hint: '검토 담당자', icon: <UserPlus /> },
    { id: 'schedule', label: '실행 예약 만들기', hint: '다음 점검', mark: <ChartNoAxesColumn /> },
  ] },
  { id: 'sharing', label: '결과 공유', commands: [
    { id: 'post', label: '채널에 결과 공유', hint: '#운영-업데이트', mark: <MessageSquare />, shortcut: ['Ctrl', 'Shift', 'P'] },
    { id: 'save', label: '작업 결과 보관', hint: '운영 노트', mark: <FileText /> },
  ] },
  { id: 'general', label: '일반 작업', commands: [
    { id: 'notes', label: '검토 메모 작성', hint: 'Draft note', icon: <FileText />, shortcut: ['N'] },
    { id: 'invite', label: '검토자 초대', hint: '공동 검토', icon: <UserPlus /> },
    { id: 'inbox', label: '확인할 알림 보기', hint: 'Inbox', icon: <Inbox /> },
    { id: 'upload', label: '실행 자료 업로드', hint: '최대 20MB', icon: <Upload /> },
  ] },
  { id: 'navigation', label: '화면 이동', commands: [
    { id: 'home', label: '워크스페이스 홈', hint: 'Overview', icon: <House /> },
    { id: 'activity', label: '도구 호출 기록', hint: 'Activity log', icon: <Terminal /> },
    { id: 'settings', label: '에이전트 설정', hint: 'Runtime settings', icon: <Settings /> },
    { id: 'theme', label: '화면 테마 전환', hint: 'Appearance', icon: <Moon /> },
    { id: 'help', label: '운영 도움말', hint: 'Help center', icon: <CircleHelp /> },
  ] },
]; }
type CommandRow = CommandMenuCommand & { rowKey: string; assistant?: boolean };
/** Canonical decomposition without combining marks, lowercased: accents and case are ignored. */
const fold = (text: string) => text.normalize('NFD').replace(/\p{M}/gu, '').toLowerCase();
/** Marks the first match only when folding keeps the UTF-16 length of the shown text (no recomposition), so Hangul or decomposed accents stay unmarked while still matching. */
function Highlight({ text, query }: { text: string; query: string }) { const folded = fold(text); query = fold(query); const at = query && folded.length === text.length ? folded.indexOf(query) : -1; return at < 0 ? <>{text}</> : <>{text.slice(0, at)}<mark>{text.slice(at, at + query.length)}</mark>{text.slice(at + query.length)}</>; }
function CommandIcon({ command }: { command: CommandMenuCommand }) {
  const node = useRef<HTMLSpanElement>(null); const [failed, setFailed] = useState(false); const art = command.mark ?? command.icon;
  useEffect(() => { setFailed(false); }, [art]);
  // Image errors do not bubble; a capture listener on the slot still sees them.
  useEffect(() => { const slot = node.current; if (!slot) return; const fail = (event: Event) => { if (event.target instanceof HTMLImageElement) setFailed(true); }; slot.addEventListener('error', fail, true); return () => slot.removeEventListener('error', fail, true); }, []);
  return <span ref={node} className="aui-command-menu-icon" data-mark={!!command.mark} aria-hidden="true">{failed ? <span className="aui-command-menu-fallback">{command.label.match(/[\p{L}\p{N}]/u)?.[0]?.toUpperCase()}</span> : art}</span>;
}
export function CommandMenu({ groups: suppliedGroups, placeholder = '실행할 작업이나 기록 찾기…', onRun, onClose, autoFocus = false, open: controlledOpen, defaultOpen = true, onOpenChange, assistantFallback = true, modal = false, mobileOverlay = true, label = 'AgentDesk 명령 검색', labels, className = '' }: CommandMenuProps) {
  const id = useId();
  const text: CommandMenuLabels = { ...commandMenuDefaultLabels, ...labels };
  const root = useRef<HTMLDivElement>(null), panel = useRef<HTMLDivElement>(null), input = useRef<HTMLInputElement>(null), trigger = useRef<HTMLButtonElement>(null), list = useRef<HTMLDivElement>(null), content = useRef<HTMLDivElement>(null);
  const [internalOpen, setInternalOpen] = useState(defaultOpen), [query, setQuery] = useState(''), [active, setActive] = useState<string | null>(null), [recent, setRecent] = useState(''), [pulse, setPulse] = useState(''), [details, setDetails] = useState(false), [viewport, setViewport] = useState<CSSProperties>({}), [mobile, setMobile] = useState(() => typeof window !== 'undefined' && window.innerWidth < 900), [listHeight, setListHeight] = useState<number>(), [layoutVersion, setLayoutVersion] = useState(0), [indicator, setIndicator] = useState({ top: 0, height: 0, first: true }), [modifier, setModifier] = useState('Ctrl');
  const recentTimer = useRef<ReturnType<typeof setTimeout> | null>(null), pulseTimer = useRef<ReturnType<typeof setTimeout> | null>(null), keyboardMove = useRef(false);
  // Drop empty entries, treat non-array command lists as empty and coerce names to strings.
  const groups = useMemo(() => (suppliedGroups === undefined ? defaultGroups() : Array.isArray(suppliedGroups) ? suppliedGroups : []).filter(Boolean).map((group) => ({ ...group, label: String(group.label ?? ''), commands: (Array.isArray(group.commands) ? group.commands : []).filter(Boolean).map((command) => ({ ...command, label: String(command.label ?? ''), hint: command.hint === undefined || command.hint === null || command.hint === '' ? undefined : String(command.hint) })) })), [suppliedGroups]);
  const open = controlledOpen ?? internalOpen;
  const isModal = modal || mobile && mobileOverlay;
  const q = query.trim(), fq = fold(q);
  const semanticRecommended = groups.some((group) => group.recommended !== undefined);
  // Rows are identified by category and original position, so duplicate ids never share selection.
  const filtered = groups.map((group, index) => ({ ...group, key: `group-${index}`, recommended: semanticRecommended ? group.recommended === true : index === 0, commands: group.commands.map((command, at): CommandRow => ({ ...command, rowKey: `${index}:${at}` })).filter((command) => fold(`${command.label} ${command.hint ?? ''} ${Array.isArray(command.keywords) ? command.keywords.join(' ') : command.keywords ?? ''}`).includes(fq)) })).filter((group) => group.commands.length);
  const assistant: CommandRow = { id: 'ask', rowKey: 'assistant', assistant: true, label: text.ask(q), icon: <Sparkles /> };
  // A command runs only through a host handler: its own onSelect or onRun. The assistant row exists only for onRun.
  const runnable = (command: CommandRow) => !command.disabled && (command.assistant ? !!onRun : !!command.onSelect || !!onRun);
  const asks = assistantFallback && !!onRun;
  const visibleGroups = fq && asks ? [{ id: 'assistant', key: 'assistant', label: text.askGroup, recommended: false, commands: [assistant], hiddenTitle: true }, ...filtered.map((group) => ({ ...group, hiddenTitle: false }))] : filtered.map((group) => ({ ...group, hiddenTitle: false }));
  const rows = visibleGroups.flatMap((group) => group.commands);
  const navigable = rows.filter(runnable);
  const current = navigable.find((command) => command.rowKey === active) ?? navigable[0];
  // Each recommended category numbers its first five rows; ⌘/Ctrl+1–5 runs the first five of those rows in display order.
  const numbers = new Map<string, number>(); const recommendations: CommandRow[] = [];
  filtered.filter((group) => group.recommended).forEach((group) => group.commands.filter(runnable).slice(0, 5).forEach((command, at) => { numbers.set(command.rowKey, at + 1); recommendations.push(command); }));
  function changeOpen(next: boolean) { if (controlledOpen === undefined) setInternalOpen(next); onOpenChange?.(next); if (!next) { onClose?.(); requestAnimationFrame(() => trigger.current?.focus({ preventScroll: true })); } }
  function requestClose() { changeOpen(false); }
  function run(command: CommandRow | undefined) { if (!command || !runnable(command)) return; setActive(command.rowKey); if (command.assistant) onRun?.('ask', q); else if (command.onSelect) command.onSelect(); else onRun?.(command.id); setRecent(command.assistant ? text.asked(q) : command.label); setPulse(command.rowKey); if (recentTimer.current) clearTimeout(recentTimer.current); if (pulseTimer.current) clearTimeout(pulseTimer.current); recentTimer.current = setTimeout(() => setRecent(''), 1800); pulseTimer.current = setTimeout(() => setPulse(''), 600); input.current?.focus({ preventScroll: true }); }
  const latest = useRef({ open, isModal, requestClose, changeOpen }); latest.current = { open, isModal, requestClose, changeOpen };
  useEffect(() => {
    const update = () => { const vv = window.visualViewport; setMobile(window.innerWidth < 900); setViewport({ '--aui-command-menu-vh': `${vv?.height ?? window.innerHeight}px`, '--aui-command-menu-vtop': `${vv?.offsetTop ?? 0}px` } as CSSProperties); };
    update(); setModifier(/Mac|iPhone|iPad|iPod/.test(navigator.platform) ? '⌘' : 'Ctrl'); window.addEventListener('resize', update); window.visualViewport?.addEventListener('resize', update); window.visualViewport?.addEventListener('scroll', update);
    const shortcut = (event: globalThis.KeyboardEvent) => { if ((event.metaKey || event.ctrlKey) && event.key.toLocaleLowerCase() === 'k') { event.preventDefault(); if (!latest.current.open) { latest.current.changeOpen(true); requestAnimationFrame(() => { input.current?.focus({ preventScroll: true }); input.current?.select(); }); } else { input.current?.focus({ preventScroll: true }); input.current?.select(); } } };
    document.addEventListener('keydown', shortcut); return () => { window.removeEventListener('resize', update); window.visualViewport?.removeEventListener('resize', update); window.visualViewport?.removeEventListener('scroll', update); document.removeEventListener('keydown', shortcut); };
    // Opening is deliberately a host extension; all search/execution state stays internal.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  useEffect(() => { if (open && autoFocus) input.current?.focus({ preventScroll: true }); }, [open, autoFocus]);
  useEffect(() => () => { if (recentTimer.current) clearTimeout(recentTimer.current); if (pulseTimer.current) clearTimeout(pulseTimer.current); }, []);
  useEffect(() => {
    if (!open || !isModal) return;
    const previous = document.body.style.overflow; document.body.style.overflow = 'hidden';
    const returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const frame = requestAnimationFrame(() => input.current?.focus({ preventScroll: true }));
    const key = (event: globalThis.KeyboardEvent) => {
      if (event.defaultPrevented) return;
      if (event.key === 'Escape') { event.preventDefault(); if (input.current?.value) { setQuery(''); setActive(null); } else latest.current.requestClose(); }
      if (event.key !== 'Tab') return;
      const elements = Array.from(panel.current?.querySelectorAll<HTMLElement>('button:not(:disabled):not([tabindex="-1"]),input:not(:disabled)') ?? []).filter((element) => element.getClientRects().length);
      const first = elements[0], last = elements.at(-1);
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); } else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    };
    const focus = (event: FocusEvent) => { if (event.target instanceof Node && !panel.current?.contains(event.target)) input.current?.focus({ preventScroll: true }); };
    document.addEventListener('keydown', key); document.addEventListener('focusin', focus);
    return () => { cancelAnimationFrame(frame); document.body.style.overflow = previous; document.removeEventListener('keydown', key); document.removeEventListener('focusin', focus); if (returnFocus?.isConnected) returnFocus.focus({ preventScroll: true }); };
  }, [open, isModal]);
  useLayoutEffect(() => {
    const measure = () => { if (content.current && panel.current) { const narrow = (root.current?.clientWidth ?? panel.current.clientWidth) <= 480; setListHeight(Math.min(content.current.getBoundingClientRect().height, narrow || window.matchMedia('(pointer: coarse)').matches ? 360 : 340)); } setLayoutVersion((version) => version + 1); };
    measure(); const observer = new ResizeObserver(measure); if (content.current) observer.observe(content.current);  return () => observer.disconnect();
  }, [open]);
  useLayoutEffect(() => {
    const row = content.current?.querySelector<HTMLElement>('[data-active=true]');
    if (!row) { setIndicator({ top: 0, height: 0, first: true }); return; }
    setIndicator((previous) => ({ top: row.offsetTop, height: row.offsetHeight, first: previous.height === 0 || previous.first }));
    const frame = requestAnimationFrame(() => setIndicator((previous) => ({ ...previous, first: false })));
    if (keyboardMove.current && list.current) { const scroller = list.current; const group = row.closest('.aui-command-menu-group'); const heading = group?.querySelector<HTMLElement>('.aui-command-menu-group-label'); const top = group?.querySelector('.aui-command-menu-row') === row && heading ? heading.offsetTop : row.offsetTop; if (top - 8 < scroller.scrollTop) scroller.scrollTop = Math.max(0, top - 8); else if (row.offsetTop + row.offsetHeight + 8 > scroller.scrollTop + scroller.clientHeight) scroller.scrollTop = row.offsetTop + row.offsetHeight + 8 - scroller.clientHeight; }
    keyboardMove.current = false; return () => cancelAnimationFrame(frame);
  }, [current?.rowKey, query, layoutVersion, open]);
  function moveGroup(delta: number) { const enabledGroups = visibleGroups.filter((group) => group.commands.some(runnable)); if (enabledGroups.length < 2) return false; const at = enabledGroups.findIndex((group) => group.commands.some((command) => command.rowKey === current?.rowKey)); keyboardMove.current = true; setActive(enabledGroups[(at + delta + enabledGroups.length) % enabledGroups.length].commands.find(runnable)!.rowKey); return true; }
  function keys(event: KeyboardEvent<HTMLInputElement>) {
    if (event.nativeEvent.isComposing) return;
    if ((event.metaKey || event.ctrlKey) && /^[1-5]$/.test(event.key)) { const command = recommendations[Number(event.key) - 1]; if (command) { event.preventDefault(); run(command); } return; }
    if (event.altKey && (event.key === 'ArrowDown' || event.key === 'ArrowUp') && isModal) { if (moveGroup(event.key === 'ArrowDown' ? 1 : -1)) event.preventDefault(); return; }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') { event.preventDefault(); keyboardMove.current = true; const at = navigable.findIndex((command) => command.rowKey === current?.rowKey); setActive(navigable.length ? navigable[(at + (event.key === 'ArrowDown' ? 1 : -1) + navigable.length) % navigable.length].rowKey : null); }
    else if (event.key === 'Enter') { event.preventDefault(); run(current); }
    else if (event.key === 'Tab' && !isModal) { if (moveGroup(event.shiftKey ? -1 : 1)) event.preventDefault(); }
    else if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); if (query) { setQuery(''); setActive(null); } else requestClose(); }
    else if (event.key === 'F2') { event.preventDefault(); setDetails((shown) => !shown); }
  }
  return <div ref={root} className={`aui-command-menu ${className}`} style={viewport} data-modal={isModal} data-open={open}>
    <style>{css}</style>{!open && <button ref={trigger} type="button" className="aui-command-menu-launch" aria-haspopup="dialog" aria-expanded="false" onClick={() => changeOpen(true)}><Search size={18} aria-hidden="true" /><span>{text.launch}</span><kbd aria-hidden="true">{modifier} K</kbd></button>}
    {open && <>{isModal && <div className="aui-command-menu-backdrop" aria-hidden="true" onPointerDown={requestClose} />}<div ref={panel} className="aui-command-menu-panel" role={isModal ? 'dialog' : undefined} aria-modal={isModal ? true : undefined} aria-label={isModal ? label : undefined}>
      <div className="aui-command-menu-card"><div className="aui-command-menu-search" onClick={() => input.current?.focus({ preventScroll: true })}><Search size={18} strokeWidth={1.8} aria-hidden="true" /><input ref={input} value={query} placeholder={placeholder} dir="auto" aria-label={text.search} role="combobox" aria-expanded="true" aria-autocomplete="list" aria-controls={`${id}-list`} aria-activedescendant={current ? `${id}-option-${navigable.indexOf(current)}` : undefined} autoComplete="off" spellCheck={false} onChange={(event) => { setQuery(event.target.value); setActive(null); }} onKeyDown={keys} />{navigable.length > 0 && <span className="aui-command-menu-enter" aria-hidden="true"><CornerDownLeft size={14} strokeWidth={2} />{text.enter}</span>}<button type="button" className="aui-command-menu-info" aria-label={details ? text.detailsHide : text.detailsShow} aria-expanded={details} aria-controls={`${id}-details`} onClick={(event) => { event.stopPropagation(); setDetails(!details); }}><Info size={17} aria-hidden="true" /></button>{isModal && <button type="button" className="aui-command-menu-close" aria-label={text.close} onClick={(event) => { event.stopPropagation(); requestClose(); }}><X size={18} aria-hidden="true" /></button>}</div>
      {details && <div id={`${id}-details`} className="aui-command-menu-details" role="region" aria-label={text.details}>{visibleGroups.flatMap((group) => group.commands).map((command) => <p key={command.rowKey}><strong>{command.label}</strong>{command.hint && <> · {command.hint}</>}</p>)}</div>}
      <div ref={list} className="aui-command-menu-list" role="listbox" id={`${id}-list`} aria-label={text.list} style={{ height: listHeight }} onPointerDown={(event) => { if (event.pointerType === 'mouse') event.preventDefault(); }}><div ref={content} className="aui-command-menu-list-content"><span className="aui-command-menu-highlight" data-first={indicator.first} aria-hidden="true" style={{ top: indicator.top, height: indicator.height }} />
      {visibleGroups.map((group, groupIndex) => <div className="aui-command-menu-group" key={group.key} role="group" aria-label={group.hiddenTitle ? group.label : undefined} aria-labelledby={group.hiddenTitle || !group.label ? undefined : `${id}-group-${groupIndex}`}>{!group.hiddenTitle && group.label && <div className="aui-command-menu-group-label" id={`${id}-group-${groupIndex}`} dir="auto">{group.label}</div>}{group.commands.map((command) => { const index = navigable.indexOf(command); const number = numbers.get(command.rowKey); const canRun = runnable(command); const shortcut = number ? [modifier, String(number)] : canRun ? command.shortcut ?? [] : []; return <button key={command.rowKey} type="button" className="aui-command-menu-row" id={index < 0 ? undefined : `${id}-option-${index}`} role="option" aria-selected={command.rowKey === current?.rowKey} disabled={!canRun} data-static={!canRun && !command.disabled ? '' : undefined} data-active={command.rowKey === current?.rowKey} data-pulse={pulse === command.rowKey} tabIndex={-1} onPointerMove={(event) => { if (event.pointerType === 'mouse' && canRun) { keyboardMove.current = false; setActive(command.rowKey); } }} onClick={() => run(command)}><CommandIcon command={command} /><span className="aui-command-menu-copy"><span className="aui-command-menu-name" title={command.label} dir="auto"><Highlight text={command.label} query={fq ? q : ''} /></span>{command.hint && <span className="aui-command-menu-hint" title={command.hint} dir="auto"><Highlight text={command.hint} query={fq ? q : ''} /></span>}</span>{shortcut.length > 0 && <span className="aui-command-menu-shortcut" aria-hidden="true">{shortcut.map((key, keyIndex) => <kbd key={`${key}-${keyIndex}`}>{key}</kbd>)}</span>}</button>; })}</div>)}
      {rows.length === 0 && <div className="aui-command-menu-empty">{q ? text.noMatch : asks ? `${text.empty} ${text.emptyAsk}` : text.empty}</div>}</div></div></div>
      <footer className="aui-command-menu-footer" aria-live="polite" aria-atomic="true"><div className="aui-command-menu-footer-left">{recent ? <div key={recent} className="aui-command-menu-result"><Check aria-hidden="true" /><span title={recent} dir="auto">{recent}</span></div> : navigable.length > 0 && <span className="aui-command-menu-wait"><kbd>{isModal ? 'Alt ↑ ↓' : 'Tab'}</kbd> {text.groupMove}</span>}</div><span className="aui-command-menu-global" aria-hidden="true"><kbd>{modifier}</kbd><kbd>K</kbd> {text.searchKey}</span></footer>
    </div></>}
  </div>;
}
export type CommandMenuDemoState = 'default' | 'empty' | 'long' | 'modal' | 'compact' | 'wide-compact' | 'recommendations' | 'normalization' | 'read-only' | 'item-select';
const englishCommandLabels: Partial<CommandMenuLabels> = {
  launch: 'Open command menu', search: 'Search commands', detailsShow: 'Show descriptions (F2)', detailsHide: 'Hide descriptions (F2)', close: 'Close command menu',
  details: 'Descriptions', list: 'Available commands', ask: (query) => `Ask the assistant “${query}”`, askGroup: 'Assistant', asked: (query) => `Asked the assistant: ${query}`,
  empty: 'No commands yet.', emptyAsk: 'Search to ask the assistant.', noMatch: 'No matching command.', groupMove: 'Next group', searchKey: 'Search',
};
export function CommandMenuDemo({ state = 'default' }: { state?: CommandMenuDemoState }) {
  const [executed, setExecuted] = useState(''), [requestFocus, setRequestFocus] = useState(false);
  let groups: CommandMenuGroup[] | undefined = state === 'empty' ? [] : state === 'long' ? defaultGroups().map((group, index) => ({ ...group, commands: group.commands.map((command, at) => ({ ...command, label: index === 0 && at === 0 ? '릴레이 지연과 다중 에이전트 협업 세션의 예외 처리 내역을 검토하고 다음 작업을 정리하기' : index === 1 && at === 0 ? 'agentdesk_release_validation_observability_channel_with_extended_identifier' : command.label, hint: `${command.hint ?? ''} · 긴 보조 설명도 상세 보기에서 어절 단위로 끝까지 확인합니다.`, disabled: index === 2 && at === 3, shortcut: index === 2 ? ['Ctrl', 'Shift', 'Alt', 'LongKey'] : command.shortcut })) })) : undefined;
  if (state === 'recommendations') groups = [{ id: 'before', label: '일반 작업 먼저', recommended: false, commands: [{ id: 'before', label: '일반 기록', icon: <FileText />, shortcut: ['Ctrl', '9'] }] }, { id: 'recommended', label: '추천 필터', recommended: true, commands: Array.from({ length: 7 }, (_, index) => ({ id: `recommended-${index}`, label: `추천 검증 ${index + 1}`, hint: index % 2 ? '배포 검증' : '코드 검토', shortcut: ['Z'], icon: <Bot /> })) }];
  if (state === 'read-only') return <div className="aui-command-menu-demo"><CommandMenu defaultOpen /><p className="aui-command-menu-caption">onRun이 없어 명령은 글로만 보여요. 고르거나 실행할 수 없고 실행 확인도 뜨지 않아요.</p></div>;
  if (state === 'item-select') return <div className="aui-command-menu-demo"><CommandMenu defaultOpen label="Command search" placeholder="Find a command…" labels={englishCommandLabels} groups={[{ id: 'go', label: 'Go to', commands: [
    { id: 'home', label: 'Workspace home', hint: 'Overview', icon: <House />, onSelect: () => setExecuted('onSelect · home') },
    { id: 'activity', label: 'Tool call log', hint: 'Activity', icon: <Terminal />, shortcut: ['G', 'A'], onSelect: () => setExecuted('onSelect · activity') },
    { id: 'deploy', label: 'Start release checks', hint: 'No handler · text only', icon: <Rocket />, shortcut: ['R'] },
  ] }]} /><p className="aui-command-menu-caption" role="status">{executed ? `Host received · ${executed}` : 'Only rows with their own onSelect run. The last row is text only and there is no assistant row.'}</p></div>;
  // Deliberately loose host data: empty entries, a non-array list, missing names and numeric text.
  if (state === 'normalization') groups = [
    { id: 'relay', label: '정규화 검색 · Café relay checks', recommended: true, commands: [
      { id: 'cafe', label: 'Café', hint: 'Résumé of relay checks', icon: <Terminal /> },
      { id: 'cafe-check', label: 'café 검증', hint: '한글이 섞여 정규화 길이가 달라짐', icon: <ShieldCheck /> },
      { id: 'deploy-check', label: '배포 검증', hint: 'release gate · dcserver', icon: <Rocket /> },
      { id: 'combining', label: 'Cafe\u0301 decomposed log', hint: '분해형 é라 강조를 생략합니다', icon: <FileText /> },
      { id: 'dup', label: '중복 식별자 행 A', hint: 'id: dup', icon: <Hash /> },
      { id: 'dup', label: '중복 식별자 행 B', hint: 'id: dup', icon: <Hash /> },
      { id: 'quote"\'<id>', label: '특수문자 식별자 실행', hint: 'id: quote"\'<id>', icon: <Bot /> },
      { id: 'ask', label: '도우미와 같은 식별자', hint: 'id: ask · 일반 실행', icon: <MessageSquare /> },
      { id: 'image', label: 'broken 이미지 마크', hint: '첫 글자 B로 대체', mark: <img src="data:image/svg+xml,%3Csvg" alt="" /> },
    ] },
    null,
    { id: 'loose', label: '', commands: 'not-an-array' },
    { id: 'missing', label: '불완전한 입력 정리', commands: [null, { id: 'no-label' }, { id: 'numeric', label: 1280, hint: 42 }] },
  ] as unknown as CommandMenuGroup[];
  return <div className="aui-command-menu-demo" style={state === 'compact' ? { maxWidth: 480 } : state === 'wide-compact' ? { maxWidth: 481 } : undefined}><CommandMenu groups={groups} modal={state === 'modal'} autoFocus={requestFocus} onRun={(command, text) => setExecuted(text === undefined ? `id: ${command}` : `id: ${command} · query: ${JSON.stringify(text)}`)} defaultOpen /><div className="aui-command-menu-demo-controls"><button type="button" onClick={() => setRequestFocus((focus) => !focus)}>입력 자동 초점 {requestFocus ? '끄기' : '켜기'}</button></div><p className="aui-command-menu-caption" role="status">{executed ? `앱이 받은 명령 · ${executed}` : state === 'empty' ? '빈 목록 확장 안내입니다. 검색하면 도우미 질의가 첫 행에 나타납니다.' : state === 'normalization' ? 'cafe를 입력하면 Café·café 검증·분해형 행이 모두 남지만 정규화 뒤 길이가 그대로인 Café만 칠합니다. 배포를 입력하면 배포 검증은 결과에 남되 한글 분해로 길이가 달라져 강조가 없습니다. resume는 설명만 따로 칠하고, 중복·특수문자 id 행은 각각 따로 강조·실행됩니다.' : '작업명·설명·숨은 검색어에서 배포를 찾아보고 Enter와 추천 번호를 비교하세요.'}</p></div>;
}
