---- MODULE SessionTransitionLeaseIdentity ----
\* #7353 KR-C10e. Who a session-transition lease identifies, and what the per-runtime lock registry
\* guarantees. Coordinates are src/services/ at the KR-C10e-2 head.
\*   discord/session_transition.rs:12-71    SessionTransitionLease (try_acquire, borrow, matches)
\*   discord/session_transition.rs:74-120   UnplacedLease (acquire, try_acquire, covers)
\*   discord/session_transition.rs:126-166  SessionTransitionLocks: lookup, Weak entries, prune on vacant insert
\*   engine_runtime/legacy_state.rs:7-8     the registry value, one per EngineRuntime
\*   discord/runtime_bootstrap/shared_data.rs:88,113 and discord/shared_data.rs:346,371
\*                                          the two constructors: one new EngineRuntime each
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Hosts,     \* the SharedData objects of one process (one per bot)
          Channels,
          MaxMutex,  \* bound on lock allocations
          Wiring,    \* "own": every SharedData has its own EngineRuntime (the code)
                     \* "shared": hypothetical, all SharedData point at one EngineRuntime
          Ident,     \* what `matches` compares:
                     \* "host": SharedData address + channel + lock (session_transition.rs:63-69, the code)
                     \* "registry": registry address + channel + lock (hypothetical)
                     \* "lockOnly": channel + lock (hypothetical, clause :63 dropped)
          Tokens,    \* "perHost": every SharedData has its own token hash (prod: discord/settings/read.rs:358-359 dedup)
                     \* "same": all share one (the test fixtures, discord/shared_data.rs:446)
          Prune      \* "deadOnly": prune entries with no strong holder (:150, the code)
                     \* "all": hypothetical, prune every other entry

VARIABLES reg,      \* registry: (engine, channel) -> lock id, 0 = no entry; an entry is a Weak
          home,     \* the registry key a lock was created under
          held,     \* who holds a lock's guard: free | raw | lease | unplaced
          pins,     \* unlocked Arc handles kept by a caller (control.rs:566, health/o_transition.rs:50,65)
          raws,     \* plain OwnedMutexGuards: [by, ch, m]
          leases,   \* SessionTransitionLease: [maker, ch, m, hold]
          unplaced, \* UnplacedLease: [by, tok, ch, m]
          next
vars == <<reg, home, held, pins, raws, leases, unplaced, next>>

Engines == IF Wiring = "own" THEN Hosts ELSE {"engine"}
EngineOf(s) == IF Wiring = "own" THEN s ELSE "engine"
TokenOf(s) == IF Tokens = "perHost" THEN s ELSE "token"   \* shared.token_hash
Keys == Engines \X Channels
Mutexes == 1..MaxMutex
NoKey == <<"none", "none">>

