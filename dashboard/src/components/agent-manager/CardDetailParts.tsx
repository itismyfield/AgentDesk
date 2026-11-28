import { useEffect, useRef, useState, type ReactNode } from "react";
import { Check, Copy } from "lucide-react";
import { Button } from "../../kit/free/button-clean";
import { copyText } from "./TurnTranscriptModel";

/** Kit controls are 28–32px on a fine pointer; the card detail keeps every target at 44px like the rest of the drawer. */
export const KIT_TOUCH_BUTTON = "min-h-11! min-w-11!";
export const KIT_TOUCH_FIELD = "[--h:2.75rem]!";
export const KIT_TOUCH_TABS = "[--aui-segmented-control-clean-h:2.75rem]!";

/** Small uppercase heading used by every detail section. */
export function DetailHeading({ children, id, aside }: { children: ReactNode; id?: string; aside?: ReactNode }) {
  return (
    <div className="flex min-h-6 flex-wrap items-center justify-between gap-x-3 gap-y-1">
      <h4 id={id} className="m-0 text-xs font-medium leading-4 text-th-text-secondary">{children}</h4>
      {aside}
    </div>
  );
}

/** Label/value rows in the kit detail style; empty values are left out. */
export function PropList({ items, className = "" }: { items: Array<[string, ReactNode]>; className?: string }) {
  const shown = items.filter(([, value]) => value != null && value !== "" && value !== false);
  if (shown.length === 0) return null;
  return (
    <dl className={`m-0 flex min-w-0 flex-col gap-1 ${className}`}>
      {shown.map(([label, value]) => (
        <div key={label} className="flex min-h-7 min-w-0 items-start gap-2 text-[13px] leading-5">
          <dt className="w-[5.5rem] shrink-0 pt-1 text-xs text-th-text-muted">{label}</dt>
          <dd className="m-0 flex min-w-0 flex-1 flex-wrap items-center gap-1.5 pt-0.5 text-th-text-primary [overflow-wrap:anywhere]">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

export const COPY_WELL_PREVIEW_LIMIT = 24_000;

/** A monospace well capped at 240px that scrolls inside, with a 44px copy button that reports the real result. */
export function CopyWell({ title, value, tr, testId }: { title: string; value: string; tr: (ko: string, en: string) => string; testId?: string }) {
  const [state, setState] = useState<"idle" | "done" | "error">("idle");
  const [expandedForValue, setExpandedForValue] = useState<string | null>(null);
  const expanded = expandedForValue === value;
  const timer = useRef(0);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const copy = async () => {
    try {
      await copyText(value);
      setState("done");
    } catch {
      setState("error");
    }
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setState("idle"), 1800);
  };
  const isTruncatable = value.length > COPY_WELL_PREVIEW_LIMIT;
  const renderedValue = isTruncatable && !expanded ? `${value.slice(0, COPY_WELL_PREVIEW_LIMIT)}\n…` : value;
  const label = state === "done" ? tr("복사됨", "Copied") : state === "error" ? tr("복사 실패", "Copy failed") : tr("복사", "Copy");
  return (
    <section className="flex min-w-0 flex-col gap-2" data-testid={testId}>
      <DetailHeading
        aside={
          <div className="flex flex-wrap items-center gap-1">
            {isTruncatable && (
              <Button
                variant="ghost"
                size="sm"
                className={KIT_TOUCH_BUTTON}
                aria-expanded={expanded}
                onClick={() => setExpandedForValue((prev) => (prev === value ? null : value))}
              >
                {expanded
                  ? tr("접기", "Collapse")
                  : tr(`전체 펼치기 (${value.length.toLocaleString()}자)`, `Show all (${value.length.toLocaleString()} chars)`)}
              </Button>
            )}
            <Button variant="ghost" size="sm" className={KIT_TOUCH_BUTTON} icon={state === "done" ? <Check /> : <Copy />}
              aria-label={tr(`${title} 전체 복사`, `Copy all of ${title}`)} onClick={() => void copy()}>
              {label}
            </Button>
          </div>
        }
      >
        {title}
      </DetailHeading>
      <pre
        tabIndex={0}
        aria-label={tr(`${title} 내용`, `${title} content`)}
        className="m-0 max-h-60 min-h-11 overflow-auto overscroll-contain whitespace-pre-wrap break-words rounded-[10px] bg-surface-subtle px-3.5 py-3 font-mono text-xs leading-[18px] text-th-text-primary [overflow-wrap:anywhere]"
      >
        {renderedValue}
      </pre>
      <span className="sr-only" role="status" aria-live="polite">{state === "idle" ? "" : label}</span>
    </section>
  );
}
