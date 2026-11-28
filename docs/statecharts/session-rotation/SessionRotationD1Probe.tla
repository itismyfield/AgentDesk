----------------------- MODULE SessionRotationD1Probe -----------------------
\* Design-1 probes: reachability of the held states and the no-permanent-stall properties.
EXTENDS SessionRotationD1

\* Reachability witnesses: each predicate is EXPECTED to be violated. A violation is the
\* trace proving the named state is reachable, so the safety rules are not vacuous there.
W_Done == s.phase # "Done"
W_NewOrdinaryAccepted == \A i \in Ordinary: "New" \notin s.input[i].delivered
W_NextGeneration == s.nextRequestPlans = 0
W_SeedPosted == ~(s.out.state = "Posted" /\ s.input[Seed].effects = 1)
\* Post-barrier abandonment with a person's input waiting (RecoveryHeld).
W_HeldWithInput == ~(s.phase = "Abandoned" /\ s.barrier
                     /\ \E i \in Ordinary: s.input[i].state = "Queued")
\* Post-barrier abandonment after the old CLI ended and before any new one ran.
W_HeldNoSession == ~(s.phase = "Abandoned" /\ s.barrier
                     /\ ~s.proc.oldAlive /\ ~s.proc.newAlive)
\* A dcserver crash between StopIntent and DispatchStop leaves no path to dispatch.
W_StopPermitLost == ~(s.phase = "StopIntent" /\ s.proc.stopPermit = None
                      /\ ~s.proc.stopFlight /\ s.proc.oldAlive /\ s.online)

\* Optimistic fairness: every internal step and every provider success is weakly fair.
\* Environment events (arrivals, crashes, controls, blockers, losses) are not.
Fair ==
  /\ WF_vars(Boot) /\ WF_vars(Tick) /\ WF_vars(Expire)
  /\ WF_vars(AcquireCapability) /\ WF_vars(Reserve) /\ WF_vars(Stage)
  /\ WF_vars(MissingBody) /\ WF_vars(Barrier) /\ WF_vars(StopIntent)
  /\ WF_vars(DispatchStop) /\ WF_vars(StopCompletes) /\ WF_vars(ProveStopped)
  /\ WF_vars(LaunchIntent) /\ WF_vars(DispatchLaunch) /\ WF_vars(LaunchCompletes)
  /\ WF_vars(BindSuccessor) /\ WF_vars(CaptureSuccessor) /\ WF_vars(RetireOld)
  /\ WF_vars(ArmPost) /\ WF_vars(SendHttp) /\ WF_vars(HttpAccept)
  /\ WF_vars(PostReceipt) /\ WF_vars(RefreshOutput) /\ WF_vars(EndTurn)
  /\ \A i \in Ordinary: WF_vars(PinOrdinary(i))
  /\ \A i \in Inputs: /\ WF_vars(Arm(i)) /\ WF_vars(DispatchInput(i))
                      /\ WF_vars(InputAccept(i)) /\ WF_vars(NativeReceipt(i))
                      /\ WF_vars(SettleInput(i))
  /\ \A d \in Debt: WF_vars(DrainDebt(d))
  /\ \A b \in Blocks: WF_vars(RemoveBlocker(b))
FairSpec == Init /\ [][Next]_vars /\ Fair

\* "No permanent stall": a waiting person's input eventually leaves Queued, and the
\* rotation's input barrier is eventually released.
QueuedLeaves == \A i \in Ordinary: (s.input[i].state = "Queued") ~> (s.input[i].state # "Queued")
BarrierReleases == s.barrier ~> ~s.barrier
=============================================================================
