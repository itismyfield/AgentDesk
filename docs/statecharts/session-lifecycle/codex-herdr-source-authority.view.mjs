export const title = "Codex Herdr first input and runtime progress";

export function state({ owner, listing, lookup, marker, writer }) {
  if (writer === "blocked" && lookup === "waiting") {
    return "No worker driving the pending database lookup or timer";
  }
  if (owner === "submitted") {
    return marker === "expected" ? "First input admitted" : "Stale first input admitted";
  }
  if (owner === "refused") return "First input refused";
  if (writer === "blocked") return "Writer waiting on pane ownership";
  if (writer === "deferred" && owner === "authority") {
    return lookup === "waiting"
      ? "Writer yielded while the lookup holds the pane"
      : "Writer yielded while input is checked";
  }
  if (lookup === "waiting") {
    return listing ? "Database lookup holds both pane locks" : "Database lookup leaves pane free";
  }
  if (owner === "authority") return "Pane ownership locked";
  if (listing) return "Listing order locked";
  if (lookup !== "none") return "Database lookup returned";
  return "Codex pane started";
}

export function step(action, _from, to) {
  if (["PollWriter", "RetryWriter", "WakeWriter"].includes(action)) {
    if (to.writer === "blocked") return "writer waits for the pane";
    if (to.writer === "deferred") return "writer defers this poll";
    return "writer observes the pane";
  }
  if (action === "BeginLookup") return "check the Pending execution";
  if (action === "FinishLookup") return "database responds";
  if (action === "TakeListing") return "enter the listing order";
  if (action === "TakeAuthority") return "take pane ownership";
  if (action === "FinishOwner") {
    return to.owner === "submitted" ? "admit the first input" : "refuse the changed execution";
  }
  if (action === "PollDiscovery") {
    return to.discovery === "waiting" ? "intake discovery waits on a blocking thread" : "intake discovery observes the pane";
  }
  if (action === "WakeDiscovery") return "intake discovery observes the pane";
  if (action === "ChangeMarker") return "pane starts another execution";
  if (action === "DriverTick") return "runtime processes a timer";
}
