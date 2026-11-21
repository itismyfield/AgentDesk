import type { ButtonHTMLAttributes, CSSProperties, ElementType, HTMLAttributes, ReactNode } from "react";
import { Link, type LinkProps } from "react-router";
import { twMerge } from "tailwind-merge";
import { Badge } from "../../kit/free/badge-clean";
import { Card, CardContent, CardFooter, CardHeader, type CardProps } from "../../kit/free/card-clean";
import { Loading, Skeleton } from "../../kit/free/skeleton-clean";

export { CardContent, CardFooter, CardHeader };

export type DotTone = "neutral" | "ok" | "info" | "warn" | "err";
export type ChipTone = DotTone | "accent";
export type TrackSegment = "done" | "now" | "fail" | "wait" | "todo";

export const cn = (...classes: Array<string | undefined | null | false>) => twMerge(classes.filter(Boolean).join(" "));

export const TONE_COLOR: Record<DotTone, string> = {
  neutral: "var(--fg-faint)",
  ok: "var(--ok)",
  info: "var(--info)",
  warn: "var(--warn)",
  err: "var(--err)",
};

export const DONE_TRACK_COLOR = "color-mix(in srgb, var(--ok) 70%, var(--bg-3))";

const TRACK_COLOR: Record<TrackSegment, string> = {
  done: DONE_TRACK_COLOR,
  now: "var(--info)",
  fail: "var(--err)",
  wait: "var(--warn)",
  todo: "var(--bg-3)",
};

const METER_COLOR: Record<"accent" | "ok" | "info" | "warn" | "err" | "muted", string> = {
  accent: "var(--accent)",
  ok: DONE_TRACK_COLOR,
  info: "var(--info)",
  warn: "var(--warn)",
  err: "var(--err)",
  muted: "var(--fg-muted)",
};

const CHIP_STYLE: Record<ChipTone, CSSProperties> = {
  neutral: { background: "var(--bg-3)", color: "var(--fg-dim)" },
  ok: { background: "var(--ok-soft)", color: "var(--ok-fg)" },
  info: { background: "var(--info-soft)", color: "var(--info-fg)" },
  warn: { background: "var(--warn-soft)", color: "var(--warn-fg)" },
  err: { background: "var(--err-soft)", color: "var(--err-fg)" },
  accent: { background: "var(--accent-soft)", color: "var(--accent-fg)" },
};

const COUNT_BADGE_VARS = {
  "--ui-bdgc-neutral-bg": "var(--bg-3)",
  "--ui-bdgc-neutral-fg": "var(--fg-dim)",
  "--ui-bdgc-warning-bg": "var(--warn-soft)",
  "--ui-bdgc-warning-fg": "var(--warn-fg)",
  "--ui-bdgc-danger-bg": "var(--err-soft)",
  "--ui-bdgc-danger-fg": "var(--err-fg)",
} as CSSProperties;

/** Shared hover tint and selected-row accent bar for list and inspector rows. */
export const ROW_HOVER_CLASS = "transition-colors hover:bg-[color-mix(in_oklch,var(--fg)_5%,transparent)]";
export const SELECTED_ROW_STYLE: CSSProperties = {
  background: "color-mix(in srgb, var(--accent) 8%, var(--bg-2))",
  boxShadow: "inset 2px 0 0 var(--accent)",
};

/** Value color helper: only warning and error states leave the body text color. */
export function toneText(tone: DotTone): string | undefined {
  return tone === "warn" || tone === "err" ? TONE_COLOR[tone] : undefined;
}

/** Kit card mapped onto dashboard surface tokens (`--bg-2`, `--line-soft`, 14px radius, 14px/20px body).
 * Unlayered `card-clean` CSS owns root layout, padding, surface, and 44px coarse targets; set root layout via props/inline `style` and place inner layout on a child wrapper. */
