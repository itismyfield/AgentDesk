"use client";
// Adapted from affectUI (https://github.com/flohoeller/affectui @ e610c3ba), MIT License, Copyright (c) 2026 Florian Höller (adfects). See LICENSE.affectui.
// Changes: aui- class prefix; coarse-pointer targets at least 44px; max-width containment for phone, tablet and desktop.

import { useEffect, useId, useLayoutEffect, useRef, useState, type ComponentPropsWithRef, type ReactNode } from "react";

/*
 * Text Area – a single self-contained file, clean style.
 * The multi-line twin of the Text Field: a label, a hint the error takes the place of, a character count, a box that
 * grows with the text between minRows and maxRows (then scrolls), an optional button that gives it more room and
 * ⌘/Ctrl+Enter to send. White with a hairline shadow; focus only firms up the edge – no ring.
 * The styles are injected below, so no extra files are needed.
 */

export type TextAreaProps = Omit<ComponentPropsWithRef<"textarea">, "rows"> & {
  /** Shown above the field and tied to it */
  label?: ReactNode;
  /** A quiet line under the field – the error takes its place while there is one */
  hint?: ReactNode;
  /** Marks the field invalid and shows this under it, in place of the hint */
  error?: ReactNode;
  /** "Optional" next to the label */
  optional?: boolean;
  /** Shows "120/2000" under the field; needs maxLength */
  counter?: boolean;
  /** Height when empty, in lines */
  minRows?: number;
  /** It grows with the text up to this many lines, then scrolls */
  maxRows?: number;
  /** A small button in the corner that gives the field more room (expandedRows) and back */
  expandable?: boolean;
  /** Lines while expanded */
  expandedRows?: number;
  /** Controls the expanded state from outside */
  expanded?: boolean;
  onExpandedChange?: (expanded: boolean) => void;
  /** ⌘/Ctrl+Enter – e.g. save or send without leaving the keyboard */
  onSubmitShortcut?: (value: string) => void;
  /** Extra class on the textarea itself; className goes on the wrapper */
  inputClassName?: string;
};

