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
  return (
    <span className="relative group inline-block ml-1 cursor-help">
      <span
        className="inline-flex items-center justify-center w-4 h-4 rounded-full text-xs font-bold"
        style={{ backgroundColor: "rgba(148,163,184,0.2)", color: "var(--th-text-muted)" }}
      >
        ?
      </span>
      <span className="absolute hidden group-hover:block bottom-full left-0 mb-2 px-3 py-2 text-xs rounded-lg whitespace-pre-wrap w-72 z-50 shadow-lg"
        style={{ backgroundColor: "#1e293b", color: "#e2e8f0", border: "1px solid rgba(148,163,184,0.3)" }}
      >
        {text}
      </span>
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
      className="rounded-xl border p-4 space-y-2"
      style={{ borderColor: "rgba(148,163,184,0.16)", backgroundColor: "rgba(15,23,42,0.36)" }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
        {title}
      </div>
      {items.map((item) => (
        <div
          key={item.key}
          className="rounded-lg border px-3 py-2 text-sm"
          style={{
            borderColor: item.ok ? "rgba(16,185,129,0.22)" : "rgba(248,113,113,0.24)",
            backgroundColor: item.ok ? "rgba(16,185,129,0.08)" : "rgba(127,29,29,0.18)",
          }}
        >
          <div className="flex items-center gap-2">
            <span style={{ color: item.ok ? "#86efac" : "#fca5a5" }}>{item.ok ? "✓" : "!"}</span>
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
  const statusMeta = (status: StepStatusItem["status"]) => {
    switch (status) {
      case "complete":
        return {
          icon: "✓",
          label: isKo ? "완료" : "Complete",
          borderColor: "rgba(16,185,129,0.24)",
          backgroundColor: "rgba(16,185,129,0.08)",
          iconColor: "#86efac",
        };
      case "active":
        return {
          icon: "•",
          label: isKo ? "진행 중" : "Active",
          borderColor: "rgba(99,102,241,0.34)",
          backgroundColor: "rgba(99,102,241,0.12)",
          iconColor: "#c4b5fd",
        };
      case "blocked":
        return {
          icon: "!",
          label: isKo ? "보완 필요" : "Needs attention",
          borderColor: "rgba(248,113,113,0.24)",
          backgroundColor: "rgba(127,29,29,0.18)",
          iconColor: "#fca5a5",
        };
      default:
        return {
          icon: "○",
          label: isKo ? "대기" : "Pending",
          borderColor: "rgba(148,163,184,0.18)",
          backgroundColor: "rgba(15,23,42,0.32)",
          iconColor: "var(--th-text-muted)",
        };
    }
  };

  return (
    <div className="space-y-2">
      <div className="relative">
        <div className="pointer-events-none absolute inset-y-0 left-0 w-5 bg-gradient-to-r from-[color:var(--th-bg-surface)] to-transparent sm:hidden" />
        <div className="pointer-events-none absolute inset-y-0 right-0 w-8 bg-gradient-to-l from-[color:var(--th-bg-surface)] to-transparent sm:hidden" />
        <div className="flex gap-2 overflow-x-auto pb-1" role="list" aria-label={isKo ? "온보딩 단계" : "Onboarding steps"}>
          {items.map((item) => (
            <div
              key={item.step}
              ref={(node) => setItemRef(item.step, node)}
              role="listitem"
              aria-current={item.status === "active" ? "step" : undefined}
              className="min-w-[7.25rem] rounded-xl border px-3 py-2 sm:min-w-[8.5rem]"
              style={{
                borderColor: statusMeta(item.status).borderColor,
                backgroundColor: statusMeta(item.status).backgroundColor,
              }}
            >
              <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                Step {item.step}
              </div>
              <div className="mt-1 flex items-center gap-2">
                <span aria-hidden="true" style={{ color: statusMeta(item.status).iconColor }}>
                  {statusMeta(item.status).icon}
                </span>
                <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                  {item.label}
                </span>
              </div>
              <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                {statusMeta(item.status).label}
              </div>
            </div>
          ))}
        </div>
      </div>
      <div className="text-[11px] sm:hidden" style={{ color: "var(--th-text-muted)" }}>
        {isKo ? "가로 스크롤로 전체 단계를 확인할 수 있습니다." : "Swipe horizontally to see every step."}
      </div>
    </div>
  );
}
