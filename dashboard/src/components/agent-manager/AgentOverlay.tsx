import { useEffect, useLayoutEffect, useRef, type BaseSyntheticEvent, type KeyboardEvent, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { AgentManagerStyles } from "./AgentManagerStyles";

const FOCUSABLE =
  'a[href],button:not(:disabled),input:not(:disabled),select:not(:disabled),textarea:not(:disabled),[tabindex]:not([tabindex="-1"])';

export type AgentOverlayVariant = "sheet" | "drawer";

interface AgentOverlayProps {
  /** sheet: a centred card that becomes a bottom sheet below 900px; drawer: a centred card that becomes a full-screen page below 900px. */
  variant: AgentOverlayVariant;
  /** Accessible name when no labelled heading is given. */
  ariaLabel?: string;
  ariaLabelledBy?: string;
  onClose: () => void;
  header: ReactNode;
  footer?: ReactNode;
  children: ReactNode;
  /** Max width of the centred card from 900px up. */
  widthClassName?: string;
  /** Element that gets the focus on open; defaults to the first control. */
  initialFocusRef?: RefObject<HTMLElement | null>;
  as?: "div" | "form";
  onSubmit?: (event?: BaseSyntheticEvent) => void;
  testId?: string;
  /** Body without padding, for content that draws its own columns. */
  flushBody?: boolean;
}

/**
 * Agent manager overlay shell: page behind goes inert, Tab stays inside, Esc and the backdrop close it,
 * and the focus returns to the opener. Safe-area insets are kept on phones.
 */
export function AgentOverlay({
  variant,
  ariaLabel,
  ariaLabelledBy,
  onClose,
  header,
  footer,
  children,
  widthClassName = "sm:max-w-3xl",
  initialFocusRef,
  as = "div",
  onSubmit,
  testId,
  flushBody = false,
}: AgentOverlayProps) {
  const isMobile = useIsMobile();
  const panelRef = useRef<HTMLElement | null>(null);
  const rootRef = useRef<HTMLDivElement | null>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  // Capture the opener before the panel takes the focus, give it back on close.
  const openerRef = useRef<HTMLElement | null>(null);
  if (openerRef.current === null && typeof document !== "undefined") {
    openerRef.current = document.activeElement as HTMLElement | null;
  }

  useLayoutEffect(() => {
    const root = rootRef.current;
    if (!root) return undefined;
    const restore: Array<[HTMLElement, boolean]> = [];
    for (const child of Array.from(document.body.children)) {
      if (child instanceof HTMLElement && child !== root && !child.contains(root)) {
        restore.push([child, child.inert]);
        child.inert = true;
      }
    }
    const target = initialFocusRef?.current
      ?? panelRef.current?.querySelector<HTMLElement>("[data-autofocus]")
      ?? panelRef.current?.querySelector<HTMLElement>(FOCUSABLE)
      ?? panelRef.current;
    target?.focus({ preventScroll: true });
    const opener = openerRef.current;
    return () => {
      restore.forEach(([element, inert]) => {
        element.inert = inert;
      });
      if (opener?.isConnected) window.setTimeout(() => opener.focus({ preventScroll: true }), 0);
    };
    // Mount-only: re-running would steal the focus back while the user works inside.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const previous = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = previous;
    };
  }, []);

  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    // Nested kit controls (select sheets, menus) handle and prevent their own keys first.
    if (event.defaultPrevented) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeRef.current();
      return;
    }
    if (event.key !== "Tab" || !panelRef.current) return;
    const items = Array.from(panelRef.current.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
      (node) => node.getClientRects().length > 0,
    );
    if (!items.length) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  const fullScreen = isMobile && variant === "drawer";
  const sheet = isMobile && variant === "sheet";
  const Panel = as;
  const panelClass = [
    "agent-kit-scope relative flex w-full min-w-0 flex-col overflow-hidden bg-(--bg-1) text-(--fg) outline-none [overflow-wrap:anywhere] [word-break:keep-all]",
    fullScreen
      ? "h-full"
      : sheet
        ? "rounded-t-(--r-xl) border-t border-(--line) shadow-(--shadow-md) motion-safe:animate-[agent-sheet-in_240ms_cubic-bezier(.23,1,.32,1)]"
        : `max-h-[min(90dvh,960px)] rounded-(--r-xl) border border-(--line) shadow-(--shadow-md) motion-safe:animate-[agent-card-in_200ms_cubic-bezier(.23,1,.32,1)] ${widthClassName}`,
  ].join(" ");

  return createPortal(
    <div
      ref={rootRef}
      data-testid={testId}
      className={`fixed inset-0 z-[80] flex ${
        fullScreen
          ? "items-stretch"
          : sheet
            ? "items-end justify-center"
            : "items-center justify-center p-6"
      } bg-[color-mix(in_oklch,black_45%,transparent)] motion-safe:animate-[agent-fade-in_160ms_ease-out]`}
      style={
        fullScreen
          ? { background: "var(--bg-1)" }
          : sheet
            ? { paddingTop: "calc(0.75rem + env(safe-area-inset-top))" }
            : undefined
      }
      onPointerDown={(event) => {
        if (event.target === event.currentTarget) closeRef.current();
      }}
    >
      <AgentManagerStyles />
      <Panel
        ref={panelRef as never}
        role="dialog"
        aria-modal="true"
        aria-label={ariaLabelledBy ? undefined : ariaLabel}
        aria-labelledby={ariaLabelledBy}
        tabIndex={-1}
        className={panelClass}
        onKeyDown={onKeyDown}
        onSubmit={onSubmit}
        style={
          fullScreen
            ? {
                paddingTop: "env(safe-area-inset-top)",
                paddingLeft: "env(safe-area-inset-left)",
                paddingRight: "env(safe-area-inset-right)",
              }
            : sheet
              ? { maxHeight: "100%", paddingLeft: "env(safe-area-inset-left)", paddingRight: "env(safe-area-inset-right)" }
              : undefined
        }
      >
        <div className="shrink-0 border-b border-(--line-soft) px-4 py-3 sm:px-5">{header}</div>
        <div
          className={`min-h-0 flex-1 overflow-y-auto overscroll-contain ${flushBody ? "" : "px-4 py-4 sm:px-5"}`}
          style={!footer && (fullScreen || sheet) ? { paddingBottom: "max(1rem, env(safe-area-inset-bottom))" } : undefined}
        >
          {children}
        </div>
        {footer ? (
          <div
            className="shrink-0 border-t border-(--line-soft) px-4 py-3 sm:px-5"
            style={fullScreen || sheet ? { paddingBottom: "max(0.75rem, env(safe-area-inset-bottom))" } : undefined}
          >
            {footer}
          </div>
        ) : null}
      </Panel>
    </div>,
    document.body,
  );
}
