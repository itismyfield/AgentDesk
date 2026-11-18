import type { ReactNode } from "react";
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
      className="block min-w-0 flex-1 truncate rounded-sm text-[13px] font-medium text-(--fg) hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent)"
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
      className="inline-flex min-h-11 shrink-0 items-center rounded-(--r-sm) px-2 text-xs text-(--fg-muted) hover:bg-(--bg-2) hover:text-(--fg) focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent) sm:min-h-7"
      title={deepLink}
    >
      {label}
    </a>
  );
}

/** Flat detail section: a quiet heading with an optional action on the right, then the content. */
export function AgentDetailSection({
  title,
  aside,
  children,
  className = "",
  labelledBy,
}: {
  title: ReactNode;
  aside?: ReactNode;
  children: ReactNode;
  className?: string;
  labelledBy?: string;
}) {
  return (
    <section className={`flex min-w-0 flex-col gap-2.5 ${className}`.trim()} aria-labelledby={labelledBy}>
      <div className="flex min-w-0 items-center justify-between gap-2">
        <h3 id={labelledBy} className="m-0 text-xs font-medium leading-4 text-(--fg-muted)">
          {title}
        </h3>
        {aside}
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

/** Tinted well for free text such as the current work or the personality prompt. */
export function SectionWell({ children, className = "" }: { children: ReactNode; className?: string }) {
  return (
    <div className={`rounded-(--r-md) bg-[color-mix(in_oklch,var(--fg)_4%,var(--bg-1))] px-3.5 py-3 ${className}`.trim()}>
      {children}
    </div>
  );
}
