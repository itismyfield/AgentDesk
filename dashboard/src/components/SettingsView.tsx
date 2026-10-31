import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import type { CompanySettings } from "../types";
import { STORAGE_KEYS } from "../lib/storageKeys";
import { writeLocalStorageValue } from "../lib/useLocalStorage";
import { SettingsAiPanel } from "./settings/SettingsAiPanel";
import { connectionStatusRows, connectorsNeedingSetup } from "./settings/SettingsConnectionsModel";
import { SettingsConnectionsPanel } from "./settings/SettingsConnectionsPanel";
import { RUNTIME_FIELDS } from "./settings/SettingsCopy";
import { SettingsGeneralPanel } from "./settings/SettingsGeneralPanel";
import { SettingsMachinePanel } from "./settings/SettingsMachinePanel";
import { machinesNeedingAttention } from "./settings/SettingsMachineModel";
import { useSettingsMetaCatalog } from "./settings/SettingsMetaCatalog";
import {
  SETTINGS_PANEL_QUERY_KEY,
  readSettingsPanelFromUrl,
  readStoredSettingsPanel,
  type PendingDangerousConfigSave,
  type SettingRowMeta,
  type SettingsNotificationType,
  type SettingsPanel,
} from "./settings/SettingsModel";
import { SettingRow } from "./settings/SettingsPanels";
import type { SettingsPanelContext, SourceStatus } from "./settings/SettingsPanelTypes";
import { SettingsSaveBar } from "./settings/SettingsSaveBar";
import { SettingsViewLayout } from "./settings/SettingsViewLayout";
import { SettingsWorkflowPanel } from "./settings/SettingsWorkflowPanel";
import { getDangerousConfigKeys } from "./settings/settingsDangerousConfig";
import { useMachineNodes } from "./settings/useMachineNodes";
import { useConfigEntriesDraft, useGeneralDraft, useRuntimeConfigDraft, type GeneralField } from "./settings/useSettingsDrafts";
import { useConnectionStatus, useOperatorConnectors, useProviderAccounts } from "./settings/useSettingsRemoteData";

interface SettingsViewProps {
  settings: CompanySettings;
  onSave: (patch: Record<string, unknown>) => Promise<void>;
  isKo: boolean;
  onNotify?: (message: string, type?: SettingsNotificationType) => string | void;
}

const GENERAL_FIELDS = new Set<string>(["companyName", "ceoName", "language", "theme"]);
const RUNTIME_KEYS = new Set(RUNTIME_FIELDS.map((field) => field.key));

function sourceStatus(loaded: boolean, error: boolean): SourceStatus {
  return error ? "error" : loaded ? "ready" : "loading";
}

