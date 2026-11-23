----------------------- MODULE TurnPresenceTyping -----------------------
EXTENDS Naturals
CONSTANTS GuardNative, DeferToToken, Entry, Tui
VARIABLES phase, supervised, native, token, sinceSend, sent, tick
vars == <<phase, supervised, native, token, sinceSend, sent, tick>>

\* "closed" is the published Idle boundary. Restore starts with a surviving row
\* after boot, which may be open or closed. Tui is the channel's supported TUI.
Init ==
    /\ phase = "before"
    /\ supervised \in (IF Tui THEN BOOLEAN ELSE {FALSE})
    /\ native = FALSE /\ token = FALSE /\ sinceSend = 0 /\ sent = {} /\ tick = FALSE

Attach ==
    /\ phase = "before"
    /\ phase' \in (IF Entry = "restore" THEN {"closed", "busy"} ELSE {"busy"})
    /\ native' = ~(GuardNative /\ supervised)
    /\ token' = TRUE /\ sent' = {}
    /\ UNCHANGED <<supervised, sinceSend, tick>>

Close ==
    /\ phase = "busy"
    /\ phase' = "closed" /\ sent' = {}
    /\ token' \in BOOLEAN
    /\ UNCHANGED <<supervised, native, sinceSend, tick>>

Register ==
    /\ Tui /\ phase # "before" /\ ~supervised
    /\ supervised' = TRUE /\ sent' = {} /\ sinceSend' = 0
    /\ UNCHANGED <<phase, native, token, tick>>

\* The TUI close and restart row do not deliver a matching native completion.
\* For non-TUI work a matching completion can terminate the native loop.
CompleteNative ==
    /\ ~Tui /\ native
    /\ native' = FALSE /\ phase' = "closed" /\ sent' = {}
    /\ UNCHANGED <<supervised, token, sinceSend, tick>>

\* A tick represents a successful, due refresh opportunity (6s for presence,
\* 8s for native). Idle clears the prior send event, as in the publication model.
Min2(n) == IF n > 2 THEN 2 ELSE n
Refresh ==
    /\ phase # "before"
    /\ tick' = ~tick
    /\ native' = (native /\ ~(GuardNative /\ supervised))
    /\ sent' = (IF supervised /\ phase = "busy" /\ ~(DeferToToken /\ token)
                   THEN {"supervisor"} ELSE {})
               \cup (IF native' THEN {"native"} ELSE {})
    /\ sinceSend' = IF sent' = {} THEN Min2(sinceSend + 1) ELSE 0
    /\ UNCHANGED <<phase, supervised, token>>

Next == Attach \/ Close \/ Register \/ CompleteNative \/ Refresh
Spec == Init /\ [][Next]_vars /\ WF_vars(Attach) /\ WF_vars(Refresh)
NoTypingWhileClosed == (supervised /\ phase = "closed") => sent = {}
OnlySupervisorSends == supervised => "native" \notin sent
BusyRefreshWithinDeadline == (supervised /\ phase = "busy") => sinceSend = 0
BusyTypingContinues == (<>[] (supervised /\ phase = "busy")) => []<> ("supervisor" \in sent)
FallbackRefreshWithinDeadline == (~supervised /\ phase = "busy") => sinceSend = 0
=============================================================================
