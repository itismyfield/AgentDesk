import type { Notification } from "../components/NotificationCenter";

export function notificationColor(type: Notification["type"]): string {
  switch (type) {
    case "success":
      return "#34d399";
    case "warning":
      return "#fbbf24";
    case "error":
      return "#f87171";
    default:
      return "#60a5fa";
  }
}

export function formatRelativeTime(timestamp: number, isKo: boolean): string {
  const diffMs = Date.now() - timestamp;
  const seconds = Math.max(1, Math.floor(diffMs / 1000));
  if (seconds < 60) return isKo ? `${seconds}초 전` : `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return isKo ? `${minutes}분 전` : `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return isKo ? `${hours}시간 전` : `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return isKo ? `${days}일 전` : `${days}d ago`;
}
