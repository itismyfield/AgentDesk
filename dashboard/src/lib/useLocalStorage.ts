import { useCallback, useRef, useSyncExternalStore, type Dispatch, type SetStateAction } from "react";

export type LocalStorageDefaultValue<T> = T | (() => T);

interface LocalStorageReadOptions<T> {
  validate?: (value: unknown) => value is T;
  legacy?: (raw: string) => T | null;
  warnOnInvalid?: boolean;
}

type StorageListener = () => void;

const storageSubscribers = new Map<string, Set<StorageListener>>();
const invalidStorageWarnings = new Map<string, string>();
let storageEventListenerAttached = false;

function resolveDefaultValue<T>(defaultValue: LocalStorageDefaultValue<T>): T {
  return typeof defaultValue === "function" ? (defaultValue as () => T)() : defaultValue;
}

function getBrowserStorage(): Storage | null {
  if (typeof window === "undefined") {
    return null;
  }
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function warnInvalidStorageValue(key: string, raw: string, error?: unknown): void {
  if (invalidStorageWarnings.get(key) === raw) {
    return;
  }
  invalidStorageWarnings.set(key, raw);
  const reason = error instanceof Error ? error.message : "Invalid JSON";
  console.warn(`Invalid localStorage value for "${key}". Falling back to the default value.`, {
    key,
    raw,
    reason,
  });
}

function readLocalStorageRawValue(key: string): string | null {
  const storage = getBrowserStorage();
  if (!storage) {
    return null;
  }
  try {
    return storage.getItem(key);
  } catch {
    return null;
  }
}

function parseLocalStorageValue<T>(
  key: string,
  raw: string | null,
  defaultValue: LocalStorageDefaultValue<T>,
  options: LocalStorageReadOptions<T> = {},
): T {
  if (raw === null) {
    return resolveDefaultValue(defaultValue);
  }

  try {
    const parsed = JSON.parse(raw) as unknown;
    if (options.validate && !options.validate(parsed)) {
      if (options.warnOnInvalid !== false) {
        warnInvalidStorageValue(key, raw);
      }
      return resolveDefaultValue(defaultValue);
    }
    invalidStorageWarnings.delete(key);
    return parsed as T;
  } catch (error) {
    if (options.legacy) {
      const legacyValue = options.legacy(raw);
      if (legacyValue !== null) {
        invalidStorageWarnings.delete(key);
        return legacyValue;
      }
    }
    if (options.warnOnInvalid !== false) {
      warnInvalidStorageValue(key, raw, error);
    }
    return resolveDefaultValue(defaultValue);
  }
}

function notifyStorageSubscribers(key: string): void {
  const listeners = storageSubscribers.get(key);
  if (!listeners) {
    return;
  }
  listeners.forEach((listener) => listener());
}

function handleWindowStorage(event: StorageEvent): void {
  const storage = getBrowserStorage();
  if (!storage || event.storageArea !== storage || !event.key) {
    return;
  }
  invalidStorageWarnings.delete(event.key);
  notifyStorageSubscribers(event.key);
}

function ensureStorageEventListener(): void {
  if (storageEventListenerAttached || typeof window === "undefined") {
    return;
  }
  window.addEventListener("storage", handleWindowStorage);
  storageEventListenerAttached = true;
}

export function subscribeToLocalStorageKey(key: string, listener: StorageListener): () => void {
  ensureStorageEventListener();
  const listeners = storageSubscribers.get(key) ?? new Set<StorageListener>();
  listeners.add(listener);
  storageSubscribers.set(key, listeners);
  return () => {
    const current = storageSubscribers.get(key);
    if (!current) {
      return;
    }
    current.delete(listener);
    if (current.size === 0) {
      storageSubscribers.delete(key);
    }
  };
}

export function readLocalStorageValue<T>(
  key: string,
  defaultValue: LocalStorageDefaultValue<T>,
  options: LocalStorageReadOptions<T> = {},
): T {
  return parseLocalStorageValue(key, readLocalStorageRawValue(key), defaultValue, options);
}

export function writeLocalStorageValue<T>(key: string, value: T): void {
  const serialized = JSON.stringify(value);
  if (typeof serialized === "undefined") {
    removeLocalStorageValue(key);
    return;
  }

  const storage = getBrowserStorage();
  if (storage) {
    try {
      storage.setItem(key, serialized);
      invalidStorageWarnings.delete(key);
    } catch {
      // Ignore storage write failures and still notify same-tab subscribers.
    }
  }
  notifyStorageSubscribers(key);
}

export function removeLocalStorageValue(key: string): void {
  const storage = getBrowserStorage();
  if (storage) {
    try {
      storage.removeItem(key);
    } catch {
      // Ignore storage cleanup failures.
    }
  }
  invalidStorageWarnings.delete(key);
  notifyStorageSubscribers(key);
}

export function useLocalStorage<T>(
  key: string,
  defaultValue: LocalStorageDefaultValue<T>,
): readonly [T, Dispatch<SetStateAction<T>>] {
  const defaultValueRef = useRef<{ key: string; value: T } | null>(null);
  if (defaultValueRef.current === null || defaultValueRef.current.key !== key) {
    defaultValueRef.current = { key, value: resolveDefaultValue(defaultValue) };
  }
  const snapshotRef = useRef<{ key: string; raw: string | null; value: T } | null>(null);

  const getSnapshot = useCallback(
    () => {
      const raw = readLocalStorageRawValue(key);
      const cachedSnapshot = snapshotRef.current;
      if (cachedSnapshot && cachedSnapshot.key === key && cachedSnapshot.raw === raw) {
        return cachedSnapshot.value;
      }
      const nextValue = parseLocalStorageValue(
        key,
        raw,
        () => defaultValueRef.current?.value as T,
      );
      snapshotRef.current = { key, raw, value: nextValue };
      return nextValue;
    },
    [key],
  );

  const value = useSyncExternalStore(
    useCallback((listener) => subscribeToLocalStorageKey(key, listener), [key]),
    getSnapshot,
    () => defaultValueRef.current?.value as T,
  );

  const setValue = useCallback(
    (nextValue: SetStateAction<T>) => {
      const currentValue = getSnapshot();
      const resolvedValue =
        typeof nextValue === "function"
          ? (nextValue as (previousValue: T) => T)(currentValue)
          : nextValue;
      writeLocalStorageValue(key, resolvedValue);
    },
    [getSnapshot, key],
  );

  return [value, setValue] as const;
}
