import type { ServerCampaignDefinition, ServerCampaignDocument, ServerCampaignView } from "./serverCampaigns";

// Synthetic read-contract examples, not evidence of executor or repository support.
const timestamp = "2026-10-28T05:00:00Z";
const definition: ServerCampaignDefinition = {
  repository: "adk", title: "Release", goal: "Integrate and verify the release", profile_version: "v1",
  nodes: [
    {
      id: "A", title: "Implement", issue_url: "https://github.com/itismyfield/AgentDesk/issues/7438",
      acceptance: [
        { id: "main", description: "Included in main", verification: "main" },
        { id: "deploy", description: "Verify deployed release", verification: "deployment" },
      ],
      dependencies: [], group: null,
    },
    {
      id: "B", title: "Dependent work", issue_url: "https://github.com/itismyfield/AgentDesk/issues/7439",
      acceptance: [{ id: "test", description: "Required tests pass", verification: "test" }],
      dependencies: ["A"], group: "release",
    },
    {
      id: "C", title: "Independent work", issue_url: "https://github.com/itismyfield/AgentDesk/issues/7440",
      acceptance: [{ id: "ops", description: "Operational verification", verification: "operational" }],
      dependencies: [], group: "release",
    },
  ],
};
type Runtime = ServerCampaignView["nodes"][number]["runtime"];
type CommandAvailability = ServerCampaignView["commands"][number];
const runtime: Runtime = {
  stage: "requirements", disposition: "pending", series: 1,
  quality_failures: 0, repair_cycles: 0, review_rounds: 0, current_attempt: null,
  main_integrated: false, acceptance_complete: false, evidence_current: false, wait_reason: null,
};
const commands: CommandAvailability[] = [
  { command: "edit_definition", rejection: null },
  { command: "start", rejection: "execution_unavailable" },
  { command: "stop_immediate", rejection: "invalid_state" },
  { command: "stop_finish_stage", rejection: "invalid_state" },
  { command: "resume", rejection: "execution_unavailable" },
  { command: "prepare_retry", rejection: "execution_unavailable" },
];
const runningCommands: CommandAvailability[] = commands.map((entry) => ({
  ...entry,
  rejection: entry.command === "edit_definition" ? "invalid_state"
    : entry.command === "stop_immediate" || entry.command === "stop_finish_stage" ? null : entry.rejection,
}));
const prepared: ServerCampaignDocument = {
  definition,
  view: {
    execution_mode: "server", campaign_id: "release", as_of: timestamp,
    definition_revision: 7, run_id: null, control_version: null,
    last_event_sequence: 1, last_event_at: timestamp,
    control: "prepared", stop_mode: null, stop_confirmed: false, display: "prepared",
    execution_readiness: "unavailable", unknown: false, failed_count: 0, blocked_count: 0,
    reasons: [
      { code: "predecessor_main", target: "B" },
      { code: "execution_unavailable", target: "release" },
    ],
    commands,
    nodes: [
      { id: "A", runtime, direct_successors: ["B"], ready_after_main: ["B"] },
      { id: "B", runtime, direct_successors: [], ready_after_main: [] },
      { id: "C", runtime, direct_successors: [], ready_after_main: [] },
    ],
    sessions: [], unreconciled_actions: [],
  },
};
function example(view: Partial<ServerCampaignView>): ServerCampaignDocument {
  return structuredClone({ definition, view: { ...prepared.view, ...view } });
}
function node(id: string, changes: Partial<Runtime>, readyAfterMain: string[] = []): ServerCampaignView["nodes"][number] {
  return {
    id, runtime: { ...runtime, ...changes },
    direct_successors: id === "A" ? ["B"] : [], ready_after_main: readyAfterMain,
  };
}
const failed: Partial<Runtime> = {
  stage: "implementation", disposition: "failed", quality_failures: 4, repair_cycles: 3, review_rounds: 3,
};
const complete: Partial<Runtime> = {
  stage: "complete", disposition: "complete", main_integrated: true, acceptance_complete: true, evidence_current: true,
};
const failedRunning = example({
  control: "running", display: "running", run_id: "run-1", control_version: 1, last_event_sequence: 20,
  commands: runningCommands, failed_count: 1, blocked_count: 1,
  reasons: [
    { code: "predecessor_failed", target: "B" },
    { code: "execution_unavailable", target: "release" },
  ],
  nodes: [
    node("A", failed), node("B", { disposition: "blocked" }),
    node("C", { stage: "implementation", disposition: "running", current_attempt: "attempt-C" }),
  ],
});
const failedBlockedComplete = structuredClone(failedRunning);
failedBlockedComplete.view.display = "unable_to_progress";
failedBlockedComplete.view.nodes[2] = node("C", complete);

