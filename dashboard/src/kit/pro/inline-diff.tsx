// Kit provenance: itismyfield/agentdesk-ui-kit@3024685b30
import { forwardRef, useEffect, useId, useImperativeHandle, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import { Check, ChevronLeft, ChevronRight, Columns2, RotateCcw, Sparkles, Undo2, X } from 'lucide-react';

// Independently implemented from clean-specs/inline-diff/spec.md (11/17 upstream sync).
export type DiffSegment = { type: 'equal'; text: string } | { type: 'change'; remove: string; insert: string };
type Change = Extract<DiffSegment, { type: 'change' }>;
type Decision = 'accepted' | 'rejected';
type Review = Record<number, Decision>;
export interface InlineDiffHandle {
  highlight: (index: number | null) => void;
  accept: (index: number) => void;
  reject: (index: number) => void;
  acceptAll: () => void;
  rejectAll: () => void;
  undo: () => void;
  reset: () => void;
}
export interface InlineDiffLabels {
  regionLabel: string;
  proposalUnit: string;
  prevProposal: string;
  nextProposal: string;
  rejectAll: string;
  acceptAll: string;
  reviewDone: string;
  undo: string;
  originalColumn: string;
  suggestedColumn: string;
  emptyBody: string;
  decideGroup: string;
  decideProposalGroup: (index: string) => string;
  acceptCurrent: string;
  rejectCurrent: string;
  accept: string;
  reject: string;
  toolsSummary: string;
  unifiedView: string;
  splitView: string;
  undoLast: string;
  restartReview: string;
  sheetAria: (index: string) => string;
  sheetTitle: (index: string, total: string) => string;
  closeSheet: string;
  beforeHeading: string;
  beforeEmpty: string;
  afterHeading: string;
  afterEmpty: string;
  whitespaceChange: string;
  replaceChange: (before: string, after: string) => string;
  insertChange: (after: string) => string;
  removeChange: (before: string) => string;
  proposalAria: (index: string, total: string, detail: string) => string;
  remainingNotice: (remaining: string) => string;
  allDoneNotice: string;
  singleDecidedNotice: (detail: string, accepted: boolean, ending: string) => string;
  multiDecidedNotice: (decided: string, accepted: boolean, ending: string) => string;
  undoneNotice: (undone: string, remaining: string) => string;
}
export const inlineDiffDefaultLabels: InlineDiffLabels = {
  regionLabel: '문장 제안 검토',
  proposalUnit: '개 제안',
  prevProposal: '이전 제안',
  nextProposal: '다음 제안',
  rejectAll: '모두 거절',
  acceptAll: '모두 수락',
  reviewDone: '검토 완료',
  undo: '실행 취소',
  originalColumn: '원문 · 검토 결과',
  suggestedColumn: '제안 · 검토 결과',
  emptyBody: '비교할 문장이 없습니다. 원문과 수정 제안을 연결해 주세요.',
  decideGroup: '제안 결정',
  decideProposalGroup: (index) => `제안 ${index} 결정`,
  acceptCurrent: '현재 제안 수락',
  rejectCurrent: '현재 제안 거절',
  accept: '수락',
  reject: '거절',
  toolsSummary: '검토 보조 도구',
  unifiedView: '통합 보기',
  splitView: '좌우 비교',
  undoLast: '마지막 결정 취소',
  restartReview: '처음부터 검토',
  sheetAria: (index) => `제안 ${index} 검토`,
  sheetTitle: (index, total) => `제안 ${index} / ${total}`,
  closeSheet: '제안 검토 닫기',
  beforeHeading: '이전 내용',
  beforeEmpty: '추가 제안 · 이전 내용 없음',
  afterHeading: '제안 내용',
  afterEmpty: '삭제 제안 · 새 내용 없음',
  whitespaceChange: '공백 변경',
  replaceChange: (before, after) => `“${before}”을 “${after}”으로 바꾸기`,
  insertChange: (after) => `“${after}” 추가`,
  removeChange: (before) => `“${before}” 삭제`,
  proposalAria: (index, total, detail) => `전체 ${total}개 중 제안 ${index}. ${detail}. Y 또는 Enter 수락, N·Backspace·Delete 거절.`,
  remainingNotice: (remaining) => `${remaining}개 제안이 남았습니다.`,
  allDoneNotice: '모든 제안 검토를 마쳤습니다.',
  singleDecidedNotice: (detail, accepted, ending) => `${detail} 제안을 ${accepted ? '수락' : '거절'}했습니다. ${ending}`,
  multiDecidedNotice: (decided, accepted, ending) => `${decided}개 제안을 모두 ${accepted ? '수락' : '거절'}했습니다. ${ending}`,
  undoneNotice: (undone, remaining) => `마지막 결정 ${undone}개를 되돌렸습니다. ${remaining}개 제안이 남았습니다.`,
};
export interface InlineDiffProps {
  original?: string;
  suggested?: string;
  /** Supplied boundaries, including an empty array, take precedence over automatic comparison. */
  changes?: DiffSegment[];
  onChange?: (text: string) => void;
  onDone?: (text: string) => void;
  label?: string;
  labels?: Partial<InlineDiffLabels>;
  defaultView?: 'unified' | 'split';
  /** Explicit log view; ordinary prose always wraps. */
  preserveLines?: boolean;
  /** Optional secondary controls keep the default reading card compact. */
  showTools?: boolean;
  /** Role-based appearance overrides, applied as this component's CSS custom properties. */
  appearance?: InlineDiffAppearance;
  className?: string;
}
export interface InlineDiffAppearance { font?: string; ink?: string; text?: string; textSecondary?: string; tray?: string; surface?: string; line?: string; subtle?: string; add?: string; remove?: string; success?: string; danger?: string; primary?: string; onPrimary?: string; focus?: string; ease?: string; cardShadow?: string; radius?: string; radiusMedium?: string }
const appearanceVars: Record<keyof InlineDiffAppearance, string> = { font: 'font', ink: 'fg', text: 'dim', textSecondary: 'muted', tray: 'tray', surface: 'card', line: 'line', subtle: 'hover', add: 'add-base', remove: 'remove-base', success: 'ok', danger: 'err', primary: 'primary', onPrimary: 'on-primary', focus: 'focus', ease: 'e', cardShadow: 'shadow', radius: 'radius', radiusMedium: 'radius-md' };
const count = (value: number) => value.toLocaleString('en-US');
const toText = (value: unknown) => value == null ? '' : String(value);

// Flags, modifier/ZWJ emoji, words with combining marks, whitespace runs, then any other symbol with its marks.
const emoji = '\\p{Extended_Pictographic}(?:\\p{Emoji_Modifier}|\\uFE0F|\\u20E3)*';
const tokenPattern = new RegExp(`\\p{Regional_Indicator}{2}|${emoji}(?:\\u200D${emoji})*|[\\p{L}\\p{M}\\p{N}_'’]+|\\s+|[^\\p{L}\\p{N}_'’\\s]\\p{M}*`, 'gu');
const tokenize = (text: string) => text.match(tokenPattern) ?? [];
type Edit = [kind: 'equal' | 'remove' | 'insert', token: string];

/** Myers shortest edit script capped at `limit` insertions + deletions; null when the cap is exceeded. */
function boundedEdits(a: string[], b: string[], limit: number): Edit[] | null {
  const n = a.length, m = b.length, max = Math.min(limit, n + m), offset = max + 1;
  const v = new Int32Array(2 * max + 3), trace: Int32Array[] = [];
  for (let d = 0; d <= max; d++) {
    trace.push(v.slice(offset - d, offset + d + 1));
    for (let k = -d; k <= d; k += 2) {
      // Ties step right (delete) so equal-length keeps prefer removing the original token first.
      let x = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1]) ? v[offset + k + 1] : v[offset + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) { x++; y++; }
      v[offset + k] = x;
      if (x >= n && y >= m) return backtrack(trace, a, b);
    }
  }
  return null;
}
function backtrack(trace: Int32Array[], a: string[], b: string[]) {
  const edits: Edit[] = [];
  let x = a.length, y = b.length;
  for (let d = trace.length - 1; d >= 0; d--) {
    let previousX = 0, previousY = 0;
    if (d > 0) {
      const v = trace[d], at = (k: number) => v[k + d], k = x - y;
      const previousK = k === -d || (k !== d && at(k - 1) < at(k + 1)) ? k + 1 : k - 1;
      previousX = at(previousK); previousY = previousX - previousK;
    }
    while (x > previousX && y > previousY) edits.push(['equal', a[--x]]), y--;
    if (d > 0) edits.push(x === previousX ? ['insert', b[--y]] : ['remove', a[--x]]);
    x = previousX; y = previousY;
  }
  return edits.reverse();
}

