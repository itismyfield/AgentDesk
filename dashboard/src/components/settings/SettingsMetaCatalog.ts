import { useCallback, useMemo } from "react";
import type { CompanySettings, VoiceConfigResponse } from "../../types";
import {
  CATEGORIES,
  SETTING_GROUPS,
  SYSTEM_CATEGORY_META,
  VOICE_SENSITIVITY_OPTIONS,
  findVoiceAliasConflict,
  metaFromConfigEntry,
  voiceConfigComparable,
  type ConfigEditValue,
  type ConfigEntry,
  type SettingRowMeta,
  type SettingsPanel,
} from "./SettingsModel";

type UseSettingsMetaCatalogArgs = {
  activePanel: SettingsPanel;
  ceoName: string;
  ceoNameError: string | null;
  companyName: string;
  companyNameError: string | null;
  configEdits: Record<string, ConfigEditValue>;
  configEntries: ConfigEntry[];
  language: CompanySettings["language"];
  panelQuery: string;
  rcDefaults: Record<string, number>;
  rcValues: Record<string, number>;
  settings: CompanySettings;
  theme: CompanySettings["theme"];
  tr: (ko: string, en: string) => string;
  voiceConfig: VoiceConfigResponse | null;
  voiceDraft: VoiceConfigResponse | null;
};

export function useSettingsMetaCatalog({
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
}: UseSettingsMetaCatalogArgs) {
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
            hintKo: `${field.descriptionKo} · ${field.min}-${field.max}${field.unit}`,
            hintEn: `${field.descriptionEn} · ${field.min}-${field.max}${field.unit}`,
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
    for (const meta of allMetas) {
      const group = String(meta.group);
      counts[group] = (counts[group] ?? 0) + 1;
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
        return allMetas.some((meta) => {
          if (meta.group !== item.id) return false;
          const haystack =
            `${meta.key} ${meta.labelKo ?? ""} ${meta.labelEn ?? ""} ${meta.hintKo ?? ""} ${meta.hintEn ?? ""}`.toLowerCase();
          return haystack.includes(panelQueryNormalized);
        });
      }),
    [allMetas, navItems, panelQueryNormalized],
  );
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

  return {
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
  };
}
