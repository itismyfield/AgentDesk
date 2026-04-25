# Auto-Queue Lifecycle

```plantuml
@startuml auto-queue-lifecycle
!theme plain
skinparam ActivityDiamondFontSize 11
skinparam ActivityFontSize 11
skinparam NoteFontSize 10
skinparam PartitionFontSize 12
skinparam PartitionFontStyle bold

title Auto-Queue Lifecycle — AgentDesk

|#LightBlue|Rust (API / DB)|
|#LightGreen|JS (Policy Engine)|
|#LightYellow|GitHub / Git|

' ============================================================
' 1. QUEUE CREATION
' ============================================================

|Rust (API / DB)|
start
:POST /api/auto-queue/dispatch
(or /generate + /activate);

:Normalize dispatch groups
  - Validate issue numbers
  - Resolve kanban cards
  - Apply agent assignments
  - Validate dispatchable states;

:Smart Planner: build_group_plan()
  - Extract file paths from descriptions
  - Detect dependency edges (#N refs)
  - Compute path similarity (Jaccard/Overlap)
  - Union-Find grouping
  - Topological sort within groups;

:Create **auto_queue_runs** row
  status = 'generated'
  max_concurrent_threads
  thread_group_count;

:Create **auto_queue_entries** rows
  status = 'pending'
  thread_group, batch_phase, priority_rank;

note right
  Entry states:
  **pending** -> dispatched -> done
  **pending** -> dispatched -> done
  **pending** -> skipped (external / invalid)
  dispatched -> pending (tick recovery)
end note

' ============================================================
' 2. ACTIVATION (Run promotion + first dispatches)
' ============================================================

:POST /api/auto-queue/dispatch-next
(or auto-activate from /dispatch);

if (Run status?) then (generated/pending)
  :Promote run -> **active**;
elseif (paused) then
  if (Blocking phase gate?) then (yes)
    :Return "waiting on phase gate";
    stop
  else (no)
    :Return "run is paused";
    stop
  endif
else (active)
endif

:Clear inactive slot assignments
Release completed group slots;

:Determine current_batch_phase
(min pending phase in run);

:Identify dispatchable groups
  1. Preferred thread_group (if specified)
  2. Already-assigned slot groups with pending entries
  3. Active groups with pending + no dispatched entries
  4. New pending groups up to max_concurrent;

' ============================================================
' 3. SLOT ALLOCATION + DISPATCH CREATION (per group)
' ============================================================

partition "For each dispatchable group" {

  :Get first pending entry
  (lowest priority_rank in current phase);

  if (Agent busy outside AQ?) then (yes)
    :Skip — defer to next tick;
    detach
  else (no)
  endif

  if (Card in non-dispatchable state?\ne.g. backlog) then (yes)
    :Silent walk: free transitions
    backlog -> ready -> requested
    (hooks fire at each step);
  else (no)
  endif

  if (Preflight metadata?) then (consult_required)
    :Create **consultation** dispatch
    to counterpart provider;
    :Entry -> **dispatched**;
    detach
  elseif (invalid / already_applied) then
    :Entry -> **skipped**;
    detach
  else (normal)
  endif

  if (Card already terminal?) then (yes)
    :Entry -> **skipped**;
    detach
  else (no)
  endif

  if (Card has active dispatch?) then (yes)
    :Attach existing dispatch_id
    Entry -> **dispatched**;
    detach
  else (no)
  endif

  :allocate_slot_for_group_agent()
  auto_queue_slots table;

  if (Free slot available?) then (yes)
    :Assign slot_index to entry;
  else (no)
    :Skip group — pool exhausted;
    detach
  endif

  :Reserve entry -> **dispatched**
  (optimistic lock with status = 'pending');

  :create_dispatch()
  type = "implementation"
  context = {auto_queue, entry_id,
             thread_group, slot_index};
}

' ============================================================
' 4. AGENT PROCESSES (implementation work)
' ============================================================

:Agent receives dispatch via Discord
Executes in worktree (isolated branch);

:Agent completes work
Card transitions: requested -> in_progress -> review;

' ============================================================
' 5. REVIEW CYCLE
' ============================================================

|JS (Policy Engine)|
:onReviewEnter fires
(review-automation.js);

if (Review enabled?) then (no)
  :Card -> terminal (done);
  :Entry -> **done**;
else (yes)
  if (Counter-model channel exists?) then (no)
    :Auto-approve -> terminal;
    :Entry -> **done**;
  else (yes)
    :Create **review** dispatch
    to counter-model channel;

    :Agent reviews code
    Produces verdict;

    :onDispatchCompleted fires
    or onReviewVerdict (API);

    if (Verdict?) then (pass / approved)
      if (Next pipeline stage?) then (yes)
        :Dispatch next stage
        (dev-deploy / e2e-test / etc.);
      else (no)
        if (Has tracked worktree branch?) then (yes)
          :Create **create-pr** dispatch
          Seed pr_tracking;
        else (no)
          :Card -> terminal (done);
        endif
      endif
    elseif (improve / reject / rework) then

      if (Repeated findings detected?\n(Jaccard similarity >= 0.5)) then (yes)
        if (Session reset already tried?) then (yes)
          :Escalate -> pending_decision
          Notify PMD;
          detach
        elseif (Approach change already tried?) then
          :Create **rework** dispatch
          force_new_session = true
          (session reset);
        else (first repeat)
          :Create **rework** dispatch
          "[Approach Change]" prompt;
        endif
      else (no)
        :review_status = suggestion_pending
        or create **rework** dispatch
        Card -> in_progress (rework target);
      endif

      :Card returns to in_progress
      Re-enters review cycle on completion;
      detach
    else (no verdict)
      :Create **review-decision** dispatch
      for original agent to inspect;
      detach
    endif
  endif
endif

' ============================================================
' 6. AUTO-MERGE
' ============================================================

|JS (Policy Engine)|
:onCardTerminal fires
(merge-automation.js);

if (merge_automation_enabled?) then (yes)

  |JS (Policy Engine)|
  :Load PR tracking for card
  resolveTerminalMergeCandidate();

  if (Tracked PR exists in 'merge' state?) then (yes)
    :Check allowed author;
    if (Allowed?) then (yes)
      |GitHub / Git|
      :enableAutoMerge()
      gh pr merge --auto;
    else (no)
      :Skip auto-merge;
    endif
  else (no)
    |JS (Policy Engine)|
    :tryDirectMergeOrTrackPr();

    |GitHub / Git|
    :attemptDirectMerge()
    cherry-pick commits onto main;

    if (Cherry-pick succeeds?) then (yes)
      :git push origin main
      Track as "closed";
    else (conflict)
      :cherry-pick --abort
      createOrLocateConflictPr();

      :Track PR state = "wait-ci"
      blocked_reason = "ci:waiting";

      note right
        OnTick5min later:
        - processTrackedMergeQueue()
        - detectConflictingPrs()
           -> dispatch rebase
        - cleanupMergedWorktrees()
      end note
    endif
  endif
else (no)
endif

' ============================================================
' 7. CONTINUATION (onCardTerminal — auto-queue.js)
' ============================================================

|JS (Policy Engine)|
:onCardTerminal fires
(auto-queue.js);

:Find done entry for card
(prefer 'done' over 'skipped',
 prefer originating run);

if (Done entry found in active run?) then (no)
  detach
else (yes)
endif

:continueRunAfterEntry()
(runId, agentId, doneGroup, donePhase);

if (Phase > 0 and current phase done?) then (yes)
  if (Phase gate required?\n(multi-phase run)) then (yes)

    :_createPhaseGateDispatches()
    - Build gate groups per agent
    - Create phase-gate dispatches
    - Pause run
    - Save gate state to kv_meta;

    :Agent evaluates phase gate
    Returns verdict;

    :onDispatchCompleted fires;

    if (All gates pass?) then (yes)
      if (Final phase?) then (yes)
        :completeRunAndNotify();
      else (no)
        :clearPhaseGateState()
        Resume run -> active
        activateRun() for next phase;
      endif
    else (fail)
      :pauseRun()
      Notify PMD
      Save failed state;
      detach
    endif
    detach
  else (no gate needed)
    :activateRun() for next phase;
    detach
  endif
else (no / phase 0)
endif

if (Remaining entries == 0?) then (yes)
  :finalizeRunWithoutPhaseGate()
  or completeRunAndNotify();

  |Rust (API / DB)|
  :Run -> **completed**
  Release all slots
  Notify channels;
  stop
else (no)
endif

if (Group still has entries?) then (yes)
  if (Agent busy?) then (no)
    :activateRun(runId, doneGroup)
    -> dispatch next in same group;
  else (yes)
    :Defer — tick will recover;
  endif
else (group done)
  :activateRun(runId, null)
  -> start next available group;
endif

detach

' ============================================================
' 8. ERROR RECOVERY (onTick1min — auto-queue.js)
' ============================================================

|JS (Policy Engine)|
partition "onTick1min — Error Recovery" {

  :Recovery path 1: Terminal pending cleanup
  Find pending entries whose card
  is already terminal -> **skip**;

  :Stale run detection
  Active/paused runs with 0
  pending+dispatched entries
  -> finalizeRunWithoutPhaseGate();

  :Recovery path 2: Idle agent dispatch
  Find active runs with pending entries
  -> activateRun() for each;

  :Recovery path 3: Stuck dispatched entries
  (dispatched > 2 min ago AND
   dispatch_id is NULL / cancelled / failed / phantom)
  -> Reset entry to **pending**
     (tick_recovery);
}

|JS (Policy Engine)|
:onCardTransition — Auto-skip
If external progress moves card
to kickoff/next state while pending
-> Entry -> **skipped**
   (external_progress);

detach

@enduml
```
