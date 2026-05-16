import { useCallback, useEffect, useState, type CSSProperties, type FormEvent, type ReactNode } from "react";
import type {
  CompanySettings,
  VoiceAgentConfig,
  VoiceConfigResponse,
  VoiceGlobalConfig,
} from "../types";
import * as api from "../api";
import { STORAGE_KEYS } from "../lib/storageKeys";
import { writeLocalStorageValue } from "../lib/useLocalStorage";
import { SurfaceEmptyState as SettingsEmptyState } from "./common/SurfacePrimitives";
import { SettingRow } from "./settings/SettingsPanels";
import { useSettingsMetaCatalog } from "./settings/SettingsMetaCatalog";
import { SettingsViewLayout } from "./settings/SettingsViewLayout";
import { getDangerousConfigKeys } from "./settings/settingsDangerousConfig";
import { useSettingsPipelineSelector } from "./settings/useSettingsPipelineSelector";
import {
  GENERAL_FIELD_LIMITS,
  SETTINGS_PANEL_QUERY_KEY,
  applyConfigEdits,
  cloneVoiceConfig,
  isReadOnlyConfigKey,
  readStoredRuntimeCategory,
  readStoredSettingsPanel,
  readSettingsPanelFromUrl,
  voiceSaveBody,
  type ConfigEditValue,
  type ConfigEntry,
  type PendingDangerousConfigSave,
  type SettingRowMeta,
  type SettingsNotificationType,
  type SettingsPanel,
} from "./settings/SettingsModel";

interface SettingsViewProps {
  settings: CompanySettings;
  onSave: (patch: Record<string, unknown>) => Promise<void>;
  isKo: boolean;
  onNotify?: (message: string, type?: SettingsNotificationType) => string | void;
}