function usePanelRoute() {
  const [activePanel, setActivePanel] = useState<SettingsPanel>(() => readStoredSettingsPanel());

  useEffect(() => {
    writeLocalStorageValue(STORAGE_KEYS.settingsPanel, activePanel);
    // Also rewrites legacy ids such as ?settingsPanel=runtime to the tab they now open.
    if (new URLSearchParams(window.location.search).get(SETTINGS_PANEL_QUERY_KEY) !== activePanel) {
      const url = new URL(window.location.href);
      url.searchParams.set(SETTINGS_PANEL_QUERY_KEY, activePanel);
      window.history.replaceState(window.history.state, "", url);
    }
  }, [activePanel]);

  useEffect(() => {
    const handlePopState = () => {
      const panelFromUrl = readSettingsPanelFromUrl();
      if (panelFromUrl) setActivePanel(panelFromUrl);
    };
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  const changePanel = useCallback((panel: SettingsPanel) => {
    setActivePanel((current) => {
      if (current !== panel) {
        const url = new URL(window.location.href);
        url.searchParams.set(SETTINGS_PANEL_QUERY_KEY, panel);
        window.history.pushState(window.history.state, "", url);
      }
      return panel;
    });
  }, []);

  return [activePanel, changePanel] as const;
}

export default function SettingsView({ settings, onSave, isKo, onNotify }: SettingsViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const notify = useCallback(
    (ko: string, en: string, type: SettingsNotificationType = "info") => {
      onNotify?.(tr(ko, en), type);
    },
    [onNotify, tr],
  );

  const general = useGeneralDraft(settings, tr);
  const runtime = useRuntimeConfigDraft();
  const config = useConfigEntriesDraft();
  const [activePanel, changePanel] = usePanelRoute();
  const [query, setQuery] = useState("");
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [editorOpen, setEditorOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [pendingDangerousSave, setPendingDangerousSave] = useState<PendingDangerousConfigSave | null>(null);

  const connection = useConnectionStatus();
  const connectors = useOperatorConnectors();
  const machines = useMachineNodes();
  const accounts = useProviderAccounts({ enabled: activePanel === "ai", notify, tr });

  const runtimeStatus = sourceStatus(runtime.loaded, runtime.error);
  const configStatus = sourceStatus(config.loaded, config.error);
  const statusRows = useMemo(
    () => connectionStatusRows(connection.onboarding, connection.health, tr),
    [connection.health, connection.onboarding, tr],
  );
  const attention = useMemo<Partial<Record<SettingsPanel, number>>>(() => ({
    general: Number(Boolean(general.companyNameError)) + Number(Boolean(general.ceoNameError)),
    connections: statusRows.filter((row) => row.tone === "warn").length + connectorsNeedingSetup(connectors.data),
    machine: machinesNeedingAttention(machines.data, Date.now()),
  }), [connectors.data, general.ceoNameError, general.companyNameError, machines.data, statusRows]);

  const catalog = useSettingsMetaCatalog({
    activePanel,
    attention,
    configEdits: config.edits,
    configEntries: config.entries,
    general,
    query,
    runtime: {
      values: runtime.values,
      defaults: runtime.defaults,
      changedKeys: runtime.changedKeys,
      ready: runtimeStatus === "ready",
    },
    tr,
  });

  const { setFieldValue } = general;
  const { change: changeRuntime, resetToDefault } = runtime;
  const { edit: editConfig } = config;
  const handleRowChange = useCallback((key: string, value: string | boolean | number) => {
    if (GENERAL_FIELDS.has(key)) {
      setFieldValue(key as GeneralField, String(value));
    } else if (RUNTIME_KEYS.has(key)) {
      changeRuntime(key, value);
    } else {
      editConfig(key, typeof value === "boolean" ? value : String(value));
    }
  }, [changeRuntime, editConfig, setFieldValue]);

  const renderRow = useCallback((meta: SettingRowMeta, options?: { extra?: ReactNode }) => (
    <SettingRow
      key={meta.key}
      meta={meta}
      isKo={isKo}
      onChange={handleRowChange}
      onReset={RUNTIME_KEYS.has(meta.key) ? resetToDefault : undefined}
      extra={options?.extra}
    />
  ), [handleRowChange, isKo, resetToDefault]);

  const ctx: SettingsPanelContext = {
    tr,
    isKo,
    queryActive: catalog.queryActive,
    matches: catalog.matches,
    rowsFor: catalog.rowsFor,
    renderRow,
  };

  const changeCount = general.changedFields.length + runtime.changedKeys.size + Object.keys(config.edits).length;
  const generalChanged = general.changedFields.length > 0;
  const blockedReason = generalChanged && general.invalid ? (general.companyNameError ?? general.ceoNameError) : null;

  const performSave = async () => {
    setSaving(true);
    let failed = false;
    if (generalChanged) {
      try { await onSave(general.payload); } catch { failed = true; }
    }
    if (runtime.dirty) {
      try { await runtime.save(); } catch { failed = true; }
    }
    if (config.dirty) {
      try { await config.save({ ...config.edits }); } catch { failed = true; }
    }
    setSaving(false);
    if (failed) {
      notify(
        "일부 설정을 저장하지 못했어요. 바뀐 값은 남아 있으니 다시 저장해 주세요.",
        "Some settings were not saved. Your changes are kept; try saving again.",
        "error",
      );
    } else {
      notify("설정을 저장했어요.", "Saved settings.", "success");
    }
  };

  const handleSave = () => {
    if (blockedReason) return;
    const dangerousKeys = getDangerousConfigKeys(config.edits);
    if (dangerousKeys.length > 0) {
      setPendingDangerousSave({ edits: { ...config.edits }, keys: dangerousKeys });
      return;
    }
    void performSave();
  };

  const handleDiscard = () => {
    general.discard();
    runtime.discard();
    config.discard();
  };

  const warningPct = Number(runtime.values.rateLimitWarningPct ?? runtime.defaults.rateLimitWarningPct ?? 80);
  const dangerPct = Number(runtime.values.rateLimitDangerPct ?? runtime.defaults.rateLimitDangerPct ?? 95);

  const panelBody = (() => {
    switch (activePanel) {
      case "workflow":
        return (
          <SettingsWorkflowPanel
            ctx={ctx}
            configStatus={configStatus}
            runtimeStatus={runtimeStatus}
            editorOpen={editorOpen}
            onToggleEditor={() => setEditorOpen((open) => !open)}
            notify={notify}
          />
        );
      case "ai":
        return (
          <SettingsAiPanel
            ctx={ctx}
            accounts={accounts}
            configStatus={configStatus}
            runtimeStatus={runtimeStatus}
            warningPct={warningPct}
            dangerPct={dangerPct}
          />
        );
      case "connections":
        return (
          <SettingsConnectionsPanel
            ctx={ctx}
            statusRows={statusRows}
            connectors={connectors}
            configStatus={configStatus}
            onRerunSetup={() => setShowOnboarding(true)}
          />
        );
      case "machine":
        return <SettingsMachinePanel ctx={ctx} />;
      case "general":
      default:
        return <SettingsGeneralPanel ctx={ctx} />;
    }
  })();

  return (
    <SettingsViewLayout
      activePanel={activePanel}
      activeNavItem={catalog.activeNavItem}
      navItems={catalog.filteredNavItems}
      matchingCount={catalog.activeMatchCount}
      onPanelChange={changePanel}
      query={query}
      queryActive={catalog.queryActive}
      setQuery={setQuery}
      showNoMatches={catalog.queryActive && !catalog.filteredNavItems.some((item) => item.id === activePanel)}
      saveBar={(
        <SettingsSaveBar
          changeCount={changeCount}
          blockedReason={blockedReason}
          saving={saving}
          onDiscard={handleDiscard}
          onSave={handleSave}
          tr={tr}
        />
      )}
      pendingDangerousSave={pendingDangerousSave}
      saving={saving}
      onCancelDangerousSave={() => setPendingDangerousSave(null)}
      onConfirmDangerousSave={() => {
        setPendingDangerousSave(null);
        void performSave();
      }}
      showOnboarding={showOnboarding}
      onCloseOnboarding={() => setShowOnboarding(false)}
      isKo={isKo}
      tr={tr}
    >
      {panelBody}
    </SettingsViewLayout>
  );
}
