---- MODULE SessionRotationSelector ----
\* After an in-pane /clear the hook moves the pane to a new Claude session. The channel's cached
\* selector (cache) and the stored selector (db) must follow it before the next turn reads them.
EXTENDS TLC

CONSTANTS Design,     \* "before" | "fix1" | "fix2" | "fixed"
          Restarts,   \* whether a dcserver restart may happen
          Caches,     \* starting caches: "old", or "none" when the channel holds no session
          Owners,     \* "watcher", or "herdr" for a reconnected Herdr pane with no watcher
          KeyMatches, \* whether the pane-name key resolves the channel's canonical row
          Hops,       \* whether the pane may hop once more (new -> c) before any Discord turn
          Bots        \* "own": the idle loop runs on the channel's bot; "other": on another bot

VARIABLES cache, cleared, db, up, outages, rotation, followed, pinned, held, phase, read,
          started, stale, last, owner, keyMatch, hopped, bot

vars == <<cache, cleared, db, up, outages, rotation, followed, pinned, held, phase, read,
          started, stale, last, owner, keyMatch, hopped, bot>>

Init ==
    /\ cache \in Caches /\ cleared = FALSE /\ db = "old"
    /\ up = TRUE /\ outages = 0
    /\ rotation = "none"          \* none | pending | done | lost
    /\ followed = FALSE           \* a watcherless pane's selector already followed this target
    /\ pinned \in BOOLEAN         \* a turn on the frozen transcript still holds the channel
    /\ held = pinned              \* the queued input waits behind a running turn
    /\ phase = "idle"             \* the queued input's turn start: idle | read | done
    /\ read = "-"                 \* what that turn start read before writing the cache
    /\ started = "-" /\ stale = FALSE
    /\ last = "none"              \* last outside writer of the stored selector: none | clear | newer
    /\ owner \in Owners /\ keyMatch \in KeyMatches /\ bot \in Bots
    /\ hopped = FALSE

Target == IF hopped THEN "c" ELSE "new"
\* sessions the pane left; fixed keeps all of them, the earlier designs only the first
Left == IF hopped THEN {"old", "new"} ELSE {"old"}
Base == IF Design = "fixed" THEN Left ELSE {"old"}

Followed == IF ~cleared /\ cache \in Base THEN Target ELSE cache
RowFound == owner = "watcher" \/ bot = "own" \/ Design = "fixed"
Write == IF Design \in {"fix2", "fixed"}
         THEN ~cleared /\ Followed \in {Target, "none"} /\ db \in Base /\ RowFound
         ELSE Followed = Target /\ cache # Target /\ db \in Base /\ keyMatch
Ok == ~Write \/ up
\* fixed: the settle pass waits while a turn start holds the channel transition
Free == Design # "fixed" \/ phase # "read"

Keep(v) == UNCHANGED v
Rest == <<cleared, up, outages, phase, read, started, stale, last, owner, keyMatch, hopped, bot>>

Adopt == /\ rotation = "none" /\ rotation' = "pending"
         /\ Keep(<<cache, db, followed, pinned, held>>) /\ Keep(Rest)

\* a second hook adoption on the same pane while the record stays
Hop == /\ Hops /\ rotation = "pending" /\ ~hopped /\ hopped' = TRUE /\ followed' = FALSE
       /\ Keep(<<cache, cleared, db, up, outages, rotation, pinned, held, phase, read, started,
                stale, last, owner, keyMatch, bot>>)

FrozenFinish == /\ pinned /\ pinned' = FALSE /\ held' = FALSE
                /\ Keep(<<cache, db, rotation, followed>>) /\ Keep(Rest)

WatcherSettle ==
    /\ Design # "before" /\ owner = "watcher" /\ rotation = "pending" /\ Free
    /\ cache' = Followed
    /\ db' = IF Write /\ up THEN Target ELSE db
    /\ rotation' = IF Design = "fix1" \/ Ok THEN "done" ELSE "pending"
    /\ pinned' = FALSE /\ held' = FALSE
    /\ Keep(followed) /\ Keep(Rest)

