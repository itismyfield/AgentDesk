"use client";
// Adapted from affectUI (https://github.com/flohoeller/affectui @ e610c3ba), MIT License, Copyright (c) 2026 Florian Höller (adfects). See LICENSE.affectui.
// Changes: aui- class prefix; coarse pointers keep the same track, thumb, travel and label size and only widen the
// invisible hit area to at least 44px around the track; max-width containment for phone, tablet and desktop.

import { useId, type ComponentPropsWithRef, type ReactNode } from "react";

/*
 * Switch – a single self-contained file, clean style.
 * On or off for something that takes effect right away: a real checkbox with role="switch", a label and a quiet
 * second line, the switch at the start or at the end of the row (settings rows put it on the right). The thumb
 * slides and stretches while it's pressed. Focus shows a ring only from the keyboard.
 * The styles are injected below, so no extra files are needed.
 */

export type SwitchProps = Omit<ComponentPropsWithRef<"input">, "type" | "size" | "role"> & {
  /** Shown next to the switch; clicking it toggles */
  label?: ReactNode;
  /** A quiet second line under the label */
  description?: ReactNode;
  /** start: the switch before the text · end: after it, across the row – the settings look */
  placement?: "start" | "end";
  /** sm 16 · md 20 high */
  size?: "sm" | "md";
};

export function Switch({ label, description, placement = "start", size = "md", className, id, disabled, ref, "aria-describedby": describedBy, ...rest }: SwitchProps) {
  const autoId = useId();
  const inputId = id ?? `${autoId}-input`;
  const descId = `${autoId}-desc`;

  return (
    <div
      className={["aui-switch-clean", className].filter(Boolean).join(" ")}
      data-placement={placement === "end" ? "end" : "start"}
      data-size={size === "sm" ? "sm" : "md"}
      data-disabled={disabled || undefined}
    >
      {/* React 19 hoists this into <head> once, no matter how many switches render */}
      <style href="ui-switch-clean" precedence="default">
        {css}
      </style>
      <label className="aui-switch-clean__row" htmlFor={inputId}>
        <span className="aui-switch-clean__control">
          <input
            {...rest}
            ref={ref}
            id={inputId}
            type="checkbox"
            role="switch"
            className="aui-switch-clean__input"
            disabled={disabled}
            aria-describedby={[describedBy, description ? descId : null].filter(Boolean).join(" ") || undefined}
          />
          <span className="aui-switch-clean__track" aria-hidden="true">
            <span className="aui-switch-clean__thumb" />
          </span>
        </span>
        {(label != null || description != null) && (
          <span className="aui-switch-clean__text">
            {label != null && <span className="aui-switch-clean__label">{label}</span>}
            {description != null && (
              <span className="aui-switch-clean__desc" id={descId}>
                {description}
              </span>
            )}
          </span>
        )}
      </label>
    </div>
  );
}

