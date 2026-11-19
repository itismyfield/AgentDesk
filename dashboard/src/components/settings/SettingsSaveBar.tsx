import { Check } from "lucide-react";
import { Button } from "../../kit/free/button-clean";
import { formatSettingValue, type CatalogItem } from "./SettingsModel";

interface SettingsSaveBarProps {
  changes: CatalogItem[];
  blockedReason: string | null;
  saving: boolean;
  onDiscard: () => void;
  onSave: () => void;
  tr: (ko: string, en: string) => string;
}

/** The pending-changes bar under the list: what changes, then cancel or save. Hidden while nothing is unsaved. */
export function SettingsSaveBar({ changes, blockedReason, saving, onDiscard, onSave, tr }: SettingsSaveBarProps) {
  if (changes.length === 0 && !saving) return null;
  const first = changes[0];
  const more = changes.length > 1 ? tr(` 외 ${changes.length - 1}건`, ` and ${changes.length - 1} more`) : "";
  const summary = first
    ? `${first.label} ${formatSettingValue(first.saved, first, tr)} → ${formatSettingValue(first.draft ?? "", first, tr)}${more}`
    : "";
  return (
    <div
      role="region"
      aria-label={tr("저장하지 않은 변경", "Unsaved changes")}
      data-testid="settings-save-bar"
      className="sticky bottom-3 z-20 flex flex-wrap items-center gap-3 rounded-[var(--r-md)] bg-[var(--th-card-bg)] px-4 py-3 shadow-[0_0_0_1px_var(--accent),var(--shadow-sm,0_4px_16px_rgb(0_0_0/0.12))]"
    >
      <div className="min-w-0 flex-[1_1_16rem] text-[13px] leading-5" aria-live="polite">
        <div className="font-semibold text-th-text-primary">{tr(`변경 ${changes.length}건`, `${changes.length} ${changes.length === 1 ? "change" : "changes"}`)}</div>
        <div className={`break-keep [overflow-wrap:anywhere] ${blockedReason ? "text-[var(--err)]" : "text-th-text-muted"}`}>
          {blockedReason ?? tr(`${summary} · 저장하면 바로 적용돼요`, `${summary} · applies when saved`)}
        </div>
      </div>
      <div className="flex shrink-0 gap-2">
        <Button variant="secondary" onClick={onDiscard} disabled={saving}>
          {tr("취소", "Cancel")}
        </Button>
        <Button variant="primary" icon={<Check />} loading={saving} onClick={onSave} disabled={saving || Boolean(blockedReason)}>
          {saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}
        </Button>
      </div>
    </div>
  );
}
