import type { UiLanguage } from "../i18n";

export type { UiLanguage };

// Office
export interface Office {
  id: string;
  name: string;
  name_ko: string;
  icon: string;
  color: string;
  description: string | null;
  sort_order: number;
  created_at: number;
  agent_count?: number;
  department_count?: number;
}

// Department
export interface Department {
  id: string;
  name: string;
  name_ko: string;
  name_ja?: string | null;
  name_zh?: string | null;
  icon: string;
  color: string;
  description: string | null;
  prompt: string | null;
  office_id?: string | null;
  sort_order: number;
  created_at: number;
  agent_count?: number;
}

export type AgentStatus = "idle" | "working" | "break" | "offline";
export type CliProvider = "claude" | "codex" | "gemini" | "qwen" | "opencode" | "copilot" | "antigravity" | "api";
export type MeetingReviewDecision = "reviewing" | "approved" | "hold";

export type ActivitySource = "idle" | "agentdesk";

export interface Agent {
  id: string;
  name: string;
  alias?: string | null;
  name_ko: string;
  name_ja?: string | null;
  name_zh?: string | null;
  department_id: string | null;
  department?: Department;
  acts_as_planning_leader?: number | null;
  cli_provider?: CliProvider;
  role_id?: string | null;
  session_info?: string | null;
  activity_source?: ActivitySource;
  agentdesk_working_count?: number;
  current_thread_channel_id?: string | null;
  workflow_pack_key?: string | null;
  department_name?: string | null;
  department_name_ko?: string | null;
  department_color?: string | null;
  avatar_emoji: string;
  sprite_number?: number | null;
  personality: string | null;
  status: AgentStatus;
  current_task_id?: string | null;
  stats_tasks_done: number;
  stats_xp: number;
  stats_tokens: number;
  discord_channel_id?: string | null;
  discord_channel_id_alt?: string | null;
  discord_channel_id_codex?: string | null;
  created_at: number;
}

export interface MeetingPresence {
  agent_id: string;
  seat_index: number;
  phase: "kickoff" | "review";
  task_id: string | null;
  decision?: MeetingReviewDecision | null;
  until: number;
}

export interface SubAgent {
  id: string;
  parentAgentId: string;
  task: string;
  status: "working" | "done";
}

export interface CrossDeptDelivery {
  id: string;
  fromAgentId: string;
  toAgentId: string;
}

export interface CeoOfficeCall {
  id: string;
  fromAgentId: string;
  seatIndex: number;
  phase: "kickoff" | "review";
  action?: "arrive" | "speak" | "dismiss";
  line?: string;
  decision?: MeetingReviewDecision;
  taskId?: string;
  instant?: boolean;
  holdUntil?: number;
}

// Task
export type TaskStatus =
  | "inbox"
  | "planned"
  | "collaborating"
  | "in_progress"
  | "review"
  | "done"
  | "pending"
  | "cancelled";
export type TaskType = "general" | "development" | "design" | "analysis" | "presentation" | "documentation";
export const WORKFLOW_PACK_KEYS = [
  "development",
  "novel",
  "report",
  "video_preprod",
  "web_research_report",
  "roleplay",
  "cookingheart",
] as const;
export type WorkflowPackKey = (typeof WORKFLOW_PACK_KEYS)[number] | (string & {});

export interface Task {
  id: string;
  title: string;
  description: string | null;
  department_id: string | null;
  assigned_agent_id: string | null;
  assigned_agent?: Agent;
  agent_name?: string | null;
  agent_name_ko?: string | null;
  agent_avatar?: string | null;
  project_id?: string | null;
  status: TaskStatus;
  priority: number;
  task_type: TaskType;
  workflow_pack_key?: WorkflowPackKey;
  workflow_meta_json?: string | null;
  output_format?: string | null;
  project_path: string | null;
  result: string | null;
  started_at: number | null;
  completed_at: number | null;
  created_at: number;
  updated_at: number;
  source_task_id?: string | null;
  subtask_total?: number;
  subtask_done?: number;
  hidden?: number;
}

export type AssignmentMode = "auto" | "manual";

export interface Project {
  id: string;
  name: string;
  project_path: string;
  core_goal: string;
  default_pack_key?: WorkflowPackKey;
  assignment_mode: AssignmentMode;
  assigned_agent_ids?: string[];
  last_used_at: number | null;
  created_at: number;
  updated_at: number;
  github_repo?: string | null;
}

export interface TaskLog {
  id: number;
  task_id: string;
  kind: string;
  message: string;
  created_at: number;
}

export interface MeetingMinuteEntry {
  id: number;
  meeting_id: string;
  seq: number;
  speaker_agent_id: string | null;
  speaker_name: string;
  department_name: string | null;
  role_label: string | null;
  message_type: string;
  content: string;
  created_at: number;
}

export interface MeetingMinute {
  id: string;
  task_id: string;
  meeting_type: "planned" | "review";
  round: number;
  title: string;
  status: "in_progress" | "completed" | "revision_requested" | "failed";
  started_at: number;
  completed_at: number | null;
  created_at: number;
  entries: MeetingMinuteEntry[];
}