/** Supplied segments keep their boundaries; values are stringified and empty or unknown entries are dropped. */
export function normalizeSegments(input: unknown): DiffSegment[] {
  if (!Array.isArray(input)) return [];
  return input.flatMap((item): DiffSegment[] => {
    if (!item || typeof item !== 'object') return [];
    const part = item as Record<string, unknown>;
    if (part.type === 'equal') { const text = toText(part.text); return text ? [{ type: 'equal', text }] : []; }
    if (part.type !== 'change') return [];
    const remove = toText(part.remove), insert = toText(part.insert);
    return remove || insert ? [{ type: 'change', remove, insert }] : [];
  });
}

/** Token LCS with deletion-first ties; large middles fall back to a 1500-edit Myers pass, then one replacement. */
export function diffWords(originalInput: unknown, suggestedInput: unknown): DiffSegment[] {
  const original = toText(originalInput), suggested = toText(suggestedInput);
  if (original === suggested) return original ? [{ type: 'equal', text: original }] : [];
  const left = tokenize(original), right = tokenize(suggested);
  let start = 0, endLeft = left.length, endRight = right.length;
  while (start < endLeft && start < endRight && left[start] === right[start]) start++;
  while (endLeft > start && endRight > start && left[endLeft - 1] === right[endRight - 1]) { endLeft--; endRight--; }
  const result: DiffSegment[] = [];
  const equal = (text: string) => {
    if (!text) return;
    const last = result.at(-1);
    if (last?.type === 'equal') last.text += text; else result.push({ type: 'equal', text });
  };
  const change = (remove: string, insert: string) => {
    const last = result.at(-1);
    if (last?.type === 'change') { last.remove += remove; last.insert += insert; }
    else if (remove || insert) result.push({ type: 'change', remove, insert });
  };
  equal(left.slice(0, start).join(''));
  const a = left.slice(start, endLeft), b = right.slice(start, endRight);
  if (a.length * b.length > 4_000_000) {
    const edits = boundedEdits(a, b, 1500);
    if (!edits) change(a.join(''), b.join(''));
    else edits.forEach(([kind, token]) => kind === 'equal' ? equal(token) : kind === 'remove' ? change(token, '') : change('', token));
  } else {
    const width = b.length + 1, table = new Uint32Array((a.length + 1) * width);
    for (let i = a.length - 1; i >= 0; i--) for (let j = b.length - 1; j >= 0; j--) {
      table[i * width + j] = a[i] === b[j] ? table[(i + 1) * width + j + 1] + 1 : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    }
    let i = 0, j = 0;
    while (i < a.length || j < b.length) {
      if (i < a.length && j < b.length && a[i] === b[j]) { equal(a[i++]); j++; }
      else if (i < a.length && (j === b.length || table[(i + 1) * width + j] >= table[i * width + j + 1])) change(a[i++], '');
      else change('', b[j++]);
    }
  }
  equal(left.slice(endLeft).join(''));
  // Shift a pure insertion/deletion's leading whitespace to its right-hand edge.
  for (let i = 0; i < result.length - 1; i++) {
    const item = result[i], after = result[i + 1];
    if (item.type !== 'change' || after.type !== 'equal' || (!!item.remove === !!item.insert)) continue;
    const key = item.remove ? 'remove' : 'insert';
    let lead = item[key].match(/^\s+/)?.[0], next = after.text.match(/^\s+/)?.[0];
    while (lead && lead === next) {
      const previous = result[i - 1];
      if (previous?.type === 'equal') previous.text += lead;
      else { result.splice(i, 0, { type: 'equal', text: lead }); i++; }
      item[key] = item[key].slice(lead.length) + lead;
      after.text = after.text.slice(lead.length);
      lead = item[key].match(/^\s+/)?.[0]; next = after.text.match(/^\s+/)?.[0];
    }
  }
  for (let i = 1; i < result.length - 1;) {
    const before = result[i - 1], gap = result[i], after = result[i + 1];
    if (before.type === 'change' && gap.type === 'equal' && /^\s+$/.test(gap.text) && after.type === 'change') {
      before.remove += gap.text + after.remove; before.insert += gap.text + after.insert; result.splice(i, 2);
    } else i++;
  }
  const normalized: DiffSegment[] = [];
  const appendEqual = (text: string) => {
    if (!text) return;
    const last = normalized.at(-1);
    if (last?.type === 'equal') last.text += text; else normalized.push({ type: 'equal', text });
  };
  for (const item of result) {
    if (item.type === 'equal') { appendEqual(item.text); continue; }
    let { remove, insert } = item;
    if (remove && insert) {
      const prefix = remove.match(/^\s+/)?.[0] ?? '';
      if (prefix && insert.startsWith(prefix) && remove.length > prefix.length && insert.length > prefix.length) {
        appendEqual(prefix); remove = remove.slice(prefix.length); insert = insert.slice(prefix.length);
      }
      const suffix = remove.match(/\s+$/)?.[0] ?? '';
      if (suffix && insert.endsWith(suffix) && remove.length > suffix.length && insert.length > suffix.length) {
        normalized.push({ type: 'change', remove: remove.slice(0, -suffix.length), insert: insert.slice(0, -suffix.length) }); appendEqual(suffix); continue;
      }
    }
    if (remove || insert) normalized.push({ type: 'change', remove, insert });
  }
  return normalized;
}

