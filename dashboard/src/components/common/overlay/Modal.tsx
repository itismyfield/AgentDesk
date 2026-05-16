import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { useReturnFocus } from "./useReturnFocus";

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
  const accessibleTitle = title ?? ariaLabel ?? "Modal";
  const returnFocus = useReturnFocus(open);

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) onClose();
      }}
      modal
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-[70] bg-black/45 backdrop-blur-sm" />
        <Dialog.Content
          aria-label={ariaLabel}
          aria-describedby={description ? undefined : undefined}
          onEscapeKeyDown={(event) => {
            if (!closeOnEsc) event.preventDefault();
          }}
          onInteractOutside={(event) => {
            if (!closeOnBackdrop) event.preventDefault();
          }}
          onCloseAutoFocus={returnFocus}
          className={`fixed left-1/2 top-1/2 z-[80] flex max-h-[85vh] w-[calc(100vw-2rem)] ${SIZE_CLASS[size]} -translate-x-1/2 -translate-y-1/2 flex-col rounded-2xl border bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] shadow-2xl outline-none`}
          style={{ borderColor: "var(--th-border, rgba(148,163,184,0.18))" }}
        >
          {(hideHeader || !title) && (
            <Dialog.Title className="sr-only">{accessibleTitle}</Dialog.Title>
          )}
          {!hideHeader && (title || description) && (
            <div
              className="flex items-start justify-between gap-3 border-b px-5 py-4"
              style={{ borderColor: "var(--th-border, rgba(148,163,184,0.12))" }}
            >
              <div className="min-w-0 flex-1">
                {title && (
                  <Dialog.Title className="text-base font-semibold">
                    {title}
                  </Dialog.Title>
                )}
                {description && (
                  <Dialog.Description className="mt-1 text-xs text-[var(--th-text-muted,_#94a3b8)]">
                    {description}
                  </Dialog.Description>
                )}
              </div>
              <Dialog.Close asChild>
                <button
                  type="button"
                  className="flex h-8 w-8 items-center justify-center rounded-lg border text-[var(--th-text-muted,_#94a3b8)] hover:bg-white/5"
                  style={{ borderColor: "var(--th-border, rgba(148,163,184,0.16))" }}
                  aria-label="Close"
                >
                  <X size={14} />
                </button>
              </Dialog.Close>
            </div>
          )}
          <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
