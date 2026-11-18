export type MarkerTone = "plan" | "open" | "progress" | "warn" | "err" | "done";

/** Where a kanban status sits on the way to done: a dashed ring before ready, then a growing pie, a full disc at done. */
const CARD_STEPS = ["ready", "in_progress", "review", "qa_pending", "qa_in_progress", "done"];
export function cardMarker(status: string): { tone: MarkerTone; ratio: number } {
  if (status === "backlog") return { tone: "plan", ratio: 0 };
  if (status === "failed" || status === "qa_failed") return { tone: "err", ratio: 0 };
  const step = CARD_STEPS.indexOf(status === "requested" ? "ready" : status);
  if (step < 0) return { tone: "open", ratio: 0 };
  const ratio = step / (CARD_STEPS.length - 1);
  return { tone: ratio >= 1 ? "done" : ratio > 0 ? "progress" : "open", ratio };
}

const NODE_MARKERS: Record<string, { tone: MarkerTone; ratio: number }> = {
  pending: { tone: "open", ratio: 0 },
  running: { tone: "progress", ratio: 0.5 },
  blocked: { tone: "warn", ratio: 0 },
  failed: { tone: "err", ratio: 0 },
  completed: { tone: "done", ratio: 1 },
  skipped: { tone: "plan", ratio: 0 },
};
export function nodeMarker(status: string) {
  return NODE_MARKERS[status] ?? NODE_MARKERS.pending;
}

const TONE_CLASS: Record<MarkerTone, string> = {
  plan: "text-[var(--fg-faint)]",
  open: "text-[var(--fg-muted)]",
  progress: "text-[var(--info)]",
  warn: "text-[var(--warn)]",
  err: "text-[var(--err)]",
  done: "text-[var(--ok)]",
};
const PIE = 2 * Math.PI * 2.25;

/** Decorative progress marker; the row's status text says the same in words. */
export function StatusMarker({ tone, ratio, className = "" }: { tone: MarkerTone; ratio: number; className?: string }) {
  return <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true" data-tone={tone} className={`shrink-0 ${TONE_CLASS[tone]} ${className}`}>
    {tone === "done"
      ? <circle cx="7" cy="7" r="6.5" fill="currentColor" />
      : <>
        <circle cx="7" cy="7" r="6" fill="none" stroke="currentColor" strokeWidth="1.5" strokeDasharray={tone === "plan" ? "2 1.77" : undefined} />
        {ratio > 0 && <circle cx="7" cy="7" r="2.25" fill="none" stroke="currentColor" strokeWidth="4.5" strokeDasharray={`${ratio * PIE} ${PIE}`} transform="rotate(-90 7 7)" />}
        {(tone === "warn" || tone === "err") && <circle cx="7" cy="7" r="2" fill="currentColor" />}
      </>}
  </svg>;
}
