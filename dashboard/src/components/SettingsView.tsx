import { Check, Eye, Info } from "lucide-react";
import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type FormEvent, type ReactNode } from "react";
import type {
  Agent,
  CompanySettings,
  VoiceAgentConfig,
  VoiceConfigResponse,
  VoiceGlobalConfig,
} from "../types";
import * as api from "../api";
import type { GitHubRepoOption } from "../api";
import { STORAGE_KEYS } from "../lib/storageKeys";
import { writeLocalStorageValue } from "../lib/useLocalStorage";
import {
  SurfaceCard as SettingsCard,
  SurfaceEmptyState as SettingsEmptyState,
  SurfaceSection as SettingsSection,
} from "./common/SurfacePrimitives";
import { Modal } from "./common/overlay/Modal";
import { SettingsGlossary } from "./settings/SettingsKnowledge";
import { SettingsNavigation } from "./settings/SettingsNavigation";
import { SettingRow } from "./settings/SettingsPanels";
import { SettingsGeneralPanel } from "./settings/SettingsGeneralPanel";
import { SettingsOnboardingPanel } from "./settings/SettingsOnboardingPanel";
import { SettingsPipelinePanel } from "./settings/SettingsPipelinePanel";
import { SettingsRuntimePanel } from "./settings/SettingsRuntimePanel";
import { SettingsVoicePanel } from "./settings/SettingsVoicePanel";
import {
  getDangerousConfigKeys,
  getDangerousConfigLabel,
} from "./settings/settingsDangerousConfig";
import {
  CATEGORIES,
  GENERAL_FIELD_KEYS,
  GENERAL_FIELD_LIMITS,
  PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
  PipelineAgentCacheEntry,
  PipelineRepoCacheEntry,
  SETTING_GROUPS,
  SETTINGS_PANEL_QUERY_KEY,
  SYSTEM_CATEGORY_META,
  VOICE_SENSITIVITY_OPTIONS,
  applyConfigEdits,
  baselineSourceNote,
  cloneVoiceConfig,
  configLayerClass,
  configLayerLabel,
  configSourceClass,
  configSourceLabel,
  findVoiceAliasConflict,
  formatUnit,
  getCachedPipelineAgentEntry,
  getCachedPipelineRepoEntry,
  isBooleanConfigKey,
  isCacheFresh,
  isNumericConfigKey,
  isReadOnlyConfigKey,
  metaFromConfigEntry,
  parseBooleanConfigValue,
  readStoredRuntimeCategory,
  readStoredSettingsPanel,
  readSettingsPanelFromUrl,
  restartBehaviorNote,
  selectDefaultPipelineRepo,
  voiceConfigComparable,
  voiceSaveBody,
  writeStoredPipelineAgentCache,
  writeStoredPipelineRepoCache,
  type ConfigEditValue,
  type ConfigEntry,
  type PendingDangerousConfigSave,
  type SettingRowMeta,
  type SettingsNotificationType,
  type SettingsPanel,
} from "./settings/SettingsModel";