export default function SettingsView({
  settings,
  onSave,
  isKo,
  onNotify,
}: SettingsViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);

  const [companyName, setCompanyName] = useState(settings.companyName);
  const [ceoName, setCeoName] = useState(settings.ceoName);
  const [language, setLanguage] = useState(settings.language);
  const [theme, setTheme] = useState(settings.theme);
  const [saving, setSaving] = useState(false);

  const [rcValues, setRcValues] = useState<Record<string, number>>({});
  const [rcDefaults, setRcDefaults] = useState<Record<string, number>>({});
  const [rcLoaded, setRcLoaded] = useState(false);
  const [rcSaving, setRcSaving] = useState(false);
  const [rcDirty, setRcDirty] = useState(false);

  const [configEntries, setConfigEntries] = useState<ConfigEntry[]>([]);
  const [configEdits, setConfigEdits] = useState<Record<string, ConfigEditValue>>({});
  const [configSaving, setConfigSaving] = useState(false);
  const [pendingDangerousConfigSave, setPendingDangerousConfigSave] =
    useState<PendingDangerousConfigSave | null>(null);

  const [voiceConfig, setVoiceConfig] = useState<VoiceConfigResponse | null>(null);
  const [voiceDraft, setVoiceDraft] = useState<VoiceConfigResponse | null>(null);
  const [voiceLoaded, setVoiceLoaded] = useState(false);
  const [voiceSaving, setVoiceSaving] = useState(false);
  const [voiceError, setVoiceError] = useState<string | null>(null);

  const [activePanel, setActivePanel] = useState<SettingsPanel>(() => readStoredSettingsPanel());
  const [activeRuntimeCategoryId, setActiveRuntimeCategoryId] = useState<string>(() => readStoredRuntimeCategory());
  const [panelQuery, setPanelQuery] = useState("");
  const [showOnboarding, setShowOnboarding] = useState(false);

  const notify = useCallback(
    (ko: string, en: string, type: SettingsNotificationType = "info") => {
      onNotify?.(tr(ko, en), type);
    },
    [onNotify, tr],
  );

  const {
    pipelineAgents,
    pipelineRepos,
    pipelineSelectorError,
    pipelineSelectorLoading,
    selectedPipelineAgentId,
    selectedPipelineRepo,
    setSelectedPipelineAgentId,
    setSelectedPipelineRepo,
  } = useSettingsPipelineSelector({ activePanel, notify, tr });

  const loadConfigEntries = useCallback(async () => {
    const response = await fetch("/api/settings/config", { credentials: "include" });
    if (!response.ok) {
      throw new Error("config-load-failed");
    }
    const data = await response.json() as { entries?: ConfigEntry[] };
    const entries = Array.isArray(data.entries) ? data.entries : [];
    setConfigEntries(entries);
    return entries;
  }, []);

  const loadVoiceConfig = useCallback(async () => {
    setVoiceError(null);
    try {
      const data = await api.getVoiceConfig();
      setVoiceConfig(data);
      setVoiceDraft(cloneVoiceConfig(data));
      setVoiceLoaded(true);
      return data;
    } catch {
      setVoiceLoaded(true);
      setVoiceError(tr("음성 설정을 불러오지 못했습니다.", "Failed to load voice settings."));
      return null;
    }
  }, [tr]);

  useEffect(() => {
    setCompanyName(settings.companyName);
    setCeoName(settings.ceoName);
    setLanguage(settings.language);
    setTheme(settings.theme);
  }, [settings.companyName, settings.ceoName, settings.language, settings.theme]);

  useEffect(() => {
    writeLocalStorageValue(STORAGE_KEYS.settingsPanel, activePanel);
  }, [activePanel]);

  useEffect(() => {
    writeLocalStorageValue(STORAGE_KEYS.settingsRuntimeCategory, activeRuntimeCategoryId);
  }, [activeRuntimeCategoryId]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    if (readSettingsPanelFromUrl() !== activePanel) {
      const url = new URL(window.location.href);
      url.searchParams.set(SETTINGS_PANEL_QUERY_KEY, activePanel);
      window.history.replaceState(window.history.state, "", url);
    }
  }, [activePanel]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const handlePopState = () => {
      const panelFromUrl = readSettingsPanelFromUrl();
      if (panelFromUrl) setActivePanel(panelFromUrl);
    };
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  useEffect(() => {
    void api.getRuntimeConfig()
      .then((data) => {
        setRcValues(data?.current ?? {});
        setRcDefaults(data?.defaults ?? {});
        setRcLoaded(true);
      })
      .catch(() => {
        setRcLoaded(true);
      });

    void loadConfigEntries()
      .catch(() => {});
  }, [loadConfigEntries]);

  useEffect(() => {
    if (activePanel !== "voice" || voiceLoaded) {
      return;
    }
    void loadVoiceConfig();
  }, [activePanel, loadVoiceConfig, voiceLoaded]);

  const normalizedCompanyName = companyName.trim();
  const normalizedCeoName = ceoName.trim();
  const companyNameError =
    normalizedCompanyName.length === 0
      ? tr("회사 이름은 비워둘 수 없습니다.", "Company name is required.")
      : normalizedCompanyName.length > GENERAL_FIELD_LIMITS.companyName
        ? tr(
            `회사 이름은 ${GENERAL_FIELD_LIMITS.companyName}자 이하여야 합니다.`,
            `Company name must be ${GENERAL_FIELD_LIMITS.companyName} characters or fewer.`,
          )
        : null;
  const ceoNameError =
    normalizedCeoName.length > GENERAL_FIELD_LIMITS.ceoName
      ? tr(
          `CEO 이름은 ${GENERAL_FIELD_LIMITS.ceoName}자 이하여야 합니다.`,
          `CEO name must be ${GENERAL_FIELD_LIMITS.ceoName} characters or fewer.`,
        )
      : null;
  const generalFormInvalid = Boolean(companyNameError || ceoNameError);
  const companyDirty =
    normalizedCompanyName !== settings.companyName.trim() ||
    normalizedCeoName !== settings.ceoName.trim() ||
    language !== settings.language ||
    theme !== settings.theme;
  const configDirty = Object.keys(configEdits).length > 0;

  const {
    activeNavItem,
    filteredNavItems,
    generalMetas,
    groupedConfigEntries,
    isRowVisible,
    matchingKeysInActivePanel,
    onboardingMetas,
    panelQueryNormalized,
    pipelineMetas,
    runtimeMetas,
    voiceAliasConflict,
    voiceDirty,
  } = useSettingsMetaCatalog({
    activePanel,
    ceoName,
    ceoNameError,
    companyName,
    companyNameError,
    configEdits,
    configEntries,
    language,
    panelQuery,
    rcDefaults,
    rcValues,
    settings,
    theme,
    tr,
    voiceConfig,
    voiceDraft,
  });

  const handlePanelChange = useCallback((panel: SettingsPanel, mode: "push" | "replace" = "push") => {
    setActivePanel((current) => {
      if (typeof window !== "undefined" && !(current === panel && mode === "push")) {
        const url = new URL(window.location.href);
        url.searchParams.set(SETTINGS_PANEL_QUERY_KEY, panel);
        if (mode === "replace") {
          window.history.replaceState(window.history.state, "", url);
        } else {
          window.history.pushState(window.history.state, "", url);
        }
      }
      return panel;
    });
  }, []);

  const openOnboarding = useCallback(() => {
    handlePanelChange("onboarding");
    setShowOnboarding(true);
  }, [handlePanelChange]);

  const inputStyle: CSSProperties = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };
  const primaryActionClass = "inline-flex min-h-[44px] shrink-0 items-center justify-center whitespace-nowrap rounded-2xl px-5 py-2.5 text-sm font-medium text-white transition-colors disabled:opacity-50";
  const primaryActionStyle: CSSProperties = { background: "var(--th-accent-primary)" };
  const secondaryActionClass = "inline-flex min-h-[44px] items-center justify-center whitespace-nowrap rounded-2xl border px-5 py-2.5 text-sm font-medium transition-[opacity,color,border-color] hover:opacity-100";
  const secondaryActionStyle: CSSProperties = {
    borderColor: "rgba(148,163,184,0.28)",
    color: "var(--th-text-secondary)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
  };
  const subtleButtonClass = "inline-flex items-center justify-center whitespace-nowrap rounded-full border px-3 py-1.5 text-[11px] font-medium transition-colors";
  const subtleButtonStyle: CSSProperties = {
    borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
    color: "var(--th-text-muted)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
  };

  const handleSave = async (event?: FormEvent<HTMLFormElement>) => {
    event?.preventDefault();
    if (generalFormInvalid) return;
    setSaving(true);
    try {
      await onSave({
        companyName: normalizedCompanyName,
        ceoName: normalizedCeoName,
        language,
        theme,
      });
      notify("일반 설정을 저장했습니다.", "Saved general settings.", "success");
    } catch {
      notify("일반 설정 저장에 실패했습니다.", "Failed to save general settings.", "error");
    } finally {
      setSaving(false);
    }
  };

  const handleRcSave = async () => {
    setRcSaving(true);
    try {
      await api.saveRuntimeConfig(rcValues);
      setRcDirty(false);
      notify("런타임 설정을 저장했습니다.", "Saved runtime settings.", "success");
    } catch {
      notify("런타임 설정 저장에 실패했습니다.", "Failed to save runtime settings.", "error");
    } finally {
      setRcSaving(false);
    }
  };

  const handleRcChange = (key: string, value: number) => {
    setRcValues((prev) => ({ ...prev, [key]: value }));
    setRcDirty(true);
  };

  const handleRcReset = (key: string) => {
    if (rcDefaults[key] !== undefined) {
      setRcValues((prev) => ({ ...prev, [key]: rcDefaults[key] }));
      setRcDirty(true);
    }
  };

  const handleConfigEdit = (key: string, value: ConfigEditValue) => {
    if (isReadOnlyConfigKey(key)) return;
    setConfigEdits((prev) => ({ ...prev, [key]: value }));
  };

  const saveConfigEdits = async (pendingEdits: Record<string, ConfigEditValue>) => {
    if (Object.keys(pendingEdits).length === 0) return;
    const previousEntries = configEntries;
    setConfigSaving(true);
    setConfigEntries((current) => applyConfigEdits(current, pendingEdits));
    setConfigEdits({});
    try {
      const response = await fetch("/api/settings/config", {
        method: "PATCH",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(pendingEdits),
      });
      if (!response.ok) {
        throw new Error("config-save-failed");
      }
      await loadConfigEntries();
      notify(
        "파이프라인 설정을 저장했습니다.",
        "Saved pipeline settings.",
        "success",
      );
    } catch {
      setConfigEntries(previousEntries);
      setConfigEdits(pendingEdits);
      notify(
        "파이프라인 설정 저장에 실패해 이전 값으로 복원했습니다.",
        "Failed to save pipeline settings and restored the previous values.",
        "error",
      );
    } finally {
      setConfigSaving(false);
    }
  };

  const handleConfigSave = async () => {
    if (!configDirty) return;
    const pendingEdits = { ...configEdits };
    const dangerousKeys = getDangerousConfigKeys(pendingEdits);
    if (dangerousKeys.length > 0) {
      setPendingDangerousConfigSave({ edits: pendingEdits, keys: dangerousKeys });
      return;
    }
    await saveConfigEdits(pendingEdits);
  };

  const handleDangerousConfigConfirm = async () => {
    if (!pendingDangerousConfigSave) return;
    const pendingEdits = pendingDangerousConfigSave.edits;
    setPendingDangerousConfigSave(null);
    await saveConfigEdits(pendingEdits);
  };

  const updateVoiceGlobal = useCallback(
    <K extends keyof VoiceGlobalConfig>(key: K, value: VoiceGlobalConfig[K]) => {
      setVoiceDraft((current) =>
        current
          ? {
              ...current,
              global: {
                ...current.global,
                [key]: value,
              },
            }
          : current,
      );
    },
    [],
  );

  const updateVoiceAgent = useCallback(
    (agentId: string, patch: Partial<VoiceAgentConfig>) => {
      setVoiceDraft((current) =>
        current
          ? {
              ...current,
              agents: current.agents.map((agent) =>
                agent.id === agentId ? { ...agent, ...patch } : agent,
              ),
            }
          : current,
      );
    },
    [],
  );

  const handleVoiceSave = async () => {
    if (!voiceDraft || !voiceDirty || voiceAliasConflict) return;
    setVoiceSaving(true);
    setVoiceError(null);
    try {
      const saved = await api.saveVoiceConfig(voiceSaveBody(voiceDraft));
      setVoiceConfig(saved);
      setVoiceDraft(cloneVoiceConfig(saved));
      notify("음성 설정을 저장했습니다.", "Saved voice settings.", "success");
    } catch (error) {
      const message =
        error instanceof api.VoiceConfigApiError
          ? error.message
          : tr("음성 설정 저장에 실패했습니다.", "Failed to save voice settings.");
      setVoiceError(message);
      notify("음성 설정 저장에 실패했습니다.", "Failed to save voice settings.", "error");
      if (error instanceof api.VoiceConfigApiError && error.status === 409) {
        void loadVoiceConfig();
      }
    } finally {
      setVoiceSaving(false);
    }
  };

  const handleSettingRowChange = useCallback(
    (key: string, value: string | boolean | number) => {
      if (key === "companyName" && typeof value === "string") {
        setCompanyName(value);
        return;
      }
      if (key === "ceoName" && typeof value === "string") {
        setCeoName(value);
        return;
      }
      if (key === "language" && typeof value === "string") {
        setLanguage(value as typeof language);
        return;
      }
      if (key === "theme" && typeof value === "string") {
        setTheme(value as typeof theme);
        return;
      }
      if (rcDefaults[key] !== undefined && typeof value === "number") {
        handleRcChange(key, value);
        return;
      }
      if (typeof value === "boolean") {
        handleConfigEdit(key, value);
        return;
      }
      handleConfigEdit(key, String(value));
    },
    [handleRcChange, rcDefaults],
  );

  const renderSettingRow = useCallback(
    (meta: SettingRowMeta, options?: { controlOverlay?: ReactNode; trailingMeta?: ReactNode }) => {
      if (!isRowVisible(meta.key)) return null;
      return (
        <SettingRow
          key={meta.key}
          meta={meta}
          isKo={isKo}
          onChange={handleSettingRowChange}
          controlOverlay={options?.controlOverlay}
          trailingMeta={options?.trailingMeta}
        />
      );
    },
    [handleSettingRowChange, isKo, isRowVisible],
  );

  const renderSettingGroupCard = useCallback(
    (
      args: {
        titleKo: string;
        titleEn: string;
        descriptionKo: string;
        descriptionEn: string;
        rows: ReactNode[];
        totalCount: number;
      },
    ) => {
      const filteredRows = args.rows.filter(Boolean);
      const countLabel = panelQueryNormalized
        ? `${filteredRows.length}/${args.totalCount}`
        : tr(`${args.totalCount}개`, `${args.totalCount} items`);
      return (
        <div
          className="setting-group-card overflow-hidden rounded-[20px] border"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
          }}
        >
          <div
            className="flex flex-wrap items-start justify-between gap-3 border-b px-4 py-4 sm:px-5"
            style={{ borderColor: "color-mix(in srgb, var(--th-border) 60%, transparent)" }}
          >
            <div className="min-w-0">
              <div className="settings-section-title text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                {tr(args.titleKo, args.titleEn)}
              </div>
              <div className="settings-copy mt-1 text-[12px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                {tr(args.descriptionKo, args.descriptionEn)}
              </div>
            </div>
            <span
              className="settings-count-chip inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                color: "var(--th-text-muted)",
              }}
            >
              {countLabel}
            </span>
          </div>
          <div className="px-2 pb-1 pt-1 sm:px-3">
            {filteredRows.length > 0 ? (
              filteredRows
            ) : (
              <SettingsEmptyState className="text-sm">
                {tr("검색 결과가 없습니다.", "No matching settings.")}
              </SettingsEmptyState>
            )}
          </div>
        </div>
      );
    },
    [panelQueryNormalized, tr],
  );

  return (
    <SettingsViewLayout
      ctx={{
        activeNavItem, activePanel, activeRuntimeCategoryId,
        companyDirty, configDirty, configEntries, configSaving,
        filteredNavItems, generalFormInvalid, generalMetas, groupedConfigEntries,
        handleConfigSave, handleDangerousConfigConfirm, handlePanelChange,
        handleRcChange, handleRcReset, handleRcSave, handleSave, handleVoiceSave,
        inputStyle, isKo, isRowVisible, loadVoiceConfig, matchingKeysInActivePanel,
        onboardingMetas, openOnboarding, panelQuery, panelQueryNormalized,
        pendingDangerousConfigSave, pipelineAgents, pipelineMetas, pipelineRepos,
        pipelineSelectorError, pipelineSelectorLoading, primaryActionClass,
        primaryActionStyle, rcDirty, rcLoaded, rcSaving, renderSettingGroupCard,
        renderSettingRow, runtimeMetas, saving, secondaryActionClass,
        secondaryActionStyle, selectedPipelineAgentId, selectedPipelineRepo,
        setActiveRuntimeCategoryId, setPanelQuery, setPendingDangerousConfigSave,
        setSelectedPipelineAgentId, setSelectedPipelineRepo, setShowOnboarding,
        showOnboarding, subtleButtonClass, subtleButtonStyle, tr, updateVoiceAgent,
        updateVoiceGlobal, voiceAliasConflict, voiceDirty, voiceDraft, voiceError,
        voiceLoaded, voiceSaving,
      }}
    />
  );
}
