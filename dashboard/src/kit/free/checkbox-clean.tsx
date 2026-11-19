"use client";
// Adapted from affectUI (https://github.com/flohoeller/affectui @ e610c3ba), MIT License, Copyright (c) 2026 Florian Höller (adfects). See LICENSE.affectui.
// Changes: aui- class prefix; coarse-pointer targets at least 44px; max-width containment for phone, tablet and desktop.

import { useEffect, useId, useRef, type ComponentPropsWithRef, type ReactNode } from "react";

/*
 * Checkbox – a single self-contained file, clean style.
 * A real checkbox with a label and a quiet second line, a third "some of them" state, an error, and a card variant
 * where the whole row is the target. The box fills and its check draws itself. Focus shows a ring only from the
 * keyboard. The styles are injected below, so no extra files are needed.
 */

export type CheckboxProps = Omit<ComponentPropsWithRef<"input">, "type" | "size"> & {
  /** Shown next to the box; clicking it toggles */
  label?: ReactNode;
  /** A quiet second line under the label */
  description?: ReactNode;
  /** "Some of them" – a dash instead of the check, e.g. for Select all */
  indeterminate?: boolean;
  /** Marks it invalid and shows this under it */
  error?: ReactNode;
  /** plain: the box and its text · card: a bordered row that is clickable as a whole and lights up when checked */
  variant?: "plain" | "card";
};

export function Checkbox({
  label,
  description,
  indeterminate = false,
  error,
  variant = "plain",
  className,
  id,
  disabled,
  ref,
  "aria-describedby": describedBy,
  ...rest
}: CheckboxProps) {
  const autoId = useId();
  const inputId = id ?? `${autoId}-input`;
  const descId = `${autoId}-desc`;
  const errorId = `${autoId}-error`;
  const input = useRef<HTMLInputElement | null>(null);
  const showsError = error != null && typeof error !== "boolean" && error !== "";

  // Indeterminate is a property, not an attribute
  useEffect(() => {
    if (input.current) input.current.indeterminate = indeterminate;
  }, [indeterminate]);

  return (
    <div
      className={["aui-checkbox-clean", className].filter(Boolean).join(" ")}
      data-variant={variant === "card" ? "card" : "plain"}
      data-invalid={error ? true : undefined}
      data-disabled={disabled || undefined}
    >
      {/* React 19 hoists this into <head> once, no matter how many checkboxes render */}
      <style href="ui-checkbox-clean" precedence="default">
        {css}
      </style>
      <label className="aui-checkbox-clean__row" htmlFor={inputId}>
        <span className="aui-checkbox-clean__control">
          <input
            {...rest}
            ref={(el) => {
              input.current = el;
              if (el) el.indeterminate = indeterminate;
              if (typeof ref === "function") ref(el);
              else if (ref) ref.current = el;
            }}
            id={inputId}
            type="checkbox"
            className="aui-checkbox-clean__input"
            disabled={disabled}
            aria-invalid={error ? true : undefined}
            aria-describedby={[describedBy, description ? descId : null, showsError ? errorId : null].filter(Boolean).join(" ") || undefined}
            data-indeterminate={indeterminate || undefined}
          />
          <span className="aui-checkbox-clean__box" aria-hidden="true">
            <svg className="aui-checkbox-clean__mark" viewBox="0 0 16 16" width="16" height="16">
              <path className="aui-checkbox-clean__tick" d="M4 8.5 6.75 11.25 12 5" pathLength={1} />
              <path className="aui-checkbox-clean__dash" d="M4.5 8h7" pathLength={1} />
            </svg>
          </span>
        </span>
        {(label != null || description != null) && (
          <span className="aui-checkbox-clean__text">
            {label != null && <span className="aui-checkbox-clean__label">{label}</span>}
            {description != null && (
              <span className="aui-checkbox-clean__desc" id={descId}>
                {description}
              </span>
            )}
          </span>
        )}
      </label>
      {showsError && (
        <p className="aui-checkbox-clean__error" id={errorId} aria-live="polite">
          {error}
        </p>
      )}
    </div>
  );
}

