export type Tr = (ko: string, en: string) => string;
export const LABELS: Record<string, [string, string]> = {
  planned: ["계획됨", "Planned"], active: ["진행 중", "Active"], paused: ["일시 중지", "Paused"],
  completed: ["완료", "Completed"], cancelled: ["취소됨", "Cancelled"], pending: ["대기", "Pending"],
  running: ["진행 중", "Running"], blocked: ["막힘", "Blocked"], failed: ["실패", "Failed"], skipped: ["건너뜀", "Skipped"],
};
export const COLORS: Record<string, string> = { completed: "#16a34a", running: "#3b82f6", blocked: "#f59e0b", failed: "#ef4444", skipped: "#94a3b8", pending: "#94a3b8" };
export function Badge({ status, tr }: { status: string; tr: Tr }) {
  return <span className={`campaign-badge campaign-status-${status}`}>{LABELS[status] ? tr(...LABELS[status]) : status}</span>;
}
