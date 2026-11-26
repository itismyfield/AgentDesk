// Kit provenance: itismyfield/agentdesk-ui-kit@3024685b30
import { isValidElement, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from 'react';
import { Clock, ShieldCheck } from 'lucide-react';

// Clean-room implementation from the written approval-card v2 specification, its delta notes and PNG references.
// No affectUI Pro source, package, registry, site assets, or original copy is used.

export type ApprovalStatus = 'pending' | 'approved' | 'denied';
export type ApprovalRisk = 'low' | 'medium' | 'high';
export type DenyReason = 'user' | 'timeout';

export interface ApprovalDetail {
  label: ReactNode;
  value: ReactNode;
}

export interface ApprovalCardProps {
  /** Shared name of the review card and the decision record. */
  title: string;
  description?: ReactNode;
  /** Omit to hide the risk badge. */
  risk?: ApprovalRisk;
  /** Decorative header icon; SVGs are drawn 16px, unfilled, 1.7px ink stroke. Defaults to a shield with a check. */
  icon?: ReactNode;
  details?: ApprovalDetail[];
  /** Dedicated preview above the details. `undefined`/`null` fall back to `children`; '' or `false` render nothing. */
  preview?: ReactNode;
  /** Fallback preview content, used only when `preview` is not given. */
  children?: ReactNode;
  /** Controlled decision. No output changes it; the host has to pass the new value. */
  status?: ApprovalStatus;
  /** Initial decision of an uncontrolled card. Later changes are ignored. */
  defaultStatus?: ApprovalStatus;
  /**
   * Shows the approve button and the ⌘/Ctrl+Enter shortcut. Without it (and without `approveDisabledReason`) there is
   * no approve button. Receives the "always allow" choice. Return a promise to keep the card busy; a rejection keeps
   * the request pending.
   */
  onApprove?: (options: { always: boolean }) => void | PromiseLike<unknown>;
  /** Shows the deny button and the Esc shortcut. Without it there is no deny button. */
  onDeny?: (options: { reason: DenyReason }) => void;
  /** Shows the edit action. Asking the host to edit never changes the decision. */
  onEdit?: () => void;
  /**
   * Short reason the approval cannot be given right now. The approve button stays visible with
   * `aria-disabled="true"`, the reason sits next to it, and neither the button nor ⌘/Ctrl+Enter calls `onApprove`.
   * Shows the disabled button even when `onApprove` is omitted.
   */
  approveDisabledReason?: string;
  /**
   * Seconds until the request is denied with reason "timeout". Only positive values start the clock, and only when
   * `onDeny` or `onExpire` receives the timeout; without either there is no clock, countdown or spoken hint.
   */
  expiresIn?: number;
  /**
   * Fires when the clock runs out, before `onDeny({ reason: 'timeout' })`. An uncontrolled card only records the
   * timeout itself when `onExpire` or `onDeny` is given; otherwise it stays pending until the host changes it.
   */
  onExpire?: () => void;
  /**
   * Shows the "always allow" choice next to a usable approve button. Omitted: shown only when `onApprove` declares
   * the options argument, e.g. `({ always }) => …`. `true` or `false` decide it explicitly.
   * Wrapped handlers, default parameters and rest parameters have `length` 0, so pass `allowAlways` explicitly with them.
   */
  allowAlways?: boolean;
  /** Same as `labels.always`; this prop wins when both are given. */
  alwaysLabel?: string;
  /** Same as `labels.approve`; this prop wins when both are given. */
  approveLabel?: string;
  /** Same as `labels.deny`; this prop wins when both are given. */
  denyLabel?: string;
  /** Same as `labels.edit`; this prop wins when both are given. */
  editLabel?: string;
  /** Every visible and spoken text. Missing keys keep the Korean defaults. */
  labels?: Partial<ApprovalCardLabels>;
  /** Shows key caps in the buttons. The shortcuts work either way. */
  shortcuts?: boolean;
  /** Small context line (step, tool) kept on the tray above the card, before and after the decision. */
  context?: ReactNode;
  /** Where the action row sits: on the tray under the card (default) or inside the card. */
  actions?: 'tray' | 'card';
  className?: string;
  style?: CSSProperties;
}

export type ApprovalResult = 'approved' | 'denied' | 'expired';

export interface ApprovalCardLabels {
  approve: string;
  deny: string;
  edit: string;
  /** "Always allow" checkbox text. */
  always: string;
  /** Spoken after the approve label while the host is still working. */
  busy: string;
  riskLow: string;
  riskMedium: string;
  riskHigh: string;
  /** Word after the risk level; visually hidden in cards 340px and narrower. Empty hides it. */
  riskSuffix: string;
  /** Opens text that a one-line rule had to cut. */
  expand: string;
  collapse: string;
  /** Visible countdown from the m:ss or h:mm:ss clock. */
  timer: (clock: string) => string;
  /** Spoken countdown sentence used as the card description. */
  timerHint: (remaining: string) => string;
  /** Spoken remaining time. */
  remaining: (seconds: number) => string;
  /** Elapsed label on the decision record. */
  elapsed: (seconds: number) => string;
  resultApproved: string;
  resultDenied: string;
  resultExpired: string;
  /** Added to an approval given with "always allow". */
  alwaysResult: string;
  /** Polite announcement after a decision. */
  announce: (result: ApprovalResult, always: boolean, title: string) => string;
}

const approvalCardStyles = `
@layer kit {
.aui-approval-card {
  --aui-approval-card-tray: var(--aui-bg-2, #f0f0f0);
  --aui-approval-card-card: var(--aui-bg-1, #ffffff);
  --aui-approval-card-ink: var(--aui-fg, #171717);
  --aui-approval-card-body: var(--aui-fg-dim, #6b6b6b);
  --aui-approval-card-faint: var(--aui-fg-muted, #8a8a8a);
  --aui-approval-card-line: var(--aui-line, #ececec);
  --aui-approval-card-soft: #fafafa;
  --aui-approval-card-tile: #f4f4f4;
  --aui-approval-card-hover: #e4e4e5;
  --aui-approval-card-primary: var(--aui-fg, #171717);
  --aui-approval-card-primary-hover: #2e2e2e;
  --aui-approval-card-on-primary: var(--aui-bg-1, #ffffff);
  --aui-approval-card-key-primary-bg: rgba(255, 255, 255, .14);
  --aui-approval-card-key-primary-fg: rgba(255, 255, 255, .72);
  --aui-approval-card-key-bg: rgba(0, 0, 0, .055);
  --aui-approval-card-check-line: #d4d4d4;
  --aui-approval-card-check-hover: #b5b5b5;
  --aui-approval-card-focus: var(--aui-focus, rgba(0, 96, 226, .7));
  --aui-approval-card-warn: var(--aui-approval-card-warn-color, #b25e00);
  --aui-approval-card-err: var(--aui-approval-card-error-color, #c4320a);
  --aui-approval-card-low-bg: #f2f2f2;
  --aui-approval-card-low-fg: #6b6b6b;
  --aui-approval-card-medium-bg: #fbf5e2;
  --aui-approval-card-medium-fg: #8f6c00;
  --aui-approval-card-high-bg: #fdedeb;
  --aui-approval-card-high-fg: #c4320a;
  --aui-approval-card-ok-bg: #e9f6ee;
  --aui-approval-card-ok-fg: var(--aui-approval-card-success-color, #1e8a4f);
  --aui-approval-card-r: var(--aui-r-lg, 16px);
  --aui-approval-card-r-action: var(--aui-r-sm, 10px);
  --aui-approval-card-r-small: var(--aui-r-xs, 7px);
  --aui-approval-card-shadow: var(--aui-approval-card-card-shadow, 0 1px 2px rgba(0, 0, 0, .04));
  --aui-approval-card-neutral-bg: #f2f2f2;
  --aui-approval-card-neutral-fg: #6b6b6b;
  --aui-approval-card-ease: var(--aui-approval-card-motion, cubic-bezier(.22, 1, .36, 1));
  display: block; width: 100%; min-width: 0; container-type: inline-size;
  color: var(--aui-approval-card-body); font-family: var(--aui-font-sans, Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif);
  font-size: 13px; line-height: 1.45; letter-spacing: -.006em; text-align: start;
  word-break: keep-all; overflow-wrap: anywhere; -webkit-tap-highlight-color: transparent;
}
:where(.dark, [data-theme="dark"]) .aui-approval-card {
  --aui-approval-card-tray: var(--aui-bg-2, #111113);
  --aui-approval-card-card: var(--aui-bg-1, #19191b);
  --aui-approval-card-ink: var(--aui-fg, #ededed);
  --aui-approval-card-body: var(--aui-fg-dim, #a1a1a1);
  --aui-approval-card-faint: var(--aui-fg-muted, #8b8b92);
  --aui-approval-card-line: var(--aui-line, #2a2a2e);
  --aui-approval-card-soft: #1f1f22;
  --aui-approval-card-tile: #1f1f22;
  --aui-approval-card-hover: #222225;
  --aui-approval-card-primary: var(--aui-fg, #ededed);
  --aui-approval-card-primary-hover: #ffffff;
  --aui-approval-card-on-primary: var(--aui-bg-1, #19191b);
  --aui-approval-card-key-primary-bg: rgba(0, 0, 0, .08);
  --aui-approval-card-key-primary-fg: rgba(25, 25, 27, .6);
  --aui-approval-card-key-bg: rgba(255, 255, 255, .07);
  --aui-approval-card-check-line: rgba(255, 255, 255, .2);
  --aui-approval-card-check-hover: rgba(255, 255, 255, .32);
  --aui-approval-card-focus: var(--aui-focus, rgba(77, 141, 255, .8));
  --aui-approval-card-warn: var(--aui-approval-card-warn-color, #e6c24d);
  --aui-approval-card-err: var(--aui-approval-card-error-color, #ff8a7a);
  --aui-approval-card-low-bg: rgba(255, 255, 255, .07);
  --aui-approval-card-low-fg: #a1a1a1;
  --aui-approval-card-medium-bg: rgba(230, 194, 77, .12);
  --aui-approval-card-medium-fg: #e6c24d;
  --aui-approval-card-high-bg: rgba(255, 107, 95, .12);
  --aui-approval-card-high-fg: #ff8a7a;
  --aui-approval-card-ok-bg: rgba(125, 207, 159, .13);
  --aui-approval-card-ok-fg: var(--aui-approval-card-success-color, #5fc88a);
  --aui-approval-card-neutral-bg: rgba(255, 255, 255, .07);
  --aui-approval-card-neutral-fg: #a1a1a1;
}
.aui-approval-card *, .aui-approval-card *::before, .aui-approval-card *::after { box-sizing: border-box; }
.aui-approval-card-tray { position: relative; overflow: hidden; padding: 4px; border-radius: calc(var(--aui-approval-card-r) + 4px); background: var(--aui-approval-card-tray); }
.aui-approval-card-context { padding: 6px 8px 4px; color: var(--aui-approval-card-faint); font-size: 12.5px; line-height: 16px; }
.aui-approval-card [data-aui-clamp] { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-approval-card-request[data-phase="exit"] { position: absolute; top: 4px; left: 4px; right: 4px; pointer-events: none; animation: aui-approval-card-exit 280ms var(--aui-approval-card-ease) forwards; }
.aui-approval-card[data-context] .aui-approval-card-request[data-phase="exit"] { top: 30px; }
.aui-approval-card-request[data-phase="enter"] { animation: aui-approval-card-enter 420ms var(--aui-approval-card-ease) 80ms both; }
.aui-approval-card-box { padding: 16px; border-radius: var(--aui-approval-card-r); background: var(--aui-approval-card-card); box-shadow: 0 0 0 1px var(--aui-approval-card-line), var(--aui-approval-card-shadow); }
.aui-approval-card-head { display: flex; align-items: flex-start; gap: 12px; }
.aui-approval-card-icon { display: grid; place-items: center; flex: 0 0 32px; width: 32px; height: 32px; border-radius: 9px; background: var(--aui-approval-card-tile); box-shadow: inset 0 0 0 1px var(--aui-approval-card-line); color: var(--aui-approval-card-ink); }
.aui-approval-card-icon svg { width: 16px; height: 16px; fill: none; stroke: var(--aui-approval-card-ink); stroke-width: 1.7px; stroke-linecap: round; stroke-linejoin: round; }
.aui-approval-card-heading { flex: 1 1 auto; min-width: 0; padding-top: 1px; }
.aui-approval-card-heading h3 { margin: 0; color: var(--aui-approval-card-ink); font-size: 14px; font-weight: 550; line-height: 20px; letter-spacing: -.011em; }
.aui-approval-card-heading p { margin: 1px 0 0; color: var(--aui-approval-card-body); font-size: 13px; line-height: 18px; }
.aui-approval-card-more { display: inline-flex; align-items: center; margin-top: 2px; padding: 0; border: 0; border-radius: 4px; background: none; color: var(--aui-approval-card-faint); font: inherit; font-size: 12.5px; text-decoration: underline; text-underline-offset: 2px; cursor: pointer; }
.aui-approval-card-risk { display: inline-flex; align-items: center; gap: 6px; flex: none; height: 22px; margin-top: 1px; padding: 0 9px 0 8px; border-radius: 999px; background: var(--aui-approval-card-low-bg); color: var(--aui-approval-card-low-fg); font-size: 12px; font-weight: 500; line-height: 1; white-space: nowrap; }
.aui-approval-card-risk[data-risk="medium"] { background: var(--aui-approval-card-medium-bg); color: var(--aui-approval-card-medium-fg); }
.aui-approval-card-risk[data-risk="high"] { background: var(--aui-approval-card-high-bg); color: var(--aui-approval-card-high-fg); }
.aui-approval-card-risk-dot { width: 6px; height: 6px; border-radius: 50%; background: currentColor; opacity: .8; }
.aui-approval-card-panel { margin-top: 14px; padding: 12px; max-height: 360px; overflow-y: auto; overscroll-behavior: contain; scrollbar-width: thin; border-radius: calc(var(--aui-approval-card-r) - 6px); background: var(--aui-approval-card-soft); box-shadow: inset 0 0 0 1px var(--aui-approval-card-line); color: var(--aui-approval-card-body); }
.aui-approval-card-panel:focus-visible { outline: 2px solid var(--aui-approval-card-focus); outline-offset: 2px; }
.aui-approval-card-preview { min-width: 0; }
.aui-approval-card-preview + .aui-approval-card-details { margin-top: 12px; padding-top: 12px; border-top: 1px solid var(--aui-approval-card-line); }
.aui-approval-card-details { display: grid; grid-template-columns: fit-content(min(40%, 180px)) minmax(0, 1fr); gap: 6px 16px; margin: 0; }
.aui-approval-card-detail { display: contents; }
.aui-approval-card-details dt { min-width: 0; overflow: hidden; color: var(--aui-approval-card-faint); overflow-wrap: normal; word-break: keep-all; text-overflow: ellipsis; }
.aui-approval-card-details dd { min-width: 0; margin: 0; overflow-x: auto; color: var(--aui-approval-card-ink); white-space: pre-line; overflow-wrap: anywhere; scrollbar-width: thin; }
.aui-approval-card-always { position: relative; display: inline-flex; align-items: flex-start; gap: 8px; max-width: 100%; margin-top: 14px; color: var(--aui-approval-card-body); cursor: pointer; vertical-align: top; transition: opacity 160ms ease; }
.aui-approval-card-box > .aui-approval-card-always { display: flex; width: fit-content; }
.aui-approval-card-always-input { position: absolute; inset: 0; width: 100%; height: 100%; margin: 0; opacity: 0; cursor: inherit; }
.aui-approval-card-always-input:disabled { cursor: default; }
.aui-approval-card-always-box { display: grid; place-items: center; flex: 0 0 16px; width: 16px; height: 16px; margin-top: 1.5px; border-radius: calc(var(--aui-approval-card-r-small) - 2px); background: var(--aui-approval-card-card); box-shadow: inset 0 0 0 1px var(--aui-approval-card-check-line); color: var(--aui-approval-card-on-primary); pointer-events: none; transition: background-color 160ms ease, box-shadow 160ms ease; }
.aui-approval-card-always-box svg { width: 12px; height: 12px; fill: none; stroke: currentColor; stroke-width: 1.8px; stroke-linecap: round; stroke-linejoin: round; opacity: 0; transform: scale(.6); transition: opacity 160ms ease, transform 260ms var(--aui-approval-card-ease); }
.aui-approval-card-always-input:checked + .aui-approval-card-always-box { background: var(--aui-approval-card-primary); box-shadow: inset 0 0 0 1px var(--aui-approval-card-primary); }
.aui-approval-card-always-input:checked + .aui-approval-card-always-box svg { opacity: 1; transform: none; }
.aui-approval-card-always-input:focus-visible + .aui-approval-card-always-box { outline: 2px solid var(--aui-approval-card-focus); outline-offset: 2px; }
.aui-approval-card-always-text { min-width: 0; pointer-events: none; overflow-wrap: anywhere; }
.aui-approval-card-actions { display: flex; flex-wrap: wrap; align-items: center; gap: 6px 12px; }
.aui-approval-card-actions[data-place="tray"] { min-height: 44px; padding: 6px 2px 2px 10px; }
.aui-approval-card-actions[data-place="card"] { margin-top: 14px; }
.aui-approval-card-timer { display: inline-flex; align-items: center; gap: 6px; color: var(--aui-approval-card-faint); font-size: 12.5px; white-space: nowrap; transition: color 300ms ease; }
.aui-approval-card-timer[data-warn="true"] { color: var(--aui-approval-card-warn); }
.aui-approval-card-timer svg { flex: none; width: 14px; height: 14px; stroke-width: 1.5px; }
.aui-approval-card-clock { font-variant-numeric: tabular-nums; }
.aui-approval-card-buttons { display: flex; flex-wrap: wrap; justify-content: flex-end; align-items: center; gap: 4px; min-width: 0; max-width: 100%; margin-left: auto; }
.aui-approval-card-button { display: inline-flex; align-items: center; justify-content: center; gap: 8px; flex: none; max-width: 100%; min-height: 34px; padding: 0 12px; border: 0; border-radius: var(--aui-approval-card-r-action); background: transparent; color: var(--aui-approval-card-ink); font: inherit; font-size: 13px; font-weight: 500; line-height: 1; letter-spacing: inherit; white-space: nowrap; cursor: pointer; touch-action: manipulation; transition: background-color 160ms ease, box-shadow 160ms ease, opacity 160ms ease, color 160ms ease, transform 160ms var(--aui-approval-card-ease); }
.aui-approval-card-button:has(kbd) { padding-right: 8px; }
.aui-approval-card-button[data-variant="edit"] { color: var(--aui-approval-card-body); }
.aui-approval-card-button[data-variant="approve"] { background: var(--aui-approval-card-primary); color: var(--aui-approval-card-on-primary); box-shadow: 0 1px 2px rgba(0, 0, 0, .12); }
.aui-approval-card-button:active:not([aria-disabled="true"]) { transform: scale(.97); }
.aui-approval-card-label { min-width: 0; white-space: normal; overflow-wrap: anywhere; text-align: center; line-height: 1.25; padding-block: 4px; }
.aui-approval-card-button:focus-visible, .aui-approval-card-more:focus-visible { outline: 2px solid var(--aui-approval-card-focus); outline-offset: 2px; }
@media (hover: hover) {
  .aui-approval-card-button[data-variant="deny"]:hover:not([aria-disabled="true"]) { background: var(--aui-approval-card-hover); }
  .aui-approval-card-button[data-variant="edit"]:hover:not([aria-disabled="true"]) { background: var(--aui-approval-card-hover); color: var(--aui-approval-card-ink); }
  .aui-approval-card-button[data-variant="approve"]:hover:not([aria-disabled="true"]) { background: var(--aui-approval-card-primary-hover); }
  .aui-approval-card-always:hover .aui-approval-card-always-input:not(:checked):not(:disabled) + .aui-approval-card-always-box { box-shadow: inset 0 0 0 1px var(--aui-approval-card-check-hover); }
}
.aui-approval-card-button kbd { display: inline-grid; place-items: center; flex: none; min-width: 20px; height: 20px; padding: 0 5px; border-radius: calc(var(--aui-approval-card-r-small) - 1px); background: var(--aui-approval-card-key-bg); color: var(--aui-approval-card-faint); font: inherit; font-size: 11px; font-weight: 500; line-height: 1; }
.aui-approval-card-button[data-variant="approve"] kbd { background: var(--aui-approval-card-key-primary-bg); color: var(--aui-approval-card-key-primary-fg); }
.aui-approval-card-button[data-unavailable] { opacity: .45; box-shadow: none; cursor: not-allowed; }
.aui-approval-card-reason { flex: 1 1 100%; min-width: 0; margin: 0; color: var(--aui-approval-card-faint); font-size: 12.5px; line-height: 17px; text-align: end; overflow-wrap: anywhere; }
.aui-approval-card-spinner { display: inline-grid; place-items: center; flex: none; width: 20px; height: 14px; }
.aui-approval-card-spinner svg { width: 14px; height: 14px; fill: none; stroke: currentColor; stroke-width: 1.8px; stroke-linecap: round; animation: aui-approval-card-spin 800ms linear infinite; }
.aui-approval-card[data-busy] .aui-approval-card-box, .aui-approval-card[data-busy] .aui-approval-card-actions { pointer-events: none; }
.aui-approval-card[data-busy] .aui-approval-card-button[data-variant="approve"] { pointer-events: auto; cursor: progress; }
.aui-approval-card[data-busy] .aui-approval-card-button[data-variant="deny"], .aui-approval-card[data-busy] .aui-approval-card-button[data-variant="edit"], .aui-approval-card[data-busy] .aui-approval-card-always { opacity: .5; }
.aui-approval-card-error { display: flex; align-items: flex-start; gap: 6px; margin: 8px 0 0; color: var(--aui-approval-card-err); font-size: 12.5px; line-height: 17px; }
.aui-approval-card-actions[data-place="tray"] + .aui-approval-card-error { margin: 2px 10px 6px; }
.aui-approval-card-error::before { content: ""; flex: none; width: 6px; height: 6px; margin-top: 5.5px; border-radius: 50%; background: currentColor; }
.aui-approval-card-record { display: flex; align-items: center; gap: 10px; min-height: 48px; padding: 12px 14px; border-radius: var(--aui-approval-card-r); background: var(--aui-approval-card-card); box-shadow: 0 0 0 1px var(--aui-approval-card-line), var(--aui-approval-card-shadow); outline: none; animation: aui-approval-card-enter 420ms var(--aui-approval-card-ease) 120ms both; }
.aui-approval-card-record:focus-visible { outline: 2px solid var(--aui-approval-card-focus); outline-offset: -2px; }
.aui-approval-card-mark { display: grid; place-items: center; flex: none; width: 22px; height: 22px; border-radius: 50%; background: var(--aui-approval-card-neutral-bg); color: var(--aui-approval-card-neutral-fg); animation: aui-approval-card-pop 460ms var(--aui-approval-card-ease) 140ms both; }
.aui-approval-card-record[data-result="approved"] .aui-approval-card-mark { background: var(--aui-approval-card-ok-bg); color: var(--aui-approval-card-ok-fg); }
.aui-approval-card-mark svg { width: 14px; height: 14px; fill: none; stroke: currentColor; stroke-width: 1.9px; stroke-linecap: round; stroke-linejoin: round; }
.aui-approval-card-mark path { stroke-dasharray: 1; stroke-dashoffset: 0; animation: aui-approval-card-draw 420ms var(--aui-approval-card-ease) 300ms both; }
.aui-approval-card-record-title { flex: 1 1 auto; color: var(--aui-approval-card-ink); font-size: 13.5px; font-weight: 500; }
.aui-approval-card-record-meta { flex: none; color: var(--aui-approval-card-faint); white-space: nowrap; }
.aui-approval-card-sr { position: absolute; width: 1px; height: 1px; margin: -1px; padding: 0; overflow: hidden; clip-path: inset(50%); white-space: nowrap; border: 0; }
@keyframes aui-approval-card-exit { to { opacity: 0; transform: translateY(-6px) scale(.985); filter: blur(2px); } }
@keyframes aui-approval-card-enter { from { opacity: 0; transform: translateY(4px); filter: blur(2px); } }
@keyframes aui-approval-card-pop { from { opacity: 0; transform: scale(.5); } }
@keyframes aui-approval-card-draw { from { stroke-dashoffset: 1; } to { stroke-dashoffset: 0; } }
@keyframes aui-approval-card-spin { to { transform: rotate(360deg); } }
@media (hover: none) {
  .aui-approval-card-button kbd { display: none; }
  .aui-approval-card-button:has(kbd) { padding-right: 12px; }
}
.aui-approval-card[data-expanded] [data-aui-clamp] { overflow: visible; white-space: normal; text-overflow: clip; }
.aui-approval-card[data-expanded] .aui-approval-card-details dt { overflow: visible; overflow-wrap: anywhere; }
@container (max-width: 400px) {
  .aui-approval-card-actions[data-place="tray"] { padding-left: 2px; }
  .aui-approval-card-timer { flex: 1 1 100%; padding-left: 8px; }
  .aui-approval-card-buttons { flex: 1 1 100%; flex-wrap: wrap; margin-left: 0; }
  .aui-approval-card-button { flex: 1 1 0; min-width: min(100%, fit-content); height: auto; min-height: 34px; padding: 8px 10px; white-space: normal; text-align: center; line-height: 1.25; }
  .aui-approval-card-button:has(kbd) { padding-right: 10px; }
  .aui-approval-card-button kbd { display: none; }
  .aui-approval-card-reason { padding-left: 8px; text-align: start; }
  .aui-approval-card [data-aui-clamp] { overflow: visible; white-space: normal; text-overflow: clip; }
  .aui-approval-card-details dt { overflow: visible; overflow-wrap: anywhere; }
  .aui-approval-card-detail[data-long] > dt, .aui-approval-card-detail[data-long] > dd { grid-column: 1 / -1; }
  .aui-approval-card-detail[data-long] > dd { margin-top: -4px; }
  .aui-approval-card-record { flex-wrap: wrap; row-gap: 2px; }
  .aui-approval-card-record-title { flex-basis: calc(100% - 32px); }
  .aui-approval-card-record-meta { flex: 1 1 100%; padding-left: 32px; white-space: normal; }
}
@container (max-width: 340px) {
  .aui-approval-card-box { padding: 14px; }
  .aui-approval-card-head { gap: 10px; }
  .aui-approval-card-details { column-gap: 12px; }
  .aui-approval-card-risk-word { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
}
@media (pointer: coarse), (max-width: 899px) {
  .aui-approval-card .aui-approval-card-button { min-height: 44px; }
  .aui-approval-card .aui-approval-card-always { min-height: 44px; }
  .aui-approval-card-more { min-height: 44px; }
  .aui-approval-card [data-aui-clamp] { overflow: visible; white-space: normal; text-overflow: clip; }
  .aui-approval-card-details dt { overflow: visible; overflow-wrap: anywhere; }
}
@media (prefers-reduced-motion: reduce) {
  .aui-approval-card-request[data-phase], .aui-approval-card-record, .aui-approval-card-mark, .aui-approval-card-mark path { animation-duration: 1ms !important; animation-delay: 0ms !important; }
  .aui-approval-card *, .aui-approval-card *::before, .aui-approval-card *::after { transition-duration: 1ms !important; }
  .aui-approval-card-spinner svg { animation-duration: 1600ms !important; }
}
}
`;

const EASE = 'cubic-bezier(0.22, 1, 0.36, 1)';
const isApple = () => typeof navigator !== 'undefined' && /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);
const reducedMotion = () => typeof window !== 'undefined' && typeof window.matchMedia === 'function' && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
const present = (node: ReactNode) => node !== undefined && node !== null && node !== false && node !== '';
const pad2 = (value: number) => String(value).padStart(2, '0');
/** Visible clock: m:ss below an hour, then h:mm:ss with no upper limit on hours. */
export function approvalClockText(seconds: number) {
  if (seconds < 3600) return `${Math.floor(seconds / 60)}:${pad2(seconds % 60)}`;
  return `${Math.floor(seconds / 3600)}:${pad2(Math.floor(seconds / 60) % 60)}:${pad2(seconds % 60)}`;
}
/** Spoken remaining time: seconds below 120, rounded minutes below 7200, then rounded hours. */
export function approvalSpokenRemaining(seconds: number) {
  if (seconds < 120) return `${seconds}초`;
  if (seconds < 7200) return `약 ${Math.round(seconds / 60)}분`;
  return `약 ${Math.round(seconds / 3600)}시간`;
}
const failureText = (failure: unknown) => failure instanceof Error ? failure.message : typeof failure === 'string' ? failure : '';
const textLength = (node: ReactNode) => plainText(node).length;
const plainText = (node: ReactNode): string => {
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (isValidElement<{ children?: ReactNode }>(node)) return plainText(node.props.children);
  return Array.isArray(node) ? node.map(plainText).join('') : '';
};
const longToken = (node: ReactNode) => /\S{25,}/.test(plainText(node));

