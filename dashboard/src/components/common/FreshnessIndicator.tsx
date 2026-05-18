import { useEffect, useRef, useState, type CSSProperties } from "react";
import { getSystemHealthTone, type SystemHealthTone } from "../../theme/statusTokens";

interface FreshnessIndicatorProps {
  /**
   * Last-event timestamp. Accepts ms-since-epoch or ISO string.
   * Renders the `emptyLabel` (or "—") when null/undefined.
   */
  timestamp: number | string | null | undefined;
  /** Seconds after which the value is considered stale (warning tone). Default 30. */
  staleAfterSeconds?: number;
  /** Seconds after which the value is considered critically stale. Default 120. */
  criticalAfterSeconds?: number;
  /** Custom label prefix; defaults to "업데이트". */
  label?: string;
  /** Label shown when `timestamp` is null/undefined. Defaults to "—". */
  emptyLabel?: string;
  /** Render-time tick interval in ms. Default 5000. */
  tickMs?: number;
  /** Optional className/style passthrough. */
  className?: string;
  style?: CSSProperties;
  /** Compact form (omits the prefix label). */
  compact?: boolean;
  /**
   * Opt-in screen-reader announcement when the freshness *tone* escalates
   * (healthy → warning → critical). The relative-time text itself is never
   * announced — only the transition is, so we don't spam assistive tech with
   * "5초 전 ... 10초 전 ... 15초 전".
   * Off by default.
   */
  announceToneChange?: boolean;
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

type FreshnessTone = SystemHealthTone;

function deriveTone(
  deltaSeconds: number,
  staleAfterSeconds: number,
  criticalAfterSeconds: number,
): FreshnessTone {
  if (deltaSeconds >= criticalAfterSeconds) return "critical";
  if (deltaSeconds >= staleAfterSeconds) return "warning";
  return "healthy";
}

/**
 * Tiny "n초 전" indicator that escalates tone as data ages. Use this anywhere a
 * value depends on a real-time stream (WS event, polled metric, last sync) so
 * users can tell at a glance whether the screen reflects current reality.
 *
 * Renders nothing of significance when timestamp is null — explicit empty label
 * so it never silently looks fresh.
 *
 * A11y: the ticking time text is NOT a live region by default — that would
 * cause screen readers to repeat "5초 전, 10초 전, …" on every tick. Pass
 * `announceToneChange` to opt into one polite announcement per healthy ↔
 * warning ↔ critical transition.
 */
export function FreshnessIndicator({
  timestamp,
  staleAfterSeconds = 30,
  criticalAfterSeconds = 120,
  label = "업데이트",
  emptyLabel = "—",
  tickMs = 5_000,
  className,
  style,
  compact = false,
  announceToneChange = false,
}: FreshnessIndicatorProps) {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (timestamp == null) return;
    const id = window.setInterval(() => setNow(Date.now()), tickMs);
    return () => window.clearInterval(id);
  }, [timestamp, tickMs]);

  const ms = toMs(timestamp);
  // Track which tone we previously rendered so we can fire one polite
  // announcement *only* when the tone transitions, not on every tick.
  const lastToneRef = useRef<FreshnessTone | "empty" | null>(null);
  const announcedToneRef = useRef<FreshnessTone | "empty" | null>(null);

  if (ms == null) {
    const tone = getSystemHealthTone("unknown");
    lastToneRef.current = "empty";
    const shouldAnnounce =
      announceToneChange && announcedToneRef.current !== "empty";
    if (shouldAnnounce) announcedToneRef.current = "empty";
    return (
      <span
        className={className}
        style={{
          display: "inline-flex",
          alignItems: "center",
          gap: 4,
          fontSize: 11,
          color: tone.text,
          ...style,
        }}
        {...(shouldAnnounce ? { role: "status", "aria-live": "polite" } : {})}
      >
        <span
          aria-hidden
          style={{ width: 6, height: 6, borderRadius: "50%", background: tone.accent }}
        />
        {emptyLabel}
      </span>
    );
  }

  const deltaSeconds = Math.max(0, (now - ms) / 1000);
  const toneName = deriveTone(deltaSeconds, staleAfterSeconds, criticalAfterSeconds);
  const tone = getSystemHealthTone(toneName);
  const text = formatRelative(deltaSeconds);

  // Announcement only fires once per transition: if the tone changed since
  // last render *and* differs from the last announced tone, we attach the
  // live region attributes for this render. Subsequent renders at the same
  // tone are plain spans, so screen readers stay quiet.
  const toneChanged = lastToneRef.current !== toneName;
  const shouldAnnounce =
    announceToneChange && toneChanged && announcedToneRef.current !== toneName;
  if (shouldAnnounce) announcedToneRef.current = toneName;
  lastToneRef.current = toneName;

  return (
    <span
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
      {...(shouldAnnounce ? { role: "status", "aria-live": "polite" } : {})}
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
