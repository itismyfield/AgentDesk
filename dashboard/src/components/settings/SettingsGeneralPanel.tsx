import type { CSSProperties, FormEvent } from "react";

import {
  SurfaceCallout as SettingsCallout,
  SurfaceSubsection as SettingsSubsection,
} from "../common/SurfacePrimitives";
import { StorageSurfaceCard } from "./SettingsPanels";
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
        descriptionKo: "회사 정보와 표시 환경, 메타 설정.",
        descriptionEn: "Company identity, display environment, and meta settings.",
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
            "일반 설정은 한 번에 저장되며 기존 `settings` JSON과 병합해 hidden key 손실을 막습니다. 회사 이름은 필수이고 텍스트 입력은 저장 시 trim 처리됩니다.",
            "General settings save together and merge into the existing `settings` JSON so hidden keys are preserved. Company name is required, and text inputs are trimmed on save.",
          )}
        </p>
      </SettingsCallout>

      <SettingsSubsection
        title={tr("저장 경로", "Storage surfaces")}
        description={tr(
          "이 화면의 값이 어디에 저장되는지 먼저 보여줍니다. 저장면을 숨기면 운영자가 설정의 실제 영향 범위를 오해하게 됩니다.",
          "Show where each setting is persisted. Hiding storage surfaces makes the UI misleading for operators.",
        )}
      >
        <div className="grid gap-3 md:grid-cols-2 2xl:grid-cols-4">
          <StorageSurfaceCard
            title={tr("회사 설정 JSON", "Company settings JSON")}
            body={tr(
              "`/api/settings`가 `kv_meta['settings']` 전체 JSON을 저장합니다. 부분 patch가 아니라 full replace라서 merged save가 필요합니다.",
              "`/api/settings` stores the full `kv_meta['settings']` JSON. It is a full replace API, so the UI must send a merged save.",
            )}
            footer={tr("source: kv_meta['settings']", "source: kv_meta['settings']")}
          />
          <StorageSurfaceCard
            title={tr("런타임 설정", "Runtime config")}
            body={tr(
              "폴링 주기와 cache TTL 같은 값은 `kv_meta['runtime-config']`에 저장되고 재시작 없이 반영됩니다.",
              "Polling intervals and cache TTL values live in `kv_meta['runtime-config']` and apply without restart.",
            )}
            footer={tr("source: kv_meta['runtime-config']", "source: kv_meta['runtime-config']")}
          />
          <StorageSurfaceCard
            title={tr("정책/파이프라인 키", "Policy and pipeline keys")}
            body={tr(
              "리뷰, 타임아웃, context compact 같은 값은 개별 `kv_meta` 키로 저장되고 `/api/settings/config` whitelist를 통해 노출됩니다.",
              "Review, timeout, and context-compaction values are stored as individual `kv_meta` keys and exposed through `/api/settings/config`.",
            )}
            footer={tr("source: individual kv_meta keys", "source: individual kv_meta keys")}
          />
          <StorageSurfaceCard
            title={tr("온보딩/시크릿", "Onboarding and secrets")}
            body={tr(
              "봇 토큰과 guild/owner/provider 설정은 일반 폼이 아니라 전용 온보딩 API와 위저드가 관리합니다.",
              "Bot tokens and guild/owner/provider wiring are managed by the dedicated onboarding API and wizard rather than the general form.",
            )}
            footer={tr("source: onboarding API + kv_meta", "source: onboarding API + kv_meta")}
          />
        </div>
      </SettingsSubsection>
    </form>
  );
}
