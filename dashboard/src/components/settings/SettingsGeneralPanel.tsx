import type { SettingsPanelContext } from "./SettingsPanelTypes";
import { SettingsRowSection } from "./SettingsRowSection";

export function SettingsGeneralPanel({ ctx }: { ctx: SettingsPanelContext }) {
  return <SettingsRowSection ctx={ctx} panel="general" section="identity" />;
}
