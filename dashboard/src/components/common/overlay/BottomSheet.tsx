import { useEffect, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";
import { Backdrop } from "./Backdrop";
import { useFocusTrap } from "./useFocusTrap";

export interface BottomSheetProps {
  open: boolean;
  onClose: () => void;
  title?: string;
  children: ReactNode;
  closeOnBackdrop?: boolean;
  closeOnEsc?: boolean;
  ariaLabel?: string;
}

export function BottomSheet({
  open,
  onClose,
  title,
  children,
  closeOnBackdrop = true,
  closeOnEsc = true,
  ariaLabel,
}: BottomSheetProps) {
  const containerRef = useFocusTrap(open);

  useEffect(() => {
    if (!open || !closeOnEsc) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, closeOnEsc, onClose]);

  useEffect(() => {
    if (!open) return;
    const previous = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = previous;
    };
  }, [open]);

  if (!open || typeof document === "undefined") return null;

  return createPortal(
    <>
      <Backdrop onClick={closeOnBackdrop ? onClose : undefined} zIndex={70} />
      <div
        className="fixed inset-x-0 bottom-0 z-[80] flex justify-center"
        role="dialog"
        aria-modal="true"
        aria-label={ariaLabel ?? title ?? "Bottom sheet"}
      >
        <div
          ref={containerRef}
          tabIndex={-1}
          className="flex w-full max-w-2xl flex-col rounded-t-3xl border-t bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] shadow-2xl outline-none"
          style={{
            borderColor: "var(--th-border, rgba(148,163,184,0.18))",
            paddingBottom: "env(safe-area-inset-bottom)",
            maxHeight: "min(90vh, 720px)",
          }}
          onClick={(event) => event.stopPropagation()}
        >
          <div className="flex items-center justify-center pt-3">
            <span className="h-1 w-10 rounded-full bg-white/20" aria-hidden="true" />
          </div>
          {title && (
            <div
              className="flex items-center justify-between gap-3 border-b px-5 py-3"
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
