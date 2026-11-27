import { useEffect, useRef, useState } from "react";
import type { WSEvent } from "../types";
import { credentialScope, onCredentialChange } from "../api/authState";
import { getDashboardSocketTicket } from "../api/dashboardAuth";

type RawDashboardSocketEvent = Partial<WSEvent> & {
  type?: unknown;
  payload?: unknown;
  data?: unknown;
  ts?: unknown;
  id?: unknown;
};

// Envelope id (numeric string assigned by BroadcastBus on the server). Tracked so
// we can ask the server to replay any events that were emitted while we were
// disconnected — see #2050 P1 finding 2.
export interface WSEventWithId extends WSEvent {
  /** Server-assigned envelope id (numeric string), if present. */
  id?: string;
}

export function normalizeDashboardSocketEvent(raw: unknown): WSEventWithId | null {
  if (!raw || typeof raw !== "object") return null;
  const event = raw as RawDashboardSocketEvent;
  if (typeof event.type !== "string") return null;
  const id =
    typeof event.id === "string"
      ? event.id
      : typeof event.id === "number"
        ? String(event.id)
        : undefined;
  return {
    type: event.type as WSEvent["type"],
    payload: event.payload ?? event.data ?? null,
    ts: typeof event.ts === "number" ? event.ts : undefined,
    id,
  };
}

const LAST_EVENT_ID_STORAGE_KEY = "adk:ws:last-event-id";
// Back off from one second to thirty seconds while the server is unavailable.
const RECONNECT_INITIAL_DELAY_MS = 1_000;
const RECONNECT_MAX_DELAY_MS = 30_000;
const EVENT_META_FLUSH_INTERVAL_MS = 1_000;

export function isNumericEventId(id: string): boolean {
  return /^\d+$/.test(id);
}

onCredentialChange(() => {
  try { window.localStorage.removeItem(LAST_EVENT_ID_STORAGE_KEY); } catch { /* storage is optional */ }
});

function readPersistedLastEventId(): string | null {
  try {
    return window.localStorage.getItem(LAST_EVENT_ID_STORAGE_KEY);
  } catch {
    return null;
  }
}

function writePersistedLastEventId(id: string): void {
  try {
    window.localStorage.setItem(LAST_EVENT_ID_STORAGE_KEY, id);
  } catch {
    // ignore quota/permission errors — replay is a best-effort optimization.
  }
}

