import { Check } from "lucide-react";
import { Button } from "../../kit/free/button-clean";

interface SettingsSaveBarProps {
  changeCount: number;
  blockedReason: string | null;
  saving: boolean;
  onDiscard: () => void;
  onSave: () => void;
  tr: (ko: string, en: string) => string;
}

/** The one save control for every tab; it only appears while something is unsaved. */
export function SettingsSaveBar({ changeCount, blockedReason, saving, onDiscard, onSave, tr }: SettingsSaveBarProps) {
  if (changeCount === 0 && !saving) return null;
  return (
    <div
      role="region"
      aria-label={tr("저장하지 않은 변경", "Unsaved changes")}
      data-testid="settings-save-bar"
      className="sticky bottom-0 z-20 mt-auto shrink-0 bg-[color-mix(in_oklch,var(--th-bg-primary)_90%,transparent)] px-4 py-3 shadow-[inset_0_0.5px_0_var(--line),0_-8px_24px_color-mix(in_oklch,black_10%,transparent)] backdrop-blur pb-[max(0.75rem,env(safe-area-inset-bottom))] md:px-8"
    >
      <div className="mx-auto flex max-w-[72rem] flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="min-w-0 flex-1 text-[13px] leading-5 break-keep" aria-live="polite">
          <span className="font-medium text-th-text-primary">
            {tr(`바뀐 설정 ${changeCount}개`, `${changeCount} unsaved ${changeCount === 1 ? "change" : "changes"}`)}
          </span>
          <span className={`block text-xs leading-5 sm:ml-2 sm:inline ${blockedReason ? "text-[var(--err)]" : "text-th-text-muted"}`}>
            {blockedReason ?? tr("저장해야 적용돼요.", "Save to apply.")}
          </span>
        </div>
        <div className="flex shrink-0 gap-2">
          <Button variant="secondary" onClick={onDiscard} disabled={saving}>
            {tr("되돌리기", "Discard")}
          </Button>
          <Button variant="primary" icon={<Check />} loading={saving} onClick={onSave} disabled={saving || Boolean(blockedReason)}>
            {saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}
          </Button>
        </div>
      </div>
    </div>
  );
}
