-------------------- MODULE NativeNoKeyCustody --------------------
\* An unkeyed native turn records its no-key witness under the history lock, drops the mailbox
\* control custody and unwinds its cancel token before the effect settles the input. The queue
\* owner's exact no-key proof (retained, or refused past its readiness due) survives a restart;
\* boot settles the record from it, and owner recovery ACKs the cold source pin.
EXTENDS Naturals
CONSTANTS Mode, Keyed, AnyKeyLost, Busy, DuePassed
VARIABLES pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof,
          crashed, bootReady, next, pin, told
vars == <<pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof,
          crashed, bootReady, next, pin, told>>
Patience == 2
\* Before: no reclaim. Reclaim: custody and token return, no-key debt dropped. RetainedOnly:
\* the debt lands in process or at boot from a retained proof only, and boot adopts only channels
\* holding input. Fixed: a refused proof counts too, and a channel owing a pin is adopted.
\* DropAnyKey: boot settles any record lacking key evidence (the rejected shortcut).
Reclaims == Mode \in {"Reclaim", "RetainedOnly", "Fixed", "DropAnyKey"}
Defers == Mode \in {"RetainedOnly", "Fixed"}
Admit == ~custody \/ (anyKey /\ ~notSent)
BootProof == proof /\ (Mode = "Fixed" \/ (Mode = "RetainedOnly" /\ ~DuePassed))
\* Boot adopts a channel whose owner still owes an input, or under Fixed a source pin.
Census == ~told \/ (Mode = "Fixed" /\ pin)