function description(change: Change, words: InlineDiffLabels) {
  const before = change.remove.trim(), after = change.insert.trim();
  return !before && !after ? words.whitespaceChange : before && after ? words.replaceChange(before, after) : after ? words.insertChange(after) : words.removeChange(before);
}

const styles = `
@layer kit {
.aui-inline-diff{--aui-inline-diff-tray:var(--aui-bg-2,#f0f0f0);--aui-inline-diff-card:var(--aui-bg-1,#fff);--aui-inline-diff-fg:var(--aui-fg,#171717);--aui-inline-diff-dim:var(--aui-fg-dim,#525252);--aui-inline-diff-muted:var(--aui-fg-muted,#8a8a8a);--aui-inline-diff-line:var(--aui-line,#ececec);--aui-inline-diff-hover:var(--aui-bg-3,#f5f5f5);--aui-inline-diff-ok:var(--aui-ok,#1e8a4f);--aui-inline-diff-check:var(--aui-inline-diff-ok);--aui-inline-diff-err:var(--aui-err,#c4320a);--aui-inline-diff-add-base:#5fc88a;--aui-inline-diff-remove-base:#f04438;--aui-inline-diff-add-mix:10%;--aui-inline-diff-add-mix-active:18%;--aui-inline-diff-remove-mix:8%;--aui-inline-diff-remove-mix-active:15%;--aui-inline-diff-strike-mix:55%;--aui-inline-diff-add:var(--aui-inline-diff-add-bg,color-mix(in srgb,var(--aui-inline-diff-add-base) var(--aui-inline-diff-add-mix),transparent));--aui-inline-diff-add-active:var(--aui-inline-diff-add-active-bg,color-mix(in srgb,var(--aui-inline-diff-add-base) var(--aui-inline-diff-add-mix-active),transparent));--aui-inline-diff-remove:var(--aui-inline-diff-remove-bg,color-mix(in srgb,var(--aui-inline-diff-remove-base) var(--aui-inline-diff-remove-mix),transparent));--aui-inline-diff-remove-active:var(--aui-inline-diff-remove-active-bg,color-mix(in srgb,var(--aui-inline-diff-remove-base) var(--aui-inline-diff-remove-mix-active),transparent));--aui-inline-diff-strike:var(--aui-inline-diff-strike-color,color-mix(in srgb,var(--aui-inline-diff-remove-base) var(--aui-inline-diff-strike-mix),transparent));--aui-inline-diff-primary:var(--aui-inline-diff-fg);--aui-inline-diff-on-primary:var(--aui-inline-diff-card);--aui-inline-diff-radius:var(--aui-r-lg,16px);--aui-inline-diff-radius-md:var(--aui-r-md,10px);--aui-inline-diff-font:var(--aui-font-sans,Inter,ui-sans-serif,system-ui,sans-serif);--aui-inline-diff-pop:var(--aui-fg,#171717);--aui-inline-diff-pop-fg:var(--aui-bg-1,#fff);--aui-inline-diff-pop-hover:var(--aui-inline-diff-pop-hover-bg,rgba(255,255,255,.14));--aui-inline-diff-key:var(--aui-inline-diff-key-color,rgba(255,255,255,.55));--aui-inline-diff-focus:var(--aui-inline-diff-focus-color,rgb(0 96 226 / .7));--aui-inline-diff-shadow:var(--aui-shadow-sm,0 1px 2px #0000000a);--aui-inline-diff-e:cubic-bezier(.22,1,.36,1);width:100%;min-width:0;container-type:inline-size;color:var(--aui-inline-diff-fg);font-family:var(--aui-inline-diff-font);font-size:13px;line-height:1.5;word-break:keep-all;overflow-wrap:anywhere;-webkit-tap-highlight-color:transparent}
:where(.dark,[data-theme="dark"]) .aui-inline-diff{--aui-inline-diff-tray:var(--aui-bg-2,#111113);--aui-inline-diff-card:var(--aui-bg-1,#19191b);--aui-inline-diff-fg:var(--aui-fg,#ededed);--aui-inline-diff-dim:var(--aui-fg-dim,#b4b4b8);--aui-inline-diff-muted:var(--aui-fg-muted,#8b8b92);--aui-inline-diff-line:var(--aui-line,#2a2a2e);--aui-inline-diff-hover:var(--aui-bg-3,#232326);--aui-inline-diff-ok:var(--aui-ok,#7dcf9f);--aui-inline-diff-check:var(--aui-ok,#5fc88a);--aui-inline-diff-err:var(--aui-err,#ff8a7a);--aui-inline-diff-add-base:#7dcf9f;--aui-inline-diff-remove-base:#ff6b5f;--aui-inline-diff-add-mix:13%;--aui-inline-diff-add-mix-active:22%;--aui-inline-diff-remove-mix:12%;--aui-inline-diff-remove-mix-active:20%;--aui-inline-diff-strike-mix:60%;--aui-inline-diff-pop:var(--aui-fg,#f4f4f5);--aui-inline-diff-pop-fg:var(--aui-bg-1,#171717);--aui-inline-diff-pop-hover:var(--aui-inline-diff-pop-hover-bg,rgba(0,0,0,.08));--aui-inline-diff-key:var(--aui-inline-diff-key-color,rgba(0,0,0,.50));--aui-inline-diff-focus:var(--aui-inline-diff-focus-color,rgb(77 141 255 / .8));--aui-inline-diff-shadow:var(--aui-shadow-sm,0 1px 2px #0000004d)}
.aui-inline-diff *,.aui-inline-diff *::before,.aui-inline-diff *::after{box-sizing:border-box}
.aui-inline-diff-shell{padding:4px;border-radius:calc(var(--aui-inline-diff-radius) + 4px);background:var(--aui-inline-diff-tray)}
.aui-inline-diff-label{display:flex;align-items:center;gap:6px;margin:0;padding:6px 8px 4px;color:var(--aui-inline-diff-muted);font-size:12.5px;line-height:1.3;letter-spacing:-.003em}
.aui-inline-diff-label span{flex:1 1 auto;min-width:0;text-align:start;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.aui-inline-diff-label svg{flex:none}
.aui-inline-diff-card{position:relative;min-width:0;container-type:inline-size;container-name:aui-inline-diff-card;border-radius:var(--aui-inline-diff-radius);background:var(--aui-inline-diff-card);box-shadow:0 0 0 1px var(--aui-inline-diff-line),var(--aui-inline-diff-shadow)}
.aui-inline-diff button{display:inline-flex;align-items:center;justify-content:center;gap:6px;height:30px;margin:0;padding:0 11px;border:0;border-radius:calc(var(--aui-inline-diff-radius-md) - 2px);background:transparent;color:var(--aui-inline-diff-fg);box-shadow:inset 0 0 0 1px var(--aui-inline-diff-line);font:inherit;font-weight:500;line-height:1;letter-spacing:-.006em;white-space:nowrap;cursor:pointer;touch-action:manipulation;transition:background 160ms ease,color 160ms ease,opacity 160ms ease,transform 160ms var(--aui-inline-diff-e)}
.aui-inline-diff button:not(:disabled):hover{background:var(--aui-inline-diff-hover)}.aui-inline-diff button:not(:disabled):active{transform:scale(.97)}.aui-inline-diff button:disabled{opacity:.4;cursor:default}.aui-inline-diff button:focus-visible,.aui-inline-diff [role="button"]:focus-visible,.aui-inline-diff summary:focus-visible{outline:2px solid var(--aui-inline-diff-focus);outline-offset:2px}
.aui-inline-diff-toolbar{display:grid;padding:10px 10px 10px 16px;border-bottom:1px solid var(--aui-inline-diff-line)}
.aui-inline-diff-toolbar-layer{grid-area:1/1;display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:8px;opacity:1;transform:none;filter:none;visibility:visible;transition:opacity 320ms ease 80ms,transform 460ms var(--aui-inline-diff-e) 80ms,filter 320ms ease 80ms,visibility 0s}
.aui-inline-diff-toolbar-layer[data-hidden="true"]{opacity:0;transform:translateY(4px);filter:blur(2px);visibility:hidden;pointer-events:none;transition:opacity 260ms ease,transform 420ms var(--aui-inline-diff-e),filter 260ms ease,visibility 0s 420ms}
.aui-inline-diff-count,.aui-inline-diff-complete{display:inline-flex;align-items:center;gap:7px;font-size:13px;font-weight:500;line-height:1;letter-spacing:-.006em;white-space:nowrap}
.aui-inline-diff-count-number{display:inline-grid;place-items:center;min-width:20px;height:20px;padding:0 6px;border-radius:999px;background:var(--aui-inline-diff-add);color:var(--aui-inline-diff-ok);font-size:11.5px;font-weight:600;font-variant-numeric:tabular-nums}
.aui-inline-diff-actions,.aui-inline-diff-nav{display:flex;align-items:center;gap:6px}.aui-inline-diff-actions{margin-left:auto}.aui-inline-diff-nav{gap:2px;margin-right:2px}
.aui-inline-diff .aui-inline-diff-arrow{width:28px;padding:0;box-shadow:none;color:var(--aui-inline-diff-muted)}.aui-inline-diff .aui-inline-diff-arrow:not(:disabled):hover{color:var(--aui-inline-diff-fg)}
.aui-inline-diff .aui-inline-diff-primary{background:var(--aui-inline-diff-primary);color:var(--aui-inline-diff-on-primary);box-shadow:none}.aui-inline-diff .aui-inline-diff-primary:not(:disabled):hover{background:var(--aui-inline-diff-primary);opacity:.88}
.aui-inline-diff-check{color:var(--aui-inline-diff-check);stroke-width:1.8;stroke-dasharray:30;stroke-dashoffset:30}.aui-inline-diff-toolbar-layer[data-hidden="false"] .aui-inline-diff-check{animation:aui-inline-diff-check 420ms var(--aui-inline-diff-e) 220ms forwards}
.aui-inline-diff-body{padding:16px 18px 18px;min-width:0}.aui-inline-diff-prose{margin:0;text-align:start;color:var(--aui-inline-diff-dim);font-size:15px;line-height:1.65;letter-spacing:-.006em;white-space:pre-line;word-break:keep-all;overflow-wrap:anywhere;min-width:0}
.aui-inline-diff-proposal{border-radius:4px;cursor:pointer;outline:none}.aui-inline-diff-proposal[data-decision]{cursor:text}.aui-inline-diff-proposal del,.aui-inline-diff-proposal ins{padding:1px 2px;border-radius:4px;box-decoration-break:clone;-webkit-box-decoration-break:clone;transition:background 360ms ease,color 360ms ease,text-decoration-color 300ms ease,font-size 460ms var(--aui-inline-diff-e),padding 460ms var(--aui-inline-diff-e),margin 460ms var(--aui-inline-diff-e),opacity 300ms ease}
.aui-inline-diff-proposal del{background:var(--aui-inline-diff-remove);color:var(--aui-inline-diff-err);text-decoration:line-through;text-decoration-thickness:1.2px;text-decoration-color:var(--aui-inline-diff-strike)}.aui-inline-diff-proposal ins{background:var(--aui-inline-diff-add);color:var(--aui-inline-diff-ok);text-decoration:none}.aui-inline-diff-proposal del+ins{margin-left:3px}
.aui-inline-diff-proposal[data-active="true"] del{background:var(--aui-inline-diff-remove-active)}.aui-inline-diff-proposal[data-active="true"] ins{background:var(--aui-inline-diff-add-active)}
.aui-inline-diff-proposal[data-decision] del,.aui-inline-diff-proposal[data-decision] ins{padding:0;margin:0;background:transparent;color:inherit;text-decoration-color:transparent}.aui-inline-diff-proposal[data-decision="accepted"] del,.aui-inline-diff-proposal[data-decision="rejected"] ins{font-size:0;opacity:0}
.aui-inline-diff-popover{position:absolute;z-index:2;display:flex;align-items:center;gap:2px;padding:3px;border-radius:10px;background:var(--aui-inline-diff-pop);color:var(--aui-inline-diff-pop-fg);box-shadow:var(--aui-shadow-md,0 6px 20px -6px #00000059,0 1px 2px #0003);transform-origin:50% 100%;opacity:1;transform:none;visibility:visible;transition:opacity 180ms ease,transform 300ms var(--aui-inline-diff-e),left 320ms var(--aui-inline-diff-e),top 320ms var(--aui-inline-diff-e),visibility 0s}
.aui-inline-diff-popover[data-placement="bottom"]{transform-origin:50% 0}.aui-inline-diff-popover[data-open="false"]{opacity:0;transform:translateY(4px) scale(.96);visibility:hidden;pointer-events:none;transition:opacity 180ms ease,transform 260ms var(--aui-inline-diff-e),visibility 0s 260ms}.aui-inline-diff-popover[data-open="false"][data-placement="bottom"]{transform:translateY(-4px) scale(.96)}.aui-inline-diff-popover[data-first="true"]{transition:opacity 180ms ease,transform 300ms var(--aui-inline-diff-e)}
.aui-inline-diff .aui-inline-diff-decision{height:28px;padding:0 8px 0 7px;border-radius:7px;box-shadow:none;background:transparent;color:inherit;font-size:12.5px;letter-spacing:normal}.aui-inline-diff .aui-inline-diff-decision:hover{background:var(--aui-inline-diff-pop-hover)}.aui-inline-diff-popover button{transition:background 140ms ease}
.aui-inline-diff-divider{width:1px;height:16px;background:var(--aui-inline-diff-pop-hover)}.aui-inline-diff kbd{display:grid;place-items:center;min-width:17px;height:17px;padding:0 4px;border-radius:4px;background:var(--aui-inline-diff-pop-hover);color:var(--aui-inline-diff-key);font:500 10.5px var(--aui-inline-diff-font)}
.aui-inline-diff-split{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr);gap:18px}.aui-inline-diff-column{min-width:0}.aui-inline-diff-column+.aui-inline-diff-column{padding-left:18px;border-left:1px solid var(--aui-inline-diff-line)}.aui-inline-diff-column h3{margin:0 0 8px;color:var(--aui-inline-diff-muted);font-size:12px;font-weight:500}.aui-inline-diff[data-view="unified"] .aui-inline-diff-split,.aui-inline-diff[data-view="split"] .aui-inline-diff-unified{display:none}
.aui-inline-diff-text-scroll[data-preserve="true"]{overflow-x:auto;overscroll-behavior-x:contain}.aui-inline-diff-text-scroll[data-preserve="true"] .aui-inline-diff-prose{width:max-content;min-width:100%;white-space:pre;word-break:normal;overflow-wrap:normal}
.aui-inline-diff-tools{margin-top:12px;color:var(--aui-inline-diff-muted);font-size:12px}.aui-inline-diff-tools summary{width:fit-content;cursor:pointer}.aui-inline-diff-tools-actions{display:flex;flex-wrap:wrap;gap:6px;margin-top:8px}
.aui-inline-diff-sr{position:absolute;width:1px;height:1px;padding:0;margin:-1px;clip-path:inset(50%);overflow:hidden;white-space:nowrap;border:0}
.aui-inline-diff.aui-inline-diff-overlay{container-type:normal;width:auto}.aui-inline-diff-sheet{position:fixed;z-index:1000;inset:auto 0 0;padding:16px max(16px,env(safe-area-inset-right)) max(16px,env(safe-area-inset-bottom)) max(16px,env(safe-area-inset-left));padding-top:max(16px,env(safe-area-inset-top));max-height:75dvh;display:flex;flex-direction:column;gap:12px;background:var(--aui-inline-diff-card);border:1px solid var(--aui-inline-diff-line);border-radius:20px 20px 0 0;box-shadow:var(--aui-shadow-md,0 -8px 30px #0003)}
.aui-inline-diff-sheet-header{display:flex;align-items:center;gap:8px}.aui-inline-diff-sheet-header h2{margin:0;flex:1;font-size:15px}.aui-inline-diff .aui-inline-diff-sheet button{min-width:44px;min-height:44px;height:auto;white-space:normal}.aui-inline-diff-sheet-content{min-height:0;overflow-y:auto;overscroll-behavior:contain;color:var(--aui-inline-diff-dim);font-size:15px}.aui-inline-diff-sheet-content h3{font-size:12px;color:var(--aui-inline-diff-muted);margin:0 0 4px}.aui-inline-diff-sheet-content p{margin:0 0 12px;white-space:pre-wrap;word-break:keep-all;overflow-wrap:anywhere}.aui-inline-diff-sheet-actions{display:grid;grid-template-columns:1fr 1fr;gap:8px}
@container(max-width:600px){.aui-inline-diff[data-view="split"] .aui-inline-diff-unified{display:block}.aui-inline-diff[data-view="split"] .aui-inline-diff-split{display:none}}
@container aui-inline-diff-card (max-width:400px){.aui-inline-diff-count-unit{position:absolute;width:1px;height:1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap}.aui-inline-diff-toolbar{column-gap:5px}}
@media(max-width:400px){.aui-inline-diff-toolbar{padding-left:14px}.aui-inline-diff-label{padding-left:6px}.aui-inline-diff-body{padding:14px 14px 16px}.aui-inline-diff-toolbar .aui-inline-diff-nav{display:none}}
@media(pointer:coarse),(max-width:899px){.aui-inline-diff button{min-width:44px;min-height:44px;height:44px}.aui-inline-diff .aui-inline-diff-arrow{width:44px}.aui-inline-diff-proposal:not([data-decision]){display:inline-block;min-width:44px;min-height:44px;padding:9px 3px;vertical-align:baseline;max-width:100%}.aui-inline-diff kbd{display:none}.aui-inline-diff-tools summary{min-height:44px;display:flex;align-items:center}.aui-inline-diff .aui-inline-diff-decision{min-height:44px}}
@media(max-width:899px){.aui-inline-diff-popover{display:none}}
@keyframes aui-inline-diff-check{to{stroke-dashoffset:0}}
@media(prefers-reduced-motion:reduce){.aui-inline-diff *,.aui-inline-diff *::before,.aui-inline-diff *::after{animation-duration:1ms!important;animation-delay:0ms!important;transition:none!important;scroll-behavior:auto!important}.aui-inline-diff-toolbar-layer[data-hidden="true"]{transform:none;filter:none}.aui-inline-diff-check{stroke-dashoffset:0}}
}
`;

