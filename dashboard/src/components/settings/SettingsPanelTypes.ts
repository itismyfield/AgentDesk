import type { ReactNode } from "react";

import type { SettingRowMeta, SettingsPanel } from "./SettingsModel";

export type SettingsTr = (ko: string, en: string) => string;

export type SourceStatus = "loading" | "error" | "ready";

/** What every tab body needs from the page: copy, rows, row rendering and search. */
export interface SettingsPanelContext {
  tr: SettingsTr;
  isKo: boolean;
  queryActive: boolean;
  matches: (...texts: string[]) => boolean;
  rowsFor: (panel: SettingsPanel, section: string) => SettingRowMeta[];
  renderRow: (meta: SettingRowMeta, options?: { extra?: ReactNode }) => ReactNode;
}
