----------------------- MODULE WriterScheduledNotice -----------------------
\* One scheduled prompt of one Claude channel across the writer handoff (#8142 S3).
\* Who posts its notice: the hook path (live owner before the handoff) or the writer's
\* projection of the native row (owner after it).
EXTENDS Naturals
CONSTANTS Mut,    \* "none" or the guard a mutant drops
          Queued  \* TRUE: the hook may decide a whole queued turn before the row is written
VARIABLES pos,      \* where the row lies against the frozen cohort: floor, cohort, live
          handed,   \* the writer's frozen cohort is durable (writer_posts)
          hook,     \* wait -> fired -> done
          hookPost, \* what the hook path posted: none, clock (scheduled header), kbd (typed header)
          row,      \* the native user row is written
          evid,     \* durable Materialized evidence of the row
          verdict,  \* durable settlement: none, posted, debt, floored
          sent,     \* the writer's in-memory record of its own post
          wPosts,   \* notices the writer posted
          boots     \* writer restarts so far
vars == <<pos, handed, hook, hookPost, row, evid, verdict, sent, wPosts, boots>>

Init ==
    /\ pos \in {"floor", "cohort", "live"}
    /\ handed = FALSE /\ hook = "wait" /\ hookPost = "none" /\ row = FALSE
    /\ evid = FALSE /\ verdict = "none" /\ sent = FALSE /\ wPosts = 0 /\ boots = 0

HookFire ==
    /\ hook = "wait" /\ hook' = "fired"
    /\ UNCHANGED <<pos, handed, hookPost, row, evid, verdict, sent, wPosts, boots>>

\* The row lands right after the hook; a live row is one written past the frozen upper.
WriteRow ==
    /\ hook # "wait" /\ ~row /\ (pos = "live" <=> handed)
    /\ row' = TRUE
    /\ UNCHANGED <<pos, handed, hook, hookPost, evid, verdict, sent, wPosts, boots>>

\* synthetic_start_wiring::post_passive_prompt_notice: silent once the writer posts, otherwise
\* the hook posts under either header (S2 falls back to the typed one) or its POST fails.
HookDecide ==
    /\ hook = "fired" /\ (Queued \/ row)
    /\ hook' = "done"
    /\ IF handed /\ Mut # "no_hook_suppress"
          THEN hookPost' = hookPost
          ELSE hookPost' \in {"clock", "kbd", "none"}
    /\ UNCHANGED <<pos, handed, row, evid, verdict, sent, wPosts, boots>>

\* notice_work::start_notice_migration: floor rows are below the writer's cursor and their
\* hook finished long ago; cohort rows are written; live rows are not.
Handoff ==
    /\ ~handed
    /\ (pos \in {"floor", "cohort"} => row)
    /\ (pos = "floor" => hook = "done")
    /\ handed' = TRUE
    /\ UNCHANGED <<pos, hook, hookPost, row, evid, verdict, sent, wPosts, boots>>

\* Projection::project_record + persist_notice_evidence. Before S3 an isMeta row is not an input.
WriterRead ==
    /\ handed /\ row /\ ~evid /\ verdict = "none" /\ Mut # "before"
    /\ IF pos = "floor" /\ Mut # "no_floor"
          THEN verdict' = "floored" /\ evid' = evid
          ELSE evid' = TRUE /\ verdict' = verdict
    /\ UNCHANGED <<pos, handed, hook, hookPost, row, sent, wPosts, boots>>

\* migration::plan for a cohort row: a legacy message under either header is debt, a proven
\* absence posts. A live row posts. The ledger's Posted outcome seals the post across restarts.
LegacySeen == hookPost = "clock" \/ (hookPost = "kbd" /\ Mut # "payload_only")
Settle ==
    /\ evid /\ verdict = "none" /\ ~sent
    /\ IF pos = "cohort" /\ LegacySeen
          THEN verdict' = "debt" /\ UNCHANGED <<sent, wPosts>>
          ELSE /\ wPosts' = wPosts + 1 /\ sent' = TRUE
               /\ verdict' = IF Mut = "no_seal" THEN "none" ELSE "posted"
    /\ UNCHANGED <<pos, handed, hook, hookPost, row, evid, boots>>

\* A restart keeps the ledger and loses the writer's memory; evidence is re-projected.
Restart ==
    /\ boots < 1 /\ boots' = boots + 1 /\ sent' = FALSE
    /\ UNCHANGED <<pos, handed, hook, hookPost, row, evid, verdict, wPosts>>

Next == HookFire \/ WriteRow \/ HookDecide \/ Handoff \/ WriterRead \/ Settle \/ Restart
        \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars /\ WF_vars(HookFire) /\ WF_vars(WriteRow) /\ WF_vars(HookDecide)
        /\ WF_vars(Handoff) /\ WF_vars(WriterRead) /\ WF_vars(Settle)

Posts == wPosts + (IF hookPost = "none" THEN 0 ELSE 1)
\* A hook POST that failed before the handoff on a floor row stays lost: no retroactive post.
PostedAtMostOnce == Posts <= 1
NoRetroactivePost == pos = "floor" => wPosts = 0
\* Every prompt the writer is responsible for ends with exactly one notice.
EventuallyPostedOnce == <>[](pos = "floor" \/ Posts = 1)
=============================================================================
