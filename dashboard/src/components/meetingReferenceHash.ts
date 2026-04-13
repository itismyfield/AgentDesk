import type { RoundTableMeeting } from "../types";

export function formatMeetingReferenceHash(hash: string | null | undefined): string | null {
  const trimmed = typeof hash === "string" ? hash.trim() : "";
  if (!trimmed) return null;

  const canonicalMatch = trimmed.match(/^#(?:meeting|thread)-(.+)$/);
  if (canonicalMatch) {
    return `#${canonicalMatch[1]}`;
  }

  return trimmed.startsWith("#") ? trimmed : `#${trimmed}`;
}

export function getDisplayMeetingReferenceHashes(
  meeting: Pick<RoundTableMeeting, "meeting_hash" | "thread_hash">,
): string[] {
  return [meeting.meeting_hash, meeting.thread_hash]
    .map((hash) => formatMeetingReferenceHash(hash))
    .filter((hash): hash is string => Boolean(hash));
}
