import { useId, type ReactNode } from "react";
import { ChevronRight, RotateCcw } from "lucide-react";
import { Button } from "../../kit/free/button-clean";
import { Loading, Skeleton } from "../../kit/free/skeleton-clean";
import { formatDiscordSummary } from "./discord-routing";

export function DiscordSummaryLabel({
  summary,
}: {
  summary: {
    title: string;
    subtitle: string | null;
    webUrl: string | null;
    deepLink: string | null;
  };
}) {
  const href = summary.deepLink ?? summary.webUrl;
  const label = formatDiscordSummary(summary);

  if (!href) {
    return (
      <span className="block min-w-0 flex-1 truncate text-[13px] font-medium text-(--fg)" title={label}>
        {label}
      </span>
    );
  }

  return (
    <a
      href={href}
      className="block min-w-0 flex-1 truncate rounded-sm text-[13px] leading-11 font-medium text-(--fg) hover:underline sm:leading-5 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent)"
      title={summary.deepLink ?? summary.webUrl ?? label}
    >
      {label}
    </a>
  );
}

export function DiscordDeepLinkChip({
  deepLink,
  label,
}: {
  deepLink: string | null;
  label: string;
}) {
  if (!deepLink) return null;
  return (
    <a
      href={deepLink}
      className="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-(--r-sm) px-2 text-xs text-(--fg-muted) hover:bg-(--bg-2) hover:text-(--fg) focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent) sm:min-h-7 sm:min-w-0"
      title={deepLink}
    >
      {label}
    </a>
  );
}

/** How a section folds: phones fold all but "now", and recent turns, activity and cron start folded everywhere. */
export interface DetailFold {
  foldable: boolean;
  open: boolean;
  onToggle: () => void;
}

/** Detail section under a hairline: a quiet heading with a summary or action on the right, or, when foldable, a 48px row
 * that opens it. */
export function AgentDetailSection({
  title,
  summary,
  aside,
  fold,
  testId,
  children,
}: {
  title: ReactNode;
  summary?: ReactNode;
  aside?: ReactNode;
  fold?: DetailFold;
  testId?: string;
  children: ReactNode;
}) {
  const bodyId = useId();
  if (fold?.foldable) {
    return (
      <section className="flex min-w-0 flex-col border-t border-(--line-soft)" data-testid={testId}>
        <h3 className="m-0">
          <button
            type="button"
            aria-expanded={fold.open}
            aria-controls={fold.open ? bodyId : undefined}
            onClick={fold.onToggle}
            className="flex min-h-12 w-full min-w-0 items-center gap-2 rounded-(--r-sm) text-left text-[13px] font-medium text-(--fg) focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent)"
          >
            <span className="min-w-0 flex-1 truncate">{title}</span>
            {summary != null ? <span className="min-w-0 truncate text-xs font-normal text-(--fg-muted)">{summary}</span> : null}
            <ChevronRight aria-hidden size={14} className={`shrink-0 text-(--fg-muted) transition-transform motion-reduce:transition-none ${fold.open ? "rotate-90" : ""}`} />
          </button>
        </h3>
        {fold.open ? <div id={bodyId} className="flex min-w-0 flex-col gap-2.5 pb-4">{children}</div> : null}
      </section>
    );
  }
  return (
    <section className="flex min-w-0 flex-col gap-2.5 border-t border-(--line-soft) py-4" data-testid={testId}>
      <div className="flex min-w-0 items-center justify-between gap-2">
        <h3 className="m-0 text-xs font-medium leading-4 text-(--fg-muted)">{title}</h3>
        {aside ?? (summary != null ? <span className="min-w-0 truncate text-xs text-(--fg-muted)">{summary}</span> : null)}
      </div>
      {children}
    </section>
  );
}

export function SectionLoading({ label }: { label: string }) {
  return (
    <Loading label={label} className="flex flex-col gap-2">
      <Skeleton width="72%" height={10} />
      <Skeleton width="48%" height={10} />
    </Loading>
  );
}

export function SectionEmpty({ children }: { children: ReactNode }) {
  return <p className="m-0 text-[13px] leading-5 text-(--fg-muted)">{children}</p>;
}

export function SectionErrorRetry({ message, retryLabel, onRetry }: { message: string; retryLabel: string; onRetry?: () => void }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-2 rounded-(--r-md) bg-[color-mix(in_oklch,var(--warn)_10%,var(--bg-1))] px-3 py-2 text-xs leading-5 text-(--fg-dim)">
      <span>{message}</span>
      {onRetry ? (
        <Button variant="secondary" size="sm" icon={<RotateCcw size={13} aria-hidden />} onClick={onRetry}>
          {retryLabel}
        </Button>
      ) : null}
    </div>
  );
}

/** Tinted well for free text such as the current work or the personality prompt. */
export function SectionWell({ children, className = "" }: { children: ReactNode; className?: string }) {
  return (
    <div className={`rounded-(--r-md) bg-[color-mix(in_oklch,var(--fg)_4%,var(--bg-1))] px-3.5 py-3 ${className}`.trim()}>
      {children}
    </div>
  );
}
