export const title = "Herdr inflight row: stale-age verdict against the recorded pane";
export const state = (r) => {
  if (r.turn === "running" && r.row !== "present") return "Running turn's row hidden (answer lost)";
  const row = {present: "row kept", hidden: "row hidden from scans", absent: "row deleted"}[r.row];
  const pane = r.pane === "live" ? (r.turn === "running" ? "turn running" : "turn ended, pane idle") : "pane closed";
  const server = r.listing === "empty" ? ", replacing server lists none" : r.herdr === "down" ? ", Herdr silent" : "";
  return `${r.gen === "current" ? "this process" : "prior process"} ${r.path ? "row" : "path-less row"}: ${pane}${server}, ${row}${r.age === "fresh" ? ", fresh" : ""}${r.outages ? ", after an outage" : ""}`;
};
export const step = (action) => ({
  TurnEnds: "turn finishes", PaneCloses: "pane closed or retired", ServerReplaced: "another server answers with no panes",
  Ages: "row passes stale age", HerdrDown: "Herdr stops answering", HerdrUp: "Herdr answers again",
  Restart: "dcserver restarts (boot reaper)", OwnerEnd: "owner finalizes its turn", RowEnd: "Herdr row-end pass settles row",
  StaleCheck: "stale check reads pane gone"
}[action] || action);
