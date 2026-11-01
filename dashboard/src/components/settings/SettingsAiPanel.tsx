import { useState } from "react";
import { SettingsDisclosure, SettingsSection } from "./SettingsPanels";
import type { SettingsPanelContext, SourceStatus } from "./SettingsPanelTypes";
import { SettingsProvidersPanel } from "./SettingsProvidersPanel";
import { SettingsRowSection, sectionTitle } from "./SettingsRowSection";
import type { useProviderAccounts } from "./useSettingsRemoteData";

interface Props {
  ctx: SettingsPanelContext;
  accounts: ReturnType<typeof useProviderAccounts>;
  configStatus: SourceStatus;
  runtimeStatus: SourceStatus;
  warningPct: number;
  dangerPct: number;
}

const ACCOUNT_KEYWORDS = "계정 프로바이더 사용량 한도 로그인 account provider usage limit sign-in claude codex gemini qwen";

export function SettingsAiPanel({ ctx, accounts, configStatus, runtimeStatus, warningPct, dangerPct }: Props) {
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const accountsCopy = sectionTitle(ctx, "ai.accounts");
  const advancedCopy = sectionTitle(ctx, "ai.advanced");
  const advancedRows = ctx.rowsFor("ai", "advanced");
  const showAdvanced = runtimeStatus === "ready" && advancedRows.length > 0;

  return (
    <div className="space-y-5">
      {ctx.matches(accountsCopy.title, ACCOUNT_KEYWORDS) ? (
        <SettingsSection title={accountsCopy.title} description={accountsCopy.description} variant="open" testId="settings-section-ai-accounts">
          <SettingsProvidersPanel accounts={accounts} warningPct={warningPct} dangerPct={dangerPct} tr={ctx.tr} />
        </SettingsSection>
      ) : null}
      <SettingsRowSection ctx={ctx} panel="ai" section="usage" status={runtimeStatus} />
      <SettingsRowSection ctx={ctx} panel="ai" section="context" status={configStatus} />
      {showAdvanced ? (
        <SettingsDisclosure
          title={advancedCopy.title}
          description={advancedCopy.description}
          openLabel={ctx.tr("고급 설정 보기", "Show advanced")}
          closeLabel={ctx.tr("접기", "Hide")}
          open={advancedOpen || ctx.queryActive}
          onToggle={() => setAdvancedOpen((open) => !open)}
          testId="settings-section-ai-advanced"
        >
          {advancedRows.map((meta) => ctx.renderRow(meta))}
        </SettingsDisclosure>
      ) : null}
    </div>
  );
}
