import {
  SurfaceCard as SettingsCard,
} from "../common/SurfacePrimitives";
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
  tr,
}: SettingsOnboardingPanelProps) {
  return (
    <div className="space-y-5">
      {renderSettingGroupCard({
        titleKo: "온보딩",
        titleEn: "Onboarding",
        descriptionKo: "위저드 / /api/onboarding/* 가 관리하는 키. 일반 폼이 아니라 위저드를 사용하세요.",
        descriptionEn: "Wizard- and /api/onboarding/*-managed keys. Use the wizard instead of editing here.",
        totalCount: onboardingMetas.length,
        rows: onboardingMetas.map((meta) => renderSettingRow(meta)),
      })}

      <div className="grid gap-3 md:grid-cols-[minmax(0,1.15fr)_minmax(16rem,0.85fr)]">
        <SettingsCard
          className="rounded-3xl p-5"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
          }}
        >
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {tr("위저드가 처리하는 범위", "What the wizard covers")}
          </div>
          <div className="mt-4 space-y-3 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            <div>{tr("Discord 봇 토큰, guild/owner, provider 연결", "Discord bot token, guild/owner, and provider wiring")}</div>
            <div>{tr("기본 채널/카테고리와 role map 구성", "Default channels/categories and role-map setup")}</div>
            <div>{tr("기본 운영 파이프라인과 초기 설정 재생성", "Default operating pipeline and initial config regeneration")}</div>
          </div>
        </SettingsCard>

        <SettingsCard
          className="rounded-3xl p-5"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
          }}
        >
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {tr("권장 시점", "When to run it")}
          </div>
          <div className="mt-4 space-y-3 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            <div>{tr("새 워크스페이스를 처음 붙일 때", "When wiring a new workspace for the first time")}</div>
            <div>{tr("봇 토큰이나 owner/provider를 바꿨을 때", "When bot tokens or owner/provider settings changed")}</div>
            <div>{tr("기본 채널/정책을 다시 생성해야 할 때", "When default channels or policies need to be recreated")}</div>
          </div>
        </SettingsCard>
      </div>
    </div>
  );
}
