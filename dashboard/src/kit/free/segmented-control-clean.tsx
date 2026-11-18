/*
 * Adapted from affectUI (https://github.com/flohoeller/affectui @ f83f008), MIT License, Copyright (c) 2026 Florian Höller (adfects). See LICENSE.affectui.
 * Changes: isolated class/property names, container queries, 44px touch targets,
 * scroll-safe touch selection, reduced-motion scrolling, and resilient measurement.
 */
"use client";

import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";

/*
 * Segmented Control (Clean) – switch between two to six options, in a single self-contained file.
 * Clean style: "buttons" (the default): separate quiet chips with a hairline, the active one filled light gray;
 * "tray": a compact light-gray track with a white raised pill; "underline": text tabs with a sliding line.
 * The indicator slides to the picked option, short and crisp – its leading edge runs a touch ahead.
 * Labels keep one weight, so nothing shifts; when a label or count changes the control morphs to its new size.
 * Arrow keys, Home and End move and select; press the pill and drag it across the options to pick as you go.
 * If it doesn't fit (phones), it scrolls sideways with soft edges and keeps the active option in view.
 * The styles are injected below, so no extra files are needed.
 */

export type SegmentOption<T extends string = string> = {
  value: T;
  label?: ReactNode;
  icon?: ReactNode;
  /** A small gray number after the label, e.g. unread items */
  count?: number | string;
  /** A small tinted tag after the label, e.g. "Save 20%" */
  badge?: ReactNode;
  disabled?: boolean;
  /** Accessible name – needed when the option only shows an icon */
  ariaLabel?: string;
  /** With kind="tabs": the id of the panel this tab shows */
  controls?: string;
};

export type SegmentedControlProps<T extends string = string> = {
  options: SegmentOption<T>[];
  /** Controlled value */
  value?: T;
  /** Starting value when uncontrolled – defaults to the first enabled option */
  defaultValue?: T;
  onChange?: (value: T) => void;
  size?: "sm" | "md" | "lg";
  /** "buttons" = separate chips, the active one filled; "tray" = light track with a raised pill; "underline" = text tabs with a sliding line */
  variant?: "buttons" | "tray" | "underline";
  /** Fill the container with equal segments */
  fullWidth?: boolean;
  /** "radio" (a radiogroup, the default) or "tabs" (a tablist that switches panels) */
  kind?: "radio" | "tabs";
  disabled?: boolean;
  "aria-label"?: string;
  "aria-labelledby"?: string;
  className?: string;
};

