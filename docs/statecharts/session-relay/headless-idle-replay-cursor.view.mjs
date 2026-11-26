export const title = "Headless idle replay crosses the published answer"

const sentAnswerEnd = 706908
const laterAnswerEnd = 1312781

export function state({ s }) {
  if (s.cursor > 0 && s.prefix !== "Sentinel" && s.visiblePrefixPosts === 0)
    return "Earlier answer lost without a post"
  if (s.cursor === laterAnswerEnd) return "Read cursor passed the later answer"
  if (s.idlePostsB > 0) return "Later answer posted once"
  if (s.cursor === sentAnswerEnd)
    return s.phase === "Frame" ? "Later terminal fits in the new window" : "Cursor at the end of the published answer"
  if (s.cursor > 0) return "Cursor at the start of the published answer"
  if (s.phase === "Frame" && !s.laterAvailable) return "Window reaches only the first answer"
  if (s.phase === "Frame") return "Window ends before the later terminal"
  if (s.skippedA && s.laterAvailable) return "First answer skipped and read start unchanged"
  if (s.prefix === "Pending") return "Earlier outbox delivery pending"
  if (s.prefix === "Sent") return "Earlier outbox confirms its post"
  if (s.visiblePrefixPosts > 0) return "Earlier visible answer delivered"
  if (s.prefix === "Sentinel") return "Cursor before the ready-for-input marker"
  return "Earlier visible answer waiting"
}

export function step(action) {
  const labels = {
    CursorAppend: "later terminal appears",
    CursorConfirmPrefix: "earlier outbox confirms its post",
    CursorPrefixProof: "consume earlier published range",
    CursorGapAdvance: "move past the accounted prefix",
    CursorObserveWithoutAdvance: "identify the prefix without moving the read start",
    CursorConsumeSent: "consume the confirmed answer range",
    CursorOpenWindow: "read complete records within one MiB",
    CursorPostVisiblePrefix: "post the earlier visible answer",
    CursorSinkSkip: "skip the already posted answer",
    CursorPostB: "post the later answer",
    CursorRetireCommitted: "consume the posted range",
  }
  return labels[action] ?? action
}
