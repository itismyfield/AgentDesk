import { useEffect, useRef, useState } from "react";
import * as api from "../../api";
import type { DispatchDeliveryEvent, UiLanguage } from "../../types";
import {
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
} from "../common/SurfacePrimitives";
import { formatIso } from "./kanban-utils";

const POLL_INTERVAL_MS = 5_000;

export const DELIVERY_EVENT_STATUS_STYLE = {
  reserved: { label: "reserved", color: "#9ca3af", background: "rgba(156,163,175,0.12)" },
  sent: { label: "sent", color: "#4ade80", background: "rgba(34,197,94,0.12)" },
  fallback: { label: "fallback", color: "#fb923c", background: "rgba(249,115,22,0.14)" },
  duplicate: { label: "duplicate", color: "#60a5fa", background: "rgba(59,130,246,0.14)" },
  skipped: { label: "skipped", color: "#9ca3af", background: "rgba(156,163,175,0.12)" },
  failed: { label: "failed", color: "#f87171", background: "rgba(239,68,68,0.14)" },
} as const;

export function summarizeDeliveryError(error: string | null | undefined): string {
  if (!error?.trim()) return "-";
  const compact = error.trim().replace(/\s+/g, " ");
  return compact.length > 96 ? `${compact.slice(0, 93)}...` : compact;
}

interface DispatchDeliveryEventsPanelProps {
  dispatchId: string | null;
  locale: UiLanguage;
  tr: (ko: string, en: string) => string;
}

export default function DispatchDeliveryEventsPanel({
  dispatchId,
  locale,
  tr,
}: DispatchDeliveryEventsPanelProps) {
  const rootRef = useRef<HTMLDivElement | null>(null);
  const [isVisible, setIsVisible] = useState(true);
  const [events, setEvents] = useState<DispatchDeliveryEvent[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const node = rootRef.current;
    if (!node || typeof IntersectionObserver === "undefined") {
      setIsVisible(true);
      return;
    }
    const observer = new IntersectionObserver(
      ([entry]) => setIsVisible(Boolean(entry?.isIntersecting)),
      { threshold: 0.05 },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    setEvents([]);
    setError(null);
  }, [dispatchId]);

  useEffect(() => {
    if (!dispatchId || !isVisible) return;
    let cancelled = false;
    let intervalId: number | null = null;

    const load = async (showLoading: boolean) => {
      if (showLoading) setLoading(true);
      try {
        const response = await api.getDispatchDeliveryEvents(dispatchId);
        if (cancelled) return;
        setEvents(response.events);
        setError(null);
      } catch (loadError) {
        if (cancelled) return;
        setError(
          loadError instanceof Error
            ? loadError.message
            : tr("Delivery events를 불러오지 못했습니다.", "Failed to load delivery events."),
        );
      } finally {
        if (!cancelled && showLoading) setLoading(false);
      }
    };

    void load(true);
    intervalId = window.setInterval(() => void load(false), POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      if (intervalId !== null) window.clearInterval(intervalId);
    };
  }, [dispatchId, isVisible, tr]);

  if (!dispatchId) return null;

  return (
    <div ref={rootRef}>
      <SurfaceCard className="space-y-3">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0">
          <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
            {tr("Delivery Events", "Delivery Events")}
            <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
              #{dispatchId.slice(0, 8)}
            </span>
          </h4>
          <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {loading ? tr("불러오는 중...", "Loading...") : tr("5초마다 갱신", "Refreshes every 5s")}
          </div>
        </div>
        <span
          className="rounded-full border px-2 py-1 text-xs"
          style={{
            borderColor: "rgba(148,163,184,0.18)",
            backgroundColor: "rgba(148,163,184,0.08)",
            color: "var(--th-text-secondary)",
          }}
        >
          {events.length}
        </span>
      </div>

      {error && (
        <SurfaceNotice tone="danger" compact>
          {tr("Delivery events API 오류", "Delivery events API error")}: {error}
        </SurfaceNotice>
      )}

      {events.length === 0 && !loading ? (
        <SurfaceEmptyState className="text-sm">
          {tr("No delivery events recorded", "No delivery events recorded")}
        </SurfaceEmptyState>
      ) : (
        <div className="-mx-2 overflow-x-auto px-2">
          <table className="min-w-[720px] w-full table-fixed text-left text-xs">
            <thead style={{ color: "var(--th-text-muted)" }}>
              <tr className="[&>th]:px-2 [&>th]:py-2 [&>th]:font-medium">
                <th className="w-[150px]">{tr("created_at", "created_at")}</th>
                <th className="w-[104px]">{tr("status", "status")}</th>
                <th className="w-[74px]">{tr("attempt", "attempt")}</th>
                <th className="w-[160px]">{tr("target_channel_id", "target_channel_id")}</th>
                <th className="w-[150px]">{tr("message_id", "message_id")}</th>
                <th>{tr("error 요약", "error summary")}</th>
              </tr>
            </thead>
            <tbody>
              {events.map((event) => {
                const statusStyle =
                  DELIVERY_EVENT_STATUS_STYLE[event.status] ??
                  DELIVERY_EVENT_STATUS_STYLE.reserved;
                return (
                  <tr
                    key={event.id}
                    className="border-t align-top"
                    style={{ borderColor: "rgba(148,163,184,0.12)" }}
                  >
                    <td className="px-2 py-2 font-mono" style={{ color: "var(--th-text-secondary)" }}>
                      {formatIso(event.created_at, locale)}
                    </td>
                    <td className="px-2 py-2">
                      <span
                        className="inline-flex rounded-md px-2 py-0.5 font-medium"
                        style={{
                          backgroundColor: statusStyle.background,
                          color: statusStyle.color,
                        }}
                      >
                        {statusStyle.label}
                      </span>
                    </td>
                    <td className="px-2 py-2 font-mono" style={{ color: "var(--th-text-secondary)" }}>
                      {event.attempt}
                    </td>
                    <td className="truncate px-2 py-2 font-mono" title={event.target_channel_id ?? ""} style={{ color: "var(--th-text-secondary)" }}>
                      {event.target_channel_id ?? "-"}
                    </td>
                    <td className="truncate px-2 py-2 font-mono" title={event.message_id ?? ""} style={{ color: "var(--th-text-secondary)" }}>
                      {event.message_id ?? "-"}
                    </td>
                    <td className="px-2 py-2" title={event.error ?? ""} style={{ color: event.error ? "#fca5a5" : "var(--th-text-muted)" }}>
                      {summarizeDeliveryError(event.error)}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      </SurfaceCard>
    </div>
  );
}
