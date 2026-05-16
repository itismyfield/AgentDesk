import { Drawer as VaulDrawer } from "vaul";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { useReturnFocus } from "./useReturnFocus";

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
  const accessibleTitle = title ?? ariaLabel ?? "Bottom sheet";
  const returnFocus = useReturnFocus(open);

  return (
    <VaulDrawer.Root
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) onClose();
      }}
      direction="bottom"
      modal
      // #2204 follow-up: keep `dismissible` always true so the header X
      // (wrapped in <VaulDrawer.Close>) and any other programmatic
      // VaulDrawer.Close still fire onOpenChange. Pre-fix we set
      // dismissible={closeOnBackdrop || closeOnEsc}, which collapses both
      // user gestures and the explicit close button when both flags were
      // false. ESC and outside-click guards live in onEscapeKeyDown /
      // onInteractOutside below.
      dismissible
    >
      <VaulDrawer.Portal>
        <VaulDrawer.Overlay className="fixed inset-0 z-[70] bg-black/45 backdrop-blur-sm" />
        <VaulDrawer.Content
          aria-label={ariaLabel}
          aria-describedby={undefined}
          onEscapeKeyDown={(event) => {
            if (!closeOnEsc) event.preventDefault();
          }}
          onInteractOutside={(event) => {
            if (!closeOnBackdrop) event.preventDefault();
          }}
          onCloseAutoFocus={returnFocus}
          className="fixed inset-x-0 bottom-0 z-[80] mx-auto flex max-h-[min(90vh,720px)] w-full max-w-2xl flex-col rounded-t-3xl border-t bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] shadow-2xl outline-none"
          style={{
            borderColor: "var(--th-border, rgba(148,163,184,0.18))",
            paddingBottom: "env(safe-area-inset-bottom)",
          }}
        >
          <div className="flex items-center justify-center pt-3">
            <VaulDrawer.Handle
              className="h-1 w-10 rounded-full bg-white/20"
              aria-hidden="true"
            />
          </div>
          {title ? (
            <div
              className="flex items-center justify-between gap-3 border-b px-5 py-3"
              style={{ borderColor: "var(--th-border, rgba(148,163,184,0.12))" }}
            >
              <VaulDrawer.Title className="text-base font-semibold">
                {title}
              </VaulDrawer.Title>
              <VaulDrawer.Close asChild>
                <button
                  type="button"
                  className="flex h-8 w-8 items-center justify-center rounded-lg border text-[var(--th-text-muted,_#94a3b8)] hover:bg-white/5"
                  style={{ borderColor: "var(--th-border, rgba(148,163,184,0.16))" }}
                  aria-label="Close"
                >
                  <X size={14} />
                </button>
              </VaulDrawer.Close>
            </div>
          ) : (
            <VaulDrawer.Title className="sr-only">
              {accessibleTitle}
            </VaulDrawer.Title>
          )}
          <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
        </VaulDrawer.Content>
      </VaulDrawer.Portal>
    </VaulDrawer.Root>
  );
}
