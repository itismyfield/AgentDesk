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
        flags: [],
        labelKo: "회사 이름",
        labelEn: "Company name",
        hintKo: "대시보드와 주요 헤더에 표시되는 이름입니다.",
        hintEn: "Shown in the dashboard and primary headers.",
        inputKind: "text",
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
        flags: [],
        labelKo: "CEO 이름",
        labelEn: "CEO name",
        hintKo: "오피스와 일부 운영 UI에서 대표 인물 이름으로 사용됩니다.",
        hintEn: "Used as the representative persona name in office and ops surfaces.",
        inputKind: "text",
      },
      {
        key: "language",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.language,
        effectiveValue: language,
        flags: [],
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
      },
      {
        key: "theme",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.theme,
        effectiveValue: theme,
        flags: [],
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
            flags: overrideActive ? ["live_override"] : [],
            labelKo: field.labelKo,
            labelEn: field.labelEn,
            hintKo: `${field.descriptionKo} · ${field.min}-${field.max}${field.unit}`,
            hintEn: `${field.descriptionEn} · ${field.min}-${field.max}${field.unit}`,
            inputKind: "number",
            valueUnit: field.unit,
            numericRange: { min: field.min, max: field.max, step: field.step },
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
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        defaultValue: "welcome to AgentDesk",
        effectiveValue: tr("자동 관리", "Managed"),
        flags: ["read_only"],
        labelKo: "첫 인사 메시지",
        labelEn: "Greeting template",
        hintKo: "새 에이전트가 처음 인사할 때 사용하는 메시지입니다.",
        hintEn: "Message used when a new agent greets for the first time.",
        inputKind: "readonly",
      },
      {
        key: "trial_card_count",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        defaultValue: 2,
        effectiveValue: tr("자동 관리", "Managed"),
        flags: ["read_only"],
        labelKo: "시작 카드 수",
        labelEn: "Trial card count",
        hintKo: "새 워크스페이스에 처음 준비되는 연습 카드 수입니다.",
        hintEn: "Practice cards allocated to a new workspace.",
        inputKind: "readonly",
      },
      {
        key: "onboarding_bot_token",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: tr("연결됨", "Connected"),
        flags: ["read_only"],
        labelKo: "봇 연결",
        labelEn: "Bot connection",
        hintKo: "대시보드가 사용할 Discord 봇 연결 상태입니다.",
        hintEn: "Connection status for the Discord bot used by the dashboard.",
        inputKind: "readonly",
      },
      {
        key: "onboarding_guild_id",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: tr("연결됨", "Connected"),
        flags: ["read_only"],
        labelKo: "서버 연결",
        labelEn: "Server connection",
        hintKo: "작업을 받을 Discord 서버 연결 상태입니다.",
        hintEn: "Connection status for the Discord server that receives work.",
        inputKind: "readonly",
      },
      {
        key: "onboarding_owner_id",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: tr("설정됨", "Configured"),
        flags: ["read_only"],
        labelKo: "소유자",
        labelEn: "Owner",
        hintKo: "주요 운영 알림과 승인 기준이 되는 소유자입니다.",
        hintEn: "Owner used for primary operation notices and approvals.",
        inputKind: "readonly",
      },
      {
        key: "onboarding_provider",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: tr("설정됨", "Configured"),
        flags: ["read_only"],
        labelKo: "AI 연결",
        labelEn: "AI connection",
        hintKo: "에이전트가 사용할 AI provider 연결 상태입니다.",
        hintEn: "AI provider connection status used by agents.",
        inputKind: "readonly",
      },
    ],
    [tr],
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
        labelKo: "음성 채널 ID",
        labelEn: "Voice channel ID",
        hintKo: "음성 입력을 받을 Discord 채널입니다.",
        hintEn: "Discord channel that receives voice input.",
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
        labelKo: "대화 유지 시간",
        labelEn: "Conversation handoff time",
        hintKo: "이름을 다시 부르지 않아도 같은 에이전트에게 이어 말할 수 있는 시간입니다.",
        hintEn: "How long follow-up speech keeps talking to the same agent without repeating a name.",
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
        hintKo: "말이 겹칠 때 새 발화로 얼마나 쉽게 인식할지 정합니다.",
        hintEn: "Controls how readily overlapping speech is treated as a new utterance.",
        inputKind: "select",
        selectOptions: VOICE_SENSITIVITY_OPTIONS,
        storageLayerKo: "agentdesk.yaml voice.default_sensitivity_mode",
        storageLayerEn: "agentdesk.yaml voice.default_sensitivity_mode",
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
          labelKo: `${agent.name_ko ?? agent.name} 음성 사용`,
          labelEn: `${agent.name} voice enabled`,
          hintKo: "이 에이전트가 음성 호출을 받을지 결정합니다.",
          hintEn: "Controls whether this agent responds to voice calls.",
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
          labelKo: `${agent.name_ko ?? agent.name} 호출어`,
          labelEn: `${agent.name} wake word`,
          hintKo: "비워 두면 호출 이름만으로 연결합니다.",
          hintEn: "When empty, spoken names are enough.",
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
                messageKo: "호출 이름이 겹칩니다.",
                messageEn: "Spoken names overlap.",
              }
            : { ok: true },
          flags: voiceAliasConflict &&
            (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id)
            ? ["alert"]
            : [],
          labelKo: `${agent.name_ko ?? agent.name} 추가 호출 이름`,
          labelEn: `${agent.name} additional spoken names`,
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
          hintKo: "이 에이전트만 다르게 인식하도록 조정합니다.",
          hintEn: "Tune recognition for this agent only.",
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
      connectors: 0,
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
