import type { ReactNode } from "react";
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
      <span
        className="block min-w-0 flex-1 truncate text-xs font-medium"
        style={{ color: "var(--th-text-primary)" }}
        title={label}
      >
        {label}
      </span>
    );
  }

  return (
    <a
      href={href}
      className="block min-w-0 flex-1 truncate text-xs font-medium hover:underline"
      style={{ color: "var(--th-text-primary)" }}
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
      className="shrink-0 rounded px-1.5 py-0.5 text-xs"
      style={{ background: "rgba(88,101,242,0.15)", color: "#7289da" }}
      title={deepLink}
    >
      {label}
    </a>
  );
}

interface DetailAccordionProps {
  title: string;
  subtitle?: string | null;
  badge?: string | null;
  open: boolean;
  onToggle: () => void;
  children: ReactNode;
}

export function DetailAccordion({
  title,
  subtitle,
  badge,
  open,
  onToggle,
  children,
}: DetailAccordionProps) {
  return (
    <div
      className="px-5 py-3"
      style={{ borderBottom: "1px solid var(--th-card-border)" }}
    >
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-start justify-between gap-3 text-left"
        aria-expanded={open}
      >
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <div
              className="text-xs font-semibold uppercase tracking-widest"
              style={{ color: "var(--th-text-muted)" }}
            >
              {title}
            </div>
            {badge && (
              <span
                className="rounded-full px-2 py-0.5 text-[11px] font-medium"
                style={{
                  background: "rgba(96,165,250,0.12)",
                  color: "#93c5fd",
                }}
              >
                {badge}
              </span>
            )}
          </div>
          {subtitle && (
            <div
              className="mt-1 text-xs leading-relaxed"
              style={{ color: "var(--th-text-muted)" }}
            >
              {subtitle}
            </div>
          )}
        </div>
        <span
          className="rounded-full px-2 py-1 text-xs font-medium"
          style={{
            background: "rgba(148,163,184,0.12)",
            color: "var(--th-text-muted)",
          }}
        >
          {open ? "▲" : "▼"}
        </span>
      </button>
      {open && <div className="mt-3">{children}</div>}
    </div>
  );
}