/** Elapsed label for a decision record: floor-based minutes, hours, then days from 86400 seconds. */
export function approvalElapsedText(seconds: number) {
  if (seconds < 60) return '방금';
  if (seconds < 3600) return `${Math.floor(seconds / 60)}분 전`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}시간 전`;
  return `${Math.floor(seconds / 86400)}일 전`;
}

type Result = ApprovalResult;
function resultSentence(result: Result, always: boolean, title: string) {
  if (result === 'approved') return `승인됨${always ? ' · 계속 허용' : ''}: ${title}`;
  if (result === 'expired') return `시간 만료로 자동 거절됨: ${title}`;
  return `거절됨: ${title}`;
}

export const approvalCardDefaultLabels: ApprovalCardLabels = {
  approve: '승인',
  deny: '거절',
  edit: '수정',
  always: '이 작업은 다음에도 허용',
  busy: '처리 중',
  riskLow: '낮음',
  riskMedium: '중간',
  riskHigh: '높음',
  riskSuffix: '위험',
  expand: '잘린 내용 모두 보기',
  collapse: '한 줄로 보기',
  timer: (clock) => `${clock} 뒤 자동 거절`,
  timerHint: (remaining) => `남은 시간 ${remaining}. 그 안에 결정하지 않으면 자동으로 거절됩니다. 카드 위에 포인터가 있거나 카드 안에 초점이 있거나 승인을 처리하는 동안에는 시간이 멈춥니다.`,
  remaining: approvalSpokenRemaining,
  elapsed: approvalElapsedText,
  resultApproved: '승인됨',
  resultDenied: '거절됨',
  resultExpired: '시간 만료',
  alwaysResult: '계속 허용',
  announce: resultSentence,
};

export function ApprovalCard({
  title,
  description,
  risk,
  icon,
  details = [],
  preview,
  children,
  status: statusProp,
  defaultStatus = 'pending',
  onApprove,
  onDeny,
  onEdit,
  approveDisabledReason,
  expiresIn,
  onExpire,
  allowAlways,
  alwaysLabel,
  approveLabel,
  denyLabel,
  editLabel,
  labels: labelsProp,
  shortcuts = true,
  context,
  actions = 'tray',
  className = '',
  style,
}: ApprovalCardProps) {
  const text: ApprovalCardLabels = { ...approvalCardDefaultLabels, ...labelsProp };
  const approveCopy = approveLabel ?? text.approve;
  const denyCopy = denyLabel ?? text.deny;
  const editCopy = editLabel ?? text.edit;
  const alwaysCopy = alwaysLabel ?? text.always;
  const riskText: Record<ApprovalRisk, string> = { low: text.riskLow, medium: text.riskMedium, high: text.riskHigh };
  const resultText: Record<Result, string> = { approved: text.resultApproved, denied: text.resultDenied, expired: text.resultExpired };
  // Host-owned actions: a control exists only for a handler the host passed (or an explained, disabled approval).
  const unavailableReason = typeof approveDisabledReason === 'string' ? approveDisabledReason.trim() : '';
  const canApprove = !!onApprove && !unavailableReason;
  const showApprove = !!onApprove || !!unavailableReason;
  const showAlways = canApprove && (allowAlways ?? (onApprove?.length ?? 0) > 0);
  const id = useId();
  const controlled = statusProp !== undefined;
  const [internalStatus, setInternalStatus] = useState<ApprovalStatus>(defaultStatus);
  const status = controlled ? statusProp : internalStatus;
  const total = typeof expiresIn === 'number' && Number.isFinite(expiresIn) && expiresIn > 0 ? expiresIn * 1000 : 0;
  // A clock nobody receives would promise an automatic denial that never happens: no onDeny or onExpire, no clock.
  const timed = total > 0 && (!!onDeny || !!onExpire);

  const [always, setAlways] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [reason, setReason] = useState<DenyReason | null>(null);
  const [decidedAt, setDecidedAt] = useState<number | null>(() => status === 'pending' ? null : Date.now());
  const [now, setNow] = useState(() => Date.now());
  const [announcement, setAnnouncement] = useState('');
  const [phase, setPhase] = useState<'idle' | 'enter'>('idle');
  const [exiting, setExiting] = useState(false);
  const [requestKey, setRequestKey] = useState(0);
  const [hover, setHover] = useState(false);
  const [focusIn, setFocusIn] = useState(false);
  const [timerKey, setTimerKey] = useState(0);
  const [shown, setShown] = useState(() => Math.ceil(total / 1000));
  const [expired, setExpired] = useState(false);
  const [truncated, setTruncated] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [tracked, setTracked] = useState({ status, total });

  const rootRef = useRef<HTMLElement>(null);
  const trayRef = useRef<HTMLDivElement>(null);
  const requestRef = useRef<HTMLDivElement>(null);
  const recordRef = useRef<HTMLDivElement>(null);
  const approveRef = useRef<HTMLButtonElement>(null);
  const busyRef = useRef(false);
  const generation = useRef(0);
  const mounted = useRef(true);
  const remainingRef = useRef(total);
  const focusTarget = useRef<'record' | 'approve' | null>(null);
  const mac = useMemo(isApple, []);

  // Status transitions are derived during render so the leaving request, the record and the reset all commit together.
  if (tracked.status !== status || tracked.total !== total) {
    if (tracked.status !== status) {
      const active = typeof document !== 'undefined' ? document.activeElement : null;
      if (status === 'pending') {
        focusTarget.current = active !== null && active === recordRef.current ? 'approve' : null;
        setAlways(false); setBusy(false); setError(''); setReason(null); setAnnouncement(''); setDecidedAt(null);
        setExpired(false); setExiting(false); setPhase('enter'); setRequestKey((key) => key + 1);
        setTimerKey((key) => key + 1); setShown(Math.ceil(total / 1000));
      } else {
        if (tracked.status === 'pending') {
          focusTarget.current = active && rootRef.current?.contains(active) ? 'record' : null;
          setExiting(true); setPhase('idle');
        }
        const at = Date.now();
        setDecidedAt(at); setNow(at);
        const result: Result = status === 'approved' ? 'approved' : reason === 'timeout' ? 'expired' : 'denied';
        setAnnouncement(text.announce(result, always, title));
      }
    } else if (status === 'pending') {
      setTimerKey((key) => key + 1); setShown(Math.ceil(total / 1000)); setExpired(false);
    }
    setTracked({ status, total });
  }

  const pending = status === 'pending';
  const result: Result = status === 'approved' ? 'approved' : reason === 'timeout' ? 'expired' : 'denied';

  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  useLayoutEffect(() => { busyRef.current = false; generation.current += 1; }, [requestKey]);

  // Focus follows the decision only when it was already inside the card, and never scrolls the page.
  useLayoutEffect(() => {
    const target = focusTarget.current;
    if (!target) return;
    focusTarget.current = null;
    (target === 'record' ? recordRef.current : approveRef.current)?.focus({ preventScroll: true });
  }, [status]);

  // Height: from the previous natural height to the new one when the card switches between request and record.
  const category = pending ? 'request' : 'record';
  const lastCategory = useRef(category);
  const lastHeight = useRef(0);
  const heightAnimation = useRef<{ animation: Animation; timer: number } | null>(null);
  useLayoutEffect(() => {
    const tray = trayRef.current;
    if (!tray) return;
    lastHeight.current = tray.getBoundingClientRect().height;
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => { if (!heightAnimation.current) lastHeight.current = tray.getBoundingClientRect().height; });
    observer.observe(tray);
    return () => {
      observer.disconnect();
      if (heightAnimation.current) { window.clearTimeout(heightAnimation.current.timer); heightAnimation.current.animation.cancel(); heightAnimation.current = null; }
    };
  }, []);
  useLayoutEffect(() => {
    const tray = trayRef.current;
    if (!tray || lastCategory.current === category) return;
    lastCategory.current = category;
    const running = heightAnimation.current;
    const from = running ? tray.getBoundingClientRect().height : lastHeight.current;
    if (running) { window.clearTimeout(running.timer); running.animation.cancel(); heightAnimation.current = null; }
    const to = tray.getBoundingClientRect().height;
    lastHeight.current = to;
    if (!from || Math.abs(from - to) < 0.5 || reducedMotion() || typeof tray.animate !== 'function') return;
    const animation = tray.animate([{ height: `${from}px` }, { height: `${to}px` }], { duration: 520, easing: EASE, fill: 'forwards' });
    const timer = window.setTimeout(() => {
      animation.cancel();
      heightAnimation.current = null;
      lastHeight.current = tray.getBoundingClientRect().height;
    }, 560);
    heightAnimation.current = { animation, timer };
  }, [category]);

  // The leaving request and the entering one fall back to timers in case animation events never arrive.
  useEffect(() => {
    if (!exiting) return;
    const timer = window.setTimeout(() => setExiting(false), 600);
    return () => window.clearTimeout(timer);
  }, [exiting]);
  useEffect(() => {
    if (phase !== 'enter') return;
    const timer = window.setTimeout(() => setPhase('idle'), 700);
    return () => window.clearTimeout(timer);
  }, [phase]);

  // Elapsed time on the record refreshes every 30 s from the decision moment.
  useEffect(() => {
    if (pending || decidedAt === null) return;
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, [pending, decidedAt]);

  function approve() {
    if (!onApprove || unavailableReason || status !== 'pending' || exiting || busyRef.current) return;
    setError('');
    const result = onApprove({ always: showAlways && always });
    if (result && typeof (result as PromiseLike<unknown>).then === 'function') {
      const run = generation.current;
      busyRef.current = true;
      setBusy(true);
      (result as PromiseLike<unknown>).then(() => {
        if (!mounted.current || run !== generation.current) return;
        busyRef.current = false;
        setBusy(false);
        if (!controlled) setInternalStatus('approved');
      }, (failure: unknown) => {
        if (!mounted.current || run !== generation.current) return;
        busyRef.current = false;
        setBusy(false);
        setError(failureText(failure));
      });
    } else if (!controlled) setInternalStatus('approved');
  }

  function deny(why: DenyReason) {
    if (status !== 'pending' || exiting || busyRef.current) return;
    // No receiver, no decision: without a handler the card never records a denial the host did not see.
    if (why === 'user' ? !onDeny : !onDeny && !onExpire) return;
    setReason(why);
    onDeny?.({ reason: why });
    if (!controlled) setInternalStatus('denied');
  }

  function edit() {
    if (!onEdit || status !== 'pending' || exiting || busyRef.current) return;
    onEdit();
  }

  const expire = useRef(() => {});
  expire.current = () => {
    setExpired(true);
    onExpire?.();
    deny('timeout');
  };

  // Clock: real elapsed milliseconds, held while the pointer or focus is inside or approval is in flight.
  useEffect(() => { remainingRef.current = total; }, [timerKey]); // eslint-disable-line react-hooks/exhaustive-deps
  const ticking = pending && !exiting && timed && !expired && !hover && !focusIn && !busy;
  useEffect(() => {
    if (!ticking) return;
    let last = performance.now();
    let frame = 0;
    let finished = false;
    const step = () => {
      const time = performance.now();
      remainingRef.current -= time - last;
      last = time;
      if (remainingRef.current <= 0) {
        remainingRef.current = 0;
        finished = true;
        setShown(0);
        expire.current();
        return;
      }
      setShown(Math.ceil(remainingRef.current / 1000));
      frame = requestAnimationFrame(step);
    };
    frame = requestAnimationFrame(step);
    return () => {
      cancelAnimationFrame(frame);
      if (!finished) remainingRef.current = Math.max(0, remainingRef.current - (performance.now() - last));
    };
  }, [ticking, timerKey]);

  // Kit addition: offer a keyboard/touch path to text that a one-line rule had to cut.
  useLayoutEffect(() => {
    const request = requestRef.current;
    if (!request || !pending) return;
    const measure = () => {
      let cut = false;
      request.querySelectorAll<HTMLElement>('[data-aui-clamp], .aui-approval-card-details dt').forEach((node) => { if (node.scrollWidth > node.clientWidth + 1) cut = true; });
      setTruncated(cut);
    };
    measure();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(measure);
    observer.observe(request);
    return () => observer.disconnect();
  }, [pending, requestKey, title, description, details, alwaysCopy, expanded]);

  function onKeyDown(event: KeyboardEvent<HTMLElement>) {
    if (event.defaultPrevented || event.nativeEvent.isComposing) return;
    if (status !== 'pending' || exiting || busyRef.current) return;
    if (canApprove && event.key === 'Enter' && (event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey) {
      event.preventDefault();
      approve();
    } else if (onDeny && event.key === 'Escape' && !event.metaKey && !event.ctrlKey && !event.altKey && !event.shiftKey) {
      event.preventDefault();
      deny('user');
    }
  }

  const titleId = `${id}-title`;
  const descId = `${id}-desc`;
  const timerId = `${id}-timer`;
  const hasDescription = present(description);
  const custom = preview === undefined || preview === null ? children : preview;
  const hasCustom = present(custom);
  const hasDetails = details.length > 0;
  const showRequest = pending || exiting;
  const describedBy = pending ? [hasDescription ? descId : '', timed ? timerId : ''].filter(Boolean).join(' ') || undefined : undefined;
  const elapsed = decidedAt === null ? 0 : Math.max(0, Math.floor((now - decidedAt) / 1000));
  const reasonId = `${id}-approve-reason`;
  const hasButtons = !!onEdit || !!onDeny || showApprove;

  const actionRow = (timed || hasButtons) && <>
    <div className="aui-approval-card-actions" data-place={actions}>
      {timed && <span className="aui-approval-card-timer" data-warn={shown <= 10} aria-hidden="true">
        <Clock aria-hidden="true" /><span className="aui-approval-card-clock">{text.timer(approvalClockText(shown))}</span>
      </span>}
      {hasButtons && <div className="aui-approval-card-buttons">
        {onEdit && <button type="button" className="aui-approval-card-button" data-variant="edit" aria-disabled={busy || undefined} onClick={edit}><span className="aui-approval-card-label">{editCopy}</span></button>}
        {onDeny && <button type="button" className="aui-approval-card-button" data-variant="deny" aria-disabled={busy || undefined} aria-keyshortcuts="Escape" onClick={() => deny('user')}>
          <span className="aui-approval-card-label">{denyCopy}</span>{shortcuts && <kbd aria-hidden="true">Esc</kbd>}
        </button>}
        {showApprove && <button
          ref={approveRef}
          type="button"
          className="aui-approval-card-button"
          data-variant="approve"
          data-unavailable={unavailableReason ? '' : undefined}
          aria-disabled={busy || !!unavailableReason || undefined}
          aria-describedby={unavailableReason ? reasonId : undefined}
          aria-keyshortcuts={canApprove ? 'Meta+Enter Control+Enter' : undefined}
          onClick={approve}
        >
          {busy && <span className="aui-approval-card-spinner" aria-hidden="true"><svg viewBox="0 0 14 14"><circle cx="7" cy="7" r="5.25" pathLength={100} strokeDasharray="69 100" /></svg></span>}
          <span className="aui-approval-card-label">{approveCopy}</span>{busy && <span className="aui-approval-card-sr"> · {text.busy}</span>}
          {shortcuts && canApprove && <kbd aria-hidden="true">{mac ? '⌘↵' : 'Ctrl↵'}</kbd>}
        </button>}
      </div>}
      {unavailableReason && <p id={reasonId} className="aui-approval-card-reason">{unavailableReason}</p>}
    </div>
    {error && <p className="aui-approval-card-error" role="alert">{error}</p>}
  </>;

  return (
    <section
      ref={rootRef}
      className={`aui-approval-card ${className}`.trim()}
      style={style}
      role="group"
      aria-label={title}
      aria-describedby={describedBy}
      aria-busy={busy || undefined}
      data-actions={actions}
      data-status={status}
      data-busy={busy ? '' : undefined}
      data-context={present(context) ? '' : undefined}
      data-expanded={expanded ? '' : undefined}
      onPointerEnter={() => setHover(true)}
      onPointerLeave={() => setHover(false)}
      onFocus={() => setFocusIn(true)}
      onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setFocusIn(false); }}
      onKeyDown={onKeyDown}
    >
      <style>{approvalCardStyles}</style>
      <div ref={trayRef} className="aui-approval-card-tray">
        {present(context) && <div className="aui-approval-card-context" data-aui-clamp title={typeof context === 'string' ? context : undefined}>{context}</div>}
        {showRequest && <div
          key={`request-${requestKey}`}
          ref={requestRef}
          className="aui-approval-card-request"
          data-phase={exiting ? 'exit' : phase === 'enter' ? 'enter' : undefined}
          inert={exiting || undefined}
          aria-hidden={exiting || undefined}
          onAnimationEnd={(event) => {
            if (event.target !== event.currentTarget) return;
            if (exiting) setExiting(false); else if (phase === 'enter') setPhase('idle');
          }}
        >
          <div className="aui-approval-card-box">
            <div className="aui-approval-card-head">
              <span className="aui-approval-card-icon" aria-hidden="true">{icon ?? <ShieldCheck />}</span>
              <div className="aui-approval-card-heading">
                <h3 id={titleId} data-aui-clamp title={title}>{title}</h3>
                {hasDescription && <p id={descId} data-aui-clamp title={typeof description === 'string' ? description : undefined}>{description}</p>}
                {(truncated || expanded) && <button type="button" className="aui-approval-card-more" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>{expanded ? text.collapse : text.expand}</button>}
              </div>
              {risk && <span className="aui-approval-card-risk" data-risk={risk}>
                <span className="aui-approval-card-risk-dot" aria-hidden="true" />{riskText[risk]}{text.riskSuffix && <span className="aui-approval-card-risk-word"> {text.riskSuffix}</span>}
              </span>}
            </div>
            {(hasCustom || hasDetails) && <div className="aui-approval-card-panel">
              {hasCustom && <div className="aui-approval-card-preview">{custom}</div>}
              {hasDetails && <dl className="aui-approval-card-details">
                {details.map((detail, index) => <div key={index} className="aui-approval-card-detail" data-long={textLength(detail.label) > 10 || longToken(detail.value) || undefined}>
                  <dt title={typeof detail.label === 'string' ? detail.label : undefined}>{detail.label}</dt>
                  <dd>{detail.value}</dd>
                </div>)}
              </dl>}
            </div>}
            {showAlways && <label className="aui-approval-card-always">
              <input type="checkbox" className="aui-approval-card-always-input" checked={always} disabled={busy} onChange={(event) => setAlways(event.target.checked)} />
              <span className="aui-approval-card-always-box" aria-hidden="true"><svg viewBox="0 0 12 12"><path d="M2.6 6.3 5 8.6l4.4-5" /></svg></span>
              <span className="aui-approval-card-always-text">{alwaysCopy}</span>
            </label>}
            {actions === 'card' && actionRow}
          </div>
          {actions === 'tray' && actionRow}
        </div>}
        {!pending && <div ref={recordRef} className="aui-approval-card-record" data-result={result} tabIndex={-1}>
          <span className="aui-approval-card-mark" aria-hidden="true">
            <svg viewBox="0 0 14 14">{result === 'approved' ? <path pathLength={1} d="M3 7.3 5.9 10.2 11 4.2" /> : <path pathLength={1} d="M4 4l6 6M10 4l-6 6" />}</svg>
          </span>
          <span className="aui-approval-card-record-title" data-aui-clamp title={title}>{title}</span>
          <span className="aui-approval-card-record-meta">{resultText[result]}{result === 'approved' && always ? ` · ${text.alwaysResult}` : ''} · {text.elapsed(elapsed)}</span>
        </div>}
      </div>
      {pending && timed && <span id={timerId} hidden>{text.timerHint(text.remaining(shown))}</span>}
      <p className="aui-approval-card-sr" aria-live="polite" aria-atomic="true">{announcement}</p>
    </section>
  );
}
