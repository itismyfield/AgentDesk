import { useEffect, useId, useRef, useState } from "react";
import { MoreHorizontal } from "lucide-react";
import { Modal } from "./overlay/Modal";

export interface ActionMenuItem {
  key: string;
  label: string;
  onSelect: () => void | Promise<void>;
  danger?: boolean;
  disabled?: boolean;
  /** When set, selecting the item asks for confirmation first. */
  confirm?: { title: string; description?: string; confirmLabel: string; cancelLabel: string };
}

interface ActionMenuProps {
  /** Accessible name of the trigger, e.g. "에이전트 메뉴". */
  label: string;
  items: ActionMenuItem[];
  className?: string;
}

/** "⋯" trigger with a small menu; danger items sit here instead of beside primary buttons. */
export function ActionMenu({ label, items, className = "" }: ActionMenuProps) {
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState<ActionMenuItem | null>(null);
  const [busy, setBusy] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const menuId = useId();

  useEffect(() => {
    if (!open) return undefined;
    const close = (event: MouseEvent | KeyboardEvent) => {
      if (event instanceof KeyboardEvent ? event.key === "Escape" : !rootRef.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", close);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", close);
    };
  }, [open]);

  const run = async (item: ActionMenuItem) => {
    setBusy(true);
    try {
      await item.onSelect();
    } finally {
      setBusy(false);
      setPending(null);
    }
  };

  return (
    <div ref={rootRef} className={`relative inline-flex ${className}`.trim()} onClick={(event) => event.stopPropagation()}>
      <button
        type="button"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        onClick={() => setOpen((prev) => !prev)}
        className="flex h-11 w-11 items-center justify-center rounded-md border border-th-border-subtle text-th-text-muted transition-colors hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]"
      >
        <MoreHorizontal size={16} aria-hidden />
      </button>
      {open ? (
        <div
          id={menuId}
          role="menu"
          className="absolute top-12 right-0 z-30 min-w-40 overflow-hidden rounded-md border border-th-border-subtle bg-th-bg-surface py-1 shadow-lg"
        >
          {items.map((item) => (
            <button
              key={item.key}
              type="button"
              role="menuitem"
              disabled={item.disabled}
              onClick={() => {
                setOpen(false);
                if (item.confirm) setPending(item);
                else void run(item);
              }}
              className={`flex min-h-11 w-full items-center px-3 text-left text-sm hover:bg-surface-subtle disabled:cursor-not-allowed disabled:opacity-45 ${
                item.danger ? "text-[var(--th-accent-danger)]" : "text-th-text-primary"
              }`}
            >
              {item.label}
            </button>
          ))}
        </div>
      ) : null}
      {pending?.confirm ? (
        <Modal open onClose={() => setPending(null)} title={pending.confirm.title} description={pending.confirm.description} size="sm">
          <div className="flex justify-end gap-2 pt-2">
            <button
              type="button"
              onClick={() => setPending(null)}
              className="min-h-11 rounded-md border border-th-border-subtle px-4 text-sm text-th-text-secondary hover:bg-surface-subtle"
            >
              {pending.confirm.cancelLabel}
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => void run(pending)}
              className="min-h-11 rounded-md px-4 text-sm font-medium text-white disabled:opacity-60"
              style={{ background: pending.danger ? "var(--th-accent-danger)" : "var(--th-accent-primary)" }}
            >
              {pending.confirm.confirmLabel}
            </button>
          </div>
        </Modal>
      ) : null}
    </div>
  );
}