const OnboardingWizard = lazy(() => import("./OnboardingWizard"));

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
  const [pipelineRepos, setPipelineRepos] = useState<GitHubRepoOption[]>([]);
  const [pipelineAgents, setPipelineAgents] = useState<Agent[]>([]);
  const [selectedPipelineRepo, setSelectedPipelineRepo] = useState("");
  const [selectedPipelineAgentId, setSelectedPipelineAgentId] = useState<string | null>(null);
  const [pipelineSelectorLoading, setPipelineSelectorLoading] = useState(false);
  const [pipelineSelectorError, setPipelineSelectorError] = useState<string | null>(null);

  const [activePanel, setActivePanel] = useState<SettingsPanel>(() => readStoredSettingsPanel());
  const [activeRuntimeCategoryId, setActiveRuntimeCategoryId] = useState<string>(() => readStoredRuntimeCategory());
  const [panelQuery, setPanelQuery] = useState("");
  const [showOnboarding, setShowOnboarding] = useState(false);
  const onboardingDialogRef = useRef<HTMLDivElement | null>(null);
  const onboardingCloseButtonRef = useRef<HTMLButtonElement | null>(null);
  const notify = useCallback(
    (ko: string, en: string, type: SettingsNotificationType = "info") => {
      onNotify?.(tr(ko, en), type);
    },
    [onNotify, tr],
  );
  const applyPipelineRepoCache = useCallback((cache: PipelineRepoCacheEntry) => {
    setPipelineRepos(cache.repos);
    setSelectedPipelineRepo((current) => {
      if (current && cache.repos.some((repo) => repo.nameWithOwner === current)) {
        return current;
      }
      return selectDefaultPipelineRepo(cache.repos, cache.viewerLogin);
    });
  }, []);
  const applyPipelineAgentCache = useCallback((cache: PipelineAgentCacheEntry) => {
    setPipelineAgents(cache.agents);
    setSelectedPipelineAgentId((current) => (
      current && cache.agents.some((agent) => agent.id === current) ? current : null
    ));
  }, []);
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
    if (!showOnboarding || typeof window === "undefined") return;
    const previousActiveElement =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const focusCloseButton = window.setTimeout(() => {
      onboardingCloseButtonRef.current?.focus();
    }, 0);
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setShowOnboarding(false);
        return;
      }
      if (event.key !== "Tab") return;
      const dialog = onboardingDialogRef.current;
      if (!dialog) return;
      const focusable = Array.from(
        dialog.querySelectorAll<HTMLElement>(
          'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ),
      );
      if (focusable.length === 0) {
        event.preventDefault();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.clearTimeout(focusCloseButton);
      window.removeEventListener("keydown", handleKeyDown);
      previousActiveElement?.focus();
    };
  }, [showOnboarding]);

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

  useEffect(() => {
    if (activePanel !== "pipeline") {
      return;
    }
    let stale = false;
    const cachedRepoEntry = getCachedPipelineRepoEntry();
    const cachedAgentEntry = getCachedPipelineAgentEntry();
    const hasCachedRepos = (cachedRepoEntry?.repos.length ?? 0) > 0;
    const shouldRefreshRepos = !isCacheFresh(cachedRepoEntry);
    const shouldRefreshAgents = !isCacheFresh(cachedAgentEntry);

    if (cachedRepoEntry) {
      applyPipelineRepoCache(cachedRepoEntry);
      setPipelineSelectorError(null);
    }
    if (cachedAgentEntry) {
      applyPipelineAgentCache(cachedAgentEntry);
    }

    if (!shouldRefreshRepos && !shouldRefreshAgents) {
      return;
    }

    setPipelineSelectorLoading(true);
    if (!hasCachedRepos) {
      setPipelineSelectorError(null);
    }

    const repoPromise = shouldRefreshRepos
      ? api.getGitHubRepos({
          timeoutMs: PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
          maxRetries: 0,
        })
      : Promise.resolve(null);
    const agentPromise = shouldRefreshAgents
      ? api.getAgents(undefined, {
          timeoutMs: PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
          maxRetries: 0,
        })
      : Promise.resolve(null);

    void Promise.allSettled([repoPromise, agentPromise])
      .then(([repoResult, agentResult]) => {
        if (stale) return;

        if (repoResult.status === "fulfilled" && repoResult.value) {
          const nextRepoCache: PipelineRepoCacheEntry = {
            viewerLogin: repoResult.value.viewer_login,
            repos: repoResult.value.repos,
            fetchedAt: Date.now(),
          };
          applyPipelineRepoCache(nextRepoCache);
          writeStoredPipelineRepoCache(nextRepoCache);
          setPipelineSelectorError(null);
        } else if (!hasCachedRepos) {
          setPipelineSelectorError(
            tr(
              "파이프라인 에디터용 repo 목록을 불러오지 못했습니다. 마지막 성공값이 없어 에디터를 열 수 없습니다.",
              "Failed to load repository options for the pipeline editor, and no cached data is available yet.",
            ),
          );
          notify(
            "파이프라인 에디터용 repo 목록을 불러오지 못했습니다.",
            "Failed to load repository options for the pipeline editor.",
            "error",
          );
        }

        if (agentResult.status === "fulfilled" && agentResult.value) {
          const nextAgentCache: PipelineAgentCacheEntry = {
            agents: agentResult.value,
            fetchedAt: Date.now(),
          };
          applyPipelineAgentCache(nextAgentCache);
          writeStoredPipelineAgentCache(nextAgentCache);
        }
      })
      .finally(() => {
        if (!stale) {
          setPipelineSelectorLoading(false);
        }
      });
    return () => {
      stale = true;
      setPipelineSelectorLoading(false);
    };
  }, [
    activePanel,
    applyPipelineAgentCache,
    applyPipelineRepoCache,
    notify,
    tr,
  ]);

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
  const generalFieldCount = GENERAL_FIELD_KEYS.length;

  const companyDirty =
    normalizedCompanyName !== settings.companyName.trim() ||
    normalizedCeoName !== settings.ceoName.trim() ||
    language !== settings.language ||
    theme !== settings.theme;
  const configDirty = Object.keys(configEdits).length > 0;
  const runtimeFieldCount = CATEGORIES.reduce((sum, category) => sum + category.fields.length, 0);
  const voiceAliasConflict = useMemo(() => findVoiceAliasConflict(voiceDraft), [voiceDraft]);
  const voiceDirty = useMemo(
    () => JSON.stringify(voiceConfigComparable(voiceConfig)) !== JSON.stringify(voiceConfigComparable(voiceDraft)),
    [voiceConfig, voiceDraft],
  );

  const visibleConfigEntries = useMemo(() => configEntries, [configEntries]);

  const groupedConfigEntries = useMemo(
    () =>
      (Object.keys(SYSTEM_CATEGORY_META) as Array<keyof typeof SYSTEM_CATEGORY_META>).reduce<Record<string, ConfigEntry[]>>(
        (acc, categoryKey) => {
          acc[categoryKey] = visibleConfigEntries.filter((entry) => entry.category === categoryKey);
          return acc;
        },
        {},
      ),
    [visibleConfigEntries],
  );

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

  // ----- SettingRowMeta roster -----
  // Every visible setting funnels through this catalog so the spec invariant
  // "all SettingRow rendered through SettingRowMeta" holds and search/badges
  // share the same data source.
  const generalMetas = useMemo<SettingRowMeta[]>(
    () => [
      {
        key: "companyName",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.companyName,
        effectiveValue: companyName,
        validation: companyNameError
          ? { ok: false, messageKo: companyNameError, messageEn: companyNameError }
          : { ok: true },
        flags: ["kv_meta"],
        labelKo: "회사 이름",
        labelEn: "Company name",
        hintKo: "대시보드와 주요 헤더에 표시되는 이름입니다.",
        hintEn: "Shown in the dashboard and primary headers.",
        inputKind: "text",
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
      {
        key: "ceoName",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.ceoName,
        effectiveValue: ceoName,
        validation: ceoNameError
          ? { ok: false, messageKo: ceoNameError, messageEn: ceoNameError }
          : { ok: true },
        flags: ["kv_meta"],
        labelKo: "CEO 이름",
        labelEn: "CEO name",
        hintKo: "오피스와 일부 운영 UI에서 대표 인물 이름으로 사용됩니다.",
        hintEn: "Used as the representative persona name in office and ops surfaces.",
        inputKind: "text",
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
      {
        key: "language",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.language,
        effectiveValue: language,
        flags: ["kv_meta"],
        labelKo: "언어",
        labelEn: "Language",
        hintKo: "대시보드 전반의 기본 언어와 로캘을 정합니다.",
        hintEn: "Sets the default language and locale across the dashboard.",
        inputKind: "select",
        selectOptions: [
          { value: "ko", labelKo: "한국어", labelEn: "Korean" },
          { value: "en", labelKo: "영어", labelEn: "English" },
          { value: "ja", labelKo: "일본어", labelEn: "Japanese" },
          { value: "zh", labelKo: "중국어", labelEn: "Chinese" },
        ],
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
      {
        key: "theme",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.theme,
        effectiveValue: theme,
        flags: ["kv_meta"],
        labelKo: "테마",
        labelEn: "Theme",
        hintKo: "대시보드와 오피스 화면의 기본 분위기를 정합니다.",
        hintEn: "Sets the base look and feel for dashboard and office views.",
        inputKind: "select",
        selectOptions: [
          { value: "dark", labelKo: "다크", labelEn: "Dark" },
          { value: "light", labelKo: "라이트", labelEn: "Light" },
          { value: "auto", labelKo: "자동 (시스템)", labelEn: "Auto (System)" },
        ],
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
    ],
    [
      ceoName,
      ceoNameError,
      companyName,
      companyNameError,
      language,
      settings.ceoName,
      settings.companyName,
      settings.language,
      settings.theme,
      theme,
    ],
  );

  const runtimeMetas = useMemo<SettingRowMeta[]>(
    () =>
      CATEGORIES.flatMap((category) =>
        category.fields.map<SettingRowMeta>((field) => {
          const current = rcValues[field.key] ?? rcDefaults[field.key] ?? 0;
          const def = rcDefaults[field.key] ?? 0;
          const overrideActive = current !== def;
          return {
            key: field.key,
            group: "runtime",
            source: overrideActive ? "live_override" : "runtime_config",
            editable: true,
            restartRequired: false,
            defaultValue: def,
            effectiveValue: current,
            flags: overrideActive ? ["kv_meta", "live_override"] : ["kv_meta"],
            labelKo: field.labelKo,
            labelEn: field.labelEn,
            hintKo: `${field.descriptionKo} · ${field.min}–${field.max}${field.unit}`,
            hintEn: `${field.descriptionEn} · ${field.min}–${field.max}${field.unit}`,
            inputKind: "number",
            valueUnit: field.unit,
            numericRange: { min: field.min, max: field.max, step: field.step },
            storageLayerKo: "kv_meta['runtime-config']",
            storageLayerEn: "kv_meta['runtime-config']",
            restartNoteKo: "저장 즉시 반영, 재시작 없이 다음 폴링 주기에 적용됩니다.",
            restartNoteEn: "Applies on the next poll without restart.",
          };
        }),
      ),
    [rcValues, rcDefaults],
  );

  const pipelineMetas = useMemo<SettingRowMeta[]>(
    () => configEntries.map((entry) => metaFromConfigEntry(entry, configEdits)),
    [configEntries, configEdits],
  );

  // Onboarding-managed kv_meta keys are exposed read-only here. They are
  // edited through the onboarding wizard / dedicated API, but the spec
  // requires them to be visible (with legacy_readonly chip + editable=false).
  const onboardingMetas = useMemo<SettingRowMeta[]>(
    () => [
      {
        key: "greeting_template",
        group: "onboarding",
        source: "kv_meta",
        editable: false,
        restartRequired: false,
        defaultValue: "welcome to AgentDesk",
        effectiveValue: "(managed by wizard)",
        flags: ["kv_meta", "read_only"],
        labelKo: "인사 템플릿",
        labelEn: "Greeting template",
        hintKo: "신규 에이전트 첫 메시지. 위저드에서 관리합니다.",
        hintEn: "First message for new agents. Managed by the wizard.",
        inputKind: "readonly",
        storageLayerKo: "kv_meta (wizard)",
        storageLayerEn: "kv_meta (wizard)",
      },
      {
        key: "trial_card_count",
        group: "onboarding",
        source: "kv_meta",
        editable: false,
        restartRequired: false,
        defaultValue: 2,
        effectiveValue: "(managed by wizard)",
        flags: ["kv_meta", "read_only"],
        labelKo: "트라이얼 카드 수",
        labelEn: "Trial card count",
        hintKo: "연습용으로 할당하는 카드 수입니다.",
        hintEn: "Practice cards allocated to a new workspace.",
        inputKind: "readonly",
        storageLayerKo: "kv_meta (wizard)",
        storageLayerEn: "kv_meta (wizard)",
      },
      {
        key: "onboarding_bot_token",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Discord 봇 토큰",
        labelEn: "Discord bot token",
        hintKo: "/api/onboarding/* 가 관리합니다. 위저드를 사용하세요.",
        hintEn: "Managed by /api/onboarding/*. Use the wizard.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
      {
        key: "onboarding_guild_id",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Guild ID",
        labelEn: "Guild ID",
        hintKo: "/api/onboarding/* 가 관리합니다.",
        hintEn: "Managed by /api/onboarding/*.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
      {
        key: "onboarding_owner_id",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Owner ID",
        labelEn: "Owner ID",
        hintKo: "/api/onboarding/* 가 관리합니다.",
        hintEn: "Managed by /api/onboarding/*.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
      {
        key: "onboarding_provider",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Provider 연결",
        labelEn: "Provider wiring",
        hintKo: "/api/onboarding/* 가 관리합니다.",
        hintEn: "Managed by /api/onboarding/*.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
    ],
    [],
  );

  const voiceMetas = useMemo<SettingRowMeta[]>(() => {
    const global = voiceDraft?.global;
    const metas: SettingRowMeta[] = [
      {
        key: "voice.global.lobby_channel_id",
        group: "voice",
        source: "repo_canonical",
        editable: true,
        restartRequired: false,
        effectiveValue: global?.lobby_channel_id ?? "",
        flags: [],
        labelKo: "Lobby 채널 ID",
        labelEn: "Lobby channel ID",
        hintKo: "단일 voice-lobby로 들어오는 음성을 agent alias 라우팅에 사용합니다.",
        hintEn: "Single voice-lobby channel used for agent alias routing.",
        inputKind: "text",
        storageLayerKo: "agentdesk.yaml voice.lobby_channel_id",
        storageLayerEn: "agentdesk.yaml voice.lobby_channel_id",
      },
      {
        key: "voice.global.active_agent_ttl_seconds",
        group: "voice",
        source: "repo_canonical",
        editable: true,
        restartRequired: false,
        defaultValue: 180,
        effectiveValue: global?.active_agent_ttl_seconds ?? 180,
        flags: [],
        labelKo: "Active agent TTL",
        labelEn: "Active agent TTL",
        hintKo: "alias 없이 이어 말할 수 있는 최근 agent 유지 시간입니다.",
        hintEn: "How long follow-up speech can continue without repeating an alias.",
        inputKind: "number",
        valueUnit: "s",
        numericRange: { min: 30, max: 1800, step: 30 },
        storageLayerKo: "agentdesk.yaml voice.active_agent_ttl_seconds",
        storageLayerEn: "agentdesk.yaml voice.active_agent_ttl_seconds",
      },
      {
        key: "voice.global.default_sensitivity_mode",
        group: "voice",
        source: "repo_canonical",
        editable: true,
        restartRequired: false,
        defaultValue: "normal",
        effectiveValue: global?.default_sensitivity_mode ?? "normal",
        flags: [],
        labelKo: "기본 민감도",
        labelEn: "Default sensitivity",
        hintKo: "agent별 override가 없을 때 적용할 barge-in 민감도입니다.",
        hintEn: "Barge-in sensitivity used when an agent has no override.",
        inputKind: "select",
        selectOptions: VOICE_SENSITIVITY_OPTIONS,
        storageLayerKo: "agentdesk.yaml voice.default_sensitivity_mode",
        storageLayerEn: "agentdesk.yaml voice.default_sensitivity_mode",
      },
      {
        key: "voice.global.version",
        group: "voice",
        source: "repo_canonical",
        editable: false,
        restartRequired: false,
        effectiveValue: voiceDraft?.version ?? "",
        flags: ["read_only"],
        labelKo: "설정 버전",
        labelEn: "Config version",
        hintKo: "저장 시 optimistic locking에 사용하는 버전 해시입니다.",
        hintEn: "Version hash used for optimistic locking on save.",
        inputKind: "readonly",
        storageLayerKo: "server-computed",
        storageLayerEn: "server-computed",
      },
    ];
    for (const agent of voiceDraft?.agents ?? []) {
      metas.push(
        {
          key: `voice.agent.${agent.id}.enabled`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.voice_enabled,
          flags: [],
          labelKo: `${agent.name_ko ?? agent.name} 음성 활성화`,
          labelEn: `${agent.name} voice enabled`,
          hintKo: "voice-lobby 라우팅 대상에 포함할지 결정합니다.",
          hintEn: "Controls whether this agent participates in voice-lobby routing.",
          inputKind: "toggle",
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.voice_enabled`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.voice_enabled`,
        },
        {
          key: `voice.agent.${agent.id}.wake_word`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.wake_word,
          flags: [],
          labelKo: `${agent.name_ko ?? agent.name} wake word`,
          labelEn: `${agent.name} wake word`,
          hintKo: "비어 있으면 agent alias만으로 라우팅합니다.",
          hintEn: "When empty, the agent routes by alias only.",
          inputKind: "text",
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.wake_word`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.wake_word`,
        },
        {
          key: `voice.agent.${agent.id}.aliases`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.aliases.join(", "),
          validation: voiceAliasConflict &&
            (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id)
            ? {
                ok: false,
                messageKo: `alias 충돌: ${voiceAliasConflict.normalized}`,
                messageEn: `alias collision: ${voiceAliasConflict.normalized}`,
              }
            : { ok: true },
          flags: voiceAliasConflict &&
            (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id)
            ? ["alert"]
            : [],
          labelKo: `${agent.name_ko ?? agent.name} aliases`,
          labelEn: `${agent.name} aliases`,
          hintKo: "쉼표 또는 줄바꿈으로 여러 호출명을 입력합니다.",
          hintEn: "Enter multiple spoken aliases separated by commas or new lines.",
          inputKind: "text",
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.aliases`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.aliases`,
        },
        {
          key: `voice.agent.${agent.id}.sensitivity`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.sensitivity_mode,
          flags: [],
          labelKo: `${agent.name_ko ?? agent.name} 민감도`,
          labelEn: `${agent.name} sensitivity`,
          hintKo: "agent별 barge-in 감지 민감도입니다.",
          hintEn: "Per-agent barge-in detection sensitivity.",
          inputKind: "select",
          selectOptions: VOICE_SENSITIVITY_OPTIONS,
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.sensitivity_mode`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.sensitivity_mode`,
        },
      );
    }
    return metas;
  }, [voiceAliasConflict, voiceDraft]);

  const allMetas = useMemo<SettingRowMeta[]>(
    () => [...pipelineMetas, ...runtimeMetas, ...voiceMetas, ...onboardingMetas, ...generalMetas],
    [pipelineMetas, runtimeMetas, voiceMetas, onboardingMetas, generalMetas],
  );

  const groupCounts = useMemo(() => {
    const counts: Record<string, number> = {
      pipeline: 0,
      runtime: 0,
      voice: 0,
      onboarding: 0,
      general: 0,
    };
    for (const m of allMetas) {
      const g = String(m.group);
      counts[g] = (counts[g] ?? 0) + 1;
    }
    return counts;
  }, [allMetas]);

  const navItems = useMemo(
    () =>
      SETTING_GROUPS.map((group) => ({
        id: group.id,
        title: tr(group.nameKo, group.nameEn),
        detail: tr(group.descKo, group.descEn),
        count: String(groupCounts[group.id] ?? 0),
      })),
    [groupCounts, tr],
  );
  const panelQueryNormalized = panelQuery.trim().toLowerCase();
  const filteredNavItems = useMemo(
    () =>
      navItems.filter((item) => {
        if (!panelQueryNormalized) return true;
        if (`${item.title} ${item.detail}`.toLowerCase().includes(panelQueryNormalized)) {
          return true;
        }
        // also match if any item inside the group matches the search
        return allMetas.some((meta) => {
          if (meta.group !== item.id) return false;
          const haystack =
            `${meta.key} ${meta.labelKo ?? ""} ${meta.labelEn ?? ""} ${meta.hintKo ?? ""} ${meta.hintEn ?? ""}`.toLowerCase();
          return haystack.includes(panelQueryNormalized);
        });
      }),
    [allMetas, navItems, panelQueryNormalized],
  );
  // Track which row keys match the current search inside the active panel.
  const matchingKeysInActivePanel = useMemo<Set<string>>(() => {
    const set = new Set<string>();
    if (!panelQueryNormalized) return set;
    for (const meta of allMetas) {
      if (meta.group !== activePanel) continue;
      const haystack =
        `${meta.key} ${meta.labelKo ?? ""} ${meta.labelEn ?? ""} ${meta.hintKo ?? ""} ${meta.hintEn ?? ""}`.toLowerCase();
      if (haystack.includes(panelQueryNormalized)) {
        set.add(meta.key);
      }
    }
    return set;
  }, [activePanel, allMetas, panelQueryNormalized]);
  const isRowVisible = useCallback(
    (key: string) => {
      if (!panelQueryNormalized) return true;
      return matchingKeysInActivePanel.has(key);
    },
    [matchingKeysInActivePanel, panelQueryNormalized],
  );
  const activeNavItem = navItems.find((item) => item.id === activePanel) ?? navItems[0];
  const pipelineLiveOverrideCount = useMemo(
    () => configEntries.filter((entry) => entry.override_active).length,
    [configEntries],
  );
  const pipelineReadOnlyCount = useMemo(
    () =>
      configEntries.filter(
        (entry) => isReadOnlyConfigKey(entry.key) || entry.editable === false,
      ).length,
    [configEntries],
  );

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

  // Dispatcher for SettingRow value changes — routes to the correct setter
  // based on the meta.group + key.
  const handleSettingRowChange = useCallback(
    (key: string, value: string | boolean | number) => {
      // general
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
      // runtime
      if (rcDefaults[key] !== undefined && typeof value === "number") {
        handleRcChange(key, value);
        return;
      }
      // pipeline kv_meta — value can be string or boolean
      if (typeof value === "boolean") {
        handleConfigEdit(key, value);
        return;
      }
      handleConfigEdit(key, String(value));
    },
    [handleRcChange, rcDefaults],
  );

  // Render a SettingRow for a meta with optional control overlay (e.g. range).
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

  // Render a card-shaped group of SettingRow entries (header + count chip + rows).
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

  const renderActivePanel = () => {
    switch (activePanel) {
      case "runtime":
        return (
          <SettingsRuntimePanel
            activeRuntimeCategoryId={activeRuntimeCategoryId}
            inputStyle={inputStyle}
            onCategoryChange={setActiveRuntimeCategoryId}
            onRuntimeChange={handleRcChange}
            onRuntimeReset={handleRcReset}
            onRuntimeSave={handleRcSave}
            panelQueryNormalized={panelQueryNormalized}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            rcDirty={rcDirty}
            rcLoaded={rcLoaded}
            rcSaving={rcSaving}
            renderSettingRow={renderSettingRow}
            runtimeMetas={runtimeMetas}
            subtleButtonClass={subtleButtonClass}
            subtleButtonStyle={subtleButtonStyle}
            tr={tr}
          />
        );
      case "pipeline":
        return (
          <SettingsPipelinePanel
            configDirty={configDirty}
            configEntries={configEntries}
            configSaving={configSaving}
            groupedConfigEntries={groupedConfigEntries}
            inputStyle={inputStyle}
            isKo={isKo}
            onConfigSave={handleConfigSave}
            pipelineAgents={pipelineAgents}
            pipelineMetas={pipelineMetas}
            pipelineRepos={pipelineRepos}
            pipelineSelectorError={pipelineSelectorError}
            pipelineSelectorLoading={pipelineSelectorLoading}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            renderSettingGroupCard={renderSettingGroupCard}
            renderSettingRow={renderSettingRow}
            selectedPipelineAgentId={selectedPipelineAgentId}
            selectedPipelineRepo={selectedPipelineRepo}
            setSelectedPipelineAgentId={setSelectedPipelineAgentId}
            setSelectedPipelineRepo={setSelectedPipelineRepo}
            tr={tr}
          />
        );
      case "voice":
        return (
          <SettingsVoicePanel
            inputStyle={inputStyle}
            isKo={isKo}
            isRowVisible={isRowVisible}
            loadVoiceConfig={loadVoiceConfig}
            onVoiceSave={handleVoiceSave}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            renderSettingGroupCard={renderSettingGroupCard}
            secondaryActionClass={secondaryActionClass}
            secondaryActionStyle={secondaryActionStyle}
            tr={tr}
            updateVoiceAgent={updateVoiceAgent}
            updateVoiceGlobal={updateVoiceGlobal}
            voiceAliasConflict={voiceAliasConflict}
            voiceDirty={voiceDirty}
            voiceDraft={voiceDraft}
            voiceError={voiceError}
            voiceLoaded={voiceLoaded}
            voiceSaving={voiceSaving}
          />
        );
      case "onboarding":
        return (
          <SettingsOnboardingPanel
            onboardingMetas={onboardingMetas}
            renderSettingGroupCard={renderSettingGroupCard}
            renderSettingRow={renderSettingRow}
            tr={tr}
          />
        );
      case "general":
      default:
        return (
          <SettingsGeneralPanel
            companyDirty={companyDirty}
            generalFormInvalid={generalFormInvalid}
            generalMetas={generalMetas}
            onSave={handleSave}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            renderSettingGroupCard={renderSettingGroupCard}
            renderSettingRow={renderSettingRow}
            saving={saving}
            tr={tr}
          />
        );
    }
  };
  const renderHeaderActions = () => {
    if (activePanel === "onboarding") {
      return (
        <button
          onClick={openOnboarding}
          className={secondaryActionClass}
          style={secondaryActionStyle}
        >
          {tr("온보딩 다시 실행", "Re-run onboarding")}
        </button>
      );
    }

    if (activePanel === "pipeline") {
      return (
        <>
          <button
            type="button"
            onClick={() =>
              document
                .getElementById("settings-audit-notes")
                ?.scrollIntoView({ behavior: "smooth", block: "start" })
            }
            className={secondaryActionClass}
            style={secondaryActionStyle}
          >
            <Eye size={12} />
            {tr("audit 노트", "Audit notes")}
          </button>
          <button
            onClick={handleConfigSave}
            disabled={configSaving || !configDirty}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            <Check size={12} />
            {configSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
          </button>
        </>
      );
    }

    if (activePanel === "runtime") {
      return (
        <button
          onClick={handleRcSave}
          disabled={rcSaving || !rcDirty}
          className={primaryActionClass}
          style={primaryActionStyle}
        >
          <Check size={12} />
          {rcSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
        </button>
      );
    }

    if (activePanel === "voice") {
      return (
        <>
          <button
            type="button"
            onClick={() => void loadVoiceConfig()}
            className={secondaryActionClass}
            style={secondaryActionStyle}
          >
            {tr("다시 불러오기", "Reload")}
          </button>
          <button
            type="button"
            onClick={() => void handleVoiceSave()}
            disabled={voiceSaving || !voiceDirty || Boolean(voiceAliasConflict)}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            <Check size={12} />
            {voiceSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
          </button>
        </>
      );
    }

    return (
      <button
        onClick={() => void handleSave()}
        disabled={saving || generalFormInvalid || !companyDirty}
        className={primaryActionClass}
        style={primaryActionStyle}
      >
        <Check size={12} />
        {saving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
      </button>
    );
  };
  const settingsInfoNotice = (
    <div
      className="flex items-start gap-3 rounded-[18px] border px-4 py-4 sm:px-5"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
    >
      <div
        className="grid h-7 w-7 shrink-0 place-items-center rounded-[10px]"
        style={{
          background: "var(--th-accent-primary-soft)",
          color: "var(--th-accent-primary)",
        }}
      >
        <Info size={14} />
      </div>
      <div className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
        {tr("whitelist된 ", "Only whitelisted ")}
        <code
          className="rounded px-1.5 py-0.5 text-[12px]"
          style={{
            fontFamily: "var(--font-mono)",
            color: "var(--th-text)",
            background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
          }}
        >
          kv_meta
        </code>{" "}
        {tr(
          "키와 agentdesk.yaml 음성 설정만 편집합니다. read-only 항목도 숨기지 않고 현재 상태를 그대로 보여줍니다.",
          "keys and agentdesk.yaml voice settings are editable. Read-only items stay visible so the current state remains explicit.",
        )}
      </div>
    </div>
  );
  const dangerousConfigLabels =
    pendingDangerousConfigSave?.keys.map((key) => getDangerousConfigLabel(key, isKo)).join(", ") ?? "";

  return (
    <div
      data-testid="settings-page"
      className="page fade-in mx-auto h-full w-full max-w-[1600px] min-w-0 overflow-x-hidden overflow-y-auto px-4 py-4 pb-40 sm:px-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <div className="page-header">
        <div className="min-w-0">
          <div className="page-title">{tr("설정", "Settings")}</div>
          <div className="page-sub">
            {tr(
              "카탈로그에서 꺼내 쓰는 kv_meta 설정",
              "Catalog-driven kv_meta configuration",
            )}
          </div>
        </div>
        <div className="flex flex-wrap gap-2">{renderHeaderActions()}</div>
      </div>

      <div className="settings-grid mt-4 grid gap-4 md:grid-cols-[220px_minmax(0,1fr)]">
        <SettingsNavigation
          activePanel={activePanel}
          inputStyle={inputStyle}
          items={filteredNavItems}
          matchingCount={matchingKeysInActivePanel.size}
          onPanelChange={handlePanelChange}
          query={panelQuery}
          queryActive={Boolean(panelQueryNormalized)}
          setQuery={setPanelQuery}
          tr={tr}
        />

        <div className="min-w-0 space-y-4">
          {settingsInfoNotice}
          <SettingsGlossary isKo={isKo} />

          <SettingsCard
            id="settings-panel-content"
            role="tabpanel"
            aria-labelledby={`settings-tab-${activePanel}`}
            tabIndex={-1}
            className="min-w-0 rounded-[28px] border px-4 py-4 outline-none sm:px-5 sm:py-5"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
              background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
            }}
          >
            <div className="flex flex-wrap items-start justify-between gap-3 border-b pb-4" style={{ borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)" }}>
              <div className="min-w-0">
                <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                  {activeNavItem.title}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {activeNavItem.detail}
                </div>
              </div>
              {activeNavItem.count ? (
                <span
                  className="inline-flex items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                    background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {activeNavItem.count}
                </span>
              ) : null}
            </div>
            <div className="mt-5 min-w-0">
              {renderActivePanel()}
            </div>
          </SettingsCard>
        </div>
      </div>

      <Modal
        open={Boolean(pendingDangerousConfigSave)}
        onClose={() => setPendingDangerousConfigSave(null)}
        title={tr("위험 설정 저장 확인", "Confirm risky settings")}
        description={tr(
          "자동화, 리뷰 게이트, 컨텍스트 초기화에 영향을 주는 설정입니다.",
          "These settings affect automation, review gates, or context clearing.",
        )}
        size="sm"
      >
        <div className="space-y-4">
          <div className="rounded-2xl border px-4 py-3 text-sm leading-6"
            style={{
              borderColor: "rgba(251, 191, 36, 0.35)",
              background: "rgba(251, 191, 36, 0.10)",
              color: "var(--th-text)",
            }}
          >
            {tr(
              "저장하면 진행 중인 카드의 리뷰/머지/컨텍스트 정책이 즉시 달라질 수 있습니다.",
              "Saving can immediately change review, merge, or context policy for active cards.",
            )}
          </div>
          <div>
            <div className="text-xs font-semibold uppercase tracking-wide" style={{ color: "var(--th-text-muted)" }}>
              {tr("변경 대상", "Changing")}
            </div>
            <div className="mt-2 rounded-xl border px-3 py-2 text-sm" style={{
              borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
              background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
              color: "var(--th-text)",
            }}>
              {dangerousConfigLabels}
            </div>
          </div>
          <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            <button
              type="button"
              onClick={() => setPendingDangerousConfigSave(null)}
              className={secondaryActionClass}
              style={secondaryActionStyle}
            >
              {tr("취소", "Cancel")}
            </button>
            <button
              type="button"
              onClick={() => void handleDangerousConfigConfirm()}
              disabled={configSaving}
              className={primaryActionClass}
              style={primaryActionStyle}
            >
              <Check size={12} />
              {configSaving ? tr("저장 중...", "Saving...") : tr("확인 후 저장", "Confirm and save")}
            </button>
          </div>
        </div>
      </Modal>

      {showOnboarding && (
        <div className="fixed inset-0 z-50 overflow-y-auto bg-[#0a0e1a]" role="dialog" aria-modal="true" aria-label="Onboarding wizard">
          <div className="flex min-h-screen items-start justify-center pb-16 pt-8">
            <div ref={onboardingDialogRef} className="w-full max-w-2xl">
              <div className="mb-2 flex justify-end px-4">
                <button
                  ref={onboardingCloseButtonRef}
                  onClick={() => setShowOnboarding(false)}
                  className="min-h-[44px] rounded-lg border px-4 py-2.5 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[color:var(--th-accent-primary)] focus-visible:ring-offset-2 focus-visible:ring-offset-[#0a0e1a]"
                  style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-muted)" }}
                >
                  ✕ {tr("닫기", "Close")}
                </button>
              </div>
              <Suspense fallback={<div className="py-8 text-center" style={{ color: "var(--th-text-muted)" }}>Loading...</div>}>
                <OnboardingWizard
                  isKo={isKo}
                  onComplete={() => {
                    setShowOnboarding(false);
                    window.location.reload();
                  }}
                />
              </Suspense>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
