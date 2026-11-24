// Kit provenance: itismyfield/agentdesk-ui-kit@b6d2ab1
// Adapted from affectUI (https://github.com/flohoeller/affectui @ f83f008), MIT License, Copyright (c) 2026 Florian Höller (adfects). See LICENSE.affectui.
// Changes: isolated slug styles; responsive label wrapping and 44px touch targets; complete reduced-motion support; AgentDesk demos.
"use client";


import { isValidElement, useEffect, useLayoutEffect, useRef, useState, type ComponentPropsWithRef, type ReactNode } from "react";

/*
 * Button – a single self-contained file, clean style.
 * Clean style: a dark chip, a white button with hairline shadows, a quiet ghost and a minimal red fill.
 * The styles are injected below, so no extra files are needed.
 */

export type ButtonProps = ComponentPropsWithRef<"button"> & {
  /** primary: a dark chip · secondary: white with a hairline shadow · ghost: text only · destructive: a light red fill, for deleting */
  variant?: "primary" | "secondary" | "ghost" | "destructive";
  size?: "sm" | "md" | "lg";
  /** Shows a shimmer over the label and blocks clicks */
  loading?: boolean;
  /** Icon before the label, e.g. an inline SVG */
  icon?: ReactNode;
  /** Icon after the label */
  iconRight?: ReactNode;
};

export function Button({
  variant = "primary",
  size = "md",
  loading = false,
  icon,
  iconRight,
  children,
  className,
  disabled,
  onClick,
  type = "button",
  ref,
  ...rest
}: ButtonProps) {
  // The shimmer copies the label, so it only works on plain text
  const text = typeof children === "string" ? children : undefined;
  const iconOnly = !children && Boolean(icon || iconRight);

  // Morph: when label or icon change, the width glides to its new size and the new content blurs in.
  // Nothing animates on the first render.
  const inner = useRef<HTMLButtonElement | null>(null);
  const lastWidth = useRef(0);
  const [ready, setReady] = useState(false);
  useEffect(() => setReady(true), []);
  const labelKey = text ?? "label";
  const iconKey = loading && icon ? "spinner" : contentKey(icon);
  const iconRightKey = contentKey(iconRight);

  const content = `${labelKey}|${iconKey}|${iconRightKey}`;
  const lastContent = useRef(content);

  // Runs after every render, so the remembered width is always current
  useLayoutEffect(() => {
    const el = inner.current;
    if (!el) return;
    const width = el.getBoundingClientRect().width;
    const from = lastWidth.current;
    const changed = lastContent.current !== content;
    lastWidth.current = width;
    lastContent.current = content;
    if (!changed || !from || Math.abs(from - width) < 0.5) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    el.animate([{ width: `${from}px` }, { width: `${width}px` }], {
      duration: 480,
      easing: "cubic-bezier(0.25, 1, 0.5, 1)",
    });
  });

  return (
    <>
      {/* React 19 hoists this into <head> once, no matter how many buttons render */}
      <style href="aui-button-clean" precedence="default">
        {css}
      </style>
      <button
        type={type}
        className={["aui-button-clean", className].filter(Boolean).join(" ")}
        data-variant={variant}
        data-size={size}
        data-loading={loading || undefined}
        data-icon-only={iconOnly || undefined}
        data-icon-left={(!iconOnly && Boolean(icon)) || undefined}
        data-icon-right={(!iconOnly && Boolean(iconRight)) || undefined}
        data-ready={ready || undefined}
        ref={(el) => {
          inner.current = el;
          if (typeof ref === "function") ref(el);
          else if (ref) ref.current = el;
        }}
        disabled={disabled}
        aria-busy={loading || undefined}
        aria-disabled={loading || undefined}
        onClick={(event) => {
          if (loading) {
            event.preventDefault();
            return;
          }
          onClick?.(event);
        }}
        {...rest}
      >
        {/* New keys remount the parts that changed, which replays their enter animation */}
        {icon && (
          <span className="aui-button-clean__icon" aria-hidden="true" key={`icon-${iconKey}`}>
            {loading ? <Spinner /> : icon}
          </span>
        )}
        {children != null && (
          <span className="aui-button-clean__label" data-text={text} key={`label-${labelKey}`}>
            {children}
          </span>
        )}
        {iconRight && (
          <span className="aui-button-clean__icon" aria-hidden="true" key={`icon-right-${iconRightKey}`}>
            {iconRight}
          </span>
        )}
      </button>
    </>
  );
}

