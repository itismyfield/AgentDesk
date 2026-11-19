import type { ReactNode } from "react";
import { Link } from "react-router";
import { Badge, type BadgeTone } from "../../kit/free/badge-clean";
import { Loading, Skeleton } from "../../kit/free/skeleton-clean";
import type { HomeTone, StageSegment } from "./homeModel";

export const TONE_COLOR: Record<HomeTone, string> = {
  ok: "var(--ok)",
  info: "var(--info)",
  warn: "var(--warn)",
  err: "var(--err)",
  neutral: "var(--fg-faint)",
};
const BADGE_TONE: Record<HomeTone, BadgeTone> = { ok: "success", info: "info", warn: "warning", err: "danger", neutral: "neutral" };
export const DONE_COLOR = "color-mix(in srgb, var(--ok) 70%, var(--bg-3))";
const SEGMENT_COLOR: Record<StageSegment, string> = { done: DONE_COLOR, now: "var(--info)", fail: "var(--err)", todo: "var(--bg-3)" };
/** Row hover tint shared with the routines list. */
export const ROW_HOVER = "hover:bg-[color-mix(in_oklch,var(--fg)_5%,transparent)]";

/** Text color for a value: only warnings and failures leave the body color. */
export function toneText(tone: HomeTone): string | undefined {
  return tone === "warn" || tone === "err" ? TONE_COLOR[tone] : undefined;
}

export function Dot({ tone }: { tone: HomeTone }) {
  return <span aria-hidden className="size-2 shrink-0 rounded-full" style={{ background: TONE_COLOR[tone] }} />;
}

export function Chip({ tone, children }: { tone: HomeTone; children: ReactNode }) {
  return <Badge tone={BADGE_TONE[tone]} className="shrink-0">{children}</Badge>;
}

/** Card frame shared by every home section: a title row, then the body flush with the card edges. */
export function HomeCard({ title, count, countTone, aside, testId, className = "", children }: {
  title: string;
  count?: number | null;
  countTone?: HomeTone;
  aside?: ReactNode;
  testId?: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <section
      aria-label={title}
      data-testid={testId}
      className={`min-w-0 overflow-hidden rounded-[var(--r-lg)] border border-[var(--line-soft)] bg-[var(--bg-2)] text-[var(--fg)] ${className}`}
    >
      <header className="flex min-h-12 flex-wrap items-center gap-x-3 px-4 py-0.5">
        <h2 className="m-0 flex min-h-11 flex-[1_0_auto] items-center text-sm font-semibold">
          {title}
          {count != null ? (
            <span className="ml-1.5 tabular-nums" style={{ color: countTone ? TONE_COLOR[countTone] : "var(--fg-muted)" }}>{count}</span>
          ) : null}
        </h2>
        {aside}
      </header>
      {children}
    </section>
  );
}

export function HeaderLink({ to, children }: { to: string; children: ReactNode }) {
  return (
    <Link
      to={to}
      className="-mx-1.5 inline-flex min-h-11 shrink-0 items-center rounded-[var(--r-sm)] px-1.5 text-xs font-medium text-[var(--accent)] hover:underline focus-visible:outline-2 focus-visible:outline-[var(--accent)]"
    >
      {children}
    </Link>
  );
}

export function Muted({ children, className = "" }: { children: ReactNode; className?: string }) {
  return <span className={`text-xs text-[var(--fg-muted)] ${className}`}>{children}</span>;
}

/** A short line in place of rows: empty, failed or not measured. */
export function CardNote({ children }: { children: ReactNode }) {
  return <p className="m-0 border-t border-[var(--line-soft)] px-4 py-5 text-sm text-[var(--fg-muted)]">{children}</p>;
}

export function CardSkeleton({ label, rows = 3 }: { label: string; rows?: number }) {
  return (
    <Loading label={label} className="flex flex-col gap-3 border-t border-[var(--line-soft)] px-4 py-4">
      {Array.from({ length: rows }, (_, index) => <Skeleton key={index} width={`${90 - index * 15}%`} height={10} line={20} />)}
    </Loading>
  );
}

export function StageTrack({ segments }: { segments: StageSegment[] }) {
  return (
    <span aria-hidden className="flex gap-[3px]">
      {segments.map((segment, index) => (
        <i key={index} className="h-1.5 flex-1 rounded-full" style={{ background: SEGMENT_COLOR[segment] }} />
      ))}
    </span>
  );
}

export function RatioBar({ ratio, color = "var(--accent)" }: { ratio: number; color?: string }) {
  return (
    <span aria-hidden className="block h-1.5 overflow-hidden rounded-full bg-[var(--bg-3)]">
      <i className="block h-full rounded-full" style={{ width: `${Math.min(100, Math.max(0, ratio * 100))}%`, background: color }} />
    </span>
  );
}
