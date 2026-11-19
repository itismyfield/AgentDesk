import { useCallback, useEffect, useMemo, useState } from "react";
import * as api from "../../api";
import type { RuntimeConfigMap, RuntimeConfigValue } from "../../api";
import { applyConfigEdits, type ConfigEditValue, type ConfigEntry } from "./SettingsModel";

function withoutKeys<T>(record: Record<string, T>, keys: string[]): Record<string, T> {
  const next = { ...record };
  for (const key of keys) delete next[key];
  return next;
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

  /** Saves one key on top of the saved snapshot, leaving other unsaved drafts alone (the phone sheet). */
  const saveKey = useCallback(async (key: string, value: RuntimeConfigValue, isExplicit: boolean) => {
    const nextValues = { ...saved.values, [key]: value };
    const nextExplicit = new Set(saved.explicit);
    if (isExplicit) nextExplicit.add(key);
    else nextExplicit.delete(key);
    await api.saveRuntimeConfig({ ...nextValues, __runtimeConfigExplicitKeys: Array.from(nextExplicit).sort() });
    setSaved({ values: nextValues, explicit: nextExplicit });
    setValues((current) => ({ ...current, [key]: value }));
    setExplicit((current) => {
      const next = new Set(current);
      if (isExplicit) next.add(key);
      else next.delete(key);
      return next;
    });
  }, [saved]);

  return {
    saved: saved.values,
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
    saveKey,
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
    const original = entries.find((entry) => entry.key === key);
    if (!original || original.editable === false) return;
    setEdits((current) => {
      if (String(value) === String(original.value ?? "")) return withoutKeys(current, [key]);
      return { ...current, [key]: value };
    });
  }, [entries]);

  const discard = useCallback(() => setEdits({}), []);

  /** Saves the given edits; other pending edits stay pending. */
  const save = useCallback(async (pending: Record<string, ConfigEditValue>) => {
    const previous = entries;
    const keys = Object.keys(pending);
    setEntries((current) => applyConfigEdits(current, pending));
    setEdits((current) => withoutKeys(current, keys));
    try {
      await api.patchSettingsConfig(pending);
      await load();
    } catch (saveError) {
      setEntries(previous);
      setEdits((current) => ({ ...current, ...pending }));
      throw saveError;
    }
  }, [entries, load]);

  return { entries, edits, loaded, error, edit, discard, save, dirty: Object.keys(edits).length > 0 };
}