export const InlineDiff = forwardRef<InlineDiffHandle, InlineDiffProps>(function InlineDiff({ original = '', suggested = '', changes, onChange, onDone, label, labels, defaultView = 'unified', preserveLines = false, showTools = false, appearance, className = '' }, ref) {
  const id = useId();
  const words: InlineDiffLabels = { ...inlineDiffDefaultLabels, ...labels };
  // Falsy supplied changes fall back to comparison; any other value is cleaned with its boundaries kept.
  const segments = useMemo(() => changes ? normalizeSegments(changes) : diffWords(original, suggested), [changes, original, suggested]);
  const sourceKey = JSON.stringify(segments);
  const changeSegments = segments.filter((item): item is Change => item.type === 'change');
  const [review, setReview] = useState<Review>({});
  const [hover, setHover] = useState<number | null>(null), [keyboard, setKeyboard] = useState<number | null>(null), [external, setExternal] = useState<number | null>(null);
  const [historyCount, setHistoryCount] = useState(0), [announcement, setAnnouncement] = useState('');
  const [view, setView] = useState(defaultView), [mobile, setMobile] = useState(false), [sheetOpen, setSheetOpen] = useState(false);
  const [lastActive, setLastActive] = useState<number | null>(null), [position, setPosition] = useState({ left: 8, top: 0 }), [placement, setPlacement] = useState<'top' | 'bottom'>('top');
  const root = useRef<HTMLElement>(null), card = useRef<HTMLDivElement>(null), popover = useRef<HTMLDivElement>(null), sheet = useRef<HTMLDivElement>(null), undoButton = useRef<HTMLButtonElement>(null), closeButton = useRef<HTMLButtonElement>(null);
  const decisions = useRef<Review>({}), history = useRef<{ previous: Review; indices: number[] }[]>([]), last = useRef<number | null>(null), leaveTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined), restoreFocus = useRef(false), wasOpen = useRef(false), touchPress = useRef(false), focusSheet = useRef(false);
  const pending = changeSegments.map((_, index) => index).filter((index) => !review[index]);
  const valid = (index: number | null): index is number => index !== null && Number.isInteger(index) && index >= 0 && index < changeSegments.length && !decisions.current[index];
  const active = [hover, keyboard, external].find(valid) ?? null;
  const complete = changeSegments.length > 0 && pending.length === 0;
  const popIndex = active ?? lastActive;
  const visibleProposal = (index: number) => Array.from(root.current?.querySelectorAll<HTMLElement>(`[data-change-index="${index}"]`) ?? []).find((element) => element.getClientRects().length > 0);
  const textFor = (next: Review) => { let at = 0; return segments.map((item) => item.type === 'equal' ? item.text : next[at++] === 'accepted' ? item.insert : item.remove).join(''); };
  const cancelLeave = () => clearTimeout(leaveTimer.current);
  const leave = () => { cancelLeave(); leaveTimer.current = setTimeout(() => setHover(null), 140); };
  function scrollToChange(index: number) {
    requestAnimationFrame(() => {
      const target = visibleProposal(index); if (!target) return;
      const box = target.getBoundingClientRect(), bottom = mobile && sheet.current ? (sheet.current?.getBoundingClientRect().top ?? window.innerHeight) : window.innerHeight;
      const offset = box.top < 64 ? box.top - 64 : box.bottom > bottom - 24 ? box.bottom - bottom + 24 : 0;
      if (offset) window.scrollBy({ top: offset, behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'instant' : 'smooth' });
    });
  }
  function focusChange(index: number | null) {
    requestAnimationFrame(() => { if (index === null) undoButton.current?.focus({ preventScroll: true }); else { visibleProposal(index)?.focus({ preventScroll: true }); scrollToChange(index); } });
  }
  function clearState() {
    decisions.current = {}; history.current = []; last.current = null; cancelLeave(); setReview({}); setHistoryCount(0); setHover(null); setExternal(null); setAnnouncement(''); setSheetOpen(false); setLastActive(null);
    const element = document.activeElement as HTMLElement | null;
    const index = element && root.current?.contains(element) ? Number(element.dataset.changeIndex) : NaN;
    setKeyboard(Number.isInteger(index) && index >= 0 && index < changeSegments.length ? index : null);
  }
  useLayoutEffect(() => { clearState(); }, [sourceKey]); // Semantic equality intentionally retains decisions.
  useEffect(() => {
    const query = window.matchMedia('(max-width:899px)'); const sync = () => setMobile(query.matches); sync(); query.addEventListener('change', sync);
    return () => { query.removeEventListener('change', sync); cancelLeave(); };
  }, []);
  useEffect(() => {
    const outside = (event: PointerEvent) => { if (!root.current?.contains(event.target as Node) && !sheet.current?.contains(event.target as Node)) { if (mobile && sheetOpen) closeSheet(); else { setExternal(null); setHover(null); } } };
    const keyboardInput = () => { touchPress.current = false; };
    document.addEventListener('pointerdown', outside); document.addEventListener('keydown', keyboardInput, true);
    return () => { document.removeEventListener('pointerdown', outside); document.removeEventListener('keydown', keyboardInput, true); };
  }, [mobile, sheetOpen, active]);
  useLayoutEffect(() => {
    if (active === null) { wasOpen.current = false; return; }
    last.current = active; setLastActive(active);
    const place = () => {
      const target = visibleProposal(active), panel = card.current, popup = popover.current;
      if (!target || !panel || !popup) return;
      const first = target.getClientRects()[0] ?? target.getBoundingClientRect(), bounds = panel.getBoundingClientRect();
      const scale = bounds.width ? panel.offsetWidth / bounds.width : 1;
      const center = (first.left - bounds.left + first.width / 2) * scale;
      const above = (first.top - bounds.top) * scale - popup.offsetHeight - 6, flip = above < 4;
      setPlacement(flip ? 'bottom' : 'top');
      setPosition({ left: Math.min(Math.max(center - popup.offsetWidth / 2, 8), Math.max(8, panel.offsetWidth - popup.offsetWidth - 8)), top: flip ? (first.bottom - bounds.top) * scale + 6 : above });
    };
    place(); const observer = new ResizeObserver(place); if (card.current) observer.observe(card.current);
    window.addEventListener('resize', place); window.addEventListener('scroll', place, true);
    return () => { observer.disconnect(); window.removeEventListener('resize', place); window.removeEventListener('scroll', place, true); };
  }, [active, sourceKey, review, view, mobile]);
  useEffect(() => { if (mobile && sheetOpen && focusSheet.current) { closeButton.current?.focus({ preventScroll: true }); focusSheet.current = false; } }, [mobile, sheetOpen]);
  function decide(indices: number[], decision: Decision, moveFocus = false) {
    const current = decisions.current, accepted = [...new Set(indices)].filter(valid); if (!accepted.length) return;
    history.current.push({ previous: { ...current }, indices: accepted });
    const next = { ...current }; accepted.forEach((index) => { next[index] = decision; }); decisions.current = next; setReview(next); setHistoryCount(history.current.length);
    last.current = Math.max(...accepted); setExternal(null); setHover(null); setKeyboard(null); cancelLeave();
    const nextPending = changeSegments.map((_, index) => index).filter((index) => !next[index]);
    const text = textFor(next); onChange?.(text);
    const ending = nextPending.length ? words.remainingNotice(count(nextPending.length)) : words.allDoneNotice;
    setAnnouncement(accepted.length === 1 ? words.singleDecidedNotice(description(changeSegments[accepted[0]], words), decision === 'accepted', ending) : words.multiDecidedNotice(count(accepted.length), decision === 'accepted', ending));
    const nextIndex = nextPending.find((index) => index > last.current!) ?? nextPending[0] ?? null;
    if (!nextPending.length) { setSheetOpen(false); onDone?.(text); }
    else if (mobile && sheetOpen) { setExternal(nextIndex); scrollToChange(nextIndex!); }
    if (moveFocus) focusChange(nextIndex);
  }
  function undo(moveFocus = false) {
    const entry = history.current.pop(); if (!entry) return;
    decisions.current = entry.previous; setReview(entry.previous); setHistoryCount(history.current.length);
    const remaining = changeSegments.length - Object.keys(entry.previous).length; onChange?.(textFor(entry.previous)); setAnnouncement(words.undoneNotice(count(entry.indices.length), count(remaining)));
    if (moveFocus) focusChange(Math.min(...entry.indices));
  }
  function highlight(index: number | null) { cancelLeave(); setExternal(valid(index) ? index : null); if (mobile) setSheetOpen(valid(index)); }
  useImperativeHandle(ref, () => ({ highlight, accept: (index) => decide([index], 'accepted'), reject: (index) => decide([index], 'rejected'), acceptAll: () => decide(changeSegments.map((_, index) => index), 'accepted'), rejectAll: () => decide(changeSegments.map((_, index) => index), 'rejected'), undo: () => undo(), reset: clearState }));
  function navigate(delta: number) {
    if (!pending.length) return;
    const from = active ?? last.current;
    const target = delta > 0 ? pending.find((index) => from === null || index > from) ?? pending[0] : [...pending].reverse().find((index) => from === null || index < from) ?? pending[pending.length - 1];
    cancelLeave(); setHover(null); setExternal(target); last.current = target;
    if (mobile) { setKeyboard(null); if (!sheetOpen) focusSheet.current = true; setSheetOpen(true); }
    else if (keyboard !== null && root.current?.contains(document.activeElement)) focusChange(target);
    scrollToChange(target);
  }
  function closeSheet() {
    const target = active; setSheetOpen(false); setExternal(null); setHover(null); setKeyboard(null); restoreFocus.current = true;
    if (target !== null) requestAnimationFrame(() => { visibleProposal(target)?.focus({ preventScroll: true }); restoreFocus.current = false; });
  }
  function proposalKey(event: KeyboardEvent<HTMLElement>, index: number) {
    if (event.altKey || event.ctrlKey || event.metaKey || event.nativeEvent.isComposing) return;
    if (event.key.toLowerCase() === 'y' || event.key === 'Enter') { event.preventDefault(); decide([index], 'accepted', true); }
    else if (event.key.toLowerCase() === 'n' || event.key === 'Backspace' || event.key === 'Delete') { event.preventDefault(); decide([index], 'rejected', true); }
    else if (event.key === 'Escape') { event.preventDefault(); event.currentTarget.blur(); setKeyboard(null); }
  }
  function prose(side: 'unified' | 'original' | 'suggested') {
    let at = 0;
    return <p className="aui-inline-diff-prose" dir="auto">{segments.map((item, segmentIndex) => {
      if (item.type === 'equal') return <span key={segmentIndex}>{item.text}</span>;
      const index = at++, decision = review[index], accessible = !decision;
      const before = side !== 'suggested' ? item.remove : '', after = side !== 'original' ? item.insert : '';
      return <span key={segmentIndex} className="aui-inline-diff-proposal" data-change-index={index} data-decision={decision} data-active={active === index} role={accessible ? 'button' : undefined} tabIndex={accessible ? 0 : undefined} aria-label={accessible ? words.proposalAria(count(index + 1), count(changeSegments.length), description(item, words)) : undefined} aria-pressed={accessible ? active === index : undefined}
        onPointerEnter={(event) => { if (!decision && event.pointerType === 'mouse') { cancelLeave(); setHover(index); } }} onPointerLeave={(event) => { if (event.pointerType === 'mouse') leave(); }}
        onPointerDown={() => { touchPress.current = true; }} onPointerCancel={() => { touchPress.current = false; }}
        onClick={(event) => { if (decision) return; if (mobile || event.detail === 0 && window.matchMedia('(pointer:coarse),(max-width:899px)').matches) { const close = sheetOpen && active === index; setHover(null); setKeyboard(null); setExternal(close ? null : index); focusSheet.current = !close; setSheetOpen(!close); } else setExternal(index); touchPress.current = false; }}
        onFocus={() => { if (!decision) { setKeyboard(index); if (mobile && !restoreFocus.current && !touchPress.current) setSheetOpen(true); } }} onBlur={() => setKeyboard((current) => current === index ? null : current)} onKeyDown={(event) => proposalKey(event, index)}>
        {before && <del aria-hidden={decision === 'accepted' || undefined}>{before}</del>}{after && <ins aria-hidden={decision === 'rejected' || undefined}>{after}</ins>}
        {decision && side === 'suggested' && decision === 'rejected' ? <span>{item.remove}</span> : null}{decision && side === 'original' && decision === 'accepted' ? <span>{item.insert}</span> : null}
      </span>;
    })}</p>;
  }
  const stopMouseFocus = (event: React.PointerEvent) => { if (event.pointerType === 'mouse') event.preventDefault(); };
  const popupDecision = (decision: Decision) => { if (active !== null) decide([active], decision, mobile || keyboard === active); };
  const content = popIndex === null ? null : changeSegments[popIndex];
  const rootStyle = appearance ? Object.fromEntries(Object.entries(appearance).filter(([, value]) => value).map(([key, value]) => [`--aui-inline-diff-${appearanceVars[key as keyof InlineDiffAppearance]}`, value])) as CSSProperties : undefined;
  return <section ref={root} className={`aui-inline-diff ${className}`} style={rootStyle} data-view={view} aria-label={label || words.regionLabel} onKeyDown={(event) => { if (event.key === 'Escape') { setExternal(null); if (mobile && sheetOpen) closeSheet(); } }}>
    <style>{styles}</style>
    <div className="aui-inline-diff-shell">
      {label && <p className="aui-inline-diff-label" title={label}><Sparkles size={13} aria-hidden="true" /><span dir="auto">{label}</span></p>}
      <div ref={card} className="aui-inline-diff-card">
        <div className="aui-inline-diff-toolbar">
          <div className="aui-inline-diff-toolbar-layer" data-hidden={complete} aria-hidden={complete} inert={complete}>
            <span className="aui-inline-diff-count"><span className="aui-inline-diff-count-number">{count(pending.length)}</span><span className="aui-inline-diff-count-unit">{words.proposalUnit}</span></span>
            <div className="aui-inline-diff-actions"><div className="aui-inline-diff-nav"><button className="aui-inline-diff-arrow" type="button" aria-label={words.prevProposal} disabled={!pending.length} onPointerDown={stopMouseFocus} onClick={() => navigate(-1)}><ChevronLeft size={14} strokeWidth={1.6} aria-hidden="true" /></button><button className="aui-inline-diff-arrow" type="button" aria-label={words.nextProposal} disabled={!pending.length} onPointerDown={stopMouseFocus} onClick={() => navigate(1)}><ChevronRight size={14} strokeWidth={1.6} aria-hidden="true" /></button></div><button type="button" disabled={!pending.length} onClick={() => decide(pending, 'rejected', true)}>{words.rejectAll}</button><button className="aui-inline-diff-primary" type="button" disabled={!pending.length} onClick={() => decide(pending, 'accepted', true)}>{words.acceptAll}</button></div>
          </div>
          <div className="aui-inline-diff-toolbar-layer" data-hidden={!complete} aria-hidden={!complete} inert={!complete}><span className="aui-inline-diff-complete"><Check className="aui-inline-diff-check" size={16} aria-hidden="true" />{words.reviewDone}</span><button ref={undoButton} type="button" disabled={!historyCount} onClick={() => undo(true)}><Undo2 size={14} strokeWidth={1.6} aria-hidden="true" />{words.undo}</button></div>
        </div>
        <div className="aui-inline-diff-body"><div className="aui-inline-diff-unified"><div className="aui-inline-diff-text-scroll" data-preserve={preserveLines}>{prose('unified')}</div></div><div className="aui-inline-diff-split"><div className="aui-inline-diff-column"><h3>{words.originalColumn}</h3><div className="aui-inline-diff-text-scroll" data-preserve={preserveLines}>{prose('original')}</div></div><div className="aui-inline-diff-column"><h3>{words.suggestedColumn}</h3><div className="aui-inline-diff-text-scroll" data-preserve={preserveLines}>{prose('suggested')}</div></div></div>{!segments.length && <p className="aui-inline-diff-prose">{words.emptyBody}</p>}</div>
        <div ref={popover} className="aui-inline-diff-popover" role="group" aria-label={active === null ? words.decideGroup : words.decideProposalGroup(count(active + 1))} aria-hidden={active === null || mobile} inert={active === null || mobile} data-open={active !== null} data-placement={placement} data-first={!wasOpen.current} style={position} onPointerEnter={() => { cancelLeave(); if (popIndex !== null) setHover(popIndex); }} onPointerLeave={leave} onTransitionEnd={() => { if (active !== null) wasOpen.current = true; }}><button className="aui-inline-diff-decision" type="button" tabIndex={-1} aria-label={words.acceptCurrent} onPointerDown={stopMouseFocus} onClick={() => popupDecision('accepted')}><Check size={14} strokeWidth={1.8} aria-hidden="true" />{words.accept}<kbd aria-hidden="true">Y</kbd></button><span className="aui-inline-diff-divider" aria-hidden="true" /><button className="aui-inline-diff-decision" type="button" tabIndex={-1} aria-label={words.rejectCurrent} onPointerDown={stopMouseFocus} onClick={() => popupDecision('rejected')}><X size={14} strokeWidth={1.8} aria-hidden="true" />{words.reject}<kbd aria-hidden="true">N</kbd></button></div>
      </div>
    </div>
    <span className="aui-inline-diff-sr" role="status" aria-live="polite" aria-atomic="true">{announcement}</span>
    {showTools && <details className="aui-inline-diff-tools"><summary>{words.toolsSummary}</summary><div className="aui-inline-diff-tools-actions"><button type="button" aria-pressed={view === 'split'} onClick={() => setView(view === 'split' ? 'unified' : 'split')}><Columns2 size={14} aria-hidden="true" />{view === 'split' ? words.unifiedView : words.splitView}</button><button type="button" disabled={!historyCount} onClick={() => undo()}><Undo2 size={14} aria-hidden="true" />{words.undoLast}</button><button type="button" onClick={clearState}><RotateCcw size={14} aria-hidden="true" />{words.restartReview}</button></div></details>}
    {mobile && sheetOpen && active !== null && content && createPortal(<div className="aui-inline-diff aui-inline-diff-overlay"><div ref={sheet} className="aui-inline-diff-sheet" role="dialog" aria-label={words.sheetAria(count(active + 1))} aria-describedby={`${id}-sheet-description`} onKeyDown={(event) => { if (event.key === 'Escape') { event.preventDefault(); closeSheet(); } }}><div className="aui-inline-diff-sheet-header"><h2>{words.sheetTitle(count(active + 1), count(changeSegments.length))}</h2><button type="button" aria-label={words.prevProposal} onClick={() => navigate(-1)}><ChevronLeft size={18} aria-hidden="true" /></button><button type="button" aria-label={words.nextProposal} onClick={() => navigate(1)}><ChevronRight size={18} aria-hidden="true" /></button><button ref={closeButton} type="button" aria-label={words.closeSheet} onClick={closeSheet}><X size={18} aria-hidden="true" /></button></div><div className="aui-inline-diff-sheet-content" id={`${id}-sheet-description`}><h3>{words.beforeHeading}</h3><p dir="auto">{content.remove || words.beforeEmpty}</p><h3>{words.afterHeading}</h3><p dir="auto">{content.insert || words.afterEmpty}</p></div><div className="aui-inline-diff-sheet-actions"><button type="button" onClick={() => popupDecision('rejected')}><X size={16} aria-hidden="true" />{words.reject}</button><button className="aui-inline-diff-primary" type="button" onClick={() => decide([active], 'accepted', true)}><Check size={16} aria-hidden="true" />{words.accept}</button></div></div></div>, document.body)}
  </section>;
});
