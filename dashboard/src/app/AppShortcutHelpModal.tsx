import { Modal } from "../components/common/overlay/Modal";
import { PRIMARY_ROUTES } from "./routes";

interface AppShortcutHelpModalProps {
  isKo: boolean;
  onClose: () => void;
}

export function AppShortcutHelpModal({ isKo, onClose }: AppShortcutHelpModalProps) {
  const rows = [
    { label: isKo ? "명령 팔레트" : "Command palette", combo: "⌘K" },
    { label: isKo ? "도움말" : "Help", combo: "?" },
    ...PRIMARY_ROUTES.map(route => ({
      label: isKo ? route.labelKo : route.labelEn,
      combo: `Alt+${route.shortcutKey}`,
    })),
  ];
  return (
    <Modal open onClose={onClose} title={isKo ? "키보드 단축키" : "Keyboard Shortcuts"} description={isKo ? "자주 쓰는 조작을 빠르게 확인하세요" : "Quick access to common controls"}>
      <div data-testid="shortcut-help-modal" className="divide-y divide-th-border-subtle">
        {rows.map(row => (
          <div key={row.combo} className="flex min-h-11 items-center justify-between gap-3 py-2 text-sm text-th-text-secondary">
            <span>{row.label}</span>
            <kbd className="rounded border border-th-border-subtle px-2 py-1 text-xs text-th-text-primary">{row.combo}</kbd>
          </div>
        ))}
      </div>
    </Modal>
  );
}