export function SegmentedControl<T extends string = string>({
  options,
  value,
  defaultValue,
  onChange,
  size = "md",
  variant = "buttons",
  fullWidth = false,
  kind = "radio",
  disabled = false,
  "aria-label": ariaLabel,
  "aria-labelledby": ariaLabelledby,
  className,
}: SegmentedControlProps<T>) {
  const [inner, setInner] = useState<T | undefined>(() => defaultValue ?? options.find((o) => !o.disabled)?.value);
  const current = value !== undefined ? value : inner;

  const root = useRef<HTMLDivElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const track = useRef<HTMLDivElement>(null);
  const pill = useRef<HTMLSpanElement>(null);
  // Latest value without waiting for a render – dragging picks several options in one frame
  const latest = useRef(current);
  latest.current = current;
  const gliding = useRef(false);
  const glideTimer = useRef(0);
  const placed = useRef<T | undefined>(undefined);
  const suppressClick = useRef(false);
  const press = useRef<{ id: number; touch: boolean; drag: boolean; x: number; moved: boolean } | null>(null);

  const buttons = () => Array.from(track.current?.querySelectorAll<HTMLElement>(":scope > .aui-segmented-control-clean__opt") ?? []);
  const activeIndex = options.findIndex((o) => o.value === current);
  const tabbable = activeIndex >= 0 && !options[activeIndex]?.disabled ? activeIndex : options.findIndex((o) => !o.disabled);

  const select = (v: T) => {
    const opt = options.find((o) => o.value === v);
    if (!opt || opt.disabled || disabled || v === latest.current) return;
    latest.current = v;
    if (value === undefined) setInner(v);
    onChange?.(v);
  };

  /** Buttons get the width of their content, so a changed label or count makes them – and the control – glide to the new size */
  const measure = () => {
    const els = buttons();
    if (!els.length || !track.current) return;
    const cs = getComputedStyle(els[0]);
    const pad = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
    const widths = els.map((b) => (b.firstElementChild as HTMLElement).offsetWidth + pad);
    if (fullWidth) {
      track.current.style.setProperty("--aui-segmented-control-clean-min", `${Math.max(...widths)}px`);
      els.forEach((b) => (b.style.width = ""));
    } else els.forEach((b, i) => (b.style.width = `${widths[i]}px`));
  };

  /** Moves the pill (or line) under the active option. Without `animate` it follows at once – unless a glide is still running */
  const place = (animate: boolean) => {
    const t = track.current;
    const p = pill.current;
    if (!t || !p) return;
    const btn = t.querySelector<HTMLElement>(":scope > .aui-segmented-control-clean__opt[data-active]");
    if (!btn || !btn.offsetWidth) {
      p.style.opacity = "0";
      return;
    }
    let x = btn.offsetLeft;
    let w = btn.offsetWidth;
    // The line sits under the text only, not under the whole button
    if (variant === "underline") {
      const content = btn.firstElementChild as HTMLElement;
      x += content.offsetLeft;
      w = content.offsetWidth;
    }
    const instant = !p.dataset.ready || (!animate && !gliding.current);
    const prev = parseFloat(p.style.left) || 0;
    // The edge facing the target leads, the other one trails – the stretch
    if (!instant && x !== prev) p.dataset.dir = x > prev ? "right" : "left";
    if (instant) p.style.transition = "none";
    p.style.opacity = "1";
    p.style.left = `${x}px`;
    p.style.right = `${t.clientWidth - x - w}px`;
    if (instant) {
      p.dataset.ready = "1";
      void p.offsetWidth;
      p.style.transition = "";
    }
  };

  /** Soft edges only on the sides that can scroll */
  const fades = () => {
    const s = scroller.current;
    if (!s) return;
    const max = s.scrollWidth - s.clientWidth;
    root.current?.toggleAttribute("data-overflow", max > 1);
    s.style.setProperty("--aui-segmented-control-clean-fade-l", s.scrollLeft > 1 ? "24px" : "0px");
    s.style.setProperty("--aui-segmented-control-clean-fade-r", s.scrollLeft < max - 1 ? "24px" : "0px");
  };

  /** Keeps the active option in view when the control scrolls */
  const reveal = (smooth: boolean) => {
    const s = scroller.current;
    const t = track.current;
    const btn = t?.querySelector<HTMLElement>(":scope > .aui-segmented-control-clean__opt[data-active]");
    if (!s || !t || !btn || s.scrollWidth <= s.clientWidth + 1) return;
    const left = t.offsetLeft + btn.offsetLeft - (s.clientWidth - btn.offsetWidth) / 2;
    s.scrollTo({ left: Math.max(0, left), behavior: smooth && !window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ? "smooth" : "auto" });
  };

  // Sizes: measured on mount and whenever a label, the container or the font changes
  useLayoutEffect(() => {
    const t = track.current;
    const s = scroller.current;
    if (!t || !s) return;
    const sync = () => {
      measure();
      place(false);
      fades();
    };
    sync();
    // Width transitions only after the first measure, so nothing grows in on load
    const ready = requestAnimationFrame(() => root.current?.setAttribute("data-ready", ""));
    // One frame later, so setting widths inside the observer never loops
    let frame = 0;
    const ro = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(sync);
    });
    ro?.observe(s);
    ro?.observe(t);
    t.querySelectorAll(".aui-segmented-control-clean__opt, .aui-segmented-control-clean__content").forEach((el) => ro?.observe(el));
    return () => {
      ro?.disconnect();
      cancelAnimationFrame(frame);
      cancelAnimationFrame(ready);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [options.length, fullWidth, variant, size]);

  // A new value: the pill glides there, focus follows if it was inside, and a scrolling control brings it into view
  useLayoutEffect(() => {
    if (placed.current === current) return;
    const first = placed.current === undefined;
    placed.current = current;
    if (!first) {
      gliding.current = true;
      clearTimeout(glideTimer.current);
      glideTimer.current = window.setTimeout(() => (gliding.current = false), 240);
    }
    place(!first);
    reveal(!first);
    const t = track.current;
    if (!first && t && t.contains(document.activeElement)) buttons()[activeIndex]?.focus({ preventScroll: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current]);

  useEffect(() => () => clearTimeout(glideTimer.current), []);

  /* ---------- Keyboard ---------- */

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const enabled = options.filter((o) => !o.disabled);
    if (!enabled.length || disabled) return;
    const prevKeys = kind === "radio" ? ["ArrowLeft", "ArrowUp"] : ["ArrowLeft"];
    const nextKeys = kind === "radio" ? ["ArrowRight", "ArrowDown"] : ["ArrowRight"];
    const at = enabled.findIndex((o) => o.value === latest.current);
    let target: SegmentOption<T> | undefined;
    if (prevKeys.includes(e.key)) target = enabled[(at - 1 + enabled.length) % enabled.length];
    else if (nextKeys.includes(e.key)) target = enabled[(at + 1) % enabled.length];
    else if (e.key === "Home") target = enabled[0];
    else if (e.key === "End") target = enabled[enabled.length - 1];
    if (!target) return;
    e.preventDefault();
    select(target.value);
    buttons()[options.indexOf(target)]?.focus();
  };

  /* ---------- Pointer: press feedback and dragging across options ---------- */

  /** The option under (or nearest to) a horizontal pointer position */
  const optionAt = (clientX: number) => {
    let best = -1;
    let dist = Infinity;
    buttons().forEach((b, i) => {
      const r = b.getBoundingClientRect();
      const d = clientX < r.left ? r.left - clientX : clientX > r.right ? clientX - r.right : 0;
      if (d < dist) {
        dist = d;
        best = i;
      }
    });
    return options[best];
  };

  const endPress = () => {
    press.current = null;
    root.current?.removeAttribute("data-pressed");
  };

  const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (disabled || e.button !== 0) return;
    const opt = optionAt(e.clientX);
    if (!opt || opt.disabled) return;
    const touch = e.pointerType === "touch";
    const s = scroller.current;
    const overflow = !!s && s.scrollWidth > s.clientWidth + 1;
    // Swipes scroll an overflowing control; tapping any option is the drag alternative.
    const drag = !touch || (!overflow && opt.value === latest.current);
    suppressClick.current = false;
    press.current = { id: e.pointerId, touch, drag, x: e.clientX, moved: false };
    if (drag) root.current?.setAttribute("data-pressed", "");
    if (!touch) {
      select(opt.value);
      try {
        e.currentTarget.setPointerCapture(e.pointerId);
      } catch {
        /* capture is a nicety */
      }
    }
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const p = press.current;
    if (!p || p.id !== e.pointerId) return;
    if (!p.moved && Math.abs(e.clientX - p.x) < (p.touch ? 8 : 3)) return;
    p.moved = true;
    suppressClick.current = true;
    if (!p.drag) return;
    const opt = optionAt(e.clientX);
    if (opt && !opt.disabled) select(opt.value);
  };

  const onPointerUp = (e: ReactPointerEvent<HTMLDivElement>) => {
    const p = press.current;
    if (!p || p.id !== e.pointerId) return;
    if (p.touch && !p.moved) {
      const opt = optionAt(e.clientX);
      if (opt) select(opt.value);
    }
    endPress();
  };

  const isTabs = kind === "tabs";

  return (
    <div className="aui-segmented-control-clean-container">
      <div
        ref={root}
        className={["aui-segmented-control-clean", className].filter(Boolean).join(" ")}
        data-size={size}
        data-variant={variant}
        data-full={fullWidth || undefined}
        data-disabled={disabled || undefined}
      >
        {/* React 19 hoists this into <head> once, no matter how many controls render */}
        <style href="aui-segmented-control-clean" precedence="default">
          {css}
        </style>

        <div ref={scroller} className="aui-segmented-control-clean__scroller" onScroll={fades}>
          <div
            ref={track}
            className="aui-segmented-control-clean__track"
            role={isTabs ? "tablist" : "radiogroup"}
            aria-label={ariaLabel}
            aria-labelledby={ariaLabelledby}
            aria-disabled={disabled || undefined}
            aria-orientation={isTabs ? "horizontal" : undefined}
            onKeyDown={onKeyDown}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onPointerCancel={endPress}
            onLostPointerCapture={endPress}
          >
            <span ref={pill} className="aui-segmented-control-clean__pill" aria-hidden="true" />
            {options.map((o, i) => {
              const active = i === activeIndex;
              return (
                <button
                  key={o.value}
                  type="button"
                  className="aui-segmented-control-clean__opt"
                  role={isTabs ? "tab" : "radio"}
                  aria-checked={isTabs ? undefined : active}
                  aria-selected={isTabs ? active : undefined}
                  aria-controls={isTabs ? o.controls : undefined}
                  aria-label={o.ariaLabel}
                  data-active={active || undefined}
                  disabled={o.disabled || disabled}
                  tabIndex={i === tabbable ? 0 : -1}
                  // Keyboard (Enter / Space) and screen readers – pointers are handled on the track
                  onClick={(event) => {
                    if (event.detail === 0 || !suppressClick.current) select(o.value);
                  }}
                >
                  <span className="aui-segmented-control-clean__content">
                    {o.icon && <span className="aui-segmented-control-clean__icon">{o.icon}</span>}
                    {/* One weight for both states – only the color changes, so nothing shifts */}
                    {o.label != null && <span className="aui-segmented-control-clean__label">{o.label}</span>}
                    {o.count != null && (
                      <span key={String(o.count)} className="aui-segmented-control-clean__count">
                        {o.count}
                      </span>
                    )}
                    {o.badge != null && <span className="aui-segmented-control-clean__badge">{o.badge}</span>}
                  </span>
                </button>
              );
            })}
          </div>
        </div>
      </div>
    </div>
  );
}