Alive(m) == m \in Mutexes /\ (held[m] # "free" \/ \E p \in pins : p.m = m)
Free(m) == m \in Mutexes /\ held[m] = "free"

\* SessionTransitionLocks::lock (:131-154): upgrade the entry, else create, insert and maybe prune.
Fresh(k) == reg[k] = 0 \/ ~Alive(reg[k])
CanResolve(k) == ~Fresh(k) \/ next <= MaxMutex
LockFor(k) == IF Fresh(k) THEN next ELSE reg[k]
Pruned(k, x) == x[1] = k[1] /\ reg[x] # 0
                /\ (Prune = "all" \/ (reg[k] = 0 /\ ~Alive(reg[x])))
RegAfter(k) == IF ~Fresh(k) THEN reg
               ELSE [x \in Keys |-> IF x = k THEN next ELSE IF Pruned(k, x) THEN 0 ELSE reg[x]]
HomeAfter(k) == IF Fresh(k) THEN [home EXCEPT ![next] = k] ELSE home
NextAfter(k) == IF Fresh(k) THEN next + 1 ELSE next
Resolve(k) == reg' = RegAfter(k) /\ home' = HomeAfter(k) /\ next' = NextAfter(k)

Init ==
    /\ reg = [k \in Keys |-> 0] /\ home = [m \in Mutexes |-> NoKey]
    /\ held = [m \in Mutexes |-> "free"]
    /\ pins = {} /\ raws = {} /\ leases = {} /\ unplaced = {} /\ next = 1

\* shared.session_transition_locks.lock(c).try_lock_owned() / .acquire(c) (:156-166)
AcquireRaw(s, c) ==
    LET k == <<EngineOf(s), c>> m == LockFor(k) IN
    /\ CanResolve(k) /\ Free(m)
    /\ held' = [held EXCEPT ![m] = "raw"]
    /\ raws' = raws \cup {[by |-> s, ch |-> c, m |-> m]}
    /\ Resolve(k) /\ UNCHANGED <<pins, leases, unplaced>>

\* SessionTransitionLease::try_acquire (:24-39)
AcquireLease(s, c) ==
    LET k == <<EngineOf(s), c>> m == LockFor(k) IN
    /\ CanResolve(k) /\ Free(m)
    /\ held' = [held EXCEPT ![m] = "lease"]
    /\ leases' = leases \cup {[maker |-> s, ch |-> c, m |-> m, hold |-> "owned"]}
    /\ Resolve(k) /\ UNCHANGED <<pins, raws, unplaced>>

\* SessionTransitionLease::borrow (:40-57): the only check is that the guard's lock is the one
\* the registry of `s` returns for `c`. A failed check is the Probe step below.
Borrow(s, c, g) ==
    LET k == <<EngineOf(s), c>> IN
    /\ g \in raws /\ ~Fresh(k) /\ reg[k] = g.m
    /\ leases' = leases \cup {[maker |-> s, ch |-> c, m |-> g.m, hold |-> "borrowed"]}
    /\ UNCHANGED <<reg, home, held, pins, raws, unplaced, next>>

\* UnplacedLease::acquire / try_acquire (:82-112): the guard plus a copy of the maker's token hash
AcquireUnplaced(s, c) ==
    LET k == <<EngineOf(s), c>> m == LockFor(k) IN
    /\ CanResolve(k) /\ Free(m)
    /\ held' = [held EXCEPT ![m] = "unplaced"]
    /\ unplaced' = unplaced \cup {[by |-> s, tok |-> TokenOf(s), ch |-> c, m |-> m]}
    /\ Resolve(k) /\ UNCHANGED <<pins, raws, leases>>

\* A lookup whose handle is dropped at once: a busy try_lock, a failed borrow, a `matches` call.
Probe(s, c) ==
    LET k == <<EngineOf(s), c>> IN
    /\ Fresh(k) /\ next <= MaxMutex
    /\ Resolve(k) /\ UNCHANGED <<held, pins, raws, leases, unplaced>>

\* A caller keeps the Arc and locks it later.
Pin(s, c) ==
    LET k == <<EngineOf(s), c>> m == LockFor(k) IN
    /\ CanResolve(k) /\ [by |-> s, ch |-> c, m |-> m] \notin pins
    /\ pins' = pins \cup {[by |-> s, ch |-> c, m |-> m]}
    /\ Resolve(k) /\ UNCHANGED <<held, raws, leases, unplaced>>
LockPin(p) ==
    /\ p \in pins /\ held[p.m] = "free"
    /\ held' = [held EXCEPT ![p.m] = "raw"]
    /\ raws' = raws \cup {[by |-> p.by, ch |-> p.ch, m |-> p.m]}
    /\ UNCHANGED <<reg, home, pins, leases, unplaced, next>>
Unpin(p) == /\ p \in pins /\ pins' = pins \ {p}
            /\ UNCHANGED <<reg, home, held, raws, leases, unplaced, next>>

\* A borrowed lease borrows the guard (`'a`), so the guard outlives it.
DropRaw(g) ==
    /\ g \in raws /\ ~\E l \in leases : l.hold = "borrowed" /\ l.m = g.m
    /\ raws' = raws \ {g} /\ held' = [held EXCEPT ![g.m] = "free"]
    /\ UNCHANGED <<reg, home, pins, leases, unplaced, next>>
DropLease(l) ==
    /\ l \in leases /\ leases' = leases \ {l}
    /\ held' = IF l.hold = "owned" THEN [held EXCEPT ![l.m] = "free"] ELSE held
    /\ UNCHANGED <<reg, home, pins, raws, unplaced, next>>
DropUnplaced(u) ==
    /\ u \in unplaced /\ unplaced' = unplaced \ {u}
    /\ held' = [held EXCEPT ![u.m] = "free"]
    /\ UNCHANGED <<reg, home, pins, raws, leases, next>>

Exhausted == next > MaxMutex /\ UNCHANGED vars

HostTakesGuard == \E s \in Hosts, c \in Channels : AcquireRaw(s, c)
HostTakesLease == \E s \in Hosts, c \in Channels : AcquireLease(s, c)
HostTakesUnplaced == \E s \in Hosts, c \in Channels : AcquireUnplaced(s, c)
HostProbes == \E s \in Hosts, c \in Channels : Probe(s, c)
HostKeepsHandle == \E s \in Hosts, c \in Channels : Pin(s, c)
HostBorrows == \E s \in Hosts, c \in Channels, g \in raws : Borrow(s, c, g)
HandleLocks == \E p \in pins : LockPin(p)
HandleDrops == \E p \in pins : Unpin(p)
GuardDrops == \E g \in raws : DropRaw(g)
LeaseDrops == \E l \in leases : DropLease(l)
UnplacedDrops == \E u \in unplaced : DropUnplaced(u)

Next ==
    \/ HostTakesGuard \/ HostTakesLease \/ HostTakesUnplaced \/ HostProbes \/ HostKeepsHandle
    \/ HostBorrows \/ HandleLocks \/ HandleDrops \/ GuardDrops \/ LeaseDrops \/ UnplacedDrops
    \/ Exhausted

Spec == Init /\ [][Next]_vars

\* `matches` (:58-71). The lease keeps its lock alive, so "the registry returns this lock" is
\* "the entry is this lock"; a dead or missing entry would return a new lock.
SameLock(l, s, c) == reg[<<EngineOf(s), c>>] = l.m
Matches(l, s, c) ==
    CASE Ident = "host" -> l.maker = s /\ l.ch = c /\ SameLock(l, s, c)
      [] Ident = "registry" -> EngineOf(l.maker) = EngineOf(s) /\ l.ch = c /\ SameLock(l, s, c)
      [] Ident = "lockOnly" -> l.ch = c /\ SameLock(l, s, c)

\* The rule: one lease fits only the transition of the SharedData that made it.
LeaseFitsOnlyItsMaker ==
    \A l \in leases, s \in Hosts, c \in Channels : Matches(l, s, c) => s = l.maker /\ c = l.ch
\* A live lease always fits its own maker and channel.
LeaseFitsItsMaker == \A l \in leases : Matches(l, l.maker, l.ch)
\* A borrowed lease stands on a guard its own maker took for the same channel.
BorrowedFromOwnGuard ==
    \A l \in leases : l.hold = "borrowed" =>
        \E g \in raws : g.m = l.m /\ g.by = l.maker /\ g.ch = l.ch
\* One runtime's channel has at most one held lock, and a held lock stays the registered one.
OneHeldLockPerChannel ==
    \A m1, m2 \in Mutexes :
        held[m1] # "free" /\ held[m2] # "free" /\ home[m1] = home[m2] => m1 = m2
HeldLockStaysRegistered == \A m \in Mutexes : held[m] # "free" => reg[home[m]] = m
\* Two SharedData never wait on each other's channel lock.
HostsHoldSeparateLocks ==
    \A g \in raws, l \in leases : g.m = l.m => g.by = l.maker
\* `covers` (:115-120): an unplaced lease covers only rows of the bot whose channel lock it holds.
UnplacedCoversOnlyItsMaker ==
    \A u \in unplaced, s \in Hosts, c \in Channels :
        (TokenOf(s) = u.tok /\ c = u.ch) => (s = u.by /\ home[u.m] = <<EngineOf(s), c>>)
\* What `covers` promises when two SharedData carry one token hash.
\* The lease holds its maker's channel lock and a copy of its maker's token hash (:102-112).
UnplacedUsesMakerRegistryAndToken ==
    \A u \in unplaced :
        u.tok = TokenOf(u.by) /\ home[u.m] = <<EngineOf(u.by), u.ch>> /\ held[u.m] = "unplaced"
\* `covers` says exactly "the row names this token hash and this channel" (:115-120).
Covers(u, tok, c) == tok = u.tok /\ c = u.ch
UnplacedCoversExactlyItsOwnerTuple ==
    \A u \in unplaced, s \in Hosts, c \in Channels :
        Covers(u, TokenOf(s), c) <=> (TokenOf(s) = TokenOf(u.by) /\ c = u.ch)
\* What the same token costs: a lease may cover the row of a SharedData whose lock it does not hold.
UnplacedHoldsCoveredHostsLock ==
    \A u \in unplaced, s \in Hosts, c \in Channels :
        Covers(u, TokenOf(s), c) => home[u.m] = <<EngineOf(s), c>>
====
