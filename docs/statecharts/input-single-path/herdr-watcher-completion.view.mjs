export const title = "A Claude Herdr answer is posted while its lease remains held";

export function state({ st }) {
  if (st.front === "Answered") return "Next answer posted";
  if (st.front === "Delivered") return "Next input delivered";
  if (!st.lease) return st.host === "Idle" ? "Lease released and owner woken" : "Lease released before the Idle reading";
  if (st.runtime === "Restoring") return "Reloading the saved turn";
  if (st.source === "Prelaunch") return "Prelaunch with a queued input";
  const front = st.front === "Held" ? "input held" : "input queued";
  const host = `Host ${st.host}, ${front}`;
  const answer = st.priorPosts === 1 ? "answer posted" : "answer pending";
  if (st.deadline === "NativeCustody") return "Native input keeps custody";
  if (st.deadline === "BackstopTerminal") return "Far backstop proves terminal";
  if (st.deadline === "Extended1800" || st.deadline === "RetryFar") {
    const deadline = st.deadline === "Extended1800" ? "deadline extended 1,800 seconds" : "far check defers";
    const condition = ["Paused", "PausedLive", "Unknown"].includes(st.recheck) ? "still unproven" : "later proof possible";
    return `${host}, ${deadline}, ${condition}`;
  }
  if (st.deadline === "Pulled") return `${host}, deadline pulled`;
  if (st.frame === "Rejected") {
    const check = st.probes === 1 ? "first strict probe passed" : "terminal refused";
    return `${host}, ${answer}, ${check}`;
  }
  const frame = st.frame === "Submitted" ? "terminal submitted" : "terminal pending";
  return `${host}, ${answer}, ${frame}`;
}

const labels = {
  NativeHandoff: "watcher attaches to the transcript",
  HostGoesIdle: "host reports Idle",
  PublishPriorAnswer: "answer appears",
  OwnerHoldsFront: "owner keeps the queued input",
  WatcherReadsFrame: "watcher checks the frame start",
  ProbeBackstop: "strict probe confirms terminal evidence",
  ExtendStrictDeadline: "the final strict check defers",
  NaturalDeadlineRechecks: "far deadline reaches its next check",
  RetryFarDeadline: "the deadline is extended again",
  CanonicalRelease: "finalizer releases the matching lease",
  OwnerDeliversFront: "owner sends the next input",
  PublishNextAnswer: "next answer appears",
  KeepNativeCustody: "native input keeps custody",
  Restart: "server restarts",
  RestoreInflight: "watcher reloads the same turn",
};

export const step = (action) => labels[action] ?? null;