// Messages
export type SenderType = "ceo" | "agent" | "system";
export type ReceiverType = "agent" | "department" | "all";
export type MessageType = "chat" | "task_assign" | "announcement" | "directive" | "report" | "status_update";

export interface Message {
  id: string;
  sender_type: SenderType;
  sender_id: string | null;
  sender_agent?: Agent;
  sender_name?: string | null;
  sender_avatar?: string | null;
  receiver_type: ReceiverType;
  receiver_id: string | null;
  content: string;
  message_type: MessageType;
  task_id: string | null;
  created_at: number;
}

export interface AuditLogEntry {
  id: string;
  actor: string;
  action: string;
  entity_type: string;
  entity_id: string;
  summary: string;
  metadata?: Record<string, unknown> | null;
  created_at: number;
}

// CLI Status
export interface CliToolStatus {
  installed: boolean;
  version: string | null;
  authenticated: boolean;
  authHint: string;
}

export type CliStatusMap = Record<CliProvider, CliToolStatus>;

// Company Stats (matches server GET /api/stats response)
export interface CompanyStats {
  tasks: {
    total: number;
    done: number;
    in_progress: number;
    inbox: number;
    planned: number;
    collaborating: number;
    review: number;
    cancelled: number;
    completion_rate: number;
  };
  agents: {
    total: number;
    working: number;
    idle: number;
  };
  top_agents: Array<{
    id: string;
    name: string;
    alias?: string | null;
    avatar_emoji: string;
    stats_tasks_done: number;
    stats_xp: number;
    stats_tokens: number;
  }>;
  tasks_by_department: Array<{
    id: string;
    name: string;
    icon: string;
    color: string;
    total_tasks: number;
    done_tasks: number;
  }>;
  recent_activity: Array<Record<string, unknown>>;
}

// SubTask
export type SubTaskStatus = "pending" | "in_progress" | "done" | "blocked";

export interface SubTask {
  id: string;
  task_id: string;
  title: string;
  description: string | null;
  status: SubTaskStatus;
  assigned_agent_id: string | null;
  blocked_reason: string | null;
  cli_tool_use_id: string | null;
  target_department_id?: string | null;
  delegated_task_id?: string | null;
  created_at: number;
  completed_at: number | null;
}

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

export type KanbanCardStatus =
  | "backlog"
  | "ready"
  | "requested"
  | "in_progress"
  | "review"
  | "blocked"
  | "done"
  | "qa_pending"
  | "qa_in_progress"
  | "qa_failed"
  | "pending_decision";

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
  transitions: { from: string; to: string; type: "free" | "gated" | "force_only"; gates?: string[] }[];
  gates: Record<string, { type: string; check?: string; description?: string }>;
  hooks: Record<string, { on_enter: string[]; on_exit: string[] }>;
  clocks: Record<string, { set: string; mode?: string }>;
  timeouts: Record<string, { duration: string; clock: string; max_retries?: number; on_exhaust?: string; condition?: string }>;
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

// Skill Catalog
export interface SkillCatalogEntry {
  name: string;
  description: string;
  description_ko: string;
  total_calls: number;
  last_used_at: number | null;
}

// WebSocket Events
export type WSEventType =
  | "task_update"
  | "agent_status"
  | "agent_created"
  | "agent_deleted"
  | "departments_changed"
  | "offices_changed"
  | "new_message"
  | "announcement"
  | "cli_output"
  | "cli_usage_update"
  | "subtask_update"
  | "cross_dept_delivery"
  | "ceo_office_call"
  | "chat_stream"
  | "task_report"
  | "dispatched_session_new"
  | "dispatched_session_update"
  | "dispatched_session_disconnect"
  | "kanban_card_created"
  | "kanban_card_updated"
  | "kanban_card_deleted"
  | "task_dispatch_created"
  | "task_dispatch_updated"
  | "round_table_new"
  | "round_table_update"
  | "connected";

export interface WSEvent {
  type: WSEventType;
  payload: unknown;
  ts?: number;
}

// CLI Model info (rich model data from providers like Codex)
export interface ReasoningLevelOption {
  effort: string; // "low" | "medium" | "high" | "xhigh"
  description: string;
}

export interface CliModelInfo {
  slug: string;
  displayName?: string;
  description?: string;
  reasoningLevels?: ReasoningLevelOption[];
  defaultReasoningLevel?: string;
}

export type CliModelsResponse = Record<string, CliModelInfo[]>;

// Settings
export interface ProviderModelConfig {
  model: string;
  subModel?: string; // 서브 에이전트(알바생) 모델 (claude, codex만 해당)
  reasoningLevel?: string; // Codex: "low"|"medium"|"high"|"xhigh"
  subModelReasoningLevel?: string; // 알바생 추론 레벨 (codex만 해당)
}

