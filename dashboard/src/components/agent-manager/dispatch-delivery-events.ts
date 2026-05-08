export const DELIVERY_EVENT_STATUS_STYLE: Record<string, { bg: string; text: string }> = {
  reserved: { bg: "rgba(156,163,175,0.12)", text: "#9ca3af" },
  sent: { bg: "rgba(34,197,94,0.16)", text: "#86efac" },
  fallback: { bg: "rgba(249,115,22,0.16)", text: "#fdba74" },
  duplicate: { bg: "rgba(59,130,246,0.14)", text: "#60a5fa" },
  skipped: { bg: "rgba(156,163,175,0.12)", text: "#9ca3af" },
  failed: { bg: "rgba(248,113,113,0.16)", text: "#fca5a5" },
};

export function getDeliveryEventStatusStyle(status: string): { bg: string; text: string } {
  return DELIVERY_EVENT_STATUS_STYLE[status] ?? { bg: "rgba(148,163,184,0.10)", text: "#94a3b8" };
}

export function deliveryEventMessagesCount(value: unknown): number {
  return Array.isArray(value) ? value.length : 0;
}

export function compactStringParts(parts: Array<string | null | undefined | false>): string[] {
  return parts.filter((part): part is string => Boolean(part));
}

export function summarizeDeliveryError(error: string | null | undefined): string {
  if (!error?.trim()) return "-";
  const compact = error.trim().replace(/\s+/g, " ");
  return compact.length > 96 ? `${compact.slice(0, 93)}...` : compact;
}
