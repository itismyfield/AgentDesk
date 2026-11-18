import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { useRef, type ReactNode } from "react";
import { getOverlayAccessibleTitle } from "./accessibility";
import { BottomSheet } from "./BottomSheet";
import { DrawerDockContext, useDrawerDock } from "./DrawerDock";
import { useIsMobile } from "./useBreakpoint";
import { useReturnFocus } from "./useReturnFocus";

export interface DrawerProps {
  open: boolean;
  onClose: () => void;
  /**
   * Visible heading. Prefer plain text; pass ariaLabel for complex titles
   * that cannot be reduced to readable text.
   */
  title?: ReactNode;
  children: ReactNode;
  side?: "right" | "left";
  width?: string;
  closeOnBackdrop?: boolean;
  closeOnEsc?: boolean;
  ariaLabel?: string;
  closeButtonClassName?: string;
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
  closeButtonClassName,
}: DrawerProps) {
  const isMobile = useIsMobile();
  const dock = useDrawerDock();
  const contentRef = useRef<HTMLDivElement>(null);
  const accessibleTitle = getOverlayAccessibleTitle(title, ariaLabel, "Drawer");
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
        closeButtonClassName={closeButtonClassName}
      >
        {children}
      </BottomSheet>
    );
  }

  const frame = (
    <>
      {title ? (
        <div
          className="flex items-center justify-between gap-3 border-b px-5 py-4"
          style={{ borderColor: "var(--th-border, rgba(148,163,184,0.12))" }}
        >
          <Dialog.Title className="min-w-0 text-base font-semibold [overflow-wrap:anywhere]">
            {title}
          </Dialog.Title>
          <Dialog.Close asChild>
            <button
              type="button"
              className={`flex h-11 w-11 shrink-0 items-center justify-center rounded-lg border text-[var(--th-text-muted,_#94a3b8)] hover:bg-white/5 ${closeButtonClassName ?? ""}`}
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
      <Dialog.Description className="sr-only">
        Drawer content
      </Dialog.Description>
      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
    </>
  );

  // Docked: a non-modal pane that fills the page's dock host, so the list beside it stays usable.
  if (dock) {
    if (!dock.host) return null;
    return (
      <Dialog.Root
        open={open}
        onOpenChange={(nextOpen) => {
          if (!nextOpen) onClose();
        }}
        modal={false}
      >
        <Dialog.Portal container={dock.host}>
          <Dialog.Content
            ref={contentRef}
            aria-label={accessibleTitle}
            data-docked=""
            onEscapeKeyDown={(event) => {
              // Escape closes the pane only from inside it; Escape in the list or a menu beside it is theirs.
              if (!closeOnEsc || !contentRef.current?.contains(event.target as Node)) event.preventDefault();
            }}
            onInteractOutside={(event) => event.preventDefault()}
            // Like a split view, opening keeps focus in the list; the pane follows it in tab order.
            onOpenAutoFocus={(event) => event.preventDefault()}
            onCloseAutoFocus={returnFocus}
            className="flex h-full min-h-0 w-full flex-col bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] outline-none"
          >
            {/* Drawers opened from inside the pane overlay as usual. */}
            <DrawerDockContext.Provider value={null}>{frame}</DrawerDockContext.Provider>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
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
        <Dialog.Overlay className="fixed inset-0 z-[70] bg-black/45" />
        <Dialog.Content
          aria-label={accessibleTitle}
          onEscapeKeyDown={(event) => {
            if (!closeOnEsc) event.preventDefault();
          }}
          onInteractOutside={(event) => {
            if (!closeOnBackdrop) event.preventDefault();
          }}
          onCloseAutoFocus={returnFocus}
          className={`fixed inset-y-0 z-[80] flex flex-col bg-[var(--th-card-bg,_#0f172a)] text-[var(--th-text,_#e2e8f0)] shadow-lg outline-none ${side === "right" ? "right-0 border-l" : "left-0 border-r"}`}
          style={{
            width,
            borderColor: "var(--th-border, rgba(148,163,184,0.18))",
          }}
        >
          {frame}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
