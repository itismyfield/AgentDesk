import { Check } from "lucide-react";
import { SETTINGS_PRIMARY_BUTTON, SETTINGS_SECONDARY_BUTTON } from "./SettingsPanels";

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
      className="sticky bottom-0 z-20 -mx-4 mt-auto shrink-0 border-t px-4 py-3 backdrop-blur sm:-mx-8 sm:px-8"
      style={{
        borderColor: "var(--th-border)",
        background: "color-mix(in srgb, var(--th-bg-primary) 92%, transparent)",
      }}
    >
      <div className="mx-auto flex max-w-[88rem] flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="min-w-0 flex-1 text-sm break-keep" aria-live="polite">
          <span className="font-medium text-th-text-primary">
            {tr(`바뀐 설정 ${changeCount}개`, `${changeCount} unsaved ${changeCount === 1 ? "change" : "changes"}`)}
          </span>
          <span className="block text-xs leading-5 text-th-text-muted sm:ml-2 sm:inline">
            {blockedReason ?? tr("저장해야 적용돼요.", "Save to apply.")}
          </span>
        </div>
        <div className="flex shrink-0 gap-2">
          <button type="button" onClick={onDiscard} disabled={saving} className={SETTINGS_SECONDARY_BUTTON}>
            {tr("되돌리기", "Discard")}
          </button>
          <button type="button" onClick={onSave} disabled={saving || Boolean(blockedReason)} className={SETTINGS_PRIMARY_BUTTON}>
            <Check size={14} aria-hidden />
            {saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}
          </button>
        </div>
      </div>
    </div>
  );
}
