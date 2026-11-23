import type { CSSProperties, ReactNode } from "react";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { cx } from "../dashboard/ui";

export const NUMERIC_STYLE: CSSProperties = {
  fontVariantNumeric: "tabular-nums",
};

export function StatsSwatch({ color, className }: { color: string; className?: string }) {
  return <span aria-hidden="true" className={cx("inline-block size-2 shrink-0 rounded-[2px]", className)} style={{ background: color }} />;
}

/** Empty, zero or loading note inside a card (kit EmptyState without its frame). */
export function StatsEmpty({
  icon,
  title,
  text,
  action,
  testId,
}: {
  icon: ReactNode;
  title: ReactNode;
  text?: ReactNode;
  action?: ReactNode;
  testId?: string;
}) {
  return (
    <div className="px-4 py-8" data-testid={testId}>
      <EmptyState bare icon={icon} title={title} text={text} action={action} />
    </div>
  );
}
