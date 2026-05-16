// Round Table Meetings
export interface ProposedIssue {
  title: string;
  body: string;
  assignee: string;
}

export interface IssueCreationResult {
  key: string;
  title: string;
  assignee: string;
  ok: boolean;
  discarded?: boolean;
  error?: string | null;
  issue_url?: string | null;
  attempted_at: number;
}

export interface RoundTableMeetingChannelOption {
  channel_id: string;
  channel_name: string;
  owner_provider: string;
  available_experts?: RoundTableMeetingExpertOption[];
}

export interface RoundTableMeetingExpertOption {
  role_id: string;
  display_name: string;
  keywords: string[];
  domain_summary?: string | null;
  strengths: string[];
  task_types: string[];
  anti_signals: string[];
  provider_hint?: string | null;
  metadata_missing?: boolean;
  metadata_confidence?: "high" | "medium" | "low";
}

export interface RoundTableMeeting {
  id: string;
  channel_id?: string | null;
  meeting_hash?: string | null;
  thread_hash?: string | null;
  agenda: string;
  summary: string | null;
  selection_reason?: string | null;
  status: "in_progress" | "completed" | "cancelled";
  primary_provider: string | null;
  reviewer_provider: string | null;
  participant_names: string[];
  total_rounds: number;
  issues_created: number;
  proposed_issues: ProposedIssue[] | null;
  issue_creation_results: IssueCreationResult[] | null;
  issue_repo?: string | null;
  started_at: number;
  completed_at: number | null;
  created_at: number;
  entries?: RoundTableEntry[];
}

export interface RoundTableEntry {
  id: number;
  meeting_id: string;
  seq: number;
  round: number;
  speaker_role_id: string | null;
  speaker_name: string;
  content: string;
  is_summary: number;
  created_at: number;
}

export type TaskDispatchStatus =
  | "pending"
  | "dispatched"
  | "in_progress"
  | "completed"
  | "failed"
  | "cancelled";

export interface TaskDispatch {
  id: string;
  kanban_card_id: string | null;
  from_agent_id: string;
  to_agent_id: string | null;
  dispatch_type: string;
  status: TaskDispatchStatus;
  title: string;
  context_file: string | null;
  result_file: string | null;
  result_summary: string | null;
  parent_dispatch_id: string | null;
  chain_depth: number;
  created_at: number;
  dispatched_at: number | null;
  completed_at: number | null;
}

export type DispatchDeliveryEventStatus =
  | "reserved"
  | "sent"
  | "fallback"
  | "duplicate"
  | "skipped"
  | "failed";

export interface DispatchDeliveryEvent {
  id: number;
  dispatch_id: string;
  correlation_id: string;
  semantic_event_id: string;
  operation: string;
  target_kind: string;
  target_channel_id: string | null;
  target_thread_id: string | null;
  status: DispatchDeliveryEventStatus;
  attempt: number;
  message_id: string | null;
  messages_json: unknown;
  fallback_kind: string | null;
  error: string | null;
  result_json: unknown;
  reserved_until: string | null;
  created_at: string;
  updated_at: string;
}

export type KanbanCardStatus =
  | "backlog"
  | "ready"
  | "requested"
  | "blocked"
  | "in_progress"
  | "review"
  | "done"
  | "qa_pending"
  | "qa_in_progress"
  | "qa_failed";

export type KanbanCardPriority = "low" | "medium" | "high" | "urgent";

export interface KanbanReviewChecklistItem {
  id: string;
  label: string;
  done: boolean;
}

export interface KanbanCardMetadata {
  retry_count?: number;
  failover_count?: number;
  timed_out_stage?: "requested" | "in_progress";
  timed_out_at?: number;
  timed_out_reason?: string;
  redispatch_count?: number;
  redispatch_reason?: string;
  review_checklist?: KanbanReviewChecklistItem[];
  reward?: {
    granted_at: number;
    agent_id: string;
    xp: number;
    tasks_done: number;
  };
  manual_review?: boolean;
  deferred_dod?: Array<{
    id: string;
    label: string;
    verified: boolean;
    deferred_at: number;
    verified_at?: number;
  }>;
}

export interface KanbanCard {
  id: string;
  title: string;
  description: string | null;
  status: KanbanCardStatus;
  github_repo: string | null;
  owner_agent_id: string | null;
  requester_agent_id: string | null;
  assignee_agent_id: string | null;
  parent_card_id: string | null;
  latest_dispatch_id: string | null;
  sort_order: number;
  priority: KanbanCardPriority;
  depth: number;
  blocked_reason: string | null;
  review_notes: string | null;
  github_issue_number: number | null;
  github_issue_url: string | null;
  review_round?: number;
  metadata?: KanbanCardMetadata | null;
  metadata_json: string | null;
  pipeline_stage_id: string | null;
  review_status: string | null;
  created_at: number;
  updated_at: number;
  started_at: number | null;
  requested_at: number | null;
  review_entered_at?: string | number | null;
  completed_at: number | null;
  latest_dispatch_status?: TaskDispatchStatus | null;
  latest_dispatch_title?: string | null;
  latest_dispatch_type?: string | null;
  latest_dispatch_result_summary?: string | null;
  latest_dispatch_chain_depth?: number | null;
  child_count?: number;
}

// Pipeline
export interface PipelineStage {
  id: string;
  repo: string;
  stage_name: string;
  stage_order: number;
  entry_skill: string | null;
  provider: string | null;
  agent_override_id: string | null;
  timeout_minutes: number;
  on_failure: "fail" | "retry" | "previous" | "goto";
  on_failure_target: string | null;
  max_retries: number;
  skip_condition: string | null;
  parallel_with: string | null;
  applies_to_agent_id: string | null;
  trigger_after: "ready" | "review_pass";
  created_at: number;
}

export interface PipelineHistoryEntry {
  id: string;
  card_id: string;
  stage_id: string;
  stage_name: string;
  status: "active" | "completed" | "failed" | "skipped" | "retrying";
  attempt: number;
  dispatch_id: string | null;
  failure_reason: string | null;
  started_at: number;
  completed_at: number | null;
}

// Pipeline Config Hierarchy (#135)
export interface PipelineConfigFull {
  name: string;
  version: number;
  states: { id: string; label: string; terminal?: boolean }[];
  transitions: {
    from: string;
    to: string;
    type: "free" | "gated" | "force_only";
    gates?: string[];
  }[];
  gates: Record<string, { type: string; check?: string; description?: string }>;
  hooks: Record<string, { on_enter: string[]; on_exit: string[] }>;
  events: Record<string, string[]>;
  clocks: Record<string, { set: string; mode?: string }>;
  timeouts: Record<
    string,
    {
      duration: string;
      clock: string;
      max_retries?: number;
      on_exhaust?: string;
      condition?: string;
    }
  >;
  phase_gate: PhaseGateConfig;
}

export interface PhaseGateConfig {
  dispatch_to: string;
  dispatch_type: string;
  pass_verdict: string;
  checks: string[];
}

export interface PipelineOverride {
  states?: PipelineConfigFull["states"];
  transitions?: PipelineConfigFull["transitions"];
  gates?: PipelineConfigFull["gates"];
  hooks?: PipelineConfigFull["hooks"];
  events?: PipelineConfigFull["events"];
  clocks?: PipelineConfigFull["clocks"];
  timeouts?: PipelineConfigFull["timeouts"];
  phase_gate?: PhaseGateConfig;
}

export interface KanbanRepoSource {
  id: string;
  repo: string;
  default_agent_id: string | null;
  pipeline_config: PipelineOverride | null;
  created_at: number;
}
