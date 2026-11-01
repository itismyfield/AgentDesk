/** Rewrites a PG `::text` timestamp ("2026-11-01 11:05:23.123456+09") as ISO. Safari reads neither the
 * space separator nor the hour-only offset; ISO input comes back unchanged. */
export function pgTimestampToIso(value: string): string {
  return value.trim().replace(" ", "T").replace(/(\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?)([+-]\d{2})$/, "$1$2:00");
}

/** Milliseconds for a server timestamp, or null when it cannot be read. */
export function parsePgTimestamp(value: string | null | undefined): number | null {
  if (!value) return null;
  const ms = Date.parse(pgTimestampToIso(value));
  return Number.isNaN(ms) ? null : ms;
}
