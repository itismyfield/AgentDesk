import { useCallback, useEffect, useMemo, useState } from "react";
import * as api from "../../api";
import type { RuntimeConfigMap, RuntimeConfigValue } from "../../api";
import type { CompanySettings } from "../../types";
import {
  GENERAL_FIELD_LIMITS,
  applyConfigEdits,
  isReadOnlyConfigKey,
  type ConfigEditValue,
  type ConfigEntry,
} from "./SettingsModel";

type Tr = (ko: string, en: string) => string;

export type GeneralField = "companyName" | "ceoName" | "language" | "theme";
type GeneralValues = Pick<CompanySettings, GeneralField>;

function pickGeneral(settings: CompanySettings): GeneralValues {
  return {
    companyName: settings.companyName,
    ceoName: settings.ceoName,
    language: settings.language,
    theme: settings.theme,
  };
}

/** Company/name/language/theme draft, reset whenever the saved settings change. */
export function useGeneralDraft(settings: CompanySettings, tr: Tr) {
  const [values, setValues] = useState<GeneralValues>(() => pickGeneral(settings));

  const { companyName: savedCompany, ceoName: savedCeo, language: savedLanguage, theme: savedTheme } = settings;
  useEffect(() => {
    setValues({ companyName: savedCompany, ceoName: savedCeo, language: savedLanguage, theme: savedTheme });
  }, [savedCompany, savedCeo, savedLanguage, savedTheme]);

  const companyName = values.companyName.trim();
  const ceoName = values.ceoName.trim();
  const companyNameError = companyName.length === 0
    ? tr("회사 이름을 입력해 주세요.", "Enter a company name.")
    : companyName.length > GENERAL_FIELD_LIMITS.companyName
      ? tr(
          `회사 이름은 ${GENERAL_FIELD_LIMITS.companyName}자까지 쓸 수 있어요.`,
          `Company name can be up to ${GENERAL_FIELD_LIMITS.companyName} characters.`,
        )
      : null;
  const ceoNameError = ceoName.length > GENERAL_FIELD_LIMITS.ceoName
    ? tr(
        `내 이름은 ${GENERAL_FIELD_LIMITS.ceoName}자까지 쓸 수 있어요.`,
        `Your name can be up to ${GENERAL_FIELD_LIMITS.ceoName} characters.`,
      )
    : null;

  const changedFields = useMemo(() => {
    const changed: GeneralField[] = [];
    if (companyName !== settings.companyName.trim()) changed.push("companyName");
    if (ceoName !== settings.ceoName.trim()) changed.push("ceoName");
    if (values.language !== settings.language) changed.push("language");
    if (values.theme !== settings.theme) changed.push("theme");
    return changed;
  }, [ceoName, companyName, settings, values.language, values.theme]);

  // Values come from text inputs and selects whose options already match the field's allowed values.
  const setFieldValue = useCallback((field: GeneralField, value: string) => {
    setValues((current) => ({ ...current, [field]: value }) as GeneralValues);
  }, []);

  const discard = useCallback(() => setValues(pickGeneral(settings)), [settings]);

  return {
    values,
    setFieldValue,
    companyNameError,
    ceoNameError,
    invalid: Boolean(companyNameError || ceoNameError),
    changedFields,
    discard,
    payload: { companyName, ceoName, language: values.language, theme: values.theme },
  };
}

/** /api/settings/runtime-config draft with the saved snapshot kept for discard and change tracking. */
export function useRuntimeConfigDraft() {
  const [saved, setSaved] = useState<{ values: RuntimeConfigMap; explicit: Set<string> }>({
    values: {},
    explicit: new Set(),
  });
  const [values, setValues] = useState<RuntimeConfigMap>({});
  const [explicit, setExplicit] = useState<Set<string>>(() => new Set());
  const [defaults, setDefaults] = useState<RuntimeConfigMap>({});
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState(false);

  useEffect(() => {
    void api.getRuntimeConfig()
      .then((data) => {
        const current = data?.current ?? {};
        const explicitKeys = new Set(data?.explicit_keys ?? []);
        setSaved({ values: current, explicit: explicitKeys });
        setValues(current);
        setExplicit(new Set(explicitKeys));
        setDefaults(data?.defaults ?? {});
        setLoaded(true);
      })
      .catch(() => {
        setError(true);
        setLoaded(true);
      });
  }, []);

  const changedKeys = useMemo(() => {
    const keys = new Set<string>();
    for (const key of new Set([...Object.keys(values), ...Object.keys(saved.values)])) {
      if (values[key] !== saved.values[key]) keys.add(key);
    }
    // Touching a value without changing it is not a change; dropping a saved override (reset to default) is.
    for (const key of saved.explicit) {
      if (!explicit.has(key)) keys.add(key);
    }
    return keys;
  }, [explicit, saved, values]);

  const change = useCallback((key: string, value: RuntimeConfigValue) => {
    setValues((current) => ({ ...current, [key]: value }));
    setExplicit((current) => new Set(current).add(key));
  }, []);

  const resetToDefault = useCallback((key: string) => {
    if (defaults[key] === undefined) return;
    setValues((current) => ({ ...current, [key]: defaults[key] }));
    setExplicit((current) => {
      const next = new Set(current);
      next.delete(key);
      return next;
    });
  }, [defaults]);

  const discard = useCallback(() => {
    setValues(saved.values);
    setExplicit(new Set(saved.explicit));
  }, [saved]);

  const save = useCallback(async () => {
    await api.saveRuntimeConfig({
      ...values,
      __runtimeConfigExplicitKeys: Array.from(explicit).sort(),
    });
    setSaved({ values, explicit: new Set(explicit) });
  }, [explicit, values]);

  return {
    values,
    defaults,
    loaded,
    error,
    changedKeys,
    dirty: changedKeys.size > 0,
    change,
    resetToDefault,
    discard,
    save,
  };
}

/** /api/settings/config entries plus pending edits. */
export function useConfigEntriesDraft() {
  const [entries, setEntries] = useState<ConfigEntry[]>([]);
  const [edits, setEdits] = useState<Record<string, ConfigEditValue>>({});
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState(false);

  const load = useCallback(async () => {
    const data = await api.getSettingsConfig();
    setEntries(Array.isArray(data.entries) ? data.entries : []);
  }, []);

  useEffect(() => {
    void load()
      .catch(() => setError(true))
      .finally(() => setLoaded(true));
  }, [load]);

  const edit = useCallback((key: string, value: ConfigEditValue) => {
    if (isReadOnlyConfigKey(key)) return;
    setEdits((current) => {
      const original = entries.find((entry) => entry.key === key);
      const originalValue = original?.value ?? original?.default ?? "";
      if (String(value) === String(originalValue)) {
        const next = { ...current };
        delete next[key];
        return next;
      }
      return { ...current, [key]: value };
    });
  }, [entries]);

  const discard = useCallback(() => setEdits({}), []);

  const save = useCallback(async (pending: Record<string, ConfigEditValue>) => {
    const previous = entries;
    setEntries((current) => applyConfigEdits(current, pending));
    setEdits({});
    try {
      await api.patchSettingsConfig(pending);
      await load();
    } catch (saveError) {
      setEntries(previous);
      setEdits(pending);
      throw saveError;
    }
  }, [entries, load]);

  return { entries, edits, loaded, error, edit, discard, save, dirty: Object.keys(edits).length > 0 };
}