const css = /* css */ `
/* Colors and motion as variables – override them on .aui-switch-clean (see Theming in the docs) */
.aui-switch-clean {
  --ui-swc-ink: var(--aui-ink, rgba(0, 0, 0, 0.88));
  --ui-swc-subtle: var(--aui-muted, rgba(0, 0, 0, 0.45));
  --ui-swc-off: rgba(0, 0, 0, 0.12);
  --ui-swc-off-hover: rgba(0, 0, 0, 0.18);
  --ui-swc-on: var(--aui-primary, rgba(0, 0, 0, 0.88));
  --ui-swc-on-hover: rgba(0, 0, 0, 0.78);
  --ui-swc-thumb: #ffffff;
  --ui-swc-thumb-shadow: 0 1px 2px rgba(0, 0, 0, 0.18), 0 0 0 0.5px rgba(0, 0, 0, 0.04);
  --ui-swc-focus: var(--aui-focus, rgba(0, 96, 226, 0.7));
  --ui-swc-ease: cubic-bezier(0.22, 1, 0.36, 1);

  /* The track: --w × --h, the thumb --h minus 4px */
  --w: 32px;
  --h: 20px;

  min-width: 0;
  font-family: var(--aui-font, "Inter", "InterVariable", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif);
  font-size: 13px;
  line-height: 20px;
  color: var(--ui-swc-ink);
  -webkit-font-smoothing: antialiased;
}
.aui-switch-clean[data-size="sm"] { --w: 26px; --h: 16px; }

.aui-switch-clean__row { display: flex; align-items: flex-start; gap: 10px; min-width: 0; cursor: pointer; -webkit-tap-highlight-color: transparent; }
/* At the end: the text first, the switch on the far right */
.aui-switch-clean[data-placement="end"] .aui-switch-clean__row { flex-direction: row-reverse; justify-content: space-between; gap: 16px; }
.aui-switch-clean[data-disabled] .aui-switch-clean__row { cursor: not-allowed; opacity: 0.5; }

/* The switch sits on the first line of the label, centered on it */
.aui-switch-clean__control { position: relative; display: grid; flex: none; place-items: center; height: max(var(--h), 20px); }
.aui-switch-clean__input { position: absolute; inset: -6px; width: calc(100% + 12px); height: calc(100% + 12px); margin: 0; opacity: 0; cursor: inherit; }
.aui-switch-clean__track {
  position: relative;
  display: block;
  width: var(--w);
  height: var(--h);
  border-radius: 999px;
  background: var(--ui-swc-off);
  pointer-events: none;
  transition: background-color 200ms ease;
}
.aui-switch-clean__thumb {
  position: absolute;
  top: 2px;
  left: 2px;
  width: calc(var(--h) - 4px);
  height: calc(var(--h) - 4px);
  border-radius: 999px;
  background: var(--ui-swc-thumb);
  box-shadow: var(--ui-swc-thumb-shadow);
  transition: transform 260ms var(--ui-swc-ease), width 160ms var(--ui-swc-ease);
}
.aui-switch-clean__row:hover .aui-switch-clean__input:not(:disabled):not(:checked) + .aui-switch-clean__track { background: var(--ui-swc-off-hover); }
.aui-switch-clean__input:checked + .aui-switch-clean__track { background: var(--ui-swc-on); }
.aui-switch-clean__row:hover .aui-switch-clean__input:not(:disabled):checked + .aui-switch-clean__track { background: var(--ui-swc-on-hover); }
.aui-switch-clean__input:checked + .aui-switch-clean__track .aui-switch-clean__thumb { transform: translateX(calc(var(--w) - var(--h))); }
/* Pressed: the thumb stretches toward where it's going */
.aui-switch-clean__input:active:not(:disabled) + .aui-switch-clean__track .aui-switch-clean__thumb { width: calc(var(--h) + 1px); }
.aui-switch-clean__input:active:not(:disabled):checked + .aui-switch-clean__track .aui-switch-clean__thumb { transform: translateX(calc(var(--w) - var(--h) - 5px)); }
.aui-switch-clean__input:focus-visible + .aui-switch-clean__track { outline: 2px solid var(--ui-swc-focus); outline-offset: 2px; }

.aui-switch-clean__text { display: grid; flex: 1; min-width: 0; }
.aui-switch-clean__label { color: var(--ui-swc-ink); overflow-wrap: anywhere; }
.aui-switch-clean__desc { font-size: 12px; line-height: 16px; color: var(--ui-swc-subtle); overflow-wrap: anywhere; }

/* Touch screens: the track, thumb, travel and label stay as on desktop (13px text next to other fields);
   only the invisible input grows into a hit area of at least 44×44px centred on the track. */
@media (pointer: coarse) {
  .aui-switch-clean__input {
    inset: auto;
    top: min(-6px, calc(50% - 22px));
    left: min(-6px, calc(50% - 22px));
    width: max(calc(100% + 12px), 44px);
    height: max(calc(100% + 12px), 44px);
  }
}

@media (prefers-reduced-motion: reduce) {
  .aui-switch-clean__track, .aui-switch-clean__thumb { transition-duration: 1ms; }
}

/* Dark mode: follows a .dark class or data-theme="dark" on any ancestor, e.g. <html> */
:where(.dark, [data-theme="dark"]) .aui-switch-clean {
  --ui-swc-ink: var(--aui-ink, rgba(255, 255, 255, 0.9));
  --ui-swc-subtle: var(--aui-muted, rgba(255, 255, 255, 0.46));
  --ui-swc-off: rgba(255, 255, 255, 0.16);
  --ui-swc-off-hover: rgba(255, 255, 255, 0.22);
  --ui-swc-on: var(--aui-primary, rgba(255, 255, 255, 0.9));
  --ui-swc-on-hover: rgba(255, 255, 255, 0.8);
  --ui-swc-thumb: #ffffff;
  --ui-swc-focus: var(--aui-focus, rgba(77, 141, 255, 0.8));
}
:where(.dark, [data-theme="dark"]) .aui-switch-clean__input:checked + .aui-switch-clean__track .aui-switch-clean__thumb { background: #161618; box-shadow: none; }

/* Kit: keep the component inside its slot and grow touch targets. */
.aui-switch-clean { max-width: 100%; min-width: 0; }
@media (pointer: coarse) {
  .aui-switch-clean:where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]),
  .aui-switch-clean :where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]) {
    min-width: 44px;
    min-height: 44px;
  }
}
`;
