import { useEffect, useState, type CSSProperties } from "react";
import { getSystemHealthTone } from "../../theme/statusTokens";

interface FreshnessIndicatorProps {
  /**
   * Last-event timestamp. Accepts ms-since-epoch or ISO string.
   * `null`/`undefined` is rendered as "데이터 없음".
   */
  timestamp: number | string | null | undefined;
  /** Seconds after which the value is considered stale (warning tone). Default 30. */
  staleAfterSeconds?: number;
  /** Seconds after which the value is considered critically stale. Default 120. */
  criticalAfterSeconds?: number;
  /** Custom label prefix; defaults to "업데이트". */
  label?: string;
  /** Render-time tick interval in ms. Default 5000. */
  tickMs?: number;
  /** Optional className/style passthrough. */
  className?: string;
  style?: CSSProperties;
  /** Compact form (omits the prefix label). */
  compact?: boolean;
}

function toMs(value: number | string | null | undefined): number | null {
  if (value == null) return null;
  if (typeof value === "number") return value > 1e12 ? value : value * 1000;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function formatRelative(deltaSeconds: number): string {
  if (deltaSeconds < 5) return "방금";
  if (deltaSeconds < 60) return `${Math.floor(deltaSeconds)}초 전`;
  if (deltaSeconds < 3600) return `${Math.floor(deltaSeconds / 60)}분 전`;
  if (deltaSeconds < 86400) return `${Math.floor(deltaSeconds / 3600)}시간 전`;
  return `${Math.floor(deltaSeconds / 86400)}일 전`;
}

/**
 * Tiny "n초 전" indicator that escalates tone as data ages. Use this anywhere a
 * value depends on a real-time stream (WS event, polled metric, last sync) so
 * users can tell at a glance whether the screen reflects current reality.
 *
 * Renders nothing of significance when timestamp is null — explicit "데이터 없음"
 * so it never silently looks fresh.
 */
export function FreshnessIndicator({
  timestamp,
  staleAfterSeconds = 30,
  criticalAfterSeconds = 120,
  label = "업데이트",
  tickMs = 5_000,
  className,
  style,
  compact = false,
}: FreshnessIndicatorProps) {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (timestamp == null) return;
    const id = window.setInterval(() => setNow(Date.now()), tickMs);
    return () => window.clearInterval(id);
  }, [timestamp, tickMs]);

  const ms = toMs(timestamp);
  if (ms == null) {
    const tone = getSystemHealthTone("unknown");
    return (
      <span
        role="status"
        className={className}
        style={{
          display: "inline-flex",
          alignItems: "center",
          gap: 4,
          fontSize: 11,
          color: tone.text,
          ...style,
        }}
      >
        <span
          aria-hidden
          style={{ width: 6, height: 6, borderRadius: "50%", background: tone.accent }}
        />
        데이터 없음
      </span>
    );
  }

  const deltaSeconds = Math.max(0, (now - ms) / 1000);
  const toneName =
    deltaSeconds >= criticalAfterSeconds
      ? "critical"
      : deltaSeconds >= staleAfterSeconds
        ? "warning"
        : "healthy";
  const tone = getSystemHealthTone(toneName);
  const text = formatRelative(deltaSeconds);

  return (
    <span
      role="status"
      aria-live="polite"
      title={new Date(ms).toLocaleString()}
      className={className}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 4,
        fontSize: 11,
        color: tone.text,
        ...style,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 6,
          height: 6,
          borderRadius: "50%",
          background: tone.accent,
          flexShrink: 0,
        }}
      />
      {compact ? text : `${label} · ${text}`}
    </span>
  );
}
