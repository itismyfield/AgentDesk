import { X } from "lucide-react";
import { PRIMARY_ROUTES } from "./routes";

interface AppShortcutHelpModalProps {
  isKo: boolean;
  modalZIndex: number;
  onClose: () => void;
}

export function AppShortcutHelpModal({
  isKo,
  modalZIndex,
  onClose,
}: AppShortcutHelpModalProps) {
  return (
    <div
      data-testid="shortcut-help-modal"
      className="fixed inset-0 flex items-center justify-center px-4"
      style={{ zIndex: modalZIndex }}
      onClick={onClose}
    >
      <div className="fixed inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        role="dialog"
        aria-modal="true"
        className="relative w-full max-w-md rounded-[2rem] border p-6 shadow-2xl"
        style={{
          borderColor: "var(--th-border-subtle)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
        }}
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-center justify-between">
          <div>
            <div
              className="text-lg font-semibold"
              style={{ color: "var(--th-text-heading)" }}
            >
              {isKo ? "키보드 단축키" : "Keyboard Shortcuts"}
            </div>
            <div
              className="mt-1 text-sm"
              style={{ color: "var(--th-text-muted)" }}
            >
              {isKo
                ? "자주 쓰는 조작을 빠르게 확인하세요"
                : "Quick access to the controls you use most"}
            </div>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="flex h-9 w-9 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
          >
            <X size={16} />
          </button>
        </div>

        <div className="mt-5 space-y-3 text-sm">
          <ShortcutRow
            label={isKo ? "명령 팔레트" : "Command palette"}
            combo="⌘K"
          />
          <ShortcutRow label={isKo ? "도움말" : "Help"} combo="?" />
          {PRIMARY_ROUTES.map((route) => (
            <ShortcutRow
              key={route.id}
              label={isKo ? route.labelKo : route.labelEn}
              combo={`Alt+${route.shortcutKey}`}
            />
          ))}
        </div>
      </div>
    </div>
  );
}

function ShortcutRow({ label, combo }: { label: string; combo: string }) {
  return (
    <div
      className="flex items-center justify-between rounded-2xl border px-3 py-2"
      style={{ borderColor: "var(--th-border-subtle)" }}
    >
      <span style={{ color: "var(--th-text-secondary)" }}>{label}</span>
      <kbd
        className="rounded-lg px-2 py-1 text-xs"
        style={{
          background: "var(--th-overlay-subtle)",
          color: "var(--th-text-primary)",
        }}
      >
        {combo}
      </kbd>
    </div>
  );
}
