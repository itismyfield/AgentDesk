import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
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
  const [alignLeft, setAlignLeft] = useState(false);
  const [pending, setPending] = useState<ActionMenuItem | null>(null);
  const [busy, setBusy] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();

  useLayoutEffect(() => {
    if (!open) return;
    const rect = triggerRef.current?.getBoundingClientRect();
    setAlignLeft(Boolean(rect && rect.width > 0 && rect.right < 176));
  }, [open]);

  useEffect(() => {
    if (!open) return undefined;
    menuRef.current?.querySelector<HTMLButtonElement>('[role="menuitem"]:not(:disabled)')?.focus({ preventScroll: true });
    const onPointerDown = (event: PointerEvent | MouseEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        setOpen(false);
        triggerRef.current?.focus();
      }
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", close, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", close, true);
    };
  }, [open]);

  const onMenuKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!open) return;
    if (event.key === "Tab") {
      setOpen(false);
      return;
    }
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    const enabled = Array.from(menuRef.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)') ?? []);
    if (enabled.length === 0) return;
    event.preventDefault();
    if (event.key === "Home") {
      enabled[0]?.focus({ preventScroll: true });
      return;
    }
    if (event.key === "End") {
      enabled[enabled.length - 1]?.focus({ preventScroll: true });
      return;
    }
    const current = enabled.indexOf(document.activeElement as HTMLButtonElement);
    const next =
      event.key === "ArrowDown"
        ? current < 0
          ? 0
          : (current + 1) % enabled.length
        : current <= 0
          ? enabled.length - 1
          : current - 1;
    enabled[next]?.focus({ preventScroll: true });
  };

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
    <div
      ref={rootRef}
      className={`relative inline-flex ${className}`.trim()}
      onClick={(event) => event.stopPropagation()}
      onKeyDown={onMenuKeyDown}
    >
      <button
        ref={triggerRef}
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
          ref={menuRef}
          role="menu"
          className={`absolute top-12 ${alignLeft ? "left-0" : "right-0"} z-30 min-w-40 max-w-[calc(100vw-1.5rem)] overflow-hidden rounded-md border border-th-border-subtle bg-th-bg-surface py-1 shadow-lg`}
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
              className="min-h-11 rounded-md px-4 text-sm font-medium disabled:opacity-60"
              style={{
                background: pending.danger ? "var(--err)" : "var(--accent)",
                color: pending.danger ? "#fff" : "var(--accent-ink)",
              }}
            >
              {pending.confirm.confirmLabel}
            </button>
          </div>
        </Modal>
      ) : null}
    </div>
  );
}