export function KitCard({
  pad = "none",
  className,
  style,
  ...rest
}: CardProps & { pad?: "none" | "sm" | "md" }) {
  return (
    <Card
      {...rest}
      className={cn(pad === "none" && "overflow-hidden", className)}
      style={{
        "--ui-crdc-radius": "var(--r-md)",
        "--ui-crdc-shadow": "none",
        "--ui-crdc-pad": pad === "md" ? "16px" : pad === "sm" ? "12px" : "0px",
        gap: pad === "none" ? 0 : pad === "sm" ? 10 : 12,
        fontSize: "14px",
        lineHeight: "20px",
        border: "1px solid var(--line-soft)",
        ...style,
      } as CSSProperties}
    />
  );
}

/** Flush section card with a title row, optional count, and trailing header slot. */
export function SectionCard({
  title,
  count,
  countTone,
  aside,
  testId,
  className,
  children,
}: {
  title: string;
  count?: number | null;
  countTone?: DotTone;
  aside?: ReactNode;
  testId?: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <KitCard as="section" aria-label={title} data-testid={testId} className={className}>
      <header className="flex min-h-11 flex-wrap items-center gap-x-3 gap-y-1 px-3 py-3 sm:px-4">
        <h2 className="m-0 flex flex-[1_0_auto] items-baseline gap-1.5 text-sm leading-5 font-semibold">
          {title}
          {count != null ? (
            <span
              className={cn("font-display tabular-nums", !countTone && "text-xs leading-4 font-normal text-[var(--fg-muted)]")}
              style={countTone ? { color: TONE_COLOR[countTone] } : undefined}
            >
              {count}
            </span>
          ) : null}
        </h2>
        {aside}
      </header>
      {children}
    </KitCard>
  );
}

/** Recessed container (`--bg-1` with `--line-soft` border and 14px radius) for strips, code wells, and columns. */
export function Well({
  as = "div",
  className,
  ...rest
}: HTMLAttributes<HTMLElement> & { as?: "div" | "section" | "aside" }) {
  const Tag = as as ElementType;
  return (
    <Tag
      {...rest}
      className={cn("min-w-0 rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)] text-[var(--fg)]", className)}
    />
  );
}

/** Non-interactive hairline-separated row inside a card or well (`selected` is visual only; use `RowLink` or `RowButton` for interactive rows). */
export function Row({
  as = "div",
  density = "md",
  divider = true,
  selected = false,
  className,
  style,
  ...rest
}: HTMLAttributes<HTMLElement> & {
  as?: "div" | "li";
  density?: "sm" | "md";
  divider?: boolean;
  selected?: boolean;
}) {
  const Tag = as as ElementType;
  return (
    <Tag
      {...rest}
      className={cn(
        "flex min-w-0 items-center gap-3",
        density === "sm" ? "px-3 py-2.5 sm:px-4" : "px-3 py-3 sm:px-4",
        divider && "border-t border-[var(--line-soft)] first:border-t-0",
        className,
      )}
      style={selected ? { ...SELECTED_ROW_STYLE, ...style } : style}
    />
  );
}

/** Interactive router-link row with 44px minimum height and focus ring inside the row edge. */
export function RowLink({
  selected = false,
  "aria-current": ariaCurrent,
  className,
  style,
  ...rest
}: LinkProps & { selected?: boolean }) {
  return (
    <Link
      {...rest}
      aria-current={ariaCurrent ?? (selected ? "true" : undefined)}
      className={cn(
        "flex min-h-11 w-full min-w-0 items-center gap-3 px-3 py-3 sm:px-4",
        ROW_HOVER_CLASS,
        "focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]",
        className,
      )}
      style={selected ? { ...SELECTED_ROW_STYLE, ...style } : style}
    />
  );
}

