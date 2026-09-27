import type { CampaignNodeLive } from "../../api/campaigns";
import { COLUMN_DEFS } from "../agent-manager/kanban-utils";
import { liveIsWorking } from "./campaignModel";

export type Tr = (ko: string, en: string) => string;
export const LABELS: Record<string, [string, string]> = {
  planned: ["계획됨", "Planned"], active: ["활성", "Active"], paused: ["일시 중지", "Paused"],
  completed: ["완료", "Completed"], cancelled: ["취소됨", "Cancelled"], pending: ["대기", "Pending"],
  running: ["진행 중", "Running"], blocked: ["막힘", "Blocked"], failed: ["실패", "Failed"], skipped: ["건너뜀", "Skipped"],
};
export const COLORS: Record<string, string> = { completed: "#16a34a", running: "#3b82f6", blocked: "#f59e0b", failed: "#ef4444", skipped: "#94a3b8", pending: "#94a3b8" };
export function Badge({ status, tr }: { status: string; tr: Tr }) {
  return <span className={`campaign-badge campaign-status-${status}`}>{LABELS[status] ? tr(...LABELS[status]) : status}</span>;
}

const DISPATCH_TYPES: Record<string, [string, string]> = {
  implementation: ["구현", "Implementation"], rework: ["재작업", "Rework"], review: ["리뷰", "Review"],
  "review-decision": ["리뷰 판정", "Review decision"], "phase-gate": ["단계 확인", "Phase check"],
  "create-pr": ["PR 생성", "PR creation"], plan: ["계획", "Planning"], "plan-review": ["계획 리뷰", "Plan review"],
  "scope-assessment": ["범위 판단", "Scope check"], consultation: ["상담", "Consultation"],
};
const DISPATCH_STATUSES: Record<string, [string, string]> = {
  pending: ["대기", "queued"], dispatched: ["배정됨", "assigned"], completed: ["완료", "done"],
  failed: ["실패", "failed"], cancelled: ["취소", "cancelled"],
};
const QUEUE_STATUSES: Record<string, [string, string]> = {
  pending: ["자동 큐 대기", "Waiting in auto-queue"], dispatched: ["자동 큐에서 실행", "Started by auto-queue"],
  done: ["자동 큐 완료", "Auto-queue done"], skipped: ["자동 큐 건너뜀", "Skipped by auto-queue"],
  failed: ["자동 큐 실패", "Auto-queue failed"], user_cancelled: ["자동 큐 취소", "Removed from auto-queue"],
};
const SESSION_STATUSES: Record<string, [string, string]> = {
  turn_active: ["작업 중", "Working"], awaiting_bg: ["백그라운드 작업 대기", "Waiting on background work"],
  awaiting_user: ["답장 대기", "Waiting for a reply"], idle: ["유휴", "Idle"],
  disconnected: ["연결 끊김", "Disconnected"], aborted: ["중단됨", "Aborted"],
};
const label = (labels: Record<string, [string, string]>, value: string | null, tr: Tr) =>
  value ? (labels[value] ? tr(...labels[value]) : value) : null;
export function cardStatusLabel(status: string, tr: Tr) {
  const column = COLUMN_DEFS.find((def) => def.status === status);
  return column ? tr(column.labelKo, column.labelEn) : status;
}
export function dispatchLabel(live: CampaignNodeLive, tr: Tr) {
  const type = label(DISPATCH_TYPES, live.dispatch_type, tr) ?? tr("작업", "Work");
  if (live.running) return `${type} ${tr("진행 중", "running")}`;
  return live.dispatch_status ? `${type} ${label(DISPATCH_STATUSES, live.dispatch_status, tr)}` : null;
}
export function sessionLabel(live: CampaignNodeLive, tr: Tr) {
  if (!live.session_status) return tr("잡은 세션 없음", "No session on it");
  const seen = live.session_seen_at ? new Date(live.session_seen_at).toLocaleString() : null;
  return [label(SESSION_STATUSES, live.session_status, tr), seen && tr(`마지막 신호 ${seen}`, `last seen ${seen}`)].filter(Boolean).join(" · ");
}
export function queueLabel(live: CampaignNodeLive, tr: Tr) { return label(QUEUE_STATUSES, live.queue_status, tr); }

/** One phrase for what the issue card is doing now: open dispatch, then queue wait, then board column.
 * Only a dispatch a session is working on right now reads as running. */
export function LiveChip({ live, tr }: { live: CampaignNodeLive | undefined; tr: Tr }) {
  if (!live) return null;
  const working = liveIsWorking(live);
  const open = live.dispatch_status === "pending" || live.dispatch_status === "dispatched";
  const text = open ? dispatchLabel(live, tr)
    : live.queue_status === "pending" ? queueLabel(live, tr)
    : `${tr("보드", "Board")}: ${cardStatusLabel(live.card_status, tr)}`;
  return <span className={`campaign-live${working ? " is-working" : ""}`}>{text}</span>;
}
