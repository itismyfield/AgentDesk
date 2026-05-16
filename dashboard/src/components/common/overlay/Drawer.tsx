import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { BottomSheet } from "./BottomSheet";
import { useIsMobile } from "./useBreakpoint";
import { useReturnFocus } from "./useReturnFocus";

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
  const accessibleTitle = title ?? ariaLabel ?? "Drawer";
  const returnFocus = useReturnFocus(open);

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
          aria-describedby={undefined}
          onEscapeKeyDown={(event) => {
            if (!closeOnEsc) event.preventDefault();
          }}
          onInteractOutside={(event) => {
            if (!closeOnBackdrop) event.preventDefault();
          }}
          onCloseAutoFocus={returnFocus}
          className={`fixed inset-y-0 z-[80] flex flex-col bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] shadow-2xl outline-none ${side === "right" ? "right-0 border-l" : "left-0 border-r"}`}
          style={{
            width,
            borderColor: "var(--th-border, rgba(148,163,184,0.18))",
          }}
        >
          {title ? (
            <div
              className="flex items-center justify-between gap-3 border-b px-5 py-4"
              style={{ borderColor: "var(--th-border, rgba(148,163,184,0.12))" }}
            >
              <Dialog.Title className="text-base font-semibold">
                {title}
              </Dialog.Title>
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
          ) : (
            <Dialog.Title className="sr-only">{accessibleTitle}</Dialog.Title>
          )}
          <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
