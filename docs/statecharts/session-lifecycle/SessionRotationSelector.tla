---- MODULE SessionRotationSelector ----
\* After an in-pane /clear the hook moves the pane to a new Claude session. The channel's cached
\* selector (cache) and the stored selector (db) must follow it before the next turn reads them.
EXTENDS TLC

CONSTANTS Design,   \* "before" | "fixed"
          Restarts  \* whether a dcserver restart may happen

VARIABLES cache, cleared, db, up, outages, rotation, pinned, held, started, last

vars == <<cache, cleared, db, up, outages, rotation, pinned, held, started, last>>

Init ==
    /\ cache = "old" /\ cleared = FALSE /\ db = "old"
    /\ up = TRUE /\ outages = 0
    /\ rotation = "none"          \* none | pending | done | lost
    /\ pinned \in BOOLEAN         \* a turn on the frozen transcript still holds the channel
    /\ held = pinned              \* the queued input waits behind a running turn
    /\ started = "-"              \* session the queued input's turn resumed
    /\ last = "none"              \* last outside writer of the stored selector: none | clear | newer

\* The cache follows only while it still names the session the pane left.
Followed == IF ~cleared /\ cache = "old" THEN "new" ELSE cache

\* hook adoption records the rotation (adoption_retry.rs settle -> session_rotation.rs record)
Adopt == /\ rotation = "none" /\ rotation' = "pending"
         /\ UNCHANGED <<cache, cleared, db, up, outages, pinned, held, started, last>>

\* the frozen turn finishes from its own transcript; the finalizer releases the queue
FrozenFinish == /\ pinned /\ pinned' = FALSE /\ held' = FALSE
                /\ UNCHANGED <<cache, cleared, db, up, outages, rotation, started, last>>

\* fixed: the stored selector moves only off the old session, and a failure keeps the rotation
FixedSettle ==
    /\ rotation = "pending"
    /\ LET c == Followed
           write == c = "new" /\ db = "old"
           ok == ~write \/ up
       IN /\ cache' = c
          /\ db' = IF write /\ up THEN "new" ELSE db
          /\ rotation' = IF ok THEN "done" ELSE "pending"
    /\ pinned' = FALSE /\ held' = FALSE
    /\ UNCHANGED <<cleared, up, outages, started, last>>

\* before: the settle releases the queue first, then moves the selector and always retires
BeforeRelease == /\ rotation = "pending" /\ pinned
                 /\ pinned' = FALSE /\ held' = FALSE
                 /\ UNCHANGED <<cache, cleared, db, up, outages, rotation, started, last>>
BeforeRetire ==
    /\ rotation = "pending" /\ ~pinned
    /\ cache' = Followed
    /\ db' = IF Followed = "new" /\ cache = "old" /\ up THEN "new" ELSE db
    /\ rotation' = "done"
    /\ held' = FALSE
    /\ UNCHANGED <<cleared, up, outages, pinned, started, last>>

Settle == IF Design = "fixed" THEN FixedSettle ELSE BeforeRelease \/ BeforeRetire

\* the queued input's turn start (session_preparation.rs restore)
TurnStart ==
    /\ rotation # "none" /\ ~held /\ started = "-"
    /\ LET c == IF Design = "fixed" /\ rotation = "pending" THEN Followed ELSE cache
       IN /\ cache' = c
          /\ started' = IF c = "none" /\ ~cleared
                           THEN (IF db = "null" THEN "none" ELSE db) ELSE c
    /\ UNCHANGED <<cleared, db, up, outages, rotation, pinned, held, last>>

\* user !clear empties the cache and the stored selector
Clear == /\ cache' = "none" /\ cleared' = TRUE /\ db' = "null" /\ last' = "clear"
         /\ UNCHANGED <<up, outages, rotation, pinned, held, started>>

\* another writer stores a newer selector
DbNewer == /\ db' = "x" /\ last' = "newer"
           /\ UNCHANGED <<cache, cleared, up, outages, rotation, pinned, held, started>>

StoreDown == /\ up /\ outages = 0 /\ up' = FALSE /\ outages' = 1
             /\ UNCHANGED <<cache, cleared, db, rotation, pinned, held, started, last>>
StoreUp == /\ ~up /\ up' = TRUE
           /\ UNCHANGED <<cache, cleared, db, outages, rotation, pinned, held, started, last>>

\* a restart loses the cache and the in-memory rotation record
Restart == /\ Restarts /\ rotation = "pending"
           /\ cache' = "none" /\ cleared' = FALSE /\ rotation' = "lost"
           /\ UNCHANGED <<db, up, outages, pinned, held, started, last>>

Next == Adopt \/ FrozenFinish \/ Settle \/ TurnStart \/ Clear \/ DbNewer
        \/ StoreDown \/ StoreUp \/ Restart

Spec == Init /\ [][Next]_vars /\ WF_vars(Settle) /\ WF_vars(StoreUp)

\* the first turn after the rotation never resumes the session the pane left
NoOldResume == started # "old"
\* a user clear or a newer stored selector is not overwritten
NoOverwrite == /\ (last = "clear" => db = "null")
               /\ (last = "newer" => db = "x")
\* the stored selector eventually leaves the old session once the pane rotated
StoredSelectorFollows == (rotation # "none") ~> (db # "old")
====
