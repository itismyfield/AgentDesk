import type { ReactNode } from "react";
import { SECTION_COPY } from "./SettingsCopy";
import { SettingsSection, SettingsStatusMessage } from "./SettingsPanels";
import type { SettingRowMeta, SettingsPanel } from "./SettingsModel";
import type { SettingsPanelContext, SourceStatus } from "./SettingsPanelTypes";

interface SettingsRowSectionProps {
  ctx: SettingsPanelContext;
  panel: SettingsPanel;
  section: string;
  status?: SourceStatus;
  extraFor?: (meta: SettingRowMeta) => ReactNode;
}

/** A titled group of setting rows; hidden when a search leaves it empty. */
export function SettingsRowSection({ ctx, panel, section, status = "ready", extraFor }: SettingsRowSectionProps) {
  const copy = SECTION_COPY[`${panel}.${section}`];
  const rows = ctx.rowsFor(panel, section);
  if (status !== "ready" && ctx.queryActive) return null;
  if (status === "ready" && rows.length === 0) return null;
  return (
    <SettingsSection
      title={copy ? ctx.tr(copy.titleKo, copy.titleEn) : section}
      description={copy?.descriptionKo ? ctx.tr(copy.descriptionKo, copy.descriptionEn ?? "") : undefined}
      testId={`settings-section-${panel}-${section}`}
    >
      {status === "loading" ? (
        <SettingsStatusMessage>{ctx.tr("불러오는 중…", "Loading…")}</SettingsStatusMessage>
      ) : status === "error" ? (
        <SettingsStatusMessage tone="error">
          {ctx.tr("이 설정을 불러오지 못했어요. 페이지를 새로고침해 다시 확인해 주세요.", "These settings could not be loaded. Refresh the page to try again.")}
        </SettingsStatusMessage>
      ) : (
        rows.map((meta) => ctx.renderRow(meta, { extra: extraFor?.(meta) }))
      )}
    </SettingsSection>
  );
}

export function sectionTitle(ctx: SettingsPanelContext, id: string): { title: string; description?: string } {
  const copy = SECTION_COPY[id];
  return {
    title: ctx.tr(copy.titleKo, copy.titleEn),
    description: copy.descriptionKo ? ctx.tr(copy.descriptionKo, copy.descriptionEn ?? "") : undefined,
  };
}
