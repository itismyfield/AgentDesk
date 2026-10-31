import { useCallback, useMemo } from "react";
import type { RuntimeConfigMap } from "../../api";
import type { CompanySettings } from "../../types";
import { PANEL_COPY, RUNTIME_APPLY_NOTE, RUNTIME_FIELDS } from "./SettingsCopy";
import {
  SETTINGS_PANELS,
  configEntryPlacement,
  metaFromConfigEntry,
  type ConfigEditValue,
  type ConfigEntry,
  type SettingRowMeta,
  type SettingsPanel,
} from "./SettingsModel";
import type { GeneralField } from "./useSettingsDrafts";

type Tr = (ko: string, en: string) => string;

export interface SettingsNavItem {
  id: SettingsPanel;
  title: string;
  detail: string;
  attention: number;
}

type UseSettingsMetaCatalogArgs = {
  activePanel: SettingsPanel;
  attention: Partial<Record<SettingsPanel, number>>;
  configEdits: Record<string, ConfigEditValue>;
  configEntries: ConfigEntry[];
  general: {
    values: Pick<CompanySettings, GeneralField>;
    changedFields: GeneralField[];
    companyNameError: string | null;
    ceoNameError: string | null;
  };
  query: string;
  runtime: {
    values: RuntimeConfigMap;
    defaults: RuntimeConfigMap;
    changedKeys: Set<string>;
    ready: boolean;
  };
  tr: Tr;
};

function validation(error: string | null): SettingRowMeta["validation"] {
  return error ? { ok: false, messageKo: error, messageEn: error } : { ok: true };
}

function rowHaystack(meta: SettingRowMeta): string {
  return `${meta.key} ${meta.labelKo} ${meta.labelEn} ${meta.hintKo ?? ""} ${meta.hintEn ?? ""}`.toLowerCase();
}

