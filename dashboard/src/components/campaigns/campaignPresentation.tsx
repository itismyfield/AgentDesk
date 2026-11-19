import type { CampaignNodeLive } from "../../api/campaigns";
import { Badge as KitBadge, type BadgeTone } from "../../kit/free/badge-clean";
import { COLUMN_DEFS } from "../agent-manager/kanban-utils";
import { liveIsWorking } from "./campaignModel";

export type Tr = (ko: string, en: string) => string;

/** The campaign page's element styles (campaigns.css) and touch sizes, for the page and for details it opens in a
 * drawer or sheet outside it. Inputs read at 16px below the mobile boundary so phones do not zoom. */
export const CAMPAIGN_SCOPE = "campaigns-panel text-th-text-primary [&_[data-widget-state]]:!rounded-lg [&_button:not([class^=aui])]:min-h-11 [&_button:not([class^=aui])]:min-w-11 [&_input:not([type=checkbox]):not([class^=aui])]:min-h-11 [&_select]:min-h-11 [&_summary]:min-h-11 [&_summary]:py-3 [&_label:not([class^=aui]):has(input[type=checkbox])]:min-h-11 max-[899px]:[&_input:not([type=checkbox]):not([class^=aui])]:!text-base max-[899px]:[&_select]:!text-base max-[899px]:[&_textarea:not([class^=aui])]:!text-base [&_h2]:text-2xl [&_h2]:font-semibold [&_h2]:tracking-tight [&_h3]:font-semibold [&_h3]:[word-break:keep-all] [&_h4]:text-xs [&_h4]:font-medium [&_a]:inline-flex [&_a]:min-h-11 [&_a]:max-w-full [&_a]:items-center [&_a]:py-2 [&_a]:[overflow-wrap:anywhere]";
export const LABELS: Record<string, [string, string]> = {
  planned: ["계획됨", "Planned"],
  active: ["활성", "Active"],
  paused: ["일시 중지", "Paused"],
  completed: ["완료", "Completed"],
  cancelled: ["취소됨", "Cancelled"],
  pending: ["대기", "Pending"],
  running: ["진행 중", "Running"],
  blocked: ["막힘", "Blocked"],
  failed: ["실패", "Failed"],
  skipped: ["건너뜀", "Skipped"],
};
export const COLORS: Record<string, string> = {
  completed: "#16a34a",
  running: "#3b82f6",
  blocked: "#f59e0b",
  failed: "#ef4444",
  skipped: "#94a3b8",
  pending: "#94a3b8",
};
// Ledger and server statuses share one tone scale: working, done, waiting on someone, failed.
const STATUS_TONES: Record<string, BadgeTone> = {
  active: "info", running: "info", assigned: "info", verifying: "info",
  completed: "success", succeeded: "success", complete: "success",
  blocked: "warning", paused: "warning", waiting: "warning", stopping: "warning", unable_to_progress: "warning", needs_inspection: "warning", interrupted: "warning",
  failed: "danger",
};
/** A status as a kit badge: tone and dot by status (a live dot while running), the words given or the ledger label. */
export function Badge({ status, tr, label, title }: { status: string; tr: Tr; label?: string; title?: string }) {
  return <KitBadge tone={STATUS_TONES[status] ?? "neutral"} dot live={status === "running"} title={title}>{label ?? (LABELS[status] ? tr(...LABELS[status]) : status)}</KitBadge>;
}

