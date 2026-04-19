import { useEffect, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";
import { Backdrop } from "./Backdrop";
import { useFocusTrap } from "./useFocusTrap";
import { useIsMobile } from "./useBreakpoint";
import { BottomSheet } from "./BottomSheet";

export interface DrawerProps {
  open: boolean;
  onClose: () => void;
  title?: string;
  children: ReactNode;
  side?: "right" | "left";
  width?: string;
  closeOnBackdrop?: boolean;
  closeOnEsc?: boolean;
  ariaLabel?: string;
}

export function Drawer({
  open,
  onClose,
  title,
  children,
  side = "right",
  width = "min(420px, 100vw)",
  closeOnBackdrop = true,
  closeOnEsc = true,
  ariaLabel,
}: DrawerProps) {
  const isMobile = useIsMobile();
  const containerRef = useFocusTrap(open && !isMobile);

  useEffect(() => {
    if (!open || !closeOnEsc || isMobile) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, closeOnEsc, onClose, isMobile]);

  useEffect(() => {
    if (!open || isMobile) return;
    const previous = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = previous;
    };
  }, [open, isMobile]);

  if (isMobile) {
    return (
      <BottomSheet
        open={open}
        onClose={onClose}
        title={title}
        closeOnBackdrop={closeOnBackdrop}
        closeOnEsc={closeOnEsc}
        ariaLabel={ariaLabel}
      >
        {children}
      </BottomSheet>
    );
  }

  if (!open || typeof document === "undefined") return null;

  return createPortal(
    <>
      <Backdrop onClick={closeOnBackdrop ? onClose : undefined} zIndex={70} />
      <div
        className={`fixed inset-y-0 z-[80] flex ${side === "right" ? "right-0" : "left-0"}`}
        role="dialog"
        aria-modal="true"
        aria-label={ariaLabel ?? title ?? "Drawer"}
      >
        <div
          ref={containerRef}
          tabIndex={-1}
          className={`flex h-full flex-col border-l bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] shadow-2xl outline-none ${side === "left" ? "border-l-0 border-r" : ""}`}
          style={{
            width,
            borderColor: "var(--th-border, rgba(148,163,184,0.18))",
          }}
          onClick={(event) => event.stopPropagation()}
        >
          {title && (
            <div
              className="flex items-center justify-between gap-3 border-b px-5 py-4"
              style={{ borderColor: "var(--th-border, rgba(148,163,184,0.12))" }}
            >
              <h2 className="text-base font-semibold">{title}</h2>
              <button
                type="button"
                onClick={onClose}
                className="flex h-8 w-8 items-center justify-center rounded-lg border text-[var(--th-text-muted,_#94a3b8)] hover:bg-white/5"
                style={{ borderColor: "var(--th-border, rgba(148,163,184,0.16))" }}
                aria-label="Close"
              >
                <X size={14} />
              </button>
            </div>
          )}
          <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
        </div>
      </div>
    </>,
    document.body,
  );
}