const css = /* css */ `
.aui-segmented-control-clean-container { container-type: inline-size; width: 100%; min-width: 0; }
@container (max-width: 420px) {
  .aui-segmented-control-clean[data-size] { --aui-segmented-control-clean-px: 12px; --aui-segmented-control-clean-fs: 14px; --aui-segmented-control-clean-gap: 12px; }
}

/* Animatable custom properties: the equal segment width and the soft edges glide too */
@property --aui-segmented-control-clean-min { syntax: "<length>"; inherits: true; initial-value: 0px; }
@property --aui-segmented-control-clean-fade-l { syntax: "<length>"; inherits: false; initial-value: 0px; }
@property --aui-segmented-control-clean-fade-r { syntax: "<length>"; inherits: false; initial-value: 0px; }

.aui-segmented-control-clean {
  --aui-segmented-control-clean-ink: var(--aui-fg, rgba(0, 0, 0, 0.88));
  --aui-segmented-control-clean-text: var(--aui-fg, rgba(0, 0, 0, 0.61));
  --aui-segmented-control-clean-hover-text: rgba(0, 0, 0, 0.75);
  --aui-segmented-control-clean-line: var(--aui-line, rgba(0, 0, 0, 0.06));
  --aui-segmented-control-clean-wash: rgba(0, 0, 0, 0.03);
  --aui-segmented-control-clean-fill: rgba(0, 0, 0, 0.06);
  --aui-segmented-control-clean-track: rgba(0, 0, 0, 0.04);
  --aui-segmented-control-clean-card: var(--aui-th-card, #ffffff);
  --aui-segmented-control-clean-card-shadow: 0 0 0 0.5px rgba(0, 0, 0, 0.06), 0 1px 1px -1px rgba(0, 0, 0, 0.1), 0 1px 2px 0 rgba(0, 0, 0, 0.05);
  --aui-segmented-control-clean-accent: var(--aui-accent, rgba(0, 96, 226, 0.765));
  --aui-segmented-control-clean-accent-soft: rgba(0, 96, 226, 0.08);
  --aui-segmented-control-clean-focus: var(--aui-th-focus, rgba(0, 96, 226, 0.35));
  --aui-segmented-control-clean-ease: cubic-bezier(0.25, 0.9, 0.3, 1);
  /* Size: md – 28px chips, 4px 10px */
  --aui-segmented-control-clean-h: 28px;
  --aui-segmented-control-clean-px: 10px;
  --aui-segmented-control-clean-fs: 13px;
  --aui-segmented-control-clean-lh: 20px;
  --aui-segmented-control-clean-icon: 16px;
  --aui-segmented-control-clean-gap: 8px;
  --aui-segmented-control-clean-pad: 2px;
  --aui-segmented-control-clean-ring: 3px;
  box-sizing: border-box;
  display: inline-flex;
  max-width: 100%;
  min-width: 0;
  vertical-align: middle;
  border-radius: 8px;
  font-family: "Inter", "InterVariable", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  font-size: var(--aui-segmented-control-clean-fs);
  font-weight: 500;
  line-height: var(--aui-segmented-control-clean-lh);
  color: var(--aui-segmented-control-clean-text);
  -webkit-font-smoothing: antialiased;
}
.aui-segmented-control-clean[data-size="sm"] { --aui-segmented-control-clean-h: 24px; --aui-segmented-control-clean-px: 8px; --aui-segmented-control-clean-fs: 12px; --aui-segmented-control-clean-lh: 16px; --aui-segmented-control-clean-icon: 14px; --aui-segmented-control-clean-gap: 6px; }
.aui-segmented-control-clean[data-size="lg"] { --aui-segmented-control-clean-h: 32px; --aui-segmented-control-clean-px: 12px; --aui-segmented-control-clean-icon: 16px; --aui-segmented-control-clean-gap: 8px; }
.aui-segmented-control-clean[data-full] { display: flex; width: 100%; }
.aui-segmented-control-clean[data-disabled] { opacity: 0.5; pointer-events: none; }
.aui-segmented-control-clean *, .aui-segmented-control-clean *::before, .aui-segmented-control-clean *::after { box-sizing: border-box; }
:where(.aui-segmented-control-clean) button { margin: 0; padding: 0; border: 0; background: none; font: inherit; color: inherit; cursor: pointer; }
:where(.aui-segmented-control-clean) svg { display: block; flex: none; }

/* Scrolls sideways only when it doesn't fit; the padding keeps focus rings from being cut off */
.aui-segmented-control-clean__scroller {
  position: relative;
  flex: 1;
  min-width: 0;
  overflow-x: auto;
  overflow-y: hidden;
  padding: var(--aui-segmented-control-clean-pad);
  border-radius: inherit;
  overscroll-behavior-x: contain;
  scrollbar-width: none;
  -webkit-mask-image: linear-gradient(to right, transparent, #000 var(--aui-segmented-control-clean-fade-l), #000 calc(100% - var(--aui-segmented-control-clean-fade-r)), transparent);
  mask-image: linear-gradient(to right, transparent, #000 var(--aui-segmented-control-clean-fade-l), #000 calc(100% - var(--aui-segmented-control-clean-fade-r)), transparent);
  transition: --aui-segmented-control-clean-fade-l 160ms ease, --aui-segmented-control-clean-fade-r 160ms ease;
}
.aui-segmented-control-clean__scroller::-webkit-scrollbar { display: none; }

.aui-segmented-control-clean__track {
  position: relative;
  display: flex;
  width: max-content;
  min-width: 100%;
  /* Horizontal drags move the indicator; vertical ones still scroll the page */
  touch-action: pan-y;
  -webkit-user-select: none;
  user-select: none;
  -webkit-tap-highlight-color: transparent;
}
/* When it scrolls, swipes scroll it instead of dragging the indicator */
.aui-segmented-control-clean[data-overflow] .aui-segmented-control-clean__track { touch-action: pan-x pan-y; }
/* Equal segments, each as wide as the widest one needs */
.aui-segmented-control-clean[data-full] .aui-segmented-control-clean__track { display: grid; grid-auto-flow: column; grid-auto-columns: minmax(var(--aui-segmented-control-clean-min), 1fr); }
.aui-segmented-control-clean[data-ready] .aui-segmented-control-clean__track { transition: --aui-segmented-control-clean-min 200ms var(--aui-segmented-control-clean-ease); }

/* An option */
.aui-segmented-control-clean__opt {
  position: relative;
  z-index: 1;
  display: flex;
  flex: none;
  align-items: center;
  justify-content: center;
  height: var(--aui-segmented-control-clean-h);
  padding: 0 var(--aui-segmented-control-clean-px);
  overflow: clip;
  border-radius: 6px;
  white-space: nowrap;
  word-break: keep-all;
  overflow-wrap: anywhere;
  touch-action: inherit;
  /* Hover washes are short and crisp, in and out */
  transition: color 90ms ease, background-color 90ms ease;
}
.aui-segmented-control-clean[data-ready]:not([data-full]) .aui-segmented-control-clean__opt { transition: color 90ms ease, background-color 90ms ease, width 200ms var(--aui-segmented-control-clean-ease); }
.aui-segmented-control-clean__opt[data-active] { color: var(--aui-segmented-control-clean-ink); }
.aui-segmented-control-clean__opt:disabled { opacity: 0.4; cursor: not-allowed; }
.aui-segmented-control-clean__opt:focus-visible { outline: 2px solid var(--aui-segmented-control-clean-focus); outline-offset: 1px; }
@media (hover: hover) {
  .aui-segmented-control-clean__opt:not([data-active]):not(:disabled):hover { background-color: var(--aui-segmented-control-clean-wash); }
}

.aui-segmented-control-clean__content { display: inline-flex; flex: none; align-items: center; gap: 6px; }
.aui-segmented-control-clean[data-size="sm"] .aui-segmented-control-clean__content { gap: 5px; }
.aui-segmented-control-clean__icon { display: grid; place-items: center; width: var(--aui-segmented-control-clean-icon); height: var(--aui-segmented-control-clean-icon); color: rgba(0, 0, 0, 0.447); transition: color 90ms ease; }
.aui-segmented-control-clean__opt[data-active] .aui-segmented-control-clean__icon { color: var(--aui-segmented-control-clean-ink); }
.aui-segmented-control-clean__icon svg { width: 100%; height: 100%; }
.aui-segmented-control-clean__label { display: block; }

.aui-segmented-control-clean__count {
  display: inline-grid;
  place-items: center;
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  border-radius: 5px;
  background: var(--aui-segmented-control-clean-fill);
  font-size: 11px;
  line-height: 16px;
  font-weight: 500;
  font-variant-numeric: tabular-nums;
  color: var(--aui-segmented-control-clean-text);
  animation: aui-segmented-control-clean-in 200ms var(--aui-segmented-control-clean-ease) both;
}
.aui-segmented-control-clean__opt[data-active] .aui-segmented-control-clean__count { background: var(--aui-segmented-control-clean-card); box-shadow: 0 0 0 0.5px var(--aui-segmented-control-clean-line); color: var(--aui-segmented-control-clean-ink); }
.aui-segmented-control-clean[data-size="sm"] .aui-segmented-control-clean__count { min-width: 14px; height: 14px; padding: 0 3px; border-radius: 4px; font-size: 10px; line-height: 14px; }
.aui-segmented-control-clean__badge {
  display: inline-flex;
  align-items: center;
  height: 18px;
  padding: 0 6px;
  border-radius: 5px;
  background: var(--aui-segmented-control-clean-accent-soft);
  font-size: 11px;
  line-height: 16px;
  font-weight: 500;
  color: var(--aui-segmented-control-clean-accent);
}
.aui-segmented-control-clean[data-size="sm"] .aui-segmented-control-clean__badge { height: 16px; padding: 0 5px; font-size: 10px; }

/* The indicator: left and right edges slide separately – the one facing the target leads a touch. Short and crisp */
.aui-segmented-control-clean__pill {
  position: absolute;
  top: 0;
  bottom: 0;
  left: 0;
  right: 100%;
  z-index: 0;
  border-radius: 6px;
  background: var(--aui-segmented-control-clean-fill);
  opacity: 0;
  pointer-events: none;
  transition: left 200ms var(--aui-segmented-control-clean-ease), right 200ms var(--aui-segmented-control-clean-ease), scale 120ms var(--aui-segmented-control-clean-ease), opacity 120ms ease;
}
.aui-segmented-control-clean__pill[data-dir="right"] { transition-duration: 220ms, 160ms, 120ms, 120ms; }
.aui-segmented-control-clean__pill[data-dir="left"] { transition-duration: 160ms, 220ms, 120ms, 120ms; }
/* Press feedback: the indicator gives a little while held or dragged */
.aui-segmented-control-clean[data-pressed]:not([data-variant="underline"]) .aui-segmented-control-clean__pill { scale: 0.97; }

/* Buttons (default): separate chips with a hairline; the light-gray fill slides to the active one.
   The control reaches a few px past its box so focus rings are never clipped, and still lines up with its neighbours */
.aui-segmented-control-clean[data-variant="buttons"] { --aui-segmented-control-clean-pad: var(--aui-segmented-control-clean-ring); }

.aui-segmented-control-clean[data-variant="buttons"] .aui-segmented-control-clean__track { gap: var(--aui-segmented-control-clean-gap); }
.aui-segmented-control-clean[data-variant="buttons"] .aui-segmented-control-clean__opt { box-shadow: inset 0 0 0 0.5px var(--aui-segmented-control-clean-line); }

/* Tray: a compact light-gray track with a white raised pill */
.aui-segmented-control-clean[data-variant="tray"] { background: var(--aui-segmented-control-clean-track); }
.aui-segmented-control-clean[data-variant="tray"] .aui-segmented-control-clean__opt { height: calc(var(--aui-segmented-control-clean-h) - var(--aui-segmented-control-clean-pad) * 2); }
.aui-segmented-control-clean[data-variant="tray"] .aui-segmented-control-clean__pill { background: var(--aui-segmented-control-clean-card); box-shadow: var(--aui-segmented-control-clean-card-shadow); }

/* Underline: text tabs on a hairline */
.aui-segmented-control-clean[data-variant="underline"] { --aui-segmented-control-clean-px: 2px; --aui-segmented-control-clean-gap: 20px; border-radius: 0; box-shadow: inset 0 -0.5px 0 var(--aui-segmented-control-clean-line); }
.aui-segmented-control-clean[data-variant="underline"] .aui-segmented-control-clean__scroller { padding: 0; }
.aui-segmented-control-clean[data-variant="underline"]:not([data-full]) .aui-segmented-control-clean__track { gap: var(--aui-segmented-control-clean-gap); }
.aui-segmented-control-clean[data-variant="underline"][data-full] { --aui-segmented-control-clean-px: 10px; }
.aui-segmented-control-clean[data-variant="underline"] .aui-segmented-control-clean__opt { height: calc(var(--aui-segmented-control-clean-h) + 6px); }
.aui-segmented-control-clean[data-variant="underline"] .aui-segmented-control-clean__opt:focus-visible { outline-offset: -2px; }
.aui-segmented-control-clean[data-variant="underline"] .aui-segmented-control-clean__pill { top: auto; height: 1.5px; border-radius: 1.5px 1.5px 0 0; background: var(--aui-segmented-control-clean-ink); }
@media (hover: hover) {
  .aui-segmented-control-clean[data-variant="underline"] .aui-segmented-control-clean__opt:not([data-active]):not(:disabled):hover { background-color: transparent; color: var(--aui-segmented-control-clean-hover-text); }
}

@keyframes aui-segmented-control-clean-in { from { opacity: 0; transform: translateY(2px); filter: blur(2px); } to { opacity: 1; transform: none; filter: none; } }

/* Touch screens: every option is a real 44px target, including icon-only options. */
@media (pointer: coarse) {
  .aui-segmented-control-clean[data-size] { --aui-segmented-control-clean-h: 44px; }
  .aui-segmented-control-clean .aui-segmented-control-clean__opt { min-width: 44px; min-height: 44px; }
  .aui-segmented-control-clean[data-variant="underline"] .aui-segmented-control-clean__opt { height: 44px; }
}
@media (prefers-reduced-motion: reduce) {
  .aui-segmented-control-clean *, .aui-segmented-control-clean *::before, .aui-segmented-control-clean *::after { animation: none !important; transition: none !important; }
}

/* Dark mode: follows a .dark class or data-theme="dark" on any ancestor, e.g. <html> */
:where(.dark, [data-theme="dark"]) .aui-segmented-control-clean {
  --aui-segmented-control-clean-ink: var(--aui-fg, rgba(255, 255, 255, 0.9));
  --aui-segmented-control-clean-text: var(--aui-fg, rgba(255, 255, 255, 0.62));
  --aui-segmented-control-clean-hover-text: rgba(255, 255, 255, 0.78);
  --aui-segmented-control-clean-line: var(--aui-line, rgba(255, 255, 255, 0.08));
  --aui-segmented-control-clean-wash: rgba(255, 255, 255, 0.04);
  --aui-segmented-control-clean-fill: rgba(255, 255, 255, 0.08);
  --aui-segmented-control-clean-track: rgba(255, 255, 255, 0.05);
  --aui-segmented-control-clean-card: var(--aui-th-card, #2c2c2f);
  --aui-segmented-control-clean-card-shadow: 0 0 0 0.5px rgba(255, 255, 255, 0.1), 0 1px 2px rgba(0, 0, 0, 0.4);
  --aui-segmented-control-clean-accent: var(--aui-accent, #4d8dff);
  --aui-segmented-control-clean-accent-soft: rgba(77, 141, 255, 0.14);
  --aui-segmented-control-clean-focus: var(--aui-th-focus, rgba(77, 141, 255, 0.45));
}
:where(.dark, [data-theme="dark"]) .aui-segmented-control-clean__icon { color: rgba(255, 255, 255, 0.46); }
:where(.dark, [data-theme="dark"]) .aui-segmented-control-clean__opt[data-active] .aui-segmented-control-clean__icon { color: var(--aui-segmented-control-clean-ink); }
`;