const DISPATCH_TYPES: Record<string, [string, string]> = {
  implementation: ["구현", "Implementation"],
  rework: ["재작업", "Rework"],
  review: ["리뷰", "Review"],
  "review-decision": ["리뷰 판정", "Review decision"],
  "phase-gate": ["단계 확인", "Phase check"],
  "create-pr": ["PR 생성", "PR creation"],
  plan: ["계획", "Planning"],
  "plan-review": ["계획 리뷰", "Plan review"],
  "scope-assessment": ["범위 판단", "Scope check"],
  consultation: ["상담", "Consultation"],
};
const DISPATCH_STATUSES: Record<string, [string, string]> = {
  pending: ["대기", "queued"],
  dispatched: ["배정됨", "assigned"],
  completed: ["완료", "done"],
  failed: ["실패", "failed"],
  cancelled: ["취소", "cancelled"],
};
const QUEUE_STATUSES: Record<string, [string, string]> = {
  pending: ["자동 큐 대기", "Waiting in auto-queue"],
  dispatched: ["자동 큐에서 실행", "Started by auto-queue"],
  done: ["자동 큐 완료", "Auto-queue done"],
  skipped: ["자동 큐 건너뜀", "Skipped by auto-queue"],
  failed: ["자동 큐 실패", "Auto-queue failed"],
  user_cancelled: ["자동 큐 취소", "Removed from auto-queue"],
};
const SESSION_STATUSES: Record<string, [string, string]> = {
  turn_active: ["작업 중", "Working"],
  awaiting_bg: ["백그라운드 작업 대기", "Waiting on background work"],
  awaiting_user: ["답장 대기", "Waiting for a reply"],
  idle: ["유휴", "Idle"],
  disconnected: ["연결 끊김", "Disconnected"],
  aborted: ["중단됨", "Aborted"],
};
const label = (labels: Record<string, [string, string]>, value: string | null, tr: Tr) => (value ? (labels[value] ? tr(...labels[value]) : value) : null);
export function cardStatusLabel(status: string, tr: Tr) {
  const column = COLUMN_DEFS.find((def) => def.status === status);
  return column ? tr(column.labelKo, column.labelEn) : status;
}
export function dispatchLabel(live: CampaignNodeLive, tr: Tr) {
  const dispatchType = live.running && live.working_dispatch_id !== undefined ? (live.working_dispatch_type ?? null) : live.dispatch_type;
  const type = label(DISPATCH_TYPES, dispatchType, tr) ?? tr("작업", "Work");
  if (live.running) return `${type} ${tr("진행 중", "running")}`;
  return live.dispatch_status ? `${type} ${label(DISPATCH_STATUSES, live.dispatch_status, tr)}` : null;
}
export function sessionLabel(live: CampaignNodeLive, tr: Tr) {
  const working = live.running && live.working_dispatch_id !== undefined;
  const status = working ? live.working_session_status : live.session_status;
  const seenAt = working ? live.working_session_seen_at : live.session_seen_at;
  if (!status) return tr("잡은 세션 없음", "No session on it");
  const seen = seenAt ? new Date(seenAt).toLocaleString() : null;
  return [label(SESSION_STATUSES, status, tr), seen && tr(`마지막 신호 ${seen}`, `last seen ${seen}`)].filter(Boolean).join(" · ");
}
export function queueLabel(live: CampaignNodeLive, tr: Tr) {
  return label(QUEUE_STATUSES, live.queue_status, tr);
}

// A card waiting in the backlog or already done adds nothing beside the task status.
const QUIET_CARD_STATUSES = new Set(["backlog", "done"]);

/** One phrase for what the issue card is doing now: working or open dispatch, then queue wait, then the card's step.
 * Only a dispatch a session is working on right now reads as running. */
export function LiveChip({ live, tr }: { live: CampaignNodeLive | undefined; tr: Tr }) {
  if (!live) return null;
  const working = liveIsWorking(live);
  const open = live.dispatch_status === "pending" || live.dispatch_status === "dispatched";
  if (!working && !open && live.queue_status !== "pending" && QUIET_CARD_STATUSES.has(live.card_status)) return null;
  const text =
    working || open
      ? dispatchLabel(live, tr)
      : live.queue_status === "pending"
        ? queueLabel(live, tr)
        : `${tr("카드", "Card")} ${cardStatusLabel(live.card_status, tr)}`;
  return <KitBadge className="campaign-live max-w-full" tone={working ? "info" : "neutral"} live={working} title={tr("연결된 이슈 카드의 지금 상태", "What the linked issue card is doing now")}>{text}</KitBadge>;
}
