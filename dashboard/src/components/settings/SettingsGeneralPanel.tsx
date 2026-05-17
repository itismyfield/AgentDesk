import type { CSSProperties, FormEvent } from "react";

import {
  SurfaceCallout as SettingsCallout,
} from "../common/SurfacePrimitives";
import type {
  RenderSettingGroupCard,
  RenderSettingRow,
  SettingsTr,
} from "./SettingsPanelTypes";
import type { SettingRowMeta } from "./SettingsModel";

interface SettingsGeneralPanelProps {
  companyDirty: boolean;
  generalFormInvalid: boolean;
  generalMetas: SettingRowMeta[];
  onSave: (event?: FormEvent<HTMLFormElement>) => Promise<void>;
  primaryActionClass: string;
  primaryActionStyle: CSSProperties;
  renderSettingGroupCard: RenderSettingGroupCard;
  renderSettingRow: RenderSettingRow;
  saving: boolean;
  tr: SettingsTr;
}

export function SettingsGeneralPanel({
  companyDirty,
  generalFormInvalid,
  generalMetas,
  onSave,
  primaryActionClass,
  primaryActionStyle,
  renderSettingGroupCard,
  renderSettingRow,
  saving,
  tr,
}: SettingsGeneralPanelProps) {
  return (
    <form className="space-y-5" onSubmit={onSave} noValidate>
      {renderSettingGroupCard({
        titleKo: "일반",
        titleEn: "General",
        descriptionKo: "대시보드에 표시되는 회사 정보와 기본 화면 환경을 정합니다.",
        descriptionEn: "Set the company details and default display preferences shown across the dashboard.",
        totalCount: generalMetas.length,
        rows: generalMetas.map((meta) => renderSettingRow(meta)),
      })}

      <SettingsCallout
        action={(
          <button
            type="submit"
            disabled={saving || !companyDirty || generalFormInvalid}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            {saving ? tr("저장 중...", "Saving...") : tr("일반 설정 저장", "Save general settings")}
          </button>
        )}
      >
        <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "회사 이름은 필수입니다. 저장하면 입력값 앞뒤 공백을 정리한 뒤 대시보드와 오피스 화면에 함께 반영됩니다.",
            "Company name is required. Saved text is trimmed and applied across the dashboard and office views.",
          )}
        </p>
      </SettingsCallout>
    </form>
  );
}
