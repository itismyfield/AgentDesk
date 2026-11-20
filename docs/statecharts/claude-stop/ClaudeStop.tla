---------------------------- MODULE ClaudeStop ----------------------------
\* One user stop on a Claude turn: what the stop read (turn identity, transcript tail, screen)
\* and whether it sent Escape. The turn itself only sets what those reads can return.
CONSTANT Policy
VARIABLES turn, tail, identity, screen, verdict, seenTail, seenIdentity, seenScreen
vars == <<turn, tail, identity, screen, verdict, seenTail, seenIdentity, seenScreen>>

Init == /\ turn = "none" /\ tail = "idle" /\ identity = "read" /\ screen = "idle"
        /\ verdict = "none" /\ seenTail = "idle" /\ seenIdentity = "read" /\ seenScreen = "idle"

Seen == UNCHANGED <<verdict, seenTail, seenIdentity, seenScreen>>

\* A prompt is submitted: its user record is the tail and the pane shows no spinner yet.
Submit == /\ turn = "none" /\ turn' = "submitted" /\ tail' = "user_submitted"
          /\ UNCHANGED <<identity, screen>> /\ Seen
\* Claude starts working; the pane shows a spinner, with or without the interrupt footer.
Start == /\ turn = "submitted" /\ turn' = "running" /\ screen' \in {"spinner", "footer"}
         /\ UNCHANGED <<tail, identity>> /\ Seen
\* An assistant record lands.
Reply == /\ turn = "running" /\ tail' = "streaming" /\ UNCHANGED <<turn, identity, screen>> /\ Seen
\* A tool returns at once while its work still runs: the tail is a user record again.
ToolReturns == /\ turn = "running" /\ tail = "streaming" /\ tail' = "user_submitted"
               /\ UNCHANGED <<turn, identity, screen>> /\ Seen
\* The turn outgrows the tail read, so its user record can no longer be named.
Outgrow == /\ turn = "running" /\ identity' = "unread" /\ UNCHANGED <<turn, tail, screen>> /\ Seen
\* The turn ends: a terminal record is the tail and the spinner is gone.
Finish == /\ turn = "running" /\ turn' = "done" /\ tail' = "idle" /\ screen' = "idle"
          /\ UNCHANGED identity /\ Seen
\* The terminal record lands a moment before the pane repaints.
FinishLate == /\ turn = "running" /\ turn' = "done" /\ tail' = "idle"
              /\ UNCHANGED <<identity, screen>> /\ Seen

Active == screen # "idle"
Open == tail \in {"user_submitted", "streaming"}
Sends ==
    CASE Policy = "before" -> identity = "read" /\ tail = "streaming" /\ Active
      [] Policy = "fixed" -> \/ identity = "read" /\ Open /\ Active
                             \/ identity = "unread" /\ tail # "idle" /\ screen = "footer"
      [] Policy = "no_screen_first" -> \/ identity = "read" /\ tail = "streaming" /\ Active
                                       \/ identity = "unread" /\ tail # "idle" /\ screen = "footer"
      [] Policy = "no_unread_footer" -> identity = "read" /\ Open /\ Active
      [] Policy = "blind" -> \/ identity = "read" /\ tail = "user_submitted"
                             \/ identity = "read" /\ Open /\ Active
                             \/ identity = "unread" /\ tail # "idle" /\ screen = "footer"
      [] Policy = "unread_spinner" -> \/ identity = "read" /\ Open /\ Active
                                      \/ identity = "unread" /\ tail # "idle" /\ Active

Stop == /\ verdict = "none"
        /\ verdict' = IF Sends THEN "sent" ELSE "refused"
        /\ seenTail' = tail /\ seenIdentity' = identity /\ seenScreen' = screen
        /\ UNCHANGED <<turn, tail, identity, screen>>

Next == Submit \/ Start \/ Reply \/ ToolReturns \/ Outgrow \/ Finish \/ FinishLate \/ Stop
        \/ UNCHANGED vars

Judged == verdict # "none"
\* (1) A read, open turn whose screen is active gets the Escape, submitted or streaming.
ActiveScreenStops ==
    (Judged /\ seenIdentity = "read" /\ seenTail # "idle" /\ seenScreen # "idle") => verdict = "sent"
\* (2) No Escape reaches a pane that shows no live turn, whatever the transcript says.
NoEscapeOnIdleScreen == verdict = "sent" => seenScreen # "idle"
\* (3) An unread turn still stops on the screen's own interrupt footer.
UnreadFooterStops ==
    (Judged /\ seenIdentity = "unread" /\ seenTail # "idle" /\ seenScreen = "footer") => verdict = "sent"
\* An unread turn needs the footer; a bare spinner is not enough without the identity fence.
UnreadNeedsFooter == (verdict = "sent" /\ seenIdentity = "unread") => seenScreen = "footer"
\* A transcript that shows the turn ended never gets an Escape, even under a stale spinner.
NoEscapeAfterEnd == verdict = "sent" => seenTail # "idle"
=============================================================================
