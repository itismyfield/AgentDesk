import { useEffect, useRef, useState } from "react";
import type { WSEvent } from "../types";

type RawDashboardSocketEvent = Partial<WSEvent> & {
  type?: unknown;
  payload?: unknown;
  data?: unknown;
  ts?: unknown;
};

export function normalizeDashboardSocketEvent(raw: unknown): WSEvent | null {
  if (!raw || typeof raw !== "object") return null;
  const event = raw as RawDashboardSocketEvent;
  if (typeof event.type !== "string") return null;
  return {
    type: event.type as WSEvent["type"],
    payload: event.payload ?? event.data ?? null,
    ts: typeof event.ts === "number" ? event.ts : undefined,
  };
}

export function useDashboardSocket(onEvent: (event: WSEvent) => void) {
  const [wsConnected, setWsConnected] = useState(false);
  const wsRef = useRef<WebSocket | null>(null);
  const wsRetryRef = useRef(0);
  const wsTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onEventRef = useRef(onEvent);

  useEffect(() => {
    onEventRef.current = onEvent;
  }, [onEvent]);

  useEffect(() => {
    let destroyed = false;

    function connect() {
      if (destroyed) return;
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      const ws = new WebSocket(`${proto}//${location.host}/ws`);
      wsRef.current = ws;

      ws.onopen = () => {
        wsRetryRef.current = 0;
        setWsConnected(true);
      };

      ws.onmessage = (ev) => {
        try {
          const event = normalizeDashboardSocketEvent(JSON.parse(ev.data));
          if (!event) return;
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
