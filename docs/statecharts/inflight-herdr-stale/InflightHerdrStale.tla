---- MODULE InflightHerdrStale ----
\* One Herdr turn's inflight row against its pane, the Herdr server, a dcserver restart and the
\* stale-age verdict that hides a row from the scan loaders (the boot reaper then deletes it).
EXTENDS Naturals

CONSTANT Mode   \* "before" | "after" | "noDead" | "noEmptyGuard" | "noPathGuard"

VARIABLES pane, turn, listing, herdr, row, age, gen, path, restarts, outages
vars == <<pane, turn, listing, herdr, row, age, gen, path, restarts, outages>>

TypeOK ==
    /\ pane \in {"live", "closed"}
    /\ turn \in {"running", "ended"}
    /\ listing \in {"full", "empty"}
    /\ herdr \in {"up", "down"}
    /\ row \in {"present", "hidden", "absent"}
    /\ age \in {"fresh", "stale"}
    /\ gen \in {"current", "prior"}
    /\ path \in BOOLEAN
    /\ restarts \in 0..1
    /\ outages \in 0..1

Init ==
    /\ pane = "live" /\ turn = "running" /\ listing = "full" /\ herdr = "up"
    /\ row = "present" /\ age = "fresh" /\ gen = "current" /\ path \in BOOLEAN
    /\ restarts = 0 /\ outages = 0

\* The provider finishes the turn; its pane stays open (idle).
TurnEnds == turn = "running" /\ turn' = "ended"
    /\ UNCHANGED <<pane, listing, herdr, row, age, gen, path, restarts, outages>>

\* The pane is closed or retired; nothing in it runs any more.
PaneCloses == pane = "live" /\ pane' = "closed" /\ turn' = "ended"
    /\ UNCHANGED <<listing, herdr, row, age, gen, path, restarts, outages>>

\* A fresh server answers on the socket and lists no pane while the old one may still run the turn.
ServerReplaced == listing = "full" /\ listing' = "empty"
    /\ UNCHANGED <<pane, turn, herdr, row, age, gen, path, restarts, outages>>

\* The row is not saved again during a Herdr turn, so a running turn's row ages too.
Ages == age = "fresh" /\ age' = "stale"
    /\ UNCHANGED <<pane, turn, listing, herdr, row, gen, path, restarts, outages>>

HerdrDown == herdr = "up" /\ outages = 0 /\ herdr' = "down" /\ outages' = 1
    /\ UNCHANGED <<pane, turn, listing, row, age, gen, path, restarts>>

HerdrUp == herdr = "down" /\ herdr' = "up"
    /\ UNCHANGED <<pane, turn, listing, row, age, gen, path, restarts, outages>>

\* dcserver restarts: the row now belongs to a prior incarnation; the boot reaper deletes a hidden row.
Restart == restarts = 0 /\ restarts' = 1 /\ gen' = "prior"
    /\ row' = (IF row = "hidden" THEN "absent" ELSE row)
    /\ UNCHANGED <<pane, turn, listing, herdr, age, path, outages>>

\* This incarnation's own turn is finalized by its owner (bridge or live end pass).
OwnerEnd == gen = "current" /\ turn = "ended" /\ row = "present" /\ row' = "absent"
    /\ UNCHANGED <<pane, turn, listing, herdr, age, gen, path, restarts, outages>>

\* The Herdr row-end pass settles a prior incarnation's path-less row it can still see in its scan.
RowEnd == gen = "prior" /\ ~path /\ turn = "ended" /\ row = "present" /\ row' = "absent"
    /\ UNCHANGED <<pane, turn, listing, herdr, age, gen, path, restarts, outages>>

\* What the stale check reads of the recorded pane on its endpoint.
Observed ==
    IF herdr = "down" THEN "unknown"
    ELSE IF listing = "empty" THEN IF Mode = "noEmptyGuard" THEN "missing" ELSE "unknown"
    ELSE IF pane = "closed" THEN IF Mode = "noDead" THEN "unknown" ELSE "missing"
    ELSE "alive"

\* Before: a non-tmux host always keeps the row. The loader gate keeps current-generation rows.
StaleHides ==
    /\ Mode # "before"
    /\ path \/ Mode = "noPathGuard"
    /\ Observed = "missing"

StaleCheck == row = "present" /\ age = "stale" /\ gen = "prior" /\ StaleHides /\ row' = "hidden"
    /\ UNCHANGED <<pane, turn, listing, herdr, age, gen, path, restarts, outages>>

Next == TurnEnds \/ PaneCloses \/ ServerReplaced \/ Ages \/ HerdrDown \/ HerdrUp
    \/ Restart \/ OwnerEnd \/ RowEnd \/ StaleCheck

Spec == Init /\ [][Next]_vars
    /\ WF_vars(Ages) /\ WF_vars(HerdrUp) /\ WF_vars(StaleCheck)
    /\ WF_vars(OwnerEnd) /\ WF_vars(RowEnd)

\* Safety: a running turn's row is never hidden or deleted (its answer would be lost or doubled).
NoLiveTurnRowHidden == turn = "running" => row = "present"
\* Safety: a path-less row stays visible to the row-end pass that settles it.
NoPathlessHidden == ~path => row # "hidden"

\* Liveness: once the pane is gone the row leaves the stale scan, unless only a replacing
\* server answers (the old one may still run the turn).
ClosedPaneRowRetires == (pane = "closed") ~> (row # "present" \/ listing = "empty")
\* Known gap: a prior incarnation's ended turn whose pane stays open (idle) keeps its row.
IdlePaneRowRetires == (turn = "ended" /\ pane = "live") ~> (row # "present")
====