\* a Herdr pane with no watcher: only the selector follows, the rotation stays
HerdrFollow ==
    /\ Design \in {"fix2", "fixed"} /\ owner = "herdr" /\ rotation = "pending" /\ ~followed
    /\ Free /\ RowFound
    /\ cache' = Followed
    /\ db' = IF Write /\ up THEN Target ELSE db
    /\ followed' = Ok
    /\ Keep(<<rotation, pinned, held>>) /\ Keep(Rest)

BeforeRelease == /\ Design = "before" /\ owner = "watcher" /\ rotation = "pending" /\ pinned
                 /\ pinned' = FALSE /\ held' = FALSE
                 /\ Keep(<<cache, db, rotation, followed>>) /\ Keep(Rest)
BeforeRetire ==
    /\ Design = "before" /\ owner = "watcher" /\ rotation = "pending" /\ ~pinned
    /\ cache' = Followed
    /\ db' = IF Followed = Target /\ cache \in Base /\ keyMatch /\ up THEN Target ELSE db
    /\ rotation' = "done" /\ held' = FALSE
    /\ Keep(<<pinned, followed>>) /\ Keep(Rest)

Settle == WatcherSettle \/ HerdrFollow \/ BeforeRelease \/ BeforeRetire

\* the queued input's turn start reads the cache, or the store when the cache is empty, under
\* the channel transition, then writes the cache and applies a rotation still pending
TurnRead == /\ rotation # "none" /\ ~held /\ phase = "idle" /\ phase' = "read"
            /\ read' = IF cache = "none" /\ ~cleared /\ db # "null" THEN db ELSE cache
            /\ Keep(<<cache, cleared, db, up, outages, rotation, followed, pinned, held, started,
                     stale, last, owner, keyMatch, hopped, bot>>)
TurnFinish ==
    /\ phase = "read" /\ phase' = "done"
    /\ LET c == IF Design # "before" /\ rotation = "pending" /\ ~cleared /\ read \in Base
                THEN Target ELSE read
       IN /\ cache' = c /\ started' = c /\ stale' = (c \in Left)
    /\ Keep(<<cleared, db, up, outages, rotation, followed, pinned, held, read, last, owner,
             keyMatch, hopped, bot>>)

\* user !clear takes the same channel transition as a turn start
Clear == /\ phase # "read"
         /\ cache' = "none" /\ cleared' = TRUE /\ db' = "null" /\ last' = "clear"
         /\ Keep(<<up, outages, rotation, followed, pinned, held, phase, read, started, stale,
                  owner, keyMatch, hopped, bot>>)

DbNewer == /\ db' = "x" /\ last' = "newer"
           /\ Keep(<<cache, cleared, up, outages, rotation, followed, pinned, held, phase, read,
                    started, stale, owner, keyMatch, hopped, bot>>)

StoreDown == /\ up /\ outages = 0 /\ up' = FALSE /\ outages' = 1
             /\ Keep(<<cache, cleared, db, rotation, followed, pinned, held, phase, read, started,
                      stale, last, owner, keyMatch, hopped, bot>>)
StoreUp == /\ ~up /\ up' = TRUE
           /\ Keep(<<cache, cleared, db, outages, rotation, followed, pinned, held, phase, read,
                    started, stale, last, owner, keyMatch, hopped, bot>>)

Restart == /\ Restarts /\ rotation = "pending" /\ phase # "read"
           /\ cache' = "none" /\ cleared' = FALSE /\ rotation' = "lost" /\ followed' = FALSE
           /\ Keep(<<db, up, outages, pinned, held, phase, read, started, stale, last, owner,
                    keyMatch, hopped, bot>>)

Next == Adopt \/ Hop \/ FrozenFinish \/ Settle \/ TurnRead \/ TurnFinish \/ Clear \/ DbNewer
        \/ StoreDown \/ StoreUp \/ Restart

Spec == Init /\ [][Next]_vars /\ WF_vars(Settle) /\ WF_vars(StoreUp) /\ WF_vars(TurnFinish)

\* the first turn after the rotation never resumes a session the pane left
NoOldResume == ~stale
\* a user clear or a newer stored selector is not overwritten
NoOverwrite == /\ (last = "clear" => db = "null")
               /\ (last = "newer" => db = "x")
\* the stored selector eventually leaves every session the pane left
StoredSelectorFollows == (rotation # "none") ~> (db \notin Left)
====