/** Shown in place of the left icon while loading */
const Spinner = () => (
  <svg
    className="aui-button-clean__spinner"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth={2}
    strokeLinecap="round"
  >
    <circle cx="12" cy="12" r="8.5" opacity={0.2} />
    <path d="M12 3.5a8.5 8.5 0 0 1 8.5 8.5" />
  </svg>
);

/** A stable id per icon component, so swapping e.g. <CopyIcon /> for <CheckIcon /> counts as a change */
const ids = new WeakMap<object, number>();
let nextId = 1;
function contentKey(node: ReactNode) {
  if (!isValidElement(node) || typeof node.type === "string") return 0;
  const type = node.type as object;
  if (!ids.has(type)) ids.set(type, nextId++);
  return ids.get(type)!;
}

/* ---------- Styles ---------- */

const css = /* css */ `
@layer kit {
/* Colors and motion as variables – override them on .aui-button-clean (see Theming in the docs) */
.aui-button-clean {
  --aui-button-clean-title: #000;
  --aui-button-clean-ink: var(--aui-fg, rgba(0, 0, 0, 0.88));
  --aui-button-clean-muted: var(--aui-fg-muted, rgba(0, 0, 0, 0.61));
  --aui-button-clean-icon-color: rgba(0, 0, 0, 0.447);
  --aui-button-clean-solid: rgba(0, 0, 0, 0.88);
  --aui-button-clean-solid-hover: rgba(0, 0, 0, 0.8);
  --aui-button-clean-on-solid: #fcfcfc;
  --aui-button-clean-surface: var(--aui-bg-2, #ffffff);
  --aui-button-clean-shadow: 0 0 0 0.5px rgba(0, 0, 0, 0.06), 0 1px 1px -1px rgba(0, 0, 0, 0.1), 0 1px 2px 0 rgba(0, 0, 0, 0.05);
  --aui-button-clean-shadow-hover: 0 0 0 0.5px rgba(0, 0, 0, 0.1), 0 1px 1px -1px rgba(0, 0, 0, 0.12), 0 1px 3px 0 rgba(0, 0, 0, 0.07);
  --aui-button-clean-wash: rgba(0, 0, 0, 0.03);
  --aui-button-clean-danger: var(--aui-err, #e5484d);
  --aui-button-clean-danger-bg: rgba(229, 72, 77, 0.08);
  --aui-button-clean-danger-bg-hover: rgba(229, 72, 77, 0.12);
  --aui-button-clean-shine: rgba(255, 255, 255, 0.82);
  --aui-button-clean-shine-on-solid: #ffffff;
  --aui-button-clean-focus: var(--aui-th-focus, rgba(0, 96, 226, 0.35));
  --aui-button-clean-ease: cubic-bezier(0.22, 1, 0.36, 1);
  --aui-button-clean-load: 650ms;

  --aui-button-clean-h: 28px;
  --aui-button-clean-px: 10px;
  --aui-button-clean-gap: 6px;
  --aui-button-clean-fs: 13px;
  --aui-button-clean-lh: 20px;
  --aui-button-clean-icon: 16px;
  --aui-button-clean-radius: 6px;

  position: relative;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: var(--aui-button-clean-gap);
  box-sizing: border-box;
  height: var(--aui-button-clean-h);
  padding: 0 var(--aui-button-clean-px);
  margin: 0;
  border: 0;
  border-radius: var(--aui-button-clean-radius);
  font-family: "Inter", "InterVariable", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  font-size: var(--aui-button-clean-fs);
  font-weight: 500;
  line-height: var(--aui-button-clean-lh);
  letter-spacing: 0;
  white-space: nowrap;
  text-decoration: none;
  cursor: pointer;
  user-select: none;
  -webkit-tap-highlight-color: transparent;
  -webkit-font-smoothing: antialiased;
  transition:
    background-color 140ms ease,
    color 140ms ease,
    box-shadow 140ms ease,
    transform 160ms var(--aui-button-clean-ease),
    opacity 140ms ease;
}

/* Sizes: 24 · 28 · 32 high */
.aui-button-clean[data-size="sm"] {
  --aui-button-clean-h: 24px;
  --aui-button-clean-px: 8px;
  --aui-button-clean-gap: 4px;
  --aui-button-clean-fs: 12px;
  --aui-button-clean-lh: 16px;
  --aui-button-clean-icon: 14px;
  --aui-button-clean-radius: 6px;
}
.aui-button-clean[data-size="lg"] {
  --aui-button-clean-h: 32px;
  --aui-button-clean-px: 12px;
  --aui-button-clean-gap: 6px;
  --aui-button-clean-fs: 13px;
  --aui-button-clean-icon: 16px;
  --aui-button-clean-radius: 6px;
}
/* An icon sits closer to the edge than the text, so both look evenly spaced */
.aui-button-clean[data-icon-left] {
  padding-left: calc(var(--aui-button-clean-px) - 2px);
}
.aui-button-clean[data-icon-right] {
  padding-right: calc(var(--aui-button-clean-px) - 2px);
}
.aui-button-clean[data-icon-only] {
  width: var(--aui-button-clean-h);
  padding: 0;
}

/* primary: a dark chip */
.aui-button-clean[data-variant="primary"] {
  background: var(--aui-button-clean-solid);
  color: var(--aui-button-clean-on-solid);
}
.aui-button-clean[data-variant="primary"]:hover:not(:disabled) {
  background: var(--aui-button-clean-solid-hover);
}

/* secondary: white with the card's hairline shadow */
.aui-button-clean[data-variant="secondary"] {
  background: var(--aui-button-clean-surface);
  color: var(--aui-button-clean-title);
  box-shadow: var(--aui-button-clean-shadow);
}
.aui-button-clean[data-variant="secondary"] .aui-button-clean__icon {
  color: var(--aui-button-clean-icon-color);
}
.aui-button-clean[data-variant="secondary"]:hover:not(:disabled) {
  box-shadow: var(--aui-button-clean-shadow-hover);
}

/* ghost: a muted label, a soft wash appears on hover */
.aui-button-clean[data-variant="ghost"] {
  background: transparent;
  color: var(--aui-button-clean-muted);
}
.aui-button-clean[data-variant="ghost"] .aui-button-clean__icon {
  color: var(--aui-button-clean-icon-color);
}
.aui-button-clean[data-variant="ghost"]:hover:not(:disabled) {
  background: var(--aui-button-clean-wash);
  color: var(--aui-button-clean-ink);
}
.aui-button-clean[data-variant="ghost"]:hover:not(:disabled) .aui-button-clean__icon {
  color: var(--aui-button-clean-ink);
}

/* destructive: red label on a minimal red fill, no stroke */
.aui-button-clean[data-variant="destructive"] {
  background: var(--aui-button-clean-danger-bg);
  color: var(--aui-button-clean-danger);
}
.aui-button-clean[data-variant="destructive"]:hover:not(:disabled) {
  background: var(--aui-button-clean-danger-bg-hover);
}

.aui-button-clean .aui-button-clean__icon {
  transition: color 140ms ease;
}
.aui-button-clean:active:not(:disabled):not([data-loading]) {
  transform: scale(0.97);
}
.aui-button-clean:focus-visible {
  outline: 2px solid var(--aui-button-clean-focus);
  outline-offset: 1px;
}
.aui-button-clean:disabled {
  opacity: 0.4;
  cursor: default;
}
.aui-button-clean[data-loading] {
  cursor: default;
}

.aui-button-clean__icon {
  display: grid;
  place-items: center;
  flex: none;
  width: var(--aui-button-clean-icon);
  height: var(--aui-button-clean-icon);
}
.aui-button-clean__icon > svg {
  width: 100%;
  height: 100%;
}

/* Morph: the button clips while its width glides; changed parts blur in */
.aui-button-clean {
  overflow: clip;
}
.aui-button-clean[data-ready] .aui-button-clean__label {
  animation: aui-button-clean-label-in 480ms cubic-bezier(0.25, 1, 0.5, 1) both;
}
.aui-button-clean[data-ready] .aui-button-clean__icon {
  animation: aui-button-clean-icon-in 520ms cubic-bezier(0.25, 1, 0.5, 1) both;
}

/* Loading: a light band sweeps across a copy of the dimmed label */
.aui-button-clean__label {
  position: relative;
}
.aui-button-clean__label::after {
  content: attr(data-text);
  content: attr(data-text) / "";
  position: absolute;
  inset: 0;
  white-space: nowrap;
  color: transparent;
  background: linear-gradient(90deg, transparent 38%, var(--aui-button-clean-shine) 50%, transparent 62%) 100% 0 / 300% 100%
    no-repeat;
  -webkit-background-clip: text;
  background-clip: text;
  opacity: 0;
  pointer-events: none;
}
.aui-button-clean[data-variant="primary"] .aui-button-clean__label::after {
  --aui-button-clean-shine: var(--aui-button-clean-shine-on-solid);
}
.aui-button-clean[data-loading] .aui-button-clean__label {
  color: color-mix(in srgb, currentColor 50%, transparent);
}
.aui-button-clean[data-loading] .aui-button-clean__label::after {
  opacity: 1;
  animation: aui-button-clean-sweep calc(var(--aui-button-clean-load) * 1.6) linear infinite;
}
.aui-button-clean__spinner {
  animation: aui-button-clean-spin 900ms linear infinite;
}

@keyframes aui-button-clean-label-in {
  from {
    opacity: 0;
    filter: blur(3px);
    transform: translateY(25%);
  }
}
@keyframes aui-button-clean-icon-in {
  from {
    opacity: 0;
    filter: blur(2px);
    transform: scale(0.7);
  }
}

@keyframes aui-button-clean-spin {
  to {
    transform: rotate(1turn);
  }
}

@keyframes aui-button-clean-sweep {
  from {
    background-position: 100% 0;
  }
  to {
    background-position: 0% 0;
  }
}

@media (prefers-reduced-motion: reduce) {
  .aui-button-clean[data-ready] .aui-button-clean__label,
  .aui-button-clean[data-ready] .aui-button-clean__icon {
    animation: none;
  }
  .aui-button-clean {
    transition-duration: 1ms;
  }
  .aui-button-clean[data-loading] .aui-button-clean__label::after {
    animation: none;
    opacity: 0;
  }
}

/* Dark mode: follows a .dark class or data-theme="dark" on any ancestor, e.g. <html> */
:where(.dark, [data-theme="dark"]) .aui-button-clean {
  /* primary inverts: a light chip with dark text */
  --aui-button-clean-solid: rgba(255, 255, 255, 0.9);
  --aui-button-clean-solid-hover: rgba(255, 255, 255, 0.82);
  --aui-button-clean-on-solid: #161618;
  --aui-button-clean-shine-on-solid: #ffffff;

  --aui-button-clean-title: rgba(255, 255, 255, 0.9);
  --aui-button-clean-ink: var(--aui-fg, rgba(255, 255, 255, 0.9));
  --aui-button-clean-muted: var(--aui-fg-muted, rgba(255, 255, 255, 0.62));
  --aui-button-clean-icon-color: rgba(255, 255, 255, 0.5);
  --aui-button-clean-surface: var(--aui-bg-2, #1c1c1e);
  --aui-button-clean-shadow: 0 0 0 0.5px rgba(255, 255, 255, 0.1), 0 1px 2px 0 rgba(0, 0, 0, 0.4);
  --aui-button-clean-shadow-hover: 0 0 0 0.5px rgba(255, 255, 255, 0.16), 0 1px 2px 0 rgba(0, 0, 0, 0.4);
  --aui-button-clean-wash: rgba(255, 255, 255, 0.06);
  --aui-button-clean-danger: var(--aui-err, #ff6369);
  --aui-button-clean-danger-bg: rgba(229, 72, 77, 0.14);
  --aui-button-clean-danger-bg-hover: rgba(229, 72, 77, 0.2);
  --aui-button-clean-shine: #ffffff;
  --aui-button-clean-focus: var(--aui-th-focus, rgba(77, 141, 255, 0.5));
}

.aui-button-clean { box-sizing: border-box; max-width: 100%; min-width: 0; min-height: var(--aui-button-clean-h); height: auto; padding-block: 4px; touch-action: manipulation; word-break: keep-all; overflow-wrap: anywhere; }
.aui-button-clean__label { min-width: 0; white-space: normal; overflow-wrap: anywhere; }
.aui-button-clean[data-icon-only] { padding-block: 0; height: var(--aui-button-clean-h); }
@media (pointer: coarse) {
  .aui-button-clean[data-size] { min-width: 44px; min-height: 44px; --aui-button-clean-h: 44px; }
  .aui-button-clean[data-icon-only] { width: 44px; height: 44px; }
}
@media (prefers-reduced-motion: reduce) {
  .aui-button-clean, .aui-button-clean *, .aui-button-clean *::before, .aui-button-clean *::after { animation: none !important; transition: none !important; }
}
}
`;
