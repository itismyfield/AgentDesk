import type { KanbanCardStatus } from "./kanban";

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
  cost_without_cache?: number;
  input_tokens?: number;
  cache_read_tokens?: number;
  cache_creation_tokens?: number;
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
