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

export type AgentStatus = "idle" | "working" | "break" | "offline" | "archived";
export type CliProvider =
  | "claude"
  | "codex"
  | "gemini"
  | "qwen"
  | "opencode"
  | "copilot"
  | "antigravity"
  | "api";
export type MeetingReviewDecision = "reviewing" | "approved" | "hold";

export type ActivitySource = "idle" | "agentdesk";

export type VoiceSensitivityMode = "normal" | "conservative";

export interface VoiceGlobalConfig {
  lobby_channel_id: string | null;
  active_agent_ttl_seconds: number;
  default_sensitivity_mode: VoiceSensitivityMode;
}

export interface VoiceAgentConfig {
  id: string;
  name: string;
  name_ko: string | null;
  voice_enabled: boolean;
  wake_word: string;
  aliases: string[];
  sensitivity_mode: VoiceSensitivityMode;
}

export interface VoiceConfigResponse {
  global: VoiceGlobalConfig;
  agents: VoiceAgentConfig[];
  version: string;
  source_path?: string | null;
}

export interface VoiceConfigPutBody {
  version?: string;
  actor?: string;
  global: VoiceGlobalConfig;
  agents: VoiceAgentConfig[];
}

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
  avatar_seed?: number | null;
  sprite_number?: number | null;
  personality: string | null;
  system_prompt?: string | null;
  status: AgentStatus;
  prompt_path?: string | null;
  prompt_content?: string | null;
  archive_state?: string | null;
  archived_at?: number | null;
  archive_reason?: string | null;
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
export type TaskType =
  | "general"
  | "development"
  | "design"
  | "analysis"
  | "presentation"
  | "documentation";
export const WORKFLOW_PACK_KEYS = [
  "development",
  "novel",
  "report",
  "video_preprod",
  "web_research_report",
  "roleplay",
  "cookingheart",
] as const;
export type WorkflowPackKey =
  | (typeof WORKFLOW_PACK_KEYS)[number]
  | (string & {});

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
export type MessageType =
  | "chat"
  | "task_assign"
  | "announcement"
  | "directive"
  | "report"
  | "status_update";

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
  /* Enrichment fields populated by the audit-logs LEFT JOIN with kanban_cards
     when entity_type === "kanban_card". Used by the agent drawer's restored
     "감사 / Audit" panel so rows can render the human-readable card title +
     issue number instead of raw `kanban_card:UUID` strings (#1258 follow-up). */
  card_title?: string | null;
  card_issue_number?: number | null;
  card_issue_url?: string | null;
  card_assigned_agent_id?: string | null;
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

export * from "./kanban";

// Skill Catalog
export interface SkillCatalogEntry {
  name: string;
  description: string;
  description_ko: string;
  total_calls: number;
  last_used_at: number | null;
}

// WebSocket Events
//
// #2050 P1/P2 finding 1 — keep this union aligned with events the server
// actually broadcasts. The 11 events removed below (task_update,
// subtask_update, departments_changed, offices_changed, new_message,
// announcement, cli_output, cli_usage_update, cross_dept_delivery,
// ceo_office_call, chat_stream, task_report) had no server emit path and
// existed only as dead-code handlers in dashboard contexts. Polling
// fallbacks in App.tsx / context providers cover the remaining state.
export type WSEventType =
  | "agent_status"
  | "agent_created"
  | "agent_deleted"
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

export type MessengerChannelsConfig = Record<
  MessengerChannelType,
  MessengerChannelConfig
>;

export interface OfficePackProfile {
  departments: Department[];
  agents: Agent[];
  updated_at: number;
}

export type OfficePackProfiles = Partial<
  Record<WorkflowPackKey, OfficePackProfile>
>;

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
export type DispatchedSessionStatus =
  | "turn_active"
  | "awaiting_bg"
  | "awaiting_user"
  | "idle"
  | "disconnected"
  | "aborted"
  | "working";

export interface DispatchedSession {
  id: string;
  session_key: string;
  instance_id?: string | null;
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
  guild_id?: string | null;
  channel_web_url?: string | null;
  channel_deeplink_url?: string | null;
  /* Issue #1241: canonical Discord deeplink fields. The dashboard renders
     these straight into anchor `href`s; the backend already formats them as
     https://discord.com/channels/{guild}/{channel} (web) and
     discord://discord.com/channels/{guild}/{channel} (Discord app). For
     thread-bound sessions `channel_id` === `thread_id` because every
     dispatched session lives inside its agent thread. */
  channel_id?: string | null;
  thread_id?: string | null;
  deeplink_url?: string | null;
  thread_deeplink_url?: string | null;
  /* The kanban card this session's active dispatch is bound to. Lets the
     restored "감사 / Audit" panel on the agent drawer deeplink each audit
     row to the most recent Discord turn for the same card without an extra
     round-trip. Returned via LEFT JOIN with task_dispatches in
     /api/agents/:id/dispatched-sessions. */
  kanban_card_id?: string | null;
}

export * from "./analytics";
