"use client";
// Adapted from affectUI (https://github.com/flohoeller/affectui @ e610c3ba), MIT License, Copyright (c) 2026 Florian Höller (adfects). See LICENSE.affectui.
// Changes: aui- class prefix; coarse-pointer targets at least 44px; max-width containment for phone, tablet and desktop.

import type { ComponentPropsWithRef, ReactNode } from "react";

/*
 * Separator – a single self-contained file, clean style.
 * A 0.5px hairline between groups: across a card or a list, or upright between tools in a bar. It can carry a short
 * label in its middle – "or" between two ways in, "Earlier" above older rows.
 * The styles are injected below, so no extra files are needed.
 */

export type SeparatorProps = Omit<ComponentPropsWithRef<"div">, "children"> & {
  /** horizontal: a line across, as wide as its container · vertical: an upright line, 16px high in a row */
  orientation?: "horizontal" | "vertical";
  /**
   * true (the default): only a visual line, hidden from screen readers · false: a real separator (role="separator")
   * that splits content into sections screen readers announce
   */
  decorative?: boolean;
  /** A short word in the middle of a horizontal line – "or", "Earlier", "Today" */
  label?: ReactNode;
};

export function Separator({ orientation = "horizontal", decorative = true, label, className, ...rest }: SeparatorProps) {
  const vertical = orientation === "vertical";
  const hasLabel = !vertical && label != null && label !== false && label !== "";
  // A labelled separator names itself after its label, when the label is text
  const name = hasLabel && (typeof label === "string" || typeof label === "number") ? String(label) : undefined;
  const a11y = decorative
    ? // A decorative line with a label: the label stays readable as plain text, only the lines are hidden
      hasLabel
      ? {}
      : { role: "none" as const }
    : {
        role: "separator" as const,
        "aria-orientation": vertical ? ("vertical" as const) : undefined,
        "aria-label": name,
      };

  return (
    <>
      {/* React 19 hoists this into <head> once, no matter how many separators render */}
      <style href="ui-separator-clean" precedence="default">
        {css}
      </style>
      <div
        className={["aui-separator-clean", className].filter(Boolean).join(" ")}
        data-orientation={vertical ? "vertical" : "horizontal"}
        data-label={hasLabel ? "" : undefined}
        {...a11y}
        {...rest}
      >
        {hasLabel && (
          <>
            <span className="aui-separator-clean__line" aria-hidden="true" />
            <span className="aui-separator-clean__label">{label}</span>
            <span className="aui-separator-clean__line" aria-hidden="true" />
          </>
        )}
      </div>
    </>
  );
}

/* ---------- Styles ---------- */

const css = /* css */ `
/* Colors as variables – override them on .aui-separator-clean (see Theming in the docs) */
.aui-separator-clean {
  --ui-sepc-line: var(--aui-border, rgba(0, 0, 0, 0.06));
  --ui-sepc-text: var(--aui-muted, rgba(0, 0, 0, 0.45));
  --ui-sepc-weight: 0.5px;
  flex: none;
  box-sizing: border-box;
}

/* A line across: a border, so it stays one crisp device pixel on any screen */
.aui-separator-clean[data-orientation="horizontal"]:not([data-label]) {
  width: 100%;
  height: 0;
  border-top: var(--ui-sepc-weight) solid var(--ui-sepc-line);
}

/* Upright between tools: 16px high, centred in its row – set height to change it */
.aui-separator-clean[data-orientation="vertical"] {
  display: inline-block;
  align-self: center;
  width: 0;
  vertical-align: middle;
  height: 16px;
  border-inline-start: var(--ui-sepc-weight) solid var(--ui-sepc-line);
}

/* With a label: line · word · line, the word in the middle */
.aui-separator-clean[data-label] {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  min-width: 0;
  font-family: var(--aui-font, "Inter", "InterVariable", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif);
  font-size: 12px;
  line-height: 16px;
  color: var(--ui-sepc-text);
  -webkit-font-smoothing: antialiased;
}
.aui-separator-clean__line {
  flex: 1 1 0;
  min-width: 12px;
  height: 0;
  border-top: var(--ui-sepc-weight) solid var(--ui-sepc-line);
}
/* A long label wraps inside the middle rather than pushing the lines out */
.aui-separator-clean__label {
  flex: 0 1 auto;
  min-width: 0;
  max-width: 80%;
  text-align: center;
  overflow-wrap: anywhere;
}

/* Windows high contrast: the line keeps a system color */
@media (forced-colors: active) {
  .aui-separator-clean { --ui-sepc-line: CanvasText; }
}

/* Dark mode: follows a .dark class or data-theme="dark" on any ancestor, e.g. <html> */
:where(.dark, [data-theme="dark"]) .aui-separator-clean {
  --ui-sepc-line: var(--aui-border, rgba(255, 255, 255, 0.08));
  --ui-sepc-text: var(--aui-muted, rgba(255, 255, 255, 0.45));
}

/* Kit: keep the component inside its slot and grow touch targets. */
.aui-separator-clean { max-width: 100%; min-width: 0; }
@media (pointer: coarse) {
  .aui-separator-clean:where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]),
  .aui-separator-clean :where(button, a[href], input, select, textarea, [role="button"], [role="tab"], [role="option"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"], [role="checkbox"], [role="radio"], [role="switch"], [role="slider"], [role="link"], [role="treeitem"]) {
    min-width: 44px;
    min-height: 44px;
  }
}
`;
