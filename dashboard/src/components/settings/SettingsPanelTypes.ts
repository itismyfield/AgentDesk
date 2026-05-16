import type { CSSProperties, ReactNode } from "react";

import type { SettingRowMeta } from "./SettingsModel";

export type SettingsTr = (ko: string, en: string) => string;

export type RenderSettingRow = (
  meta: SettingRowMeta,
  options?: { controlOverlay?: ReactNode; trailingMeta?: ReactNode },
) => ReactNode;

export type RenderSettingGroupCard = (args: {
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  rows: ReactNode[];
  totalCount: number;
}) => ReactNode;

export interface SettingsActionStyles {
  primaryActionClass: string;
  primaryActionStyle: CSSProperties;
  secondaryActionClass: string;
  secondaryActionStyle: CSSProperties;
  subtleButtonClass: string;
  subtleButtonStyle: CSSProperties;
}