export function TextArea({
  label,
  hint,
  error,
  optional = false,
  counter = false,
  minRows = 3,
  maxRows = 10,
  expandable = false,
  expandedRows = 16,
  expanded: expandedProp,
  onExpandedChange,
  onSubmitShortcut,
  className,
  inputClassName,
  id,
  value,
  defaultValue,
  onChange,
  onKeyDown,
  disabled,
  readOnly,
  maxLength,
  ref,
  "aria-describedby": describedBy,
  ...rest
}: TextAreaProps) {
  const autoId = useId();
  const inputId = id ?? `${autoId}-input`;
  const messageId = `${autoId}-message`;
  const counterId = `${autoId}-count`;
  const area = useRef<HTMLTextAreaElement | null>(null);

  // The current text, for the count and the height – follows value when controlled, the field itself otherwise
  const [own, setOwn] = useState(String(defaultValue ?? ""));
  const text = value !== undefined && value !== null ? String(value) : own;

  const [ownExpanded, setOwnExpanded] = useState(false);
  const expanded = expandable && (expandedProp ?? ownExpanded);
  // The height glides only when the room changes on purpose – never while typing
  const [gliding, setGliding] = useState(false);
  const glideTimer = useRef(0);
  useEffect(() => () => window.clearTimeout(glideTimer.current), []);

  const invalid = Boolean(error) || rest["aria-invalid"] === true || rest["aria-invalid"] === "true";
  const message = shows(error) ? error : shows(hint) ? hint : null;
  const showCount = counter && typeof maxLength === "number";
  const fallbackName =
    label == null && !rest["aria-label"] && !rest["aria-labelledby"] && typeof rest.placeholder === "string" ? rest.placeholder : undefined;

  // Fits the height to the text: at least minRows (expandedRows while expanded), at most maxRows, then it scrolls
  const fit = () => {
    const el = area.current;
    if (!el) return;
    const cs = getComputedStyle(el);
    const line = parseFloat(cs.lineHeight) || 20;
    const pad = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
    const min = (expanded ? Math.max(expandedRows, minRows) : minRows) * line + pad;
    const max = (expanded ? Math.max(expandedRows, maxRows) : Math.max(maxRows, minRows)) * line + pad;
    const before = el.style.height;
    el.style.height = "auto";
    const needed = el.scrollHeight;
    const next = Math.min(Math.max(needed, min), max);
    // Back to the old height and a read, so a glide starts from there and not from "auto"
    el.style.height = before;
    void el.offsetHeight;
    el.style.height = `${next}px`;
    el.style.overflowY = needed > max + 1 ? "auto" : "hidden";
  };

  useLayoutEffect(fit, [text, expanded, minRows, maxRows, expandedRows]); // eslint-disable-line react-hooks/exhaustive-deps

  // A narrower or wider field wraps differently – measure again
  useEffect(() => {
    const el = area.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    let width = el.clientWidth;
    const ro = new ResizeObserver(() => {
      if (el.clientWidth === width) return;
      width = el.clientWidth;
      fit();
    });
    // A new text size (a touch screen, a zoom) changes the line height too
    const mq = window.matchMedia("(pointer: coarse)");
    mq.addEventListener("change", fit);
    ro.observe(el);
    return () => {
      ro.disconnect();
      mq.removeEventListener("change", fit);
    };
  }); // eslint-disable-line react-hooks/exhaustive-deps

  // An uncontrolled field follows form.reset(), so the count and the height don't keep the old text
  useEffect(() => {
    const form = area.current?.form;
    if (!form || value !== undefined) return;
    const onReset = () => window.setTimeout(() => setOwn(area.current?.value ?? ""));
    form.addEventListener("reset", onReset);
    return () => form.removeEventListener("reset", onReset);
  }, [value]);

  const toggle = () => {
    const next = !expanded;
    setGliding(true);
    window.clearTimeout(glideTimer.current);
    glideTimer.current = window.setTimeout(() => setGliding(false), 320);
    if (expandedProp === undefined) setOwnExpanded(next);
    onExpandedChange?.(next);
  };

  return (
    <div
      className={["aui-text-area-clean", className].filter(Boolean).join(" ")}
      data-invalid={invalid || undefined}
      data-disabled={disabled || undefined}
      data-readonly={readOnly || undefined}
      data-expandable={expandable || undefined}
    >
      {/* React 19 hoists this into <head> once, no matter how many fields render */}
      <style href="ui-text-area-clean" precedence="default">
        {css}
      </style>
      {label != null && (
        <div className="aui-text-area-clean__head">
          <label className="aui-text-area-clean__label" htmlFor={inputId}>
            {label}
          </label>
          {optional && <span className="aui-text-area-clean__optional">Optional</span>}
        </div>
      )}
      <div className="aui-text-area-clean__box">
        <textarea
          {...rest}
          ref={(el) => {
            area.current = el;
            if (typeof ref === "function") ref(el);
            else if (ref) ref.current = el;
          }}
          id={inputId}
          className={["aui-text-area-clean__input", inputClassName].filter(Boolean).join(" ")}
          rows={minRows}
          value={value}
          defaultValue={defaultValue}
          disabled={disabled}
          readOnly={readOnly}
          maxLength={maxLength}
          dir={rest.dir ?? "auto"}
          aria-label={rest["aria-label"] ?? fallbackName}
          aria-invalid={invalid || undefined}
          aria-describedby={[describedBy, message ? messageId : null, showCount ? counterId : null].filter(Boolean).join(" ") || undefined}
          aria-keyshortcuts={onSubmitShortcut ? "Meta+Enter Control+Enter" : rest["aria-keyshortcuts"]}
          data-gliding={gliding || undefined}
          onChange={(e) => {
            if (value === undefined) setOwn(e.target.value);
            onChange?.(e);
          }}
          onKeyDown={(e) => {
            if (onSubmitShortcut && e.key === "Enter" && (e.metaKey || e.ctrlKey) && !e.nativeEvent.isComposing) {
              e.preventDefault();
              onSubmitShortcut(e.currentTarget.value);
            }
            onKeyDown?.(e);
          }}
        />
        {expandable && !disabled && (
          <button
            type="button"
            className="aui-text-area-clean__expand"
            aria-label={expanded ? "Less room" : "More room"}
            aria-pressed={expanded}
            aria-controls={inputId}
            onClick={toggle}
          >
            <span className="aui-text-area-clean__swap" key={expanded ? "less" : "more"}>
              {expanded ? <CollapseIcon /> : <ExpandIcon />}
            </span>
          </button>
        )}
      </div>
      {(message || showCount) && (
        <div className="aui-text-area-clean__foot">
          {/* The error swaps in where the hint was, so the form below doesn't move */}
          <p className="aui-text-area-clean__message" id={messageId} key={shows(error) ? "error" : "hint"} dir="auto" aria-live={shows(error) ? "polite" : undefined}>
            {message}
          </p>
          {showCount && (
            <span
              className="aui-text-area-clean__count"
              id={counterId}
              data-full={text.length === maxLength || undefined}
              data-over={text.length > maxLength || undefined}
            >
              {text.length.toLocaleString()}/{maxLength.toLocaleString()}
            </span>
          )}
        </div>
      )}
    </div>
  );
}

