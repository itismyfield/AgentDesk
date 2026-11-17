------------------------- MODULE SessionNameDualRead -------------------------
\* #8119 dual-read window, one channel. Bounded projection: names, marker, row
\* primary locator, probe outcome, restart. Not the full lifecycle.
EXTENDS Naturals, FiniteSets
CONSTANT Mode
Modes == {"fixed", "generator_first", "name_adopt", "probe_absent",
          "prefix_trust", "mixed_builder", "early_legacy_removal", "name_reap"}
ASSUME Mode \in Modes
VARIABLES oldLive, newLive, fbLive, userNew, userOld, row, adopted, cut, foreign
vars == <<oldLive, newLive, fbLive, userNew, userOld, row, adopted, cut, foreign>>
Managed == {"old", "new", "fb"}
Live(s) == CASE s = "old" -> oldLive [] s = "new" -> newLive [] s = "fb" -> fbLive
Init == /\ oldLive \in BOOLEAN            \* old-name managed session may be live
        /\ newLive = FALSE /\ fbLive = FALSE
        /\ userNew \in BOOLEAN            \* a user tmux session already named adk-cc
        /\ userOld \in BOOLEAN /\ (userOld => ~oldLive)
        /\ row \in IF oldLive THEN {"old"} ELSE {"none", "old"}
        /\ adopted = IF oldLive THEN "old" ELSE "none"
        /\ cut = FALSE /\ foreign = FALSE
OldEnds == /\ oldLive /\ oldLive' = FALSE
           /\ adopted' = IF adopted = "old" THEN "none" ELSE adopted
           /\ UNCHANGED <<newLive, fbLive, userNew, userOld, row, cut, foreign>>
UserTakesNew == /\ ~userNew /\ ~newLive /\ userNew' = TRUE
                /\ UNCHANGED <<oldLive, newLive, fbLive, userOld, row, adopted, cut, foreign>>
UserTakesOld == /\ ~userOld /\ ~oldLive /\ userOld' = TRUE
                /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, row, adopted, cut, foreign>>
Restart == /\ adopted' = "none"
           /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
\* Spawn under the new name; a name held by anyone else falls to the fallback candidate.
SpawnNew == IF userNew \/ newLive
            THEN /\ fbLive' = TRUE /\ adopted' = "fb" /\ row' = "fb"
                 /\ UNCHANGED <<oldLive, newLive>>
            ELSE /\ newLive' = TRUE /\ adopted' = "new" /\ row' = "new"
                 /\ UNCHANGED <<oldLive, fbLive>>
\* Resolver: stored row first, probe failure defers, marker decides ownership.
Resolve(probeOk) ==
  CASE ~probeOk /\ Mode # "probe_absent" -> UNCHANGED vars
    [] Mode = "prefix_trust" /\ row = "old" /\ ~oldLive /\ userOld ->
         /\ adopted' = "userOld"
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] Mode = "name_adopt" /\ row # "new" /\ ~(row = "old" /\ oldLive) /\ userNew /\ ~newLive ->
         /\ adopted' = "userNew"
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] Mode \notin {"generator_first"} /\ probeOk /\ row = "old" /\ oldLive ->
         /\ adopted' = "old"
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] row \in {"new", "fb"} /\ Live(row) ->
         /\ adopted' = row
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] OTHER -> SpawnNew /\ UNCHANGED <<userNew, userOld, cut, foreign>>
Turn == /\ adopted = "none"
        /\ \E probeOk \in BOOLEAN : Resolve(probeOk)
\* A caller still on the old builder (not migrated) re-derives the old name.
LegacyCaller == /\ Mode = "mixed_builder"
                /\ row \in {"new", "fb"} /\ Live(row) /\ ~oldLive /\ ~userOld
                /\ oldLive' = TRUE
                /\ UNCHANGED <<newLive, fbLive, userNew, userOld, row, adopted, cut, foreign>>
\* Cleanup sweep. Fixed: kills only marker-owned sessions neither row nor adopted names.
Reap ==
  \/ /\ Mode = "early_legacy_removal" /\ oldLive
     /\ oldLive' = FALSE /\ cut' = (cut \/ adopted = "old" \/ row = "old")
     /\ adopted' = IF adopted = "old" THEN "none" ELSE adopted
     /\ UNCHANGED <<newLive, fbLive, userNew, userOld, row, foreign>>
  \/ /\ Mode = "name_reap" /\ userNew /\ adopted # "userNew"
     /\ userNew' = FALSE /\ foreign' = TRUE
     /\ UNCHANGED <<oldLive, newLive, fbLive, userOld, row, adopted, cut>>
  \/ /\ \E s \in Managed : Live(s) /\ s # row /\ s # adopted
     /\ UNCHANGED vars
Next == OldEnds \/ UserTakesNew \/ UserTakesOld \/ Restart \/ Turn \/ LegacyCaller \/ Reap
Spec == Init /\ [][Next]_vars
NoDuplicate == Cardinality({s \in Managed : Live(s)}) <= 1
NoForeignAdoption == adopted \notin {"userNew", "userOld"}
NoForeignTouch == ~foreign
NoLiveCut == ~cut
==============================================================================