/** Interactive button row with 44px minimum height and optional selected accent bar. */
export function RowButton({
  selected = false,
  "aria-current": ariaCurrent,
  type = "button",
  className,
  style,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { selected?: boolean }) {
  return (
    <button
      {...rest}
      type={type}
      aria-current={ariaCurrent ?? (selected ? "true" : undefined)}
      className={cn(
        "flex min-h-11 w-full min-w-0 items-center gap-3 px-3 py-3 text-left sm:px-4",
        ROW_HOVER_CLASS,
        "focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]",
        className,
      )}
      style={selected ? { ...SELECTED_ROW_STYLE, ...style } : style}
    />
  );
}

/** Padded inspector/body section separated by a top hairline (`padding: 14px 16px` on desktop). */
export function InspSection({
  as = "div",
  className,
  ...rest
}: HTMLAttributes<HTMLElement> & { as?: "div" | "dl" | "ul" | "section" }) {
  const Tag = as as ElementType;
  return (
    <Tag
      {...rest}
      className={cn("m-0 flex min-w-0 flex-col gap-2 border-t border-[var(--line-soft)] px-3 py-3.5 sm:px-4", className)}
    />
  );
}

/** Key-value line (`13px`), supporting `<dl>` (`dt`/`dd`) or `<span>` pairs, leading dots, and stale timestamps. */
export function KeyValue({
  label,
  leading,
  meta,
  tone,
  num = false,
  dl = false,
  truncate = "value",
  labelClassName,
  valueClassName,
  valueTitle,
  className,
  children,
  ...rest
}: HTMLAttributes<HTMLDivElement> & {
  label: ReactNode;
  leading?: ReactNode;
  meta?: ReactNode;
  tone?: DotTone;
  num?: boolean;
  dl?: boolean;
  truncate?: "label" | "value";
  labelClassName?: string;
  valueClassName?: string;
  valueTitle?: string;
}) {
  const LabelTag = (dl ? "dt" : "span") as ElementType;
  const ValueTag = (dl ? "dd" : "span") as ElementType;
  const metaInDd = dl && meta != null;
  return (
    <div {...rest} className={cn("flex min-w-0 items-center justify-between gap-3 text-[13px] leading-5", className)}>
      <LabelTag
        className={cn(
          "flex items-center gap-2 text-[var(--fg-muted)]",
          truncate === "label" ? "min-w-0 flex-1 truncate" : "shrink-0",
          labelClassName,
        )}
      >
        {leading}
        {truncate === "label" ? <span className="min-w-0 truncate">{label}</span> : label}
      </LabelTag>
      {dl ? null : meta}
      <ValueTag
        title={valueTitle}
        className={cn(
          "m-0 text-right",
          metaInDd && "flex items-center justify-end gap-3",
          truncate === "value" ? "min-w-0 flex-1 truncate" : "shrink-0",
          num && "font-display tabular-nums",
          valueClassName,
        )}
        style={tone && !metaInDd ? { color: toneText(tone) } : undefined}
      >
        {metaInDd ? (
          <>
            {meta}
            <span className={cn(truncate === "value" && "min-w-0 truncate")} style={tone ? { color: toneText(tone) } : undefined}>
              {children}
            </span>
          </>
        ) : (
          children
        )}
      </ValueTag>
    </div>
  );
}

/** Inspector rule line: an 8px status dot aligned to the first line of text. */
export function RuleItem({
  tone = "neutral",
  className,
  children,
}: {
  tone?: DotTone;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={cn("flex min-w-0 items-start gap-2.5 text-[13px] leading-[18px]", className)}>
      <StatusDot tone={tone} className="mt-[5px]" />
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  );
}

/** 8px round status dot (`--fg-faint`, `--ok`, `--info`, `--warn`, `--err`). */
export function StatusDot({ tone = "neutral", className }: { tone?: DotTone; className?: string }) {
  return <span aria-hidden className={cn("size-2 shrink-0 rounded-full", className)} style={{ background: TONE_COLOR[tone] }} />;
}

/** 22px pill chip in the six v3 tones (`neutral`, `ok`, `info`, `warn`, `err`, `accent`). */
export function Chip({
  tone = "neutral",
  dot = false,
  className,
  style,
  children,
  ...rest
}: HTMLAttributes<HTMLSpanElement> & {
  tone?: ChipTone;
  dot?: boolean;
}) {
  return (
    <span
      {...rest}
      className={cn(
        "inline-flex h-[22px] shrink-0 items-center gap-1.5 rounded-full px-2 text-xs leading-none font-medium whitespace-nowrap [&>svg]:size-3",
        className,
      )}
      style={{ ...CHIP_STYLE[tone], ...style }}
    >
      {dot ? (
        tone === "accent" ? (
          <span aria-hidden className="size-2 shrink-0 rounded-full bg-[var(--accent-fg)]" />
        ) : (
          <StatusDot tone={tone} />
        )
      ) : null}
      {children}
    </span>
  );
}

/** 20px round numeric count badge built on the vendored kit `Badge`. */
export function CountBadge({
  count,
  max = 99,
  tone = "neutral",
  className,
  style,
}: {
  count: number;
  max?: number;
  tone?: "neutral" | "warn" | "err";
  className?: string;
  style?: CSSProperties;
}) {
  return (
    <Badge
      count={count}
      max={max}
      size="md"
      tone={tone === "warn" ? "warning" : tone === "err" ? "danger" : "neutral"}
      className={className}
      style={{ ...COUNT_BADGE_VARS, ...style }}
    />
  );
}

/** Multi-segment stage track (`done`, `now`, `fail`, `wait`, `todo`). */
export function StageTrack({ segments, className }: { segments: TrackSegment[]; className?: string }) {
  return (
    <span aria-hidden className={cn("flex items-center gap-[3px]", className)}>
      {segments.map((segment, index) => (
        <i key={index} className="block h-1.5 flex-1 rounded-full" style={{ background: TRACK_COLOR[segment] }} />
      ))}
    </span>
  );
}

/** 6px single-value progress/usage meter bar (`0..1` ratio). */
export function MeterBar({
  ratio,
  tone = "accent",
  color,
  className,
}: {
  ratio: number;
  tone?: "accent" | "ok" | "info" | "warn" | "err" | "muted";
  color?: string;
  className?: string;
}) {
  return (
    <span aria-hidden className={cn("block h-1.5 overflow-hidden rounded-full bg-[var(--bg-3)]", className)}>
      <i
        className="block h-full rounded-full"
        style={{ width: `${Math.min(100, Math.max(0, ratio * 100))}%`, background: color ?? METER_COLOR[tone] }}
      />
    </span>
  );
}

export function Cap({ className, ...rest }: HTMLAttributes<HTMLSpanElement>) {
  return <span {...rest} className={cn("text-xs leading-4 text-[var(--fg-muted)]", className)} />;
}

export function Desc({ className, ...rest }: HTMLAttributes<HTMLSpanElement>) {
  return <span {...rest} className={cn("text-[13px] leading-[18px] text-[var(--fg-muted)]", className)} />;
}

export function Eyebrow({ className, ...rest }: HTMLAttributes<HTMLDivElement>) {
  return <div {...rest} className={cn("text-xs leading-4 font-medium text-[var(--fg-muted)]", className)} />;
}

export function Num({ className, ...rest }: HTMLAttributes<HTMLSpanElement>) {
  return <span {...rest} className={cn("font-display tabular-nums", className)} />;
}

/** 44x44-safe header/inline navigation link styled with `--accent-fg`. */
export function HeaderLink({ to, className, children }: { to: string; className?: string; children: ReactNode }) {
  return (
    <Link
      to={to}
      className={cn(
        "-my-3 -mr-1.5 inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-[var(--r-sm)] px-1.5 text-xs leading-4 font-medium text-[var(--accent-fg)] hover:underline focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]",
        className,
      )}
    >
      {children}
    </Link>
  );
}

/** Short empty, partial, or error note line inside a card. */
export function CardNote({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <p className={cn("m-0 border-t border-[var(--line-soft)] px-3 py-4 text-sm text-[var(--fg-muted)] sm:px-4 sm:py-5", className)}>
      {children}
    </p>
  );
}

/** Loading skeleton placeholder inside a flush card. */
export function CardSkeleton({ label, rows = 3 }: { label: string; rows?: number }) {
  return (
    <Loading label={label} className="flex flex-col gap-3 border-t border-[var(--line-soft)] px-3 py-4 sm:px-4">
      {Array.from({ length: rows }, (_, index) => (
        <Skeleton key={index} width={`${90 - index * 15}%`} height={10} line={20} />
      ))}
    </Loading>
  );
}
