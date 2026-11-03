import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

/** Trigger button plus a centered dialog that closes on outside click or Escape and returns focus.
 * It stays inside the caller's DOM so an enclosing modal's focus trap keeps working. */
export default function PickerPopover({
  id,
  triggerClassName,
  triggerLabel,
  dialogLabel,
  trigger,
  children,
}: {
  id?: string;
  triggerClassName: string;
  triggerLabel: string;
  dialogLabel: string;
  trigger: ReactNode;
  children: (close: () => void) => ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const close = useCallback(() => {
    setOpen(false);
    buttonRef.current?.focus();
  }, []);

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) close();
    };
    const keyHandler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close();
      }
    };
    document.addEventListener("mousedown", handler);
    document.addEventListener("keydown", keyHandler, true);
    return () => {
      document.removeEventListener("mousedown", handler);
      document.removeEventListener("keydown", keyHandler, true);
    };
  }, [open, close]);

  return (
    <div className="relative" ref={ref}>
      <button
        id={id}
        ref={buttonRef}
        type="button"
        onClick={() => setOpen(!open)}
        className={`${triggerClassName} rounded-lg border flex items-center justify-center transition-all hover:scale-105 hover:shadow-md`}
        style={{ background: "var(--th-input-bg)", borderColor: "var(--th-input-border)" }}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={triggerLabel}
      >
        {trigger}
      </button>
      {open && (
        <div
          role="dialog"
          aria-label={dialogLabel}
          className="fixed left-1/2 top-1/2 z-[60] -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface shadow-xl"
        >
          {children(close)}
        </div>
      )}
    </div>
  );
}
