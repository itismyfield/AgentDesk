---------------------------- MODULE ClaudeStop ----------------------------
\* One user stop on a Claude turn: what the stop read (turn identity, transcript tail, screen)
\* and whether it sent Escape. The turn itself only sets what those reads can return.
CONSTANT Policy
VARIABLES turn, tail, identity, screen, verdict, seenTail, seenIdentity, seenScreen
vars == <<turn, tail, identity, screen, verdict, seenTail, seenIdentity, seenScreen>>

Init == /\ turn = "none" /\ tail = "idle" /\ identity = "read" /\ screen = "idle"
        /\ verdict = "none" /\ seenTail = "idle" /\ seenIdentity = "read" /\ seenScreen = "idle"

Seen == UNCHANGED <<verdict, seenTail, seenIdentity, seenScreen>>

\* A prompt is submitted: its user record is the tail and the pane shows no spinner yet. The
\* prompt's own text may carry the chrome's words ("words": an idle screen that only reads busy).
Submit == /\ turn = "none" /\ turn' = "submitted" /\ tail' = "user_submitted"
          /\ screen' \in {"idle", "words"} /\ UNCHANGED identity /\ Seen
\* Claude starts working; the pane shows a spinner, with or without the interrupt footer.
Start == /\ turn = "submitted" /\ turn' = "running" /\ screen' \in {"spinner", "footer"}
         /\ UNCHANGED <<tail, identity>> /\ Seen
\* An assistant record lands.
Reply == /\ turn = "running" /\ tail' = "streaming" /\ UNCHANGED <<turn, identity, screen>> /\ Seen
\* A tool returns at once while its work still runs: the tail is a user record again.
ToolReturns == /\ turn = "running" /\ tail = "streaming" /\ tail' = "user_submitted"
               /\ UNCHANGED <<turn, identity, screen>> /\ Seen
\* Claude appends session metadata: the tail no longer reads as an open or an ended turn.
Metadata == /\ turn \in {"running", "done"} /\ tail' = "unknown"
            /\ UNCHANGED <<turn, identity, screen>> /\ Seen
\* The turn outgrows the tail read, so its user record can no longer be named.
Outgrow == /\ turn = "running" /\ identity' = "unread" /\ UNCHANGED <<turn, tail, screen>> /\ Seen
\* The turn ends: a terminal record is the tail and the spinner is gone. The reply left on the
\* screen may quote the chrome's words.
Finish == /\ turn = "running" /\ turn' = "done" /\ tail' = "idle" /\ screen' \in {"idle", "words"}
          /\ UNCHANGED identity /\ Seen
\* The terminal record lands a moment before the pane repaints.
FinishLate == /\ turn = "running" /\ turn' = "done" /\ tail' = "idle"
              /\ UNCHANGED <<identity, screen>> /\ Seen

\* The stop's own reading takes only chrome rows; the general busy reading also took the words.
Live == screen \in {"spinner", "footer"}
Offered == screen = "footer"
WideLive == screen # "idle"
WideOffered == screen \in {"footer", "words"}
Open == tail \in {"user_submitted", "streaming"}
Sends ==
    CASE Policy = "before" -> identity = "read" /\ tail = "streaming" /\ WideLive
      [] Policy = "first_repair" -> \/ identity = "read" /\ Open /\ WideLive
                                    \/ identity = "unread" /\ tail # "idle" /\ WideOffered
      [] Policy = "fixed" -> \/ identity = "read" /\ Live
                             \/ identity = "unread" /\ Offered
      [] Policy = "no_screen_first" -> \/ identity = "read" /\ tail # "user_submitted" /\ Live
                                       \/ identity = "unread" /\ Offered
      [] Policy = "open_tail_only" -> \/ identity = "read" /\ Open /\ Live
                                      \/ identity = "unread" /\ Offered
      [] Policy = "wide_reader" -> \/ identity = "read" /\ WideLive
                                   \/ identity = "unread" /\ WideOffered
      [] Policy = "no_unread_footer" -> identity = "read" /\ Live
      [] Policy = "unread_ended" -> \/ identity = "read" /\ Live
                                    \/ identity = "unread" /\ tail # "idle" /\ Offered
      [] Policy = "blind" -> \/ identity = "read" /\ tail = "user_submitted"
                             \/ identity = "read" /\ Live
                             \/ identity = "unread" /\ Offered
      [] Policy = "unread_spinner" -> \/ identity = "read" /\ Live
                                      \/ identity = "unread" /\ Live

Stop == /\ verdict = "none"
        /\ verdict' = IF Sends THEN "sent" ELSE "refused"
        /\ seenTail' = tail /\ seenIdentity' = identity /\ seenScreen' = screen
        /\ UNCHANGED <<turn, tail, identity, screen>>

Next == Submit \/ Start \/ Reply \/ ToolReturns \/ Metadata \/ Outgrow \/ Finish \/ FinishLate
        \/ Stop \/ UNCHANGED vars

Judged == verdict # "none"
SeenLive == seenScreen \in {"spinner", "footer"}
\* (1) A read turn whose screen is live gets the Escape, whatever its tail reads.
ActiveScreenStops == (Judged /\ seenIdentity = "read" /\ SeenLive) => verdict = "sent"
\* (2) No Escape reaches a pane that shows no live turn, whatever the transcript or its text says.
NoEscapeOnIdleScreen == verdict = "sent" => SeenLive
\* (3) An unread turn still stops on the screen's own interrupt footer.
UnreadFooterStops ==
    (Judged /\ seenIdentity = "unread" /\ seenScreen = "footer") => verdict = "sent"
\* (4) An unread turn needs the footer; a bare spinner is not enough without the identity fence.
UnreadNeedsFooter == (verdict = "sent" /\ seenIdentity = "unread") => seenScreen = "footer"
=============================================================================
