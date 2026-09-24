import type { MachineResources } from "../../api/machineResources";
import type { SettingsTr } from "./SettingsPanelTypes";

export const HISTORY_WINDOW_MS = 15 * 60 * 1_000;
const HISTORY_POINT_LIMIT = 240;
const CHART_HEIGHT = 32;
const COLORS = {
  cpu: "#38bdf8", memory: "#60a5fa", gpu: "#c084fc",
  disk: "#a3e635", network: "#f472b6", networkOut: "#e879f9",
} as const;

export type TrendColor = keyof typeof COLORS;
export type TrendReading = { at: number; value: number | null; expiresAt?: number };

export function recentResources(history: MachineResources[], current: MachineResources | null | undefined, now: number) {
  const unique = new Map<number, MachineResources>();
  for (const sample of [...history, ...(current ? [current] : [])]) {
    if (sample.observed_at_ms >= now - HISTORY_WINDOW_MS && sample.observed_at_ms <= now + 5_000)
      unique.set(sample.observed_at_ms, sample);
  }
  return [...unique.values()].sort((a, b) => a.observed_at_ms - b.observed_at_ms).slice(-HISTORY_POINT_LIMIT);
}

export function readings(samples: MachineResources[], select: (sample: MachineResources) => number | null | undefined): TrendReading[] {
  return samples.map(sample => {
    const value = select(sample);
    return { at: sample.observed_at_ms, expiresAt: sample.expires_at_ms,
      value: value != null && Number.isFinite(value) && value >= 0 ? value : null };
  });
}

function lines(readings: TrendReading[], ceiling: number, start: number, end: number): string[] {
  const paths: string[] = [];
  let points: string[] = [];
  let previous: TrendReading | undefined;
  const flush = () => {
    if (points.length > 1) paths.push(points.join(" "));
    points = [];
  };
  for (const reading of readings) {
    if (previous?.expiresAt != null && reading.at > previous.expiresAt) flush();
    previous = reading;
    if (reading.value == null) { flush(); continue; }
    const x = (Math.max(0, Math.min(1, (reading.at - start) / Math.max(1, end - start))) * 100).toFixed(2);
    const y = (CHART_HEIGHT - Math.min(ceiling, reading.value) / ceiling * (CHART_HEIGHT - 2)).toFixed(2);
    points.push(`${points.length ? "L" : "M"}${x},${y}`);
  }
  flush();
  return paths;
}

export function MachineSparkline({ values, secondary, color, label, stale, tr, now = Date.now() }: {
  values: TrendReading[]; secondary?: TrendReading[]; color: TrendColor;
  label: string; stale: boolean; tr: SettingsTr; now?: number;
}) {
  const observed = [...values, ...(secondary ?? [])].flatMap(reading => reading.value == null ? [] : [reading.value]);
  const ceiling = color === "network" ? Math.max(1, ...observed) * 1.1 : 100;
  const start = now - HISTORY_WINDOW_MS;
  const end = now;
  const primaryLines = lines(values, ceiling, start, end);
  const secondaryLines = secondary ? lines(secondary, ceiling, start, end) : [];
  return <div className={`h-16 overflow-hidden border ${stale ? "opacity-60" : ""}`}
    style={{ borderColor: `${COLORS[color]}70`, backgroundColor: `${COLORS[color]}14` }} data-testid={`machine-trend-${color}`}>
    {primaryLines.length || secondaryLines.length ? <svg viewBox={`0 0 100 ${CHART_HEIGHT}`} preserveAspectRatio="none"
      className="h-full w-full" role="img" aria-label={`${label} ${tr("최근 15분 추이", "last 15 minutes trend")}`}>
      <path d={`M0,${CHART_HEIGHT} L100,${CHART_HEIGHT}`} stroke="currentColor" className="text-th-border" strokeWidth="0.7" />
      {primaryLines.map((path, index) => <path key={`primary-${index}`} d={path} fill="none" stroke={COLORS[color]} strokeWidth="1.6" vectorEffect="non-scaling-stroke" />)}
      {secondaryLines.map((path, index) => <path key={`secondary-${index}`} d={path} fill="none" stroke={COLORS.networkOut} strokeWidth="1.4" vectorEffect="non-scaling-stroke" />)}
    </svg> : <p className="flex h-full items-center justify-center px-1 text-center text-[10px] text-th-text-muted">{tr("측정값 수집 중", "Collecting samples")}</p>}
  </div>;
}

export const metricColors = COLORS;