const reviewWaiting = example({
  control: "running", display: "waiting", run_id: "run-1", control_version: 1, last_event_sequence: 21,
  commands: runningCommands,
  reasons: [
    { code: "review_resources", target: "A" }, { code: "predecessor_main", target: "B" },
    { code: "execution_unavailable", target: "release" },
  ],
  nodes: [
    node("A", { stage: "review", disposition: "assigned", wait_reason: "review_resources" }, ["B"]),
    node("B", {}), node("C", complete),
  ],
});
const mainPendingAcceptance = example({
  control: "running", display: "running", run_id: "run-1", control_version: 1, last_event_sequence: 22,
  commands: runningCommands,
  reasons: [{ code: "execution_unavailable", target: "release" }],
  nodes: [
    node("A", { stage: "final_verification", disposition: "verifying", main_integrated: true, evidence_current: true }, ["B"]),
    node("B", {}), node("C", complete),
  ],
});

const gate = { host: "host-a", gate: "launch", incarnation: "boot-9" };
const identity = { gate, session: "worker-A", incarnation: "session-3" };
const stoppingUnknown = example({
  control: "stopping", display: "stopping", stop_mode: "finish_stage", stop_confirmed: false,
  run_id: "run-1", control_version: 2, last_event_sequence: 23, unknown: true,
  commands: runningCommands,
  reasons: [
    { code: "predecessor_main", target: "B" }, { code: "execution_unavailable", target: "release" },
    { code: "observation_unknown", target: "release" },
  ],
  nodes: [
    node("A", { stage: "implementation", disposition: "running", current_attempt: "attempt-A" }),
    node("B", {}), node("C", complete),
  ],
  sessions: [{
    identity, attempt_id: "attempt-A",
    observation: { state: "unknown", reason: "Host unreachable", last_observed_at: null },
  }],
  unreconciled_actions: [{
    id: "action-A", kind: "launch", target: gate, state: "unknown", initiation_receipt: null, reconciliation: null,
    binding: {
      campaign_id: "release", run_id: "run-1", node_id: "A", series: 1, stage: "implementation",
      attempt_id: "attempt-A", role: "implementation", session: identity, repository: "adk",
      hashes: { requirements: "1".repeat(64), acceptance: "2".repeat(64), profile: "3".repeat(64) },
      workflow_version: "campaign-v1", base_sha: "a".repeat(40), candidate_sha: "b".repeat(40), control_version: 1,
    },
  }],
});
const stoppedGone = example({
  ...failedBlockedComplete.view,
  control: "stopped", display: "stopped", stop_mode: "immediate", stop_confirmed: true,
  control_version: 3, last_event_sequence: 26, commands,
  sessions: [{
    identity, attempt_id: "attempt-A",
    observation: { state: "gone", observed_at: timestamp, evidence: "Exact session incarnation exit observed" },
  }],
  unreconciled_actions: [],
});
// Resume alone retains the failed series. Retry preparation is a distinct hypothetical snapshot;
// both execution commands remain unavailable in this contract-only implementation.
const resumeWithoutRetry = structuredClone(stoppedGone);
const retryPrepared = structuredClone(stoppedGone);
retryPrepared.view.last_event_sequence = 27;
retryPrepared.view.failed_count = 0;
retryPrepared.view.nodes[0] = node("A", { series: 2 }, []);
retryPrepared.view.reasons[0] = { code: "predecessor_main", target: "B" };

const eventAdvanced = structuredClone(stoppingUnknown);
eventAdvanced.view.as_of = "2026-10-28T05:00:01Z";
eventAdvanced.view.last_event_sequence = 24;
eventAdvanced.view.last_event_at = eventAdvanced.view.as_of;
eventAdvanced.view.sessions[0].observation = { state: "alive", observed_at: eventAdvanced.view.as_of };

const sameIssueDistinctNodes = structuredClone(prepared);
sameIssueDistinctNodes.definition.nodes[1].issue_url = sameIssueDistinctNodes.definition.nodes[0].issue_url;

export const serverCampaignFixtures = {
  prepared: structuredClone(prepared), failedRunning, failedBlockedComplete, reviewWaiting,
  mainPendingAcceptance, stoppingUnknown, stoppedGone, resumeWithoutRetry, retryPrepared,
  eventAdvanced, sameIssueDistinctNodes,
} satisfies Record<string, ServerCampaignDocument>;
