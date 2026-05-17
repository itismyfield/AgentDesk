import type {
  RenderSettingGroupCard,
  RenderSettingRow,
  SettingsTr,
} from "./SettingsPanelTypes";
import type { SettingRowMeta } from "./SettingsModel";

interface SettingsOnboardingPanelProps {
  onboardingMetas: SettingRowMeta[];
  renderSettingGroupCard: RenderSettingGroupCard;
  renderSettingRow: RenderSettingRow;
  tr: SettingsTr;
}

export function SettingsOnboardingPanel({
  onboardingMetas,
  renderSettingGroupCard,
  renderSettingRow,
}: SettingsOnboardingPanelProps) {
  return (
    <div className="space-y-5">
      {renderSettingGroupCard({
        titleKo: "초기 연결",
        titleEn: "Initial setup",
        descriptionKo: "처음 연결한 봇, 서버, 소유자 상태를 확인하고 필요할 때 다시 설정합니다.",
        descriptionEn: "Review the bot, server, and owner connection set during initial setup.",
        totalCount: onboardingMetas.length,
        rows: onboardingMetas.map((meta) => renderSettingRow(meta)),
      })}
    </div>
  );
}