const css = /* css */ `
/* Colors and motion as variables – override them on .aui-checkbox-clean (see Theming in the docs) */
.aui-checkbox-clean {
  --ui-chkc-ink: var(--aui-ink, rgba(0, 0, 0, 0.88));
  --ui-chkc-subtle: var(--aui-muted, rgba(0, 0, 0, 0.45));
  --ui-chkc-surface: var(--aui-surface, #ffffff);
  --ui-chkc-edge: var(--aui-border-strong, rgba(0, 0, 0, 0.18));
  --ui-chkc-edge-hover: rgba(0, 0, 0, 0.32);
  --ui-chkc-fill: var(--aui-primary, rgba(0, 0, 0, 0.88));
  --ui-chkc-on-fill: var(--aui-on-primary, #ffffff);
  --ui-chkc-card-shadow: 0 0 0 0.5px var(--aui-border, rgba(0, 0, 0, 0.08)), 0 1px 1px -1px rgba(0, 0, 0, 0.1), 0 1px 2px 0 rgba(0, 0, 0, 0.05);
  --ui-chkc-card-shadow-hover: 0 0 0 0.5px rgba(0, 0, 0, 0.08), 0 4px 12px -6px rgba(0, 0, 0, 0.09), 0 1px 2px 0 rgba(0, 0, 0, 0.05);
  --ui-chkc-card-shadow-on: 0 0 0 0.5px rgba(0, 0, 0, 0.1), 0 1px 2px 0 rgba(0, 0, 0, 0.05);
  --ui-chkc-card-wash: rgba(0, 0, 0, 0.025);
  --ui-chkc-danger: var(--aui-danger, #c4320a);
  --ui-chkc-focus: var(--aui-focus, rgba(0, 96, 226, 0.7));
  --ui-chkc-ease: cubic-bezier(0.22, 1, 0.36, 1);

  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
  font-family: var(--aui-font, "Inter", "InterVariable", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif);
  font-size: 13px;
  line-height: 20px;
  color: var(--ui-chkc-ink);
  -webkit-font-smoothing: antialiased;
}

.aui-checkbox-clean__row { display: flex; align-items: flex-start; gap: 10px; min-width: 0; cursor: pointer; -webkit-tap-highlight-color: transparent; }
.aui-checkbox-clean[data-disabled] .aui-checkbox-clean__row { cursor: not-allowed; opacity: 0.5; }

/* The box sits on the first line of the label, centered on it */
.aui-checkbox-clean__control { position: relative; display: grid; flex: none; place-items: center; width: 16px; height: 20px; }
/* The real input covers the box (and a bit more, for touch) – invisible, but it takes the clicks and the focus */
.aui-checkbox-clean__input { position: absolute; inset: -6px; width: calc(100% + 12px); height: calc(100% + 12px); margin: 0; opacity: 0; cursor: inherit; }
.aui-checkbox-clean__box {
  display: grid;
  place-items: center;
  width: 16px;
  height: 16px;
  box-sizing: border-box;
  border-radius: 4.5px;
  background: var(--ui-chkc-surface);
  box-shadow: inset 0 0 0 1px var(--ui-chkc-edge), 0 1px 1px rgba(0, 0, 0, 0.04);
  color: var(--ui-chkc-on-fill);
  pointer-events: none;
  transition: background-color 160ms ease, box-shadow 160ms ease, transform 160ms var(--ui-chkc-ease);
}
.aui-checkbox-clean__row:hover .aui-checkbox-clean__input:not(:checked):not(:indeterminate):not(:disabled) + .aui-checkbox-clean__box { box-shadow: inset 0 0 0 1px var(--ui-chkc-edge-hover), 0 1px 1px rgba(0, 0, 0, 0.04); }
.aui-checkbox-clean__input:active:not(:disabled) + .aui-checkbox-clean__box { transform: scale(0.9); }
.aui-checkbox-clean__input:is(:checked, :indeterminate) + .aui-checkbox-clean__box { background: var(--ui-chkc-fill); box-shadow: none; }
.aui-checkbox-clean__input:focus-visible + .aui-checkbox-clean__box { outline: 2px solid var(--ui-chkc-focus); outline-offset: 2px; }
.aui-checkbox-clean[data-invalid] .aui-checkbox-clean__input:not(:checked) + .aui-checkbox-clean__box { box-shadow: inset 0 0 0 1px var(--ui-chkc-danger); }

/* The check draws itself; the dash grows from the middle */
.aui-checkbox-clean__mark { display: block; width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; stroke-linejoin: round; }
.aui-checkbox-clean__tick, .aui-checkbox-clean__dash { stroke-dasharray: 1; stroke-dashoffset: 1; }
.aui-checkbox-clean__input:checked:not(:indeterminate) + .aui-checkbox-clean__box .aui-checkbox-clean__tick { stroke-dashoffset: 0; transition: stroke-dashoffset 260ms 40ms var(--ui-chkc-ease); }
.aui-checkbox-clean__input:indeterminate + .aui-checkbox-clean__box .aui-checkbox-clean__dash { stroke-dashoffset: 0; transition: stroke-dashoffset 200ms var(--ui-chkc-ease); }

.aui-checkbox-clean__text { display: grid; min-width: 0; }
.aui-checkbox-clean__label { color: var(--ui-chkc-ink); overflow-wrap: anywhere; }
.aui-checkbox-clean__desc { font-size: 12px; line-height: 16px; color: var(--ui-chkc-subtle); overflow-wrap: anywhere; }
.aui-checkbox-clean__error { margin: 0 0 0 26px; font-size: 12px; line-height: 16px; color: var(--ui-chkc-danger); overflow-wrap: anywhere; animation: aui-checkbox-clean-message 260ms var(--ui-chkc-ease); }

/* Card: a bordered row, clickable as a whole – a soft shadow on hover; checked, it takes a faint gray fill (no dark edge) */
.aui-checkbox-clean[data-variant="card"] .aui-checkbox-clean__row {
  padding: 10px 12px;
  border-radius: 8px;
  background: var(--ui-chkc-surface);
  box-shadow: var(--ui-chkc-card-shadow);
  transition: box-shadow 200ms ease, background-color 200ms ease;
}
.aui-checkbox-clean[data-variant="card"]:not([data-disabled]) .aui-checkbox-clean__row:hover { box-shadow: var(--ui-chkc-card-shadow-hover); }
.aui-checkbox-clean[data-variant="card"] .aui-checkbox-clean__row:has(.aui-checkbox-clean__input:checked) { background: var(--ui-chkc-card-wash); box-shadow: var(--ui-chkc-card-shadow-on); }
.aui-checkbox-clean[data-variant="card"]:not([data-disabled]) .aui-checkbox-clean__row:has(.aui-checkbox-clean__input:checked):hover { box-shadow: var(--ui-chkc-card-shadow-hover); }
.aui-checkbox-clean[data-variant="card"] .aui-checkbox-clean__label { font-weight: 500; }
.aui-checkbox-clean[data-variant="card"] .aui-checkbox-clean__error { margin-left: 2px; }

/* Touch screens: 16px text and a bigger box */
@media (pointer: coarse) {
  .aui-checkbox-clean { font-size: 15px; line-height: 22px; }
  .aui-checkbox-clean__control { width: 20px; height: 22px; }
  .aui-checkbox-clean__box { width: 20px; height: 20px; border-radius: 5.5px; }
  .aui-checkbox-clean__mark { width: 18px; height: 18px; }
}

@keyframes aui-checkbox-clean-message {
  from { opacity: 0; transform: translateY(-3px); filter: blur(2px); }
}

@media (prefers-reduced-motion: reduce) {
  .aui-checkbox-clean *, .aui-checkbox-clean__row { transition-duration: 1ms !important; animation-duration: 1ms !important; }
}

/* Dark mode: follows a .dark class or data-theme="dark" on any ancestor, e.g. <html> */
:where(.dark, [data-theme="dark"]) .aui-checkbox-clean {
  --ui-chkc-ink: var(--aui-ink, rgba(255, 255, 255, 0.9));
  --ui-chkc-subtle: var(--aui-muted, rgba(255, 255, 255, 0.46));
  --ui-chkc-surface: var(--aui-surface, #1c1c1e);
  --ui-chkc-edge: var(--aui-border-strong, rgba(255, 255, 255, 0.22));
  --ui-chkc-edge-hover: rgba(255, 255, 255, 0.36);
  --ui-chkc-fill: var(--aui-primary, rgba(255, 255, 255, 0.9));
  --ui-chkc-on-fill: var(--aui-on-primary, #161618);
  --ui-chkc-card-shadow: 0 0 0 0.5px var(--aui-border, rgba(255, 255, 255, 0.14)), 0 1px 2px 0 rgba(0, 0, 0, 0.4);
  --ui-chkc-card-shadow-hover: 0 0 0 0.5px rgba(255, 255, 255, 0.16), 0 6px 14px -6px rgba(0, 0, 0, 0.45);
  --ui-chkc-card-shadow-on: 0 0 0 0.5px rgba(255, 255, 255, 0.18), 0 1px 2px 0 rgba(0, 0, 0, 0.4);
  --ui-chkc-card-wash: rgba(255, 255, 255, 0.045);
  --ui-chkc-danger: var(--aui-danger, #ff8a7a);
  --ui-chkc-focus: var(--aui-focus, rgba(77, 141, 255, 0.8));
}

/* Kit: keep the component inside its slot and grow touch targets. */
.aui-checkbox-clean { max-width: 100%; min-width: 0; }
@media (pointer: coarse) {
  .aui-checkbox-clean:where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]),
  .aui-checkbox-clean :where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]) {
    min-width: 44px;
    min-height: 44px;
  }
}
`;
