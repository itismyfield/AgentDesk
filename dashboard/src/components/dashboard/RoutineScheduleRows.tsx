import { forwardRef, type KeyboardEvent } from "react";
import { MessageSquare } from "lucide-react";
import { Badge } from "../../kit/free/badge-clean";
import { Switch } from "../../kit/free/switch-clean";
import { badgeTone, type RoutineTone } from "./routineModel";
import { cx } from "./ui";

const toneColor: Record<RoutineTone, string> = {
  info: "var(--info)",
  success: "var(--ok)",
  warn: "var(--warn)",
  neutral: "var(--fg-faint)",
  danger: "var(--err)",
};

/** Small status square; paused routines draw it as an outline. Running ones pulse unless motion is reduced. */
export function RoutineSwatch({ tone, outline = false }: { tone: RoutineTone; outline?: boolean }) {
  return (
    <span
      aria-hidden
      className={cx(
        "inline-block h-2 w-2 shrink-0 rounded-[2px] border-[1.5px]",
        tone === "info" && !outline ? "motion-safe:animate-pulse" : "",
      )}
      style={{ borderColor: toneColor[tone], background: outline ? "transparent" : toneColor[tone] }}
    />
  );
}

export interface RoutineRowView {
  id: string;
  /** Plain-language purpose; the slug stays as secondary text. */
  title: string;
  slug: string;
  schedule: string;
  agent: string | null;
  tone: RoutineTone;
  paused: boolean;
  /** Next run (absolute) or the state that replaces it: paused, detached, no schedule. */
  nextText: string;
  relative: string | null;
  latestStatus: string | null;
  latestTone: RoutineTone;
  threadUrl: string | null;
  enabled: boolean;
  label: string;
  threadLabel: string;
  switchLabel: string;
  switchHint: string;
}

const focusRing = "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--accent)]";

/**
 * Kit schedule rows: name and repeat, next run, latest result, then the thread link and the on/off state.
 * The list column is a container: at 640px or less the latest result and relative time hide (the detail has them),
 * at 440px or less name, repeat and next run stack.
 */
export const RoutineScheduleList = forwardRef<HTMLUListElement, {
  rows: RoutineRowView[];
  selectedId?: string | null;
  /** Home card: no switch, and the latest result stays visible under the name. */
  compact?: boolean;
  onOpen: (id: string, trigger: HTMLElement) => void;
}>(function RoutineScheduleList({ rows, selectedId = null, compact = false, onOpen }, ref) {
  const onKeyDown = (event: KeyboardEvent<HTMLUListElement>) => {
    const lower = event.key.toLowerCase();
    const down = event.key === "ArrowDown" || lower === "j";
    const up = event.key === "ArrowUp" || lower === "k";
    if ((!down && !up) || event.altKey || event.ctrlKey || event.metaKey) return;
    const buttons = Array.from(event.currentTarget.querySelectorAll<HTMLElement>("[data-row]"));
    const index = buttons.indexOf(event.target as HTMLElement);
    if (index < 0) return;
    event.preventDefault();
    buttons[Math.max(0, Math.min(buttons.length - 1, index + (down ? 1 : -1)))]?.focus();
  };

  return (
    <ul
      ref={ref}
      className="m-0 -mx-2 flex min-w-0 list-none flex-col gap-0.5 p-0"
      data-testid="routines-timeline-list"
      onKeyDown={onKeyDown}
    >
      {rows.map((row) => {
        const selected = selectedId === row.id;
        const latest = row.latestStatus ? (
          <Badge tone={badgeTone(row.latestTone)}>{row.latestStatus}</Badge>
        ) : null;
        return (
          <li
            key={row.id}
            data-selected={selected || undefined}
            data-paused={row.paused || undefined}
            className={cx(
              "flex min-w-0 items-center gap-1 rounded-[var(--r-sm)] pr-1 transition-colors motion-reduce:transition-none",
              selected ? "bg-[color-mix(in_oklch,var(--accent)_12%,transparent)]" : "hover:bg-[color-mix(in_oklch,var(--fg)_5%,transparent)]",
            )}
          >
            <button
              type="button"
              data-row={row.id}
              data-testid={`routine-row-${row.id}`}
              aria-current={selected ? "true" : undefined}
              aria-label={row.label}
              onClick={(event) => onOpen(row.id, event.currentTarget)}
              className={cx(
                "flex min-h-14 min-w-0 flex-1 items-center gap-3 rounded-[var(--r-sm)] py-2 pl-3 text-left text-[13px] text-[var(--fg)]",
                "@max-[440px]/routines:items-start",
                focusRing,
              )}
            >
              <span className="inline-flex shrink-0 @max-[440px]/routines:mt-1.5">
                <RoutineSwatch tone={row.tone} outline={row.paused} />
              </span>
              <span className="flex min-w-0 flex-1 flex-col">
                <strong
                  title={row.title}
                  className={cx(
                    "truncate text-[13px] font-medium leading-5 @max-[440px]/routines:whitespace-normal @max-[440px]/routines:[overflow-wrap:anywhere]",
                    row.paused ? "text-[var(--fg-muted)]" : "text-[var(--fg)]",
                  )}
                >
                  {row.title}
                </strong>
                <span className="truncate text-xs leading-5 text-[var(--fg-muted)]" title={`${row.slug} · ${row.schedule}`}>
                  <span className="font-mono text-[11px]">{row.slug}</span>
                  <span aria-hidden> · </span>
                  {row.schedule}
                  {row.agent ? ` · ${row.agent}` : ""}
                </span>
                <span className="hidden text-xs leading-4 tabular-nums text-[var(--fg-dim)] @max-[440px]/routines:block">{row.nextText}</span>
                {compact && latest ? <span className="mt-1 flex">{latest}</span> : null}
              </span>
              <span
                className={cx(
                  "flex shrink-0 flex-col items-end text-xs leading-5 tabular-nums @max-[440px]/routines:hidden",
                  row.paused ? "text-[var(--fg-muted)]" : "text-[var(--fg)]",
                )}
              >
                <span className="whitespace-nowrap">{row.nextText}</span>
                {row.relative ? (
                  <small className="whitespace-nowrap text-xs text-[var(--fg-faint)] @max-[640px]/routines:hidden">{row.relative}</small>
                ) : null}
              </span>
              {compact ? null : (
                <span className="flex w-[6.75rem] shrink-0 justify-end @max-[640px]/routines:hidden">{latest}</span>
              )}
            </button>
            {row.threadUrl ? (
              <a
                href={row.threadUrl}
                target="_blank"
                rel="noreferrer"
                data-testid={`routine-thread-${row.id}`}
                aria-label={row.threadLabel}
                title={row.threadLabel}
                className={cx(
                  "flex h-11 w-11 shrink-0 items-center justify-center rounded-[var(--r-sm)] text-[var(--fg-muted)] transition-colors hover:text-[var(--fg)]",
                  focusRing,
                )}
              >
                <MessageSquare size={16} aria-hidden />
              </a>
            ) : (
              <span className="w-11 shrink-0" aria-hidden />
            )}
            {compact ? null : (
              <span className="flex h-11 w-11 shrink-0 items-center justify-center" title={row.switchHint}>
                <Switch
                  size="sm"
                  checked={row.enabled}
                  readOnly
                  disabled
                  aria-label={row.switchLabel}
                  data-testid={`routine-switch-${row.id}`}
                />
              </span>
            )}
          </li>
        );
      })}
    </ul>
  );
});