export function useSettingsMetaCatalog({
  activePanel,
  attention,
  configEdits,
  configEntries,
  general,
  query,
  runtime,
  tr,
}: UseSettingsMetaCatalogArgs) {
  const generalMetas = useMemo<SettingRowMeta[]>(() => {
    const base = { panel: "general" as const, section: "identity", editable: true, restartRequired: false, flags: [] };
    return [
      {
        ...base,
        key: "companyName",
        effectiveValue: general.values.companyName,
        changed: general.changedFields.includes("companyName"),
        validation: validation(general.companyNameError),
        labelKo: "회사 이름",
        labelEn: "Company name",
        hintKo: "대시보드 위쪽과 주요 화면에 보이는 이름이에요.",
        hintEn: "Shown at the top of the dashboard and on main screens.",
        inputKind: "text",
      },
      {
        ...base,
        key: "ceoName",
        effectiveValue: general.values.ceoName,
        changed: general.changedFields.includes("ceoName"),
        validation: validation(general.ceoNameError),
        labelKo: "내 이름",
        labelEn: "Your name",
        hintKo: "대시보드와 에이전트가 부를 이름이에요.",
        hintEn: "What the dashboard and agents call you.",
        inputKind: "text",
      },
      {
        ...base,
        key: "language",
        effectiveValue: general.values.language,
        changed: general.changedFields.includes("language"),
        labelKo: "언어",
        labelEn: "Language",
        hintKo: "대시보드에 쓰는 언어예요.",
        hintEn: "Language used across the dashboard.",
        inputKind: "select",
        selectOptions: [
          { value: "ko", labelKo: "한국어", labelEn: "한국어 (Korean)" },
          { value: "en", labelKo: "English (영어)", labelEn: "English" },
          { value: "ja", labelKo: "日本語 (일본어)", labelEn: "日本語 (Japanese)" },
          { value: "zh", labelKo: "中文 (중국어)", labelEn: "中文 (Chinese)" },
        ],
      },
      {
        ...base,
        key: "theme",
        effectiveValue: general.values.theme,
        changed: general.changedFields.includes("theme"),
        labelKo: "테마",
        labelEn: "Theme",
        hintKo: "화면을 어둡게 또는 밝게 보여요.",
        hintEn: "Show the dashboard dark or light.",
        inputKind: "select",
        selectOptions: [
          { value: "dark", labelKo: "어둡게", labelEn: "Dark" },
          { value: "light", labelKo: "밝게", labelEn: "Light" },
          { value: "auto", labelKo: "시스템 설정 따르기", labelEn: "Match system" },
        ],
      },
    ];
  }, [general.ceoNameError, general.changedFields, general.companyNameError, general.values]);

  const runtimeMetas = useMemo<SettingRowMeta[]>(() => {
    if (!runtime.ready) return [];
    return RUNTIME_FIELDS.map<SettingRowMeta>((field) => {
      const fallback = field.inputKind === "toggle" ? false : 0;
      const current = runtime.values[field.key] ?? runtime.defaults[field.key] ?? fallback;
      const defaultValue = runtime.defaults[field.key] ?? fallback;
      return {
        key: field.key,
        panel: field.panel,
        section: field.section,
        editable: true,
        restartRequired: false,
        defaultValue,
        effectiveValue: current,
        changed: runtime.changedKeys.has(field.key),
        resettable: current !== defaultValue,
        flags: [],
        labelKo: field.labelKo,
        labelEn: field.labelEn,
        hintKo: field.hintKo,
        hintEn: field.hintEn,
        inputKind: field.inputKind,
        valueUnit: field.unit,
        numericRange: field.min !== undefined && field.max !== undefined && field.step !== undefined
          ? { min: field.min, max: field.max, step: field.step }
          : undefined,
        applyNoteKo: RUNTIME_APPLY_NOTE.ko,
        applyNoteEn: RUNTIME_APPLY_NOTE.en,
      };
    });
  }, [runtime.changedKeys, runtime.defaults, runtime.ready, runtime.values]);

  const configMetas = useMemo<SettingRowMeta[]>(
    () => configEntries.map((entry) => metaFromConfigEntry(entry, configEdits, configEntryPlacement(entry))),
    [configEdits, configEntries],
  );

  const allMetas = useMemo(
    () => [...generalMetas, ...runtimeMetas, ...configMetas],
    [configMetas, generalMetas, runtimeMetas],
  );

  const queryNormalized = query.trim().toLowerCase();
  const queryActive = queryNormalized.length > 0;
  const matches = useCallback(
    (...texts: string[]) => !queryActive || texts.join(" ").toLowerCase().includes(queryNormalized),
    [queryActive, queryNormalized],
  );
  const visibleKeys = useMemo(
    () => new Set(allMetas.filter((meta) => !queryActive || rowHaystack(meta).includes(queryNormalized)).map((meta) => meta.key)),
    [allMetas, queryActive, queryNormalized],
  );

  const rowsFor = useCallback(
    (panel: SettingsPanel, section: string) =>
      allMetas.filter((meta) => meta.panel === panel && meta.section === section && visibleKeys.has(meta.key)),
    [allMetas, visibleKeys],
  );

  const navItems = useMemo<SettingsNavItem[]>(
    () => SETTINGS_PANELS.map((panel) => ({
      id: panel,
      title: tr(PANEL_COPY[panel].titleKo, PANEL_COPY[panel].titleEn),
      detail: tr(PANEL_COPY[panel].detailKo, PANEL_COPY[panel].detailEn),
      attention: attention[panel] ?? 0,
    })),
    [attention, tr],
  );

  const panelMatches = useCallback((panel: SettingsPanel) => {
    if (!queryActive) return true;
    const copy = PANEL_COPY[panel];
    return matches(copy.titleKo, copy.titleEn, copy.detailKo, copy.detailEn, copy.keywords)
      || allMetas.some((meta) => meta.panel === panel && visibleKeys.has(meta.key));
  }, [allMetas, matches, queryActive, visibleKeys]);

  const filteredNavItems = useMemo(
    () => navItems.filter((item) => panelMatches(item.id)),
    [navItems, panelMatches],
  );
  const activeMatchCount = useMemo(
    () => allMetas.filter((meta) => meta.panel === activePanel && visibleKeys.has(meta.key)).length,
    [activePanel, allMetas, visibleKeys],
  );
  const activeNavItem = navItems.find((item) => item.id === activePanel) ?? navItems[0];

  return {
    activeMatchCount,
    activeNavItem,
    filteredNavItems,
    matches,
    queryActive,
    rowsFor,
  };
}
