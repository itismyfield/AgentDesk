---- MODULE SessionRotationSelector ----
\* After an in-pane /clear the hook moves the pane to a new Claude session. The channel's cached
\* selector (cache) and the stored selector (db) must follow it before the next turn reads them.
EXTENDS TLC

CONSTANTS Design,     \* "before" | "fix1" | "fixed"
          Restarts,   \* whether a dcserver restart may happen
          Caches,     \* starting caches: "old", or "none" when the channel holds no session
          Owners,     \* "watcher", or "herdr" for a reconnected Herdr pane with no watcher
          KeyMatches  \* whether the pane-name key resolves the channel's canonical row

VARIABLES cache, cleared, db, up, outages, rotation, followed, pinned, held, started, last,
          owner, keyMatch

vars == <<cache, cleared, db, up, outages, rotation, followed, pinned, held, started, last,
          owner, keyMatch>>

Init ==
    /\ cache \in Caches
    /\ cleared = FALSE /\ db = "old"
    /\ up = TRUE /\ outages = 0
    /\ rotation = "none"          \* none | pending | done | lost
    /\ followed = FALSE           \* a watcherless pane's selector already followed
    /\ pinned \in BOOLEAN         \* a turn on the frozen transcript still holds the channel
    /\ held = pinned              \* the queued input waits behind a running turn
    /\ started = "-"              \* session the queued input's turn resumed
    /\ last = "none"              \* last outside writer of the stored selector: none | clear | newer
    /\ owner \in Owners
    /\ keyMatch \in KeyMatches

\* The cache follows only while it still names the session the pane left.
Followed == IF ~cleared /\ cache = "old" THEN "new" ELSE cache

\* Whether the settle pass writes the stored selector, by design.
Write == IF Design = "fixed"
         THEN ~cleared /\ Followed \in {"new", "none"} /\ db = "old"
         ELSE Followed = "new" /\ cache # "new" /\ db = "old" /\ keyMatch
\* fixed: a failed write keeps the rotation; fix1 also counts a missed row as done
Ok == ~Write \/ up

Fixed2(rest) == UNCHANGED <<cleared, up, outages, started, last, owner, keyMatch>> /\ rest

\* hook adoption records the rotation
Adopt == /\ rotation = "none" /\ rotation' = "pending"
         /\ UNCHANGED <<cache, cleared, db, up, outages, followed, pinned, held, started, last,
                        owner, keyMatch>>

\* the frozen turn finishes from its own transcript; the finalizer releases the queue
FrozenFinish == /\ pinned /\ pinned' = FALSE /\ held' = FALSE
                /\ UNCHANGED <<cache, cleared, db, up, outages, rotation, followed, started, last,
                               owner, keyMatch>>

\* fix1/fixed with a watcher: the selector moves, then the queue is released
WatcherSettle ==
    /\ Design # "before" /\ owner = "watcher" /\ rotation = "pending"
    /\ cache' = Followed
    /\ db' = IF Write /\ up THEN "new" ELSE db
    /\ rotation' = IF Design = "fix1" \/ Ok THEN "done" ELSE "pending"
    /\ pinned' = FALSE /\ held' = FALSE
    /\ UNCHANGED followed
    /\ Fixed2(TRUE)

\* fixed, a Herdr pane with no watcher: only the selector follows, the rotation stays
HerdrFollow ==
    /\ Design = "fixed" /\ owner = "herdr" /\ rotation = "pending" /\ ~followed
    /\ cache' = Followed
    /\ db' = IF Write /\ up THEN "new" ELSE db
    /\ followed' = Ok
    /\ UNCHANGED <<rotation, pinned, held>>
    /\ Fixed2(TRUE)

\* before: the settle releases the queue first, then moves the selector and always retires
BeforeRelease == /\ Design = "before" /\ owner = "watcher" /\ rotation = "pending" /\ pinned
                 /\ pinned' = FALSE /\ held' = FALSE
                 /\ UNCHANGED <<cache, db, rotation, followed>>
                 /\ Fixed2(TRUE)
BeforeRetire ==
    /\ Design = "before" /\ owner = "watcher" /\ rotation = "pending" /\ ~pinned
    /\ cache' = Followed
    /\ db' = IF Followed = "new" /\ cache = "old" /\ keyMatch /\ up THEN "new" ELSE db
    /\ rotation' = "done" /\ held' = FALSE
    /\ UNCHANGED <<pinned, followed>>
    /\ Fixed2(TRUE)

Settle == WatcherSettle \/ HerdrFollow \/ BeforeRelease \/ BeforeRetire

\* the queued input's turn start (session_preparation.rs restore)
TurnStart ==
    /\ rotation # "none" /\ ~held /\ started = "-"
    /\ LET restored == IF cache = "none" /\ ~cleared /\ db # "null" THEN db ELSE cache
           c == IF Design # "before" /\ rotation = "pending" /\ ~cleared /\ restored = "old"
                THEN "new" ELSE restored
       IN /\ cache' = c
          /\ started' = c
    /\ UNCHANGED <<cleared, db, up, outages, rotation, followed, pinned, held, last, owner,
                   keyMatch>>

\* user !clear empties the cache and the stored selector
Clear == /\ cache' = "none" /\ cleared' = TRUE /\ db' = "null" /\ last' = "clear"
         /\ UNCHANGED <<up, outages, rotation, followed, pinned, held, started, owner, keyMatch>>

\* another writer stores a newer selector
DbNewer == /\ db' = "x" /\ last' = "newer"
           /\ UNCHANGED <<cache, cleared, up, outages, rotation, followed, pinned, held, started,
                          owner, keyMatch>>

StoreDown == /\ up /\ outages = 0 /\ up' = FALSE /\ outages' = 1
             /\ UNCHANGED <<cache, cleared, db, rotation, followed, pinned, held, started, last,
                            owner, keyMatch>>
StoreUp == /\ ~up /\ up' = TRUE
           /\ UNCHANGED <<cache, cleared, db, outages, rotation, followed, pinned, held, started,
                          last, owner, keyMatch>>

\* a restart loses the cache and the in-memory rotation record
Restart == /\ Restarts /\ rotation = "pending"
           /\ cache' = "none" /\ cleared' = FALSE /\ rotation' = "lost" /\ followed' = FALSE
           /\ UNCHANGED <<db, up, outages, pinned, held, started, last, owner, keyMatch>>

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