export interface RoomTheme {
  floor1: number;
  floor2: number;
  wall: number;
  accent: number;
}

export const MESSENGER_CHANNELS = [
  "telegram",
  "whatsapp",
  "discord",
  "googlechat",
  "slack",
  "signal",
  "imessage",
] as const;

export type MessengerChannelType = (typeof MESSENGER_CHANNELS)[number];

export interface MessengerSessionConfig {
  id: string;
  name: string;
  targetId: string;
  enabled: boolean;
  token?: string;
  agentId?: string;
  workflowPackKey?: WorkflowPackKey;
}

export interface MessengerChannelConfig {
  token: string;
  sessions: MessengerSessionConfig[];
  receiveEnabled?: boolean;
}

export type MessengerChannelsConfig = Record<MessengerChannelType, MessengerChannelConfig>;

export interface OfficePackProfile {
  departments: Department[];
  agents: Agent[];
  updated_at: number;
}

export type OfficePackProfiles = Partial<Record<WorkflowPackKey, OfficePackProfile>>;

export interface CompanySettings {
  companyName: string;
  ceoName: string;
  theme: "dark" | "light" | "auto";
  language: UiLanguage;
  roomThemes?: Record<string, RoomTheme>;
}

export const DEFAULT_SETTINGS: CompanySettings = {
  companyName: "AgentDesk Dashboard",
  ceoName: "CEO",
  theme: "dark",
  language: "ko",
};

// Dispatched Session (파견 인력)
export type DispatchedSessionStatus = "working" | "idle" | "disconnected";

export interface DispatchedSession {
  id: string;
  session_key: string;
  name: string | null;
  department_id: string | null;
  linked_agent_id: string | null;
  provider: CliProvider;
  model: string | null;
  status: DispatchedSessionStatus;
  session_info: string | null;
  sprite_number: number | null;
  avatar_emoji: string;
  stats_xp: number;
  tokens: number;
  connected_at: number;
  last_seen_at: number | null;
  department_name?: string | null;
  department_name_ko?: string | null;
  department_color?: string | null;
  thread_channel_id?: string | null;
}

// Dashboard Stats
export interface DashboardStats {
  agents: {
    total: number;
    working: number;
    idle: number;
    break: number;
    offline: number;
  };
  top_agents: Array<{
    id: string;
    name: string;
    alias?: string | null;
    name_ko: string;
    avatar_emoji: string;
    stats_tasks_done: number;
    stats_xp: number;
    stats_tokens: number;
  }>;
  departments: Array<{
    id: string;
    name: string;
    name_ko: string;
    icon: string;
    color: string;
    total_agents: number;
    working_agents: number;
    sum_xp?: number;
  }>;
  dispatched_count: number;
  github_closed_today?: number;
  kanban: {
    open_total: number;
    review_queue: number;
    blocked: number;
    failed: number;
    waiting_acceptance: number;
    stale_in_progress: number;
    by_status: Record<KanbanCardStatus, number>;
    top_repos: Array<{
      github_repo: string;
      open_count: number;
      pressure_count: number;
    }>;
  };
}

export interface ReceiptSnapshotModelLine {
  model: string;
  display_name: string;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  total_tokens: number;
  cost: number;
  cost_without_cache: number;
  provider: string;
}

export interface ReceiptSnapshotProviderShare {
  provider: string;
  tokens: number;
  percentage: number;
}

export interface ReceiptSnapshotAgentShare {
  agent: string;
  tokens: number;
  cost: number;
  percentage: number;
}

export interface ReceiptSnapshotStats {
  total_messages: number;
  total_sessions: number;
}

export interface ReceiptSnapshot {
  period_label: string;
  period_start: string;
  period_end: string;
  models: ReceiptSnapshotModelLine[];
  subtotal: number;
  cache_discount: number;
  total: number;
  stats: ReceiptSnapshotStats;
  providers: ReceiptSnapshotProviderShare[];
  agents: ReceiptSnapshotAgentShare[];
}

export interface TokenAnalyticsPeakDay {
  date: string;
  total_tokens: number;
  cost: number;
}

export interface TokenAnalyticsSummary {
  total_tokens: number;
  total_cost: number;
  cache_discount: number;
  total_messages: number;
  total_sessions: number;
  active_days: number;
  average_daily_tokens: number;
  peak_day?: TokenAnalyticsPeakDay | null;
}

export interface TokenAnalyticsDailyPoint {
  date: string;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  total_tokens: number;
  cost: number;
}

export interface TokenAnalyticsHeatmapCell {
  date: string;
  week_index: number;
  weekday: number;
  total_tokens: number;
  cost: number;
  level: number;
  future: boolean;
}

export interface TokenAnalyticsResponse {
  period: "7d" | "30d" | "90d" | (string & {});
  period_label: string;
  days: number;
  generated_at: string;
  summary: TokenAnalyticsSummary;
  receipt: ReceiptSnapshot;
  daily: TokenAnalyticsDailyPoint[];
  heatmap: TokenAnalyticsHeatmapCell[];
}
