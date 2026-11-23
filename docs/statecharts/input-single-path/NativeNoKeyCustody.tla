-------------------- MODULE NativeNoKeyCustody --------------------
\* An unkeyed native turn records its no-key witness under the history lock, drops the mailbox
\* control custody and unwinds its cancel token before the effect settles the input. The queue
\* owner's retained KnownNotSent proof survives a restart; boot settles the record from it.
EXTENDS Naturals
CONSTANTS Mode, Keyed, AnyKeyLost, Busy
VARIABLES pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof,
          crashed, bootReady, next
vars == <<pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof,
          crashed, bootReady, next>>
Patience == 2
\* Before: no reclaim. Reclaim: custody and token return, no-key debt dropped. Fixed: the debt
\* lands in process once the lock frees, or at boot from the owner's proof. DropAnyKey: boot
\* settles any record lacking key evidence (the rejected shortcut).
Reclaims == Mode \in {"Reclaim", "Fixed", "DropAnyKey"}
Admit == ~custody \/ (anyKey /\ ~notSent)

Init == /\ pc = "Leaf" /\ custody = TRUE /\ token = TRUE /\ notSent = FALSE
        /\ anyKey = FALSE /\ keySent = FALSE /\ tries = 0 /\ lock = (Busy # "None")
        /\ retained = FALSE /\ proof = FALSE /\ crashed = FALSE /\ bootReady = FALSE
        /\ next = FALSE

\* A key that went is recorded unless its any-key write was lost.
Leaf == /\ pc = "Leaf" /\ keySent' = Keyed /\ anyKey' = (Keyed /\ ~AnyKeyLost) /\ pc' = "Witness"
        /\ UNCHANGED <<custody, token, notSent, tries, lock, retained, proof, crashed, bootReady, next>>
\* A brief lock ends at once; a late one only after the retries ran out; a stuck one never.
Unlock == /\ lock /\ ~crashed
          /\ (Busy = "Brief" \/ (Busy = "Late" /\ pc # "Witness"))
          /\ lock' = FALSE
          /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, retained, proof, crashed, bootReady, next>>
WitnessKeyed == /\ pc = "Witness" /\ keySent /\ pc' = "Settled"
                /\ UNCHANGED <<custody, token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next>>
WitnessNoKey == /\ pc = "Witness" /\ ~keySent /\ ~lock
                /\ notSent' = TRUE /\ custody' = FALSE /\ pc' = "Unwind"
                /\ UNCHANGED <<token, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next>>
WitnessRetry == /\ pc = "Witness" /\ ~keySent /\ lock /\ Reclaims /\ tries < Patience
                /\ tries' = tries + 1
                /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, lock, retained, proof, crashed, bootReady, next>>
WitnessBusy == /\ pc = "Witness" /\ ~keySent /\ lock /\ (~Reclaims \/ tries = Patience)
               /\ custody' = (custody /\ ~Reclaims)
               /\ pc' = IF Reclaims THEN "Unwind" ELSE "Settled"
               /\ UNCHANGED <<token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next>>
Unwind == /\ pc = "Unwind" /\ token' = (token /\ ~Admit) /\ pc' = "Settled"
          /\ UNCHANGED <<custody, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next>>
\* The effect retains an unkeyed input whose record failed; the owner journals its exact proof.
Settle == /\ pc = "Settled" /\ ~keySent /\ ~notSent /\ ~retained /\ ~crashed
          /\ retained' = TRUE /\ proof' = TRUE
          /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, lock, crashed, bootReady, next>>
\* Fixed: the owed record lands on a blocking thread once the lock frees.
Deferred == /\ Mode = "Fixed" /\ ~crashed /\ ~lock /\ tries = Patience /\ pc # "Witness"
            /\ ~keySent /\ ~notSent /\ notSent' = TRUE
            /\ UNCHANGED <<pc, custody, token, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next>>
Close == /\ pc = "Settled" /\ token /\ Admit /\ ~crashed /\ token' = FALSE
         /\ UNCHANGED <<pc, custody, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next>>
\* The process dies after the effect settled; memory custody, token and the lock go, the files
\* stay, then boot runs. A death between the unwind and the owner's retain is not modelled.
Restart == /\ pc = "Settled" /\ ~crashed /\ (keySent \/ notSent \/ retained)
           /\ LET settles == ~notSent /\ ~anyKey
                    /\ \/ Mode = "Fixed" /\ proof
                       \/ Mode = "DropAnyKey"
              IN /\ notSent' = (notSent \/ settles)
                 /\ bootReady' = (notSent \/ settles \/ anyKey)
           /\ crashed' = TRUE /\ custody' = FALSE /\ token' = FALSE /\ lock' = FALSE
           /\ UNCHANGED <<pc, anyKey, keySent, tries, retained, proof, next>>
\* Decision, HTTP and routine starts wait on boot readiness after a restart.
StartNext == /\ pc = "Settled" /\ ~token /\ ~next /\ (crashed => bootReady) /\ next' = TRUE
             /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady>>

Next == Leaf \/ Unlock \/ WitnessKeyed \/ WitnessNoKey \/ WitnessRetry \/ WitnessBusy
        \/ Unwind \/ Settle \/ Deferred \/ Close \/ Restart \/ StartNext
\* A restart may or may not happen, so it carries no fairness.
Spec == Init /\ [][Next]_vars /\ WF_vars(Leaf) /\ WF_vars(Unlock) /\ WF_vars(WitnessKeyed)
        /\ WF_vars(WitnessNoKey) /\ WF_vars(WitnessRetry) /\ WF_vars(WitnessBusy)
        /\ WF_vars(Unwind) /\ WF_vars(Settle) /\ WF_vars(Deferred) /\ WF_vars(Close)
        /\ WF_vars(StartNext)

\* Safety: a retained unkeyed input has returned its control custody and cancel token.
RetainedReclaimed == retained => (~custody /\ ~token)
\* Regression guard: a keyed turn keeps custody while its token lives.
KeyedCustodyKept == (keySent /\ token) => custody
\* Safety: a record whose key went never settles as no-key, even with its any-key write lost.
NoKeyOnlyUnkeyed == notSent => ~keySent
\* Liveness: the channel's next input eventually starts, across a restart too.
NextInputStarts == <>next
====================================================================
