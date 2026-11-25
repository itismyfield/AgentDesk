import { useCallback, useRef, useSyncExternalStore } from "react";

interface ClockEntry {
  now: number;
  listeners: Set<() => void>;
  timer: number | null;
}

const clockBuckets = new Map<number, ClockEntry>();
let visibilitySubscribed = false;

function isHidden(): boolean {
  return typeof document !== "undefined" && document.hidden === true;
}

function tickBucket(entry: ClockEntry): void {
  entry.now = Date.now();
  for (const listener of entry.listeners) {
    listener();
  }
}

function startBucketTimer(intervalMs: number, entry: ClockEntry): void {
  if (entry.timer !== null || isHidden() || typeof window === "undefined") return;
  entry.timer = window.setInterval(() => tickBucket(entry), intervalMs);
}

function stopBucketTimer(entry: ClockEntry): void {
  if (entry.timer === null || typeof window === "undefined") return;
  window.clearInterval(entry.timer);
  entry.timer = null;
}

function handleVisibilityChange(): void {
  const hidden = isHidden();
  for (const [intervalMs, entry] of clockBuckets) {
    if (hidden) {
      stopBucketTimer(entry);
    } else {
      tickBucket(entry);
      startBucketTimer(intervalMs, entry);
    }
  }
}

function ensureVisibilityListener(): void {
  if (visibilitySubscribed || typeof document === "undefined") return;
  document.addEventListener("visibilitychange", handleVisibilityChange);
  visibilitySubscribed = true;
}

function releaseVisibilityListenerIfIdle(): void {
  if (!visibilitySubscribed || clockBuckets.size > 0 || typeof document === "undefined") return;
  document.removeEventListener("visibilitychange", handleVisibilityChange);
  visibilitySubscribed = false;
}

const noopSubscribe = () => () => {};
const getServerSnapshot = () => Date.now();

/** Shared reactive clock: deduplicates `setInterval` per `intervalMs` and pauses while hidden. */
export function useNow(intervalMs = 1_000, enabled = true): number {
  const safeIntervalMs = Math.max(1, Math.floor(intervalMs));
  const lastNowRef = useRef<number | null>(null);
  if (lastNowRef.current === null) {
    lastNowRef.current = Date.now();
  }

  const subscribe = useCallback(
    (onStoreChange: () => void) => {
      let entry = clockBuckets.get(safeIntervalMs);
      if (!entry) {
        entry = { now: Date.now(), listeners: new Set(), timer: null };
        clockBuckets.set(safeIntervalMs, entry);
      }
      entry.listeners.add(onStoreChange);
      ensureVisibilityListener();
      startBucketTimer(safeIntervalMs, entry);

      return () => {
        const current = clockBuckets.get(safeIntervalMs);
        if (!current) return;
        current.listeners.delete(onStoreChange);
        if (current.listeners.size === 0) {
          stopBucketTimer(current);
          clockBuckets.delete(safeIntervalMs);
          releaseVisibilityListenerIfIdle();
        }
      };
    },
    [safeIntervalMs],
  );

  const getSnapshot = useCallback(() => {
    if (!enabled) return lastNowRef.current!;
    const activeNow = clockBuckets.get(safeIntervalMs)?.now;
    if (activeNow !== undefined) {
      lastNowRef.current = activeNow;
      return activeNow;
    }
    return lastNowRef.current!;
  }, [enabled, safeIntervalMs]);

  return useSyncExternalStore(enabled ? subscribe : noopSubscribe, getSnapshot, getServerSnapshot);
}
