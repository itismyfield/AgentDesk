-------------------- MODULE NativeNoKeyCustody --------------------
\* An unkeyed native turn records its no-key witness under the history lock, drops the mailbox
\* control custody and unwinds its cancel token before the effect settles the input.
EXTENDS Naturals
CONSTANTS Mode, Keyed, Busy
VARIABLES pc, custody, token, notSent, anyKey, tries, lock, retained, next
vars == <<pc, custody, token, notSent, anyKey, tries, lock, retained, next>>
Patience == 2
Fixed == Mode = "Fixed"
\* A close passes admission with no custody, or with key evidence the leaf recorded.
Admit == ~custody \/ (anyKey /\ ~notSent)

Init == /\ pc = "Leaf" /\ custody = TRUE /\ token = TRUE /\ notSent = FALSE
        /\ anyKey = FALSE /\ tries = 0 /\ lock = (Busy # "None")
        /\ retained = FALSE /\ next = FALSE

Leaf == /\ pc = "Leaf" /\ anyKey' = Keyed /\ pc' = "Witness"
        /\ UNCHANGED <<custody, token, notSent, tries, lock, retained, next>>
\* Another registry writer's lock: a brief one ends by itself, a stuck one outlasts the patience.
Unlock == /\ lock /\ Busy = "Brief" /\ lock' = FALSE
          /\ UNCHANGED <<pc, custody, token, notSent, anyKey, tries, retained, next>>
WitnessKeyed == /\ pc = "Witness" /\ anyKey /\ pc' = "Settled"
                /\ UNCHANGED <<custody, token, notSent, anyKey, tries, lock, retained, next>>
WitnessNoKey == /\ pc = "Witness" /\ ~anyKey /\ ~lock
                /\ notSent' = TRUE /\ custody' = FALSE /\ pc' = "Unwind"
                /\ UNCHANGED <<token, anyKey, tries, lock, retained, next>>
WitnessRetry == /\ pc = "Witness" /\ ~anyKey /\ lock /\ Fixed /\ tries < Patience
                /\ tries' = tries + 1
                /\ UNCHANGED <<pc, custody, token, notSent, anyKey, lock, retained, next>>
\* Before: the failed witness skips the unwind and keeps custody. Fixed: custody goes, unwind runs.
WitnessBusy == /\ pc = "Witness" /\ ~anyKey /\ lock /\ (~Fixed \/ tries = Patience)
               /\ custody' = (custody /\ ~Fixed)
               /\ pc' = IF Fixed THEN "Unwind" ELSE "Settled"
               /\ UNCHANGED <<token, notSent, anyKey, tries, lock, retained, next>>
Unwind == /\ pc = "Unwind" /\ token' = (token /\ ~Admit) /\ pc' = "Settled"
          /\ UNCHANGED <<custody, notSent, anyKey, tries, lock, retained, next>>
\* The effect holds an input whose no-key proof landed and retains any other unkeyed one.
Settle == /\ pc = "Settled" /\ ~anyKey /\ ~notSent /\ ~retained /\ retained' = TRUE
          /\ UNCHANGED <<pc, custody, token, notSent, anyKey, tries, lock, next>>
\* FinishTurn, clear_channel, checked purge and the operator release pass the same admission.
Close == /\ pc = "Settled" /\ token /\ Admit /\ token' = FALSE
         /\ UNCHANGED <<pc, custody, notSent, anyKey, tries, lock, retained, next>>
StartNext == /\ pc = "Settled" /\ ~token /\ ~next /\ next' = TRUE
             /\ UNCHANGED <<pc, custody, token, notSent, anyKey, tries, lock, retained>>

Next == Leaf \/ Unlock \/ WitnessKeyed \/ WitnessNoKey \/ WitnessRetry \/ WitnessBusy
        \/ Unwind \/ Settle \/ Close \/ StartNext
Spec == Init /\ [][Next]_vars /\ WF_vars(Leaf) /\ WF_vars(Unlock) /\ WF_vars(WitnessKeyed)
        /\ WF_vars(WitnessNoKey) /\ WF_vars(WitnessRetry) /\ WF_vars(WitnessBusy)
        /\ WF_vars(Unwind) /\ WF_vars(Settle) /\ WF_vars(Close) /\ WF_vars(StartNext)

\* Safety: a retained unkeyed input has returned its control custody and cancel token.
RetainedReclaimed == retained => (~custody /\ ~token)
\* Regression guard: a keyed turn keeps custody while its token lives.
KeyedCustodyKept == (anyKey /\ token) => custody
\* Liveness: the channel's next input eventually starts.
NextInputStarts == <>next
====================================================================