/** Text or an element – not true, false, null or an empty string */
const shows = (v: ReactNode) => v != null && typeof v !== "boolean" && v !== "";

/* Icons: Hugeicons Free (MIT), drawn at 16px */
function ExpandIcon() {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M16.5 3h4.5v4.5M21 3l-6 6M7.5 21H3v-4.5M3 21l6-6" />
    </svg>
  );
}
function CollapseIcon() {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M14.5 5v4.5H19M14.5 9.5 20 4M9.5 19v-4.5H5M9.5 14.5 4 20" />
    </svg>
  );
}

const css = /* css */ `
/* Colors and motion as variables – override them on .aui-text-area-clean (see Theming in the docs) */
.aui-text-area-clean {
  --ui-tac-title: var(--aui-ink, rgba(0, 0, 0, 0.88));
  --ui-tac-ink: var(--aui-ink, rgba(0, 0, 0, 0.88));
  --ui-tac-muted: var(--aui-muted, rgba(0, 0, 0, 0.61));
  --ui-tac-subtle: var(--aui-muted, rgba(0, 0, 0, 0.45));
  --ui-tac-faint: var(--aui-placeholder, rgba(0, 0, 0, 0.3));
  --ui-tac-icon: var(--aui-muted, rgba(0, 0, 0, 0.447));
  --ui-tac-surface: var(--aui-surface, #ffffff);
  --ui-tac-wash: rgba(0, 0, 0, 0.04);
  --ui-tac-readonly: rgba(0, 0, 0, 0.02);
  --ui-tac-shadow: 0 0 0 0.5px var(--aui-border, rgba(0, 0, 0, 0.08)), 0 1px 1px -1px rgba(0, 0, 0, 0.1), 0 1px 2px 0 rgba(0, 0, 0, 0.05);
  --ui-tac-shadow-hover: 0 0 0 0.5px var(--aui-border-strong, rgba(0, 0, 0, 0.13)), 0 1px 1px -1px rgba(0, 0, 0, 0.12), 0 1px 2px 0 rgba(0, 0, 0, 0.06);
  --ui-tac-shadow-focus: 0 0 0 1px var(--aui-border-strong, rgba(0, 0, 0, 0.28)), 0 1px 2px 0 rgba(0, 0, 0, 0.06);
  --ui-tac-danger: var(--aui-danger, #c4320a);
  --ui-tac-danger-edge: color-mix(in srgb, var(--aui-red, #f04438) 55%, transparent);
  --ui-tac-danger-edge-focus: rgba(229, 72, 77, 0.8);
  --ui-tac-focus: var(--aui-focus, rgba(0, 96, 226, 0.7));
  --ui-tac-ease: cubic-bezier(0.22, 1, 0.36, 1);

  --fs: 13px;
  --lh: 20px;

  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
  font-family: var(--aui-font, "Inter", "InterVariable", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif);
  font-size: 13px;
  line-height: 20px;
  color: var(--ui-tac-ink);
  -webkit-font-smoothing: antialiased;
}
/* Touch screens: 16px text, so iOS doesn't zoom in on focus */
@media (pointer: coarse), (max-width: 899px) {
  .aui-text-area-clean { --fs: 16px; --lh: 24px; }
}

.aui-text-area-clean__head { display: flex; align-items: baseline; gap: 6px; min-width: 0; }
.aui-text-area-clean__label { min-width: 0; font-weight: 500; color: var(--ui-tac-title); overflow-wrap: anywhere; cursor: default; }
.aui-text-area-clean__optional { flex: none; font-size: 12px; line-height: 16px; color: var(--ui-tac-subtle); }

/* The box: white, a hairline shadow as its edge; hover and focus only make that edge firmer */
.aui-text-area-clean__box {
  position: relative;
  display: flex;
  min-width: 0;
  border-radius: var(--aui-radius, 8px);
  background: var(--ui-tac-surface);
  box-shadow: var(--ui-tac-shadow);
  cursor: text;
  transition: box-shadow 140ms ease, background-color 140ms ease;
}
.aui-text-area-clean__box:hover { box-shadow: var(--ui-tac-shadow-hover); }
.aui-text-area-clean__box:focus-within { box-shadow: var(--ui-tac-shadow-focus); }
.aui-text-area-clean[data-invalid] .aui-text-area-clean__box { box-shadow: 0 0 0 1px var(--ui-tac-danger-edge), 0 1px 2px 0 rgba(0, 0, 0, 0.05); animation: aui-text-area-clean-nudge 320ms var(--ui-tac-ease); }
.aui-text-area-clean[data-invalid] .aui-text-area-clean__box:focus-within { box-shadow: 0 0 0 1px var(--ui-tac-danger-edge-focus), 0 1px 2px 0 rgba(0, 0, 0, 0.05); }
.aui-text-area-clean[data-readonly] .aui-text-area-clean__box { background: var(--ui-tac-readonly); box-shadow: 0 0 0 0.5px rgba(0, 0, 0, 0.08); cursor: default; }
.aui-text-area-clean[data-disabled] .aui-text-area-clean__box { background: var(--ui-tac-readonly); box-shadow: 0 0 0 0.5px rgba(0, 0, 0, 0.06); cursor: not-allowed; }
.aui-text-area-clean[data-disabled] { opacity: 0.55; }

.aui-text-area-clean__input {
  display: block;
  flex: 1;
  box-sizing: border-box;
  width: 100%;
  min-width: 0;
  margin: 0;
  padding: 7px 10px;
  border: 0;
  border-radius: inherit;
  outline: 0;
  background: transparent;
  font: inherit;
  font-size: var(--fs);
  line-height: var(--lh);
  color: var(--ui-tac-ink);
  overflow-wrap: anywhere;
  resize: none;
  overflow-y: hidden;
  scrollbar-width: thin;
  -webkit-appearance: none;
  appearance: none;
}
/* The corner button keeps clear of the text */
.aui-text-area-clean[data-expandable] .aui-text-area-clean__input { padding-inline-end: 36px; }
.aui-text-area-clean__input::placeholder { color: var(--ui-tac-faint); opacity: 1; }
.aui-text-area-clean__input:disabled { cursor: not-allowed; }
.aui-text-area-clean__input[data-gliding] { transition: height 300ms var(--ui-tac-ease); }

/* More room: a small ghost button in the top corner */
.aui-text-area-clean__expand {
  position: absolute;
  top: 4px;
  inset-inline-end: 4px;
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  margin: 0;
  padding: 0;
  border: 0;
  border-radius: 5px;
  background: transparent;
  color: var(--ui-tac-icon);
  cursor: pointer;
  -webkit-tap-highlight-color: transparent;
  transition: background-color 140ms ease, color 140ms ease;
}
.aui-text-area-clean__expand:hover { background: var(--ui-tac-wash); color: var(--ui-tac-ink); }
.aui-text-area-clean__expand:focus-visible { outline: 2px solid var(--ui-tac-focus); outline-offset: -2px; }
.aui-text-area-clean__expand svg { display: block; }
.aui-text-area-clean__swap { display: grid; animation: aui-text-area-clean-swap 280ms var(--ui-tac-ease); }
@media (pointer: coarse), (max-width: 899px) {
  .aui-text-area-clean__expand { width: 32px; height: 32px; }
  .aui-text-area-clean__expand::before { content: ""; position: absolute; inset: -4px; }
  .aui-text-area-clean[data-expandable] .aui-text-area-clean__input { padding-inline-end: 42px; }
}

/* Under the field: the hint, or the error in its place, and the count */
.aui-text-area-clean__foot { display: flex; align-items: flex-start; gap: 12px; min-width: 0; }
.aui-text-area-clean__message {
  flex: 1;
  min-width: 0;
  margin: 0;
  font-size: 12px;
  line-height: 16px;
  color: var(--ui-tac-subtle);
  overflow-wrap: anywhere;
  animation: aui-text-area-clean-message 260ms var(--ui-tac-ease);
}
.aui-text-area-clean[data-invalid] .aui-text-area-clean__message { color: var(--ui-tac-danger); }
.aui-text-area-clean__count { flex: none; margin-left: auto; font-size: 12px; line-height: 16px; font-variant-numeric: tabular-nums; color: var(--ui-tac-faint); transition: color 140ms ease; }
.aui-text-area-clean__count[data-full] { color: var(--ui-tac-muted); }
.aui-text-area-clean__count[data-over] { color: var(--ui-tac-danger); }

@keyframes aui-text-area-clean-nudge {
  30% { transform: translateX(-3px); }
  65% { transform: translateX(2px); }
}
@keyframes aui-text-area-clean-message {
  from { opacity: 0; transform: translateY(-3px); filter: blur(2px); }
}
@keyframes aui-text-area-clean-swap {
  from { opacity: 0; transform: scale(0.6); filter: blur(2px); }
}

@media (prefers-reduced-motion: reduce) {
  .aui-text-area-clean *, .aui-text-area-clean[data-invalid] .aui-text-area-clean__box { animation-duration: 1ms !important; }
  .aui-text-area-clean__box, .aui-text-area-clean__input[data-gliding] { transition-duration: 1ms; }
}

/* Dark mode: follows a .dark class or data-theme="dark" on any ancestor, e.g. <html> */
:where(.dark, [data-theme="dark"]) .aui-text-area-clean {
  --ui-tac-title: var(--aui-ink, rgba(255, 255, 255, 0.92));
  --ui-tac-ink: var(--aui-ink, rgba(255, 255, 255, 0.9));
  --ui-tac-muted: var(--aui-muted, rgba(255, 255, 255, 0.62));
  --ui-tac-subtle: var(--aui-muted, rgba(255, 255, 255, 0.46));
  --ui-tac-faint: var(--aui-placeholder, rgba(255, 255, 255, 0.3));
  --ui-tac-icon: var(--aui-muted, rgba(255, 255, 255, 0.5));
  --ui-tac-surface: var(--aui-surface, #1c1c1e);
  --ui-tac-wash: rgba(255, 255, 255, 0.07);
  --ui-tac-readonly: rgba(255, 255, 255, 0.03);
  --ui-tac-shadow: 0 0 0 0.5px var(--aui-border, rgba(255, 255, 255, 0.14)), 0 1px 2px 0 rgba(0, 0, 0, 0.4);
  --ui-tac-shadow-hover: 0 0 0 0.5px var(--aui-border-strong, rgba(255, 255, 255, 0.2)), 0 1px 2px 0 rgba(0, 0, 0, 0.4);
  --ui-tac-shadow-focus: 0 0 0 1px var(--aui-border-strong, rgba(255, 255, 255, 0.3)), 0 1px 2px 0 rgba(0, 0, 0, 0.4);
  --ui-tac-danger: var(--aui-danger, #ff8a7a);
  --ui-tac-danger-edge: color-mix(in srgb, var(--aui-red, #ff6b5f) 50%, transparent);
  --ui-tac-danger-edge-focus: rgba(255, 99, 105, 0.75);
  --ui-tac-focus: var(--aui-focus, rgba(77, 141, 255, 0.8));
}
:where(.dark, [data-theme="dark"]) .aui-text-area-clean[data-readonly] .aui-text-area-clean__box,
:where(.dark, [data-theme="dark"]) .aui-text-area-clean[data-disabled] .aui-text-area-clean__box { box-shadow: 0 0 0 0.5px rgba(255, 255, 255, 0.12); }

/* Kit: keep the component inside its slot and grow touch targets. */
.aui-text-area-clean { max-width: 100%; min-width: 0; }
@media (pointer: coarse), (max-width: 899px) {
  .aui-text-area-clean:where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]),
  .aui-text-area-clean :where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]) {
    min-width: 44px;
    min-height: 44px;
  }
}
`;
