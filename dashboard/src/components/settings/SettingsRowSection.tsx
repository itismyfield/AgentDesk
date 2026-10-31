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

/** A titled group of setting rows; hidden when a search leaves it empty or its source failed (the page shows one alert). */
export function SettingsRowSection({ ctx, panel, section, status = "ready", extraFor }: SettingsRowSectionProps) {
  const copy = SECTION_COPY[`${panel}.${section}`];
  const rows = ctx.rowsFor(panel, section);
  if (status === "error") return null;
  if (status === "loading" && ctx.queryActive) return null;
  if (status === "ready" && rows.length === 0) return null;
  return (
    <SettingsSection
      title={copy ? ctx.tr(copy.titleKo, copy.titleEn) : section}
      description={copy?.descriptionKo ? ctx.tr(copy.descriptionKo, copy.descriptionEn ?? "") : undefined}
      testId={`settings-section-${panel}-${section}`}
    >
      {status === "loading" ? (
        <SettingsStatusMessage>{ctx.tr("불러오는 중…", "Loading…")}</SettingsStatusMessage>
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
