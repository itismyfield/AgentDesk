import { useId, useState } from "react";

export interface ChecklistItem {
  key: string;
  label: string;
  ok: boolean;
  detail: string;
}

export interface CompletionChecklistItem {
  key: string;
  ok: boolean;
  label: string;
  detail: string;
}

export interface StepStatusItem {
  step: number;
  label: string;
  status: "complete" | "active" | "blocked" | "pending";
}

export function Tip({ text }: { text: string }) {
  const id = useId();
  const [position, setPosition] = useState<{ left: number; top: number } | null>(null);
  return (
    <span className="inline-flex shrink-0">
      <button type="button" aria-label={text} aria-expanded={Boolean(position)} aria-describedby={position ? id : undefined}
        className="inline-flex h-11 w-11 items-center justify-center rounded-md text-xs font-semibold text-th-text-muted hover:bg-surface-subtle"
        onBlur={() => setPosition(null)} onKeyDown={event => { if (event.key === "Escape") setPosition(null); }}
        onClick={event => {
          const rect = event.currentTarget.getBoundingClientRect();
          setPosition(position ? null : { left: Math.max(16, Math.min(rect.left, window.innerWidth - 304)), top: Math.max(16, Math.min(rect.bottom + 8, window.innerHeight - 200)) });
        }}>?</button>
      {position ? <span id={id} role="tooltip" className="pointer-events-none fixed z-[90] max-h-40 w-[min(18rem,calc(100vw-2rem))] overflow-y-auto whitespace-pre-wrap rounded-md border border-th-border-subtle bg-th-bg-surface px-3 py-3 text-xs font-normal leading-5 text-th-text-primary shadow-sm" style={position}>{text}</span> : null}
    </span>
  );
}

export function ChecklistPanel({
  title,
  items,
}: {
  title: string;
  items: ChecklistItem[] | CompletionChecklistItem[];
}) {
  return (
    <div
      className="min-w-0 border-t border-th-border-subtle pt-4"

    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
        {title}
      </div>
      {items.map((item) => (
        <div
          key={item.key}
          className="border-b border-th-border-subtle py-3 text-xs last:border-b-0"

        >
          <div className="flex items-center gap-2">
            <span style={{ color: item.ok ? "var(--th-text-muted)" : "var(--th-badge-amber-text)" }}>{item.ok ? "✓" : "!"}</span>
            <span style={{ color: "var(--th-text-primary)" }}>{item.label}</span>
          </div>
          <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {item.detail}
          </div>
        </div>
      ))}
    </div>
  );
}

export function StepStatusRail({
  items,
  isKo,
  setItemRef,
}: {
  items: StepStatusItem[];
  isKo: boolean;
  setItemRef: (step: number, node: HTMLDivElement | null) => void;
}) {
  const statusMeta = (status: StepStatusItem["status"]) => ({
    complete: { icon: "✓", label: isKo ? "완료" : "Complete" },
    active: { icon: "•", label: isKo ? "진행 중" : "Active" },
    blocked: { icon: "!", label: isKo ? "보완 필요" : "Needs attention" },
    pending: { icon: "○", label: isKo ? "대기" : "Pending" },
  })[status];

  return (
    <div className="grid min-w-0 grid-cols-5 gap-1 sm:gap-2" role="list" aria-label={isKo ? "온보딩 단계" : "Onboarding steps"}>
      {items.map(item => (
        <div key={item.step} ref={node => setItemRef(item.step, node)} role="listitem" aria-current={item.status === "active" ? "step" : undefined}
          className={`min-w-0 border-b-2 px-1 py-3 sm:px-2 ${item.status === "active" ? "border-th-text-primary text-th-text-primary" : "border-th-border-subtle text-th-text-muted"}`}>
          <div className="text-[11px] tabular-nums">{item.step}</div>
          <div className="mt-1 text-xs font-medium leading-5 [overflow-wrap:anywhere]">{item.label}</div>
          <div className="mt-1 text-[10px] leading-4 [overflow-wrap:anywhere]"><span aria-hidden="true">{statusMeta(item.status).icon} </span>{statusMeta(item.status).label}</div>
        </div>
      ))}
    </div>
  );
}