Init == /\ pc = "Leaf" /\ custody = TRUE /\ token = TRUE /\ notSent = FALSE
        /\ anyKey = FALSE /\ keySent = FALSE /\ tries = 0 /\ lock = (Busy # "None")
        /\ retained = FALSE /\ proof = FALSE /\ crashed = FALSE /\ bootReady = FALSE
        /\ next = FALSE /\ pin = ~Keyed /\ told = FALSE

\* A key that went is recorded unless its any-key write was lost; its pin is not this model's.
Leaf == /\ pc = "Leaf" /\ keySent' = Keyed /\ anyKey' = (Keyed /\ ~AnyKeyLost) /\ pc' = "Witness"
        /\ UNCHANGED <<custody, token, notSent, tries, lock, retained, proof, crashed, bootReady, next, pin, told>>
\* A brief lock ends at once; a late one only after the retries ran out; a stuck one never.
Unlock == /\ lock /\ ~crashed
          /\ (Busy = "Brief" \/ (Busy = "Late" /\ pc # "Witness"))
          /\ lock' = FALSE
          /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, retained, proof, crashed, bootReady, next, pin, told>>
WitnessKeyed == /\ pc = "Witness" /\ keySent /\ pc' = "Settled"
                /\ UNCHANGED <<custody, token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, pin, told>>
\* The record lands, and the leaf ACKs its source pin.
WitnessNoKey == /\ pc = "Witness" /\ ~keySent /\ ~lock
                /\ notSent' = TRUE /\ custody' = FALSE /\ pin' = FALSE /\ pc' = "Unwind"
                /\ UNCHANGED <<token, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, told>>
WitnessRetry == /\ pc = "Witness" /\ ~keySent /\ lock /\ Reclaims /\ tries < Patience
                /\ tries' = tries + 1
                /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, lock, retained, proof, crashed, bootReady, next, pin, told>>
WitnessBusy == /\ pc = "Witness" /\ ~keySent /\ lock /\ (~Reclaims \/ tries = Patience)
               /\ custody' = (custody /\ ~Reclaims)
               /\ pc' = IF Reclaims THEN "Unwind" ELSE "Settled"
               /\ UNCHANGED <<token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, pin, told>>
Unwind == /\ pc = "Unwind" /\ token' = (token /\ ~Admit) /\ pc' = "Settled"
          /\ UNCHANGED <<custody, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, pin, told>>
\* The effect settles an unkeyed input whose record failed: retained, or refused when the leaf
\* outlived its original's readiness due. Either way the owner journals the exact attempt.
Settle == /\ pc = "Settled" /\ ~keySent /\ ~notSent /\ ~retained /\ ~crashed
          /\ retained' = TRUE /\ proof' = TRUE
          /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, lock, crashed, bootReady, next, pin, told>>
\* The drive's notice drain tells the person; the owner no longer owes the input.
Tell == /\ retained /\ ~told /\ ~crashed /\ told' = TRUE
        /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, pin>>
\* The owed record and the pin ACK land on a blocking thread once the lock frees.
Deferred == /\ Defers /\ ~crashed /\ ~lock /\ tries = Patience /\ pc # "Witness"
            /\ ~keySent /\ ~notSent /\ notSent' = TRUE /\ pin' = FALSE
            /\ UNCHANGED <<pc, custody, token, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, told>>
Close == /\ pc = "Settled" /\ token /\ Admit /\ ~crashed /\ token' = FALSE
         /\ UNCHANGED <<pc, custody, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, pin, told>>
\* The process dies after the effect settled; memory custody, token and the lock go, the files
\* stay, then boot runs. A death between the unwind and the owner's journal is not modelled.
Restart == /\ pc = "Settled" /\ ~crashed /\ (keySent \/ notSent \/ retained)
           /\ LET settles == ~notSent /\ ~anyKey /\ (BootProof \/ Mode = "DropAnyKey")
              IN /\ notSent' = (notSent \/ settles)
                 /\ bootReady' = (notSent \/ settles \/ anyKey)
           /\ crashed' = TRUE /\ custody' = FALSE /\ token' = FALSE /\ lock' = FALSE
           /\ UNCHANGED <<pc, anyKey, keySent, tries, retained, proof, next, pin, told>>
\* Owner recovery of an adopted channel ACKs a pin whose history proves no key.
Recover == /\ crashed /\ pin /\ notSent /\ Census /\ pin' = FALSE
           /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, next, told>>
\* Decision, HTTP and routine starts wait on boot readiness after a restart.
StartNext == /\ pc = "Settled" /\ ~token /\ ~next /\ (crashed => bootReady) /\ next' = TRUE
             /\ UNCHANGED <<pc, custody, token, notSent, anyKey, keySent, tries, lock, retained, proof, crashed, bootReady, pin, told>>

Next == Leaf \/ Unlock \/ WitnessKeyed \/ WitnessNoKey \/ WitnessRetry \/ WitnessBusy
        \/ Unwind \/ Settle \/ Tell \/ Deferred \/ Close \/ Restart \/ Recover \/ StartNext
\* A restart may or may not happen, and a notice may or may not go first: neither is fair.
Spec == Init /\ [][Next]_vars /\ WF_vars(Leaf) /\ WF_vars(Unlock) /\ WF_vars(WitnessKeyed)
        /\ WF_vars(WitnessNoKey) /\ WF_vars(WitnessRetry) /\ WF_vars(WitnessBusy)
        /\ WF_vars(Unwind) /\ WF_vars(Settle) /\ WF_vars(Deferred) /\ WF_vars(Close)
        /\ WF_vars(Recover) /\ WF_vars(StartNext)

\* Safety: a settled unkeyed input has returned its control custody and cancel token.
RetainedReclaimed == retained => (~custody /\ ~token)
\* Regression guard: a keyed turn keeps custody while its token lives.
KeyedCustodyKept == (keySent /\ token) => custody
\* Safety: a record whose key went never settles as no-key, even with its any-key write lost.
NoKeyOnlyUnkeyed == notSent => ~keySent
\* Liveness: the channel's next input eventually starts, across a restart too.
NextInputStarts == <>next
\* Liveness: once the lock frees or the process restarts, an unkeyed cold source pin is ACKed.
SourcePinReleased == <>(crashed \/ ~lock) => <>(~pin)
====================================================================