export function useDashboardSocket(onEvent: (event: WSEvent) => void) {
  const [wsConnected, setWsConnected] = useState(false);
  // Wall-clock timestamp (ms) of the most recently received WS event. Drives
  // freshness indicators across the dashboard — null until the first event.
  const [lastEventTs, setLastEventTs] = useState<number | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const wsRetryRef = useRef(0);
  const wsTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onEventRef = useRef(onEvent);
  // Newest event id observed during this tab's lifetime (or persisted from a
  // previous session). Used for replay_since on reconnect.
  const lastEventIdRef = useRef<string | null>(null);

  useEffect(() => {
    onEventRef.current = onEvent;
  }, [onEvent]);

  useEffect(() => {
    let destroyed = false;
    const auth = credentialScope();
    const ticketRequest = new AbortController();
    let flushTimer: ReturnType<typeof setTimeout> | null = null;
    let lastFlushAt: number | null = null;
    let pendingTs: number | null = null;
    let hasPendingEventId = false;

    // Seed from localStorage so the very first connection after a full reload
    // can still replay events the server still has in its history window.
    lastEventIdRef.current = readPersistedLastEventId();

    function clearFlushTimer() {
      if (flushTimer !== null) {
        clearTimeout(flushTimer);
        flushTimer = null;
      }
    }

    function flushPersistedEventId() {
      clearFlushTimer();
      pendingTs = null;
      if (hasPendingEventId && lastEventIdRef.current !== null && !auth.signal.aborted) {
        writePersistedLastEventId(lastEventIdRef.current);
        hasPendingEventId = false;
      }
    }

    function flushEventMeta(now: number) {
      clearFlushTimer();
      lastFlushAt = now;
      if (pendingTs !== null && !destroyed && !auth.signal.aborted) {
        setLastEventTs(pendingTs);
      }
      pendingTs = null;
      if (hasPendingEventId && lastEventIdRef.current !== null && !auth.signal.aborted) {
        writePersistedLastEventId(lastEventIdRef.current);
        hasPendingEventId = false;
      }
    }

    function recordEventMeta(eventId: string | undefined) {
      const now = Date.now();
      pendingTs = now;
      if (eventId) {
        lastEventIdRef.current = eventId;
        hasPendingEventId = true;
      }
      const elapsed = lastFlushAt === null ? EVENT_META_FLUSH_INTERVAL_MS : now - lastFlushAt;
      if (elapsed >= EVENT_META_FLUSH_INTERVAL_MS) {
        flushEventMeta(now);
        return;
      }
      if (flushTimer === null) {
        flushTimer = setTimeout(() => {
          flushTimer = null;
          if (destroyed || auth.signal.aborted) return;
          flushEventMeta(Date.now());
        }, EVENT_META_FLUSH_INTERVAL_MS - elapsed);
      }
    }

    const unsubscribeCredential = onCredentialChange(() => {
      clearFlushTimer();
      pendingTs = null;
      hasPendingEventId = false;
      lastEventIdRef.current = null;
      try { window.localStorage.removeItem(LAST_EVENT_ID_STORAGE_KEY); } catch { /* storage is optional */ }
    });

    function scheduleReconnect() {
      if (destroyed || auth.signal.aborted) return;
      if (wsTimerRef.current !== null) {
        clearTimeout(wsTimerRef.current);
        wsTimerRef.current = null;
      }
      const baseDelay = Math.min(RECONNECT_INITIAL_DELAY_MS * 2 ** wsRetryRef.current, RECONNECT_MAX_DELAY_MS);
      const delay = Math.round(baseDelay * (0.5 + Math.random() * 0.5));
      wsRetryRef.current += 1;
      wsTimerRef.current = setTimeout(() => {
        wsTimerRef.current = null;
        void connect();
      }, delay);
    }

    async function connect() {
      if (destroyed || auth.signal.aborted) return;
      let ticket: string;
      try {
        ({ ticket } = await getDashboardSocketTicket(ticketRequest.signal));
      } catch {
        scheduleReconnect();
        return;
      }
      if (destroyed || auth.signal.aborted) return;
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      const since = lastEventIdRef.current;
      const params = new URLSearchParams({ ticket });
      if (since) params.set("since", since);
      const url = `${proto}//${location.host}/ws?${params}`;
      const ws = new WebSocket(url);
      wsRef.current = ws;

      ws.onopen = () => {
        wsRetryRef.current = 0;
        setWsConnected(true);
      };

      ws.onmessage = (ev) => {
        if (destroyed || auth.signal.aborted) return;
        try {
          const event = normalizeDashboardSocketEvent(JSON.parse(ev.data));
          if (!event) return;
          // Server subscribes before replay_since, so dedupe overlapping numeric envelope ids.
          if (
            event.id &&
            isNumericEventId(event.id) &&
            lastEventIdRef.current &&
            isNumericEventId(lastEventIdRef.current) &&
            BigInt(event.id) <= BigInt(lastEventIdRef.current)
          ) {
            return;
          }
          recordEventMeta(event.id);
          onEventRef.current(event);
          window.dispatchEvent(new CustomEvent("pcd-ws-event", { detail: event }));
        } catch {
          // ignore malformed ws payload
        }
      };

      ws.onclose = () => {
        if (!destroyed && !auth.signal.aborted && pendingTs !== null) {
          flushEventMeta(Date.now());
        } else {
          flushPersistedEventId();
        }
        if (!destroyed) setWsConnected(false);
        wsRef.current = null;
        if (destroyed) return;
        scheduleReconnect();
      };

      ws.onerror = () => {
        ws.close();
      };
    }

    void connect();

    const shutdown = () => {
      destroyed = true;
      ticketRequest.abort();
      flushPersistedEventId();
      if (wsTimerRef.current !== null) {
        clearTimeout(wsTimerRef.current);
        wsTimerRef.current = null;
      }
      wsRef.current?.close();
    };
    auth.signal.addEventListener("abort", shutdown);
    return () => {
      unsubscribeCredential();
      auth.signal.removeEventListener("abort", shutdown);
      shutdown();
    };
  }, []);

  return { wsConnected, wsRef, lastEventTs };
}
