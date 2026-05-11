import { useEffect, useRef, useState } from "react";
import type { WSEvent } from "../types";

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

    // Seed from localStorage so the very first connection after a full reload
    // can still replay events the server still has in its history window.
    lastEventIdRef.current = readPersistedLastEventId();

    function connect() {
      if (destroyed) return;
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      const since = lastEventIdRef.current;
      const url =
        since && since.length > 0
          ? `${proto}//${location.host}/ws?since=${encodeURIComponent(since)}`
          : `${proto}//${location.host}/ws`;
      const ws = new WebSocket(url);
      wsRef.current = ws;

      ws.onopen = () => {
        wsRetryRef.current = 0;
        setWsConnected(true);
      };

      ws.onmessage = (ev) => {
        try {
          const event = normalizeDashboardSocketEvent(JSON.parse(ev.data));
          if (!event) return;
          if (event.id) {
            lastEventIdRef.current = event.id;
            writePersistedLastEventId(event.id);
          }
          onEventRef.current(event);
          window.dispatchEvent(new CustomEvent("pcd-ws-event", { detail: event }));
        } catch {
          // ignore malformed ws payload
        }
      };

      ws.onclose = () => {
        setWsConnected(false);
        wsRef.current = null;
        if (destroyed) return;
        const delay = Math.min(1000 * 2 ** wsRetryRef.current, 30000);
        wsRetryRef.current += 1;
        wsTimerRef.current = setTimeout(connect, delay);
      };

      ws.onerror = () => {
        ws.close();
      };
    }

    connect();

    return () => {
      destroyed = true;
      if (wsTimerRef.current) clearTimeout(wsTimerRef.current);
      wsRef.current?.close();
    };
  }, []);

  return { wsConnected, wsRef };
}
