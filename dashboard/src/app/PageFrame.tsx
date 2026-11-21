import type { ReactNode } from "react";

interface PageFrameProps {
  title: string;
  description?: ReactNode;
  /** Beside the title on desktop and above the content on phones, e.g. tabs or filters. */
  actions?: ReactNode;
  children: ReactNode;
  testId?: string;
}

/** v3 page frame: a title block over the content. Phones skip the title because the top bar already shows it. */
export function PageFrame({ title, description, actions, children, testId }: PageFrameProps) {
  return (
    <div data-testid={testId} className="h-full min-w-0 overflow-y-auto bg-[var(--bg)] text-[var(--fg)]">
      <header className="flex min-w-0 flex-wrap items-end gap-4 px-4 pt-1 sm:px-6 sm:pt-5">
        <div className="hidden min-w-0 flex-[1_1_22.5rem] sm:block">
          <h1 className="m-0 text-2xl leading-8 font-semibold tracking-[-0.01em] text-[var(--fg)]">{title}</h1>
          {description ? (
            <p className="m-0 mt-1 text-[13px] leading-[18px] text-[var(--fg-muted)] [word-break:keep-all]">{description}</p>
          ) : null}
        </div>
        {actions ? <div className="w-full min-w-0 sm:w-auto">{actions}</div> : null}
      </header>
      <div className="min-w-0 px-4 pt-3 pb-6 sm:px-6 sm:pt-4">{children}</div>
    </div>
  );
}
