import { useEffect, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";
import { Backdrop } from "./Backdrop";
import { useFocusTrap } from "./useFocusTrap";

export interface ModalProps {
  open: boolean;
  onClose: () => void;
  title?: string;
  description?: string;
  children: ReactNode;
  closeOnBackdrop?: boolean;
  closeOnEsc?: boolean;
  size?: "sm" | "md" | "lg" | "xl";
  hideHeader?: boolean;
  ariaLabel?: string;
}

const SIZE_CLASS: Record<NonNullable<ModalProps["size"]>, string> = {
  sm: "max-w-sm",
  md: "max-w-md",
  lg: "max-w-2xl",
  xl: "max-w-4xl",
};

export function Modal({
  open,
  onClose,
  title,
  description,
  children,
  closeOnBackdrop = true,
  closeOnEsc = true,
  size = "md",
  hideHeader = false,
  ariaLabel,
}: ModalProps) {
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
        className="fixed inset-0 z-[80] flex items-center justify-center p-4"
        role="dialog"
        aria-modal="true"
        aria-label={ariaLabel ?? title ?? "Modal"}
        aria-describedby={description ? "modal-desc" : undefined}
      >
        <div
          ref={containerRef}
          tabIndex={-1}
          className={`flex w-full ${SIZE_CLASS[size]} max-h-[85vh] flex-col rounded-2xl border bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] shadow-2xl outline-none`}
          style={{ borderColor: "var(--th-border, rgba(148,163,184,0.18))" }}
          onClick={(event) => event.stopPropagation()}
        >
          {!hideHeader && (title || description) && (
            <div
              className="flex items-start justify-between gap-3 border-b px-5 py-4"
              style={{ borderColor: "var(--th-border, rgba(148,163,184,0.12))" }}
            >
              <div className="min-w-0 flex-1">
                {title && <h2 className="text-base font-semibold">{title}</h2>}
                {description && (
                  <p id="modal-desc" className="mt-1 text-xs text-[var(--th-text-muted,_#94a3b8)]">
                    {description}
                  </p>
                )}
              </div>
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
