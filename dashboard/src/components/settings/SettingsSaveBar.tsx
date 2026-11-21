import { Desc, KitCard } from "../common/KitPrimitives";
import { formatSettingValue, type CatalogItem } from "./SettingsModel";
import { SettingsButton } from "./SettingsPrimitives";

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
    // card-clean's unlayered `position: relative` beats Tailwind `sticky`, so the wrapper owns placement.
    <div role="region" aria-label={tr("저장하지 않은 변경", "Unsaved changes")} data-testid="settings-save-bar" className="sticky bottom-3 z-20">
      <KitCard style={{ borderColor: "var(--accent)", boxShadow: "var(--shadow-sm)" }}>
        <div className="flex flex-wrap items-center gap-3 px-4 py-3">
          <div className="min-w-0 flex-[1_1_16rem]" aria-live="polite">
            <div className="text-sm leading-5 font-semibold text-[var(--fg)]">
              {tr(`변경 ${changes.length}건`, `${changes.length} ${changes.length === 1 ? "change" : "changes"}`)}
            </div>
            <Desc className={blockedReason ? "block break-keep [overflow-wrap:anywhere] text-[var(--err)]" : "block break-keep [overflow-wrap:anywhere]"}>
              {blockedReason ?? tr(`${summary} · 저장하면 바로 적용돼요`, `${summary} · applies when saved`)}
            </Desc>
          </div>
          <div className="flex shrink-0 gap-2">
            <SettingsButton variant="secondary" onClick={onDiscard} disabled={saving}>
              {tr("취소", "Cancel")}
            </SettingsButton>
            <SettingsButton variant="primary" loading={saving} onClick={onSave} disabled={saving || Boolean(blockedReason)}>
              {saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}
            </SettingsButton>
          </div>
        </div>
      </KitCard>
    </div>
  );
}
