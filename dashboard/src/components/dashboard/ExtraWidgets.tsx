import { useEffect, useState, useMemo } from "react";
import type { Agent, DashboardStats } from "../../types";
import * as api from "../../api/client";
import type { TFunction } from "./model";
import { getAgentLevel, getAgentTitle } from "../agent-manager/AgentInfoCard";

// ── CookingHeart Role Board Widget ──

const CH_ROLE_PREFIXES = ["ch-pmd", "ch-pd", "ch-dd", "ch-td", "ch-ad", "ch-tad", "ch-qad"];
const CH_ROLE_LABELS: Record<string, { ko: string; en: string }> = {
  "ch-pmd": { ko: "프로젝트관리", en: "PMD" },
  "ch-pd": { ko: "기획", en: "PD" },
  "ch-dd": { ko: "게임디자인", en: "DD" },
  "ch-td": { ko: "기술개발", en: "TD" },
  "ch-ad": { ko: "아트", en: "AD" },
  "ch-tad": { ko: "테크아트", en: "TAD" },
  "ch-qad": { ko: "QA", en: "QAD" },
};

interface CookingHeartWidgetProps {
  agents: Agent[];
  t: TFunction;
  isKo: boolean;
}

export function CookingHeartRoleBoardWidget({ agents, t, isKo }: CookingHeartWidgetProps) {
  const chAgents = useMemo(
    () => agents.filter((a) => CH_ROLE_PREFIXES.some((p) => a.id.startsWith(p) || a.name.startsWith(p))),
    [agents],
  );
  if (chAgents.length === 0) return null;

  const workingCount = chAgents.filter((a) => a.status === "working").length;

  return (
    <div
      className="rounded-2xl border p-4"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #ef4444 10%), var(--th-surface))",
      }}
    >
      <div className="flex items-center justify-between mb-3">
        <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
          🍳 CookingHeart
        </h3>
        <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "rgba(239,68,68,0.15)", color: "#f87171" }}>
          {workingCount}/{chAgents.length} {t({ ko: "가동", en: "active", ja: "稼働", zh: "活跃" })}
        </span>
      </div>
      <div className="grid grid-cols-2 gap-1.5">
        {chAgents.map((agent) => {
          const roleKey = CH_ROLE_PREFIXES.find((p) => agent.id.startsWith(p) || agent.name.startsWith(p));
          const roleLabel = roleKey ? (isKo ? CH_ROLE_LABELS[roleKey]?.ko : CH_ROLE_LABELS[roleKey]?.en) : "";
          const isWorking = agent.status === "working";
          return (
            <div
              key={agent.id}
              className="flex items-center gap-1.5 px-2 py-1 rounded-lg"
              style={{ background: "var(--th-bg-surface)" }}
            >
              <span className={`w-2 h-2 rounded-full shrink-0 ${isWorking ? "bg-emerald-400" : "bg-gray-400"}`} />
              <div className="min-w-0 flex-1">
                <div className="text-xs font-medium truncate" style={{ color: "var(--th-text)" }}>
                  {agent.avatar_emoji} {roleLabel || agent.alias || agent.name}
                </div>
                <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {agent.stats_xp.toLocaleString()} XP
                </div>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ── GitHub Issues Widget ──

interface GitHubIssuesWidgetProps {
  t: TFunction;
  repo?: string;
}

export function GitHubIssuesWidget({ t, repo }: GitHubIssuesWidgetProps) {
  const [data, setData] = useState<api.GitHubIssuesResponse | null>(null);

  useEffect(() => {
    api.getGitHubIssues(repo, "open", 8).then(setData).catch(() => {});
  }, [repo]);

  if (!data || data.issues.length === 0) return null;

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex items-center justify-between mb-3">
        <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
          {t({ ko: "GitHub 이슈", en: "GitHub Issues", ja: "GitHub Issues", zh: "GitHub Issues" })}
        </h3>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {data.repo}
        </span>
      </div>
      <div className="space-y-1.5 max-h-48 overflow-y-auto">
        {data.issues.map((issue) => (
          <div
            key={issue.number}
            className="flex items-start gap-2 px-2 py-1.5 rounded-lg"
            style={{ background: "var(--th-bg-surface)" }}
          >
            <span className="text-xs shrink-0 mt-0.5" style={{ color: "#34d399" }}>
              #{issue.number}
            </span>
            <div className="min-w-0 flex-1">
              <div className="text-xs font-medium truncate" style={{ color: "var(--th-text)" }}>
                {issue.title}
              </div>
              <div className="flex gap-1 flex-wrap mt-0.5">
                {issue.labels.slice(0, 3).map((label) => (
                  <span
                    key={label.name}
                    className="text-xs px-1 rounded"
                    style={{ background: `#${label.color}33`, color: `#${label.color}` }}
                  >
                    {label.name}
                  </span>
                ))}
                {issue.assignees.length > 0 && (
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    → {issue.assignees.map((a) => a.login).join(", ")}
                  </span>
                )}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

interface KanbanOpsWidgetProps {
  kanban: DashboardStats["kanban"];
  t: TFunction;
}

type OpsCategory = "review" | "acceptance" | "stalled" | "blocked";

export function KanbanOpsWidget({ kanban, t }: KanbanOpsWidgetProps) {
  const [expanded, setExpanded] = useState<OpsCategory | null>(null);
  const [cards, setCards] = useState<api.KanbanCard[]>([]);
  const [loading, setLoading] = useState(false);

  const categoryFilter: Record<OpsCategory, (c: api.KanbanCard) => boolean> = useMemo(() => ({
    review: (c) => c.status === "review",
    acceptance: (c) => c.status === "requested",
    stalled: (c) => c.status === "in_progress",
    blocked: (c) => c.status === "blocked",
  }), []);

  const handleToggle = async (cat: OpsCategory) => {
    if (expanded === cat) { setExpanded(null); return; }
    setExpanded(cat);
    setLoading(true);
    try {
      const all = await api.getKanbanCards();
      setCards(all.filter(categoryFilter[cat]));
    } catch { setCards([]); }
    setLoading(false);
  };

  const handleAction = async (cardId: string, action: "retry" | "ready" | "done") => {
    try {
      if (action === "retry") await api.retryKanbanCard(cardId);
      else if (action === "ready") await api.updateKanbanCard(cardId, { status: "ready" } as never);
      else if (action === "done") await api.updateKanbanCard(cardId, { status: "done" } as never);
      // Refresh
      if (expanded) {
        const all = await api.getKanbanCards();
        setCards(all.filter(categoryFilter[expanded]));
      }
    } catch { /* ignore */ }
  };

  const categories: Array<{ key: OpsCategory; label: string; value: number; color: string }> = [
    { key: "review", label: t({ ko: "검토 대기", en: "Review", ja: "レビュー待ち", zh: "待审查" }), value: kanban.review_queue, color: "#14b8a6" },
    { key: "acceptance", label: t({ ko: "수락 지연", en: "Ack delay", ja: "受諾遅延", zh: "接收延迟" }), value: kanban.waiting_acceptance, color: "#8b5cf6" },
    { key: "stalled", label: t({ ko: "진행 정체", en: "Stalled", ja: "停滞", zh: "停滞" }), value: kanban.stale_in_progress, color: "#f59e0b" },
    { key: "blocked", label: t({ ko: "막힘", en: "Blocked", ja: "詰まり", zh: "阻塞" }), value: kanban.blocked, color: "#ef4444" },
  ];

  return (
    <div
      className="rounded-2xl border p-4"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 92%, #0ea5e9 8%), var(--th-surface))",
      }}
    >
      <div className="flex items-center justify-between mb-3 gap-3">
        <div>
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "칸반 운영 상태", en: "Kanban Ops", ja: "カンバン運用", zh: "看板运营" })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "병목과 대기 중인 카드", en: "Bottlenecks and waiting cards", ja: "ボトルネックと待機カード", zh: "瓶颈与等待卡片" })}
          </p>
        </div>
        <span className="text-xs px-2 py-1 rounded-full bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
          {kanban.open_total}
        </span>
      </div>

      <div className="grid grid-cols-2 lg:grid-cols-4 gap-2">
        {categories.map((item) => (
          <button
            key={item.key}
            type="button"
            onClick={() => item.value > 0 && handleToggle(item.key)}
            className="rounded-xl px-3 py-2 text-left transition-all"
            style={{
              background: expanded === item.key ? `color-mix(in srgb, ${item.color} 12%, var(--th-bg-surface))` : "var(--th-bg-surface)",
              outline: expanded === item.key ? `1px solid color-mix(in srgb, ${item.color} 40%, transparent)` : "none",
              cursor: item.value > 0 ? "pointer" : "default",
            }}
          >
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{item.label}</div>
            <div className="text-xl font-black" style={{ color: item.color }}>{item.value}</div>
          </button>
        ))}
      </div>

      {/* Expanded card list */}
      {expanded && (
        <div className="mt-3 space-y-2">
          {loading ? (
            <div className="text-xs text-center py-2" style={{ color: "var(--th-text-muted)" }}>...</div>
          ) : cards.length === 0 ? (
            <div className="text-xs text-center py-2" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "카드 없음", en: "No cards", ja: "カードなし", zh: "无卡片" })}
            </div>
          ) : cards.map((card) => (
            <OpsCardRow key={card.id} card={card} t={t} onAction={handleAction} />
          ))}
        </div>
      )}

      {kanban.top_repos.length > 0 && (
        <div className="mt-4 space-y-1.5">
          <div className="text-xs font-semibold uppercase tracking-[0.14em]" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "압력 높은 Repo", en: "High-pressure repos", ja: "高圧 Repo", zh: "高压 Repo" })}
          </div>
          {kanban.top_repos.map((repo) => (
            <div
              key={repo.github_repo}
              className="flex items-center justify-between gap-3 rounded-xl px-3 py-2"
              style={{ background: "var(--th-bg-surface)" }}
            >
              <div className="min-w-0">
                <div className="truncate text-sm font-medium" style={{ color: "var(--th-text)" }}>
                  {repo.github_repo}
                </div>
                <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "열린 카드", en: "Open cards", ja: "オープンカード", zh: "开放卡片" })}: {repo.open_count}
                </div>
              </div>
              <span className="text-xs px-2 py-1 rounded-full" style={{ color: "#fca5a5", background: "rgba(239,68,68,0.12)" }}>
                {repo.pressure_count}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function OpsCardRow({ card, t, onAction }: {
  card: api.KanbanCard;
  t: TFunction;
  onAction: (id: string, action: "retry" | "ready" | "done") => void;
}) {
  const repo = card.github_repo?.replace(/^[^/]+\//, "") ?? "";
  const statusColor = card.status === "blocked" ? "#f59e0b"
    : card.status === "review" ? "#14b8a6"
    : "#8b5cf6";

  return (
    <div
      className="rounded-xl px-3 py-2 flex flex-col gap-1.5"
      style={{ background: "var(--th-bg-surface)", border: "1px solid var(--th-border)" }}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5 flex-wrap">
            <span
              className="text-xs px-1.5 py-0.5 rounded font-semibold uppercase shrink-0"
              style={{ color: statusColor, background: `color-mix(in srgb, ${statusColor} 15%, transparent)` }}
            >
              {card.status}
            </span>
            {repo && (
              <span className="text-xs px-1.5 py-0.5 rounded shrink-0" style={{ color: "var(--th-text-muted)", background: "rgba(255,255,255,0.06)" }}>
                {repo}
              </span>
            )}
          </div>
          <div className="text-sm font-medium mt-0.5 truncate" style={{ color: "var(--th-text)" }}>
            {card.title}
          </div>
          {card.blocked_reason && (
            <div className="text-xs mt-0.5" style={{ color: "#fca5a5" }}>
              {card.blocked_reason}
            </div>
          )}
          {card.latest_dispatch_result_summary && card.status === "blocked" && (
            <div className="text-xs mt-0.5" style={{ color: "#fca5a5" }}>
              {card.latest_dispatch_result_summary}
            </div>
          )}
        </div>
        {card.github_issue_url && (
          <a
            href={card.github_issue_url}
            target="_blank"
            rel="noopener noreferrer"
            className="text-xs shrink-0 hover:underline"
            style={{ color: "#93c5fd" }}
          >
            #{card.github_issue_number}
          </a>
        )}
      </div>
      <div className="flex items-center gap-1.5">
        {card.status === "blocked" && (
          <>
            <button
              type="button"
              onClick={() => onAction(card.id, "retry")}
              className="text-xs px-2 py-0.5 rounded-md font-medium transition-colors hover:brightness-110"
              style={{ color: "#67e8f9", background: "rgba(103,232,249,0.12)", border: "1px solid rgba(103,232,249,0.2)" }}
            >
              {t({ ko: "재시도", en: "Retry", ja: "再試行", zh: "重试" })}
            </button>
            <button
              type="button"
              onClick={() => onAction(card.id, "ready")}
              className="text-xs px-2 py-0.5 rounded-md font-medium transition-colors hover:brightness-110"
              style={{ color: "#a5b4fc", background: "rgba(165,180,252,0.12)", border: "1px solid rgba(165,180,252,0.2)" }}
            >
              {t({ ko: "Ready로", en: "To Ready", ja: "Readyへ", zh: "重置Ready" })}
            </button>
          </>
        )}
        <button
          type="button"
          onClick={() => onAction(card.id, "done")}
          className="text-xs px-2 py-0.5 rounded-md font-medium transition-colors hover:brightness-110"
          style={{ color: "#86efac", background: "rgba(134,239,172,0.12)", border: "1px solid rgba(134,239,172,0.2)" }}
        >
          {t({ ko: "Done", en: "Done", ja: "Done", zh: "完成" })}
        </button>
      </div>
    </div>
  );
}

// ── Machine Status Widget ──

interface MachineStatusWidgetProps {
  t: TFunction;
}

export function MachineStatusWidget({ t }: MachineStatusWidgetProps) {
  const [machines, setMachines] = useState<api.MachineStatus[]>([]);

  useEffect(() => {
    api.getMachineStatus().then(setMachines).catch(() => {});
    const timer = setInterval(() => {
      api.getMachineStatus().then(setMachines).catch(() => {});
    }, 60_000);
    return () => clearInterval(timer);
  }, []);

  if (machines.length === 0) return null;

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        {t({ ko: "머신 상태", en: "Machine Status", ja: "マシン状態", zh: "机器状态" })}
      </h3>
      <div className="flex gap-3">
        {machines.map((m) => (
          <div
            key={m.name}
            className="flex items-center gap-2 px-3 py-2 rounded-xl flex-1"
            style={{ background: "var(--th-bg-surface)" }}
          >
            <span className="text-lg">{m.name === "mac-mini" ? "🖥️" : "💻"}</span>
            <div>
              <div className="text-xs font-medium" style={{ color: "var(--th-text)" }}>{m.name}</div>
              <div className="flex items-center gap-1">
                <span
                  className={`w-2 h-2 rounded-full ${m.online ? "bg-emerald-400" : "bg-red-400 animate-pulse"}`}
                />
                <span className="text-xs" style={{ color: m.online ? "#34d399" : "#f87171" }}>
                  {m.online
                    ? t({ ko: "온라인", en: "Online", ja: "オンライン", zh: "在线" })
                    : t({ ko: "오프라인", en: "Offline", ja: "オフライン", zh: "离线" })}
                </span>
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── Activity Heatmap Widget ──

interface HeatmapWidgetProps {
  agents: Agent[];
  t: TFunction;
}

export function HeatmapWidget({ agents, t }: HeatmapWidgetProps) {
  const [data, setData] = useState<api.HeatmapData | null>(null);

  useEffect(() => {
    api.getActivityHeatmap().then(setData).catch(() => {});
  }, []);

  if (!data) return null;

  const maxCount = Math.max(1, ...data.hours.map((h) => Object.values(h.agents).reduce((s, v) => s + v, 0)));
  const currentHour = new Date().getHours();

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        {t({ ko: "오늘의 활동 히트맵", en: "Today's Activity Heatmap", ja: "今日の活動ヒートマップ", zh: "今日活动热力图" })}
      </h3>
      <div className="flex gap-[2px] items-end h-16">
        {data.hours.map((h) => {
          const total = Object.values(h.agents).reduce((s, v) => s + v, 0);
          const height = Math.max(2, (total / maxCount) * 100);
          const isCurrent = h.hour === currentHour;
          return (
            <div
              key={h.hour}
              className="flex-1 rounded-t relative group cursor-default"
              style={{
                height: `${height}%`,
                background: total === 0
                  ? "rgba(100,116,139,0.15)"
                  : isCurrent
                    ? "#6366f1"
                    : `rgba(99,102,241,${0.2 + (total / maxCount) * 0.6})`,
                minWidth: 0,
              }}
              title={`${h.hour}:00 — ${total} events`}
            />
          );
        })}
      </div>
      <div className="flex justify-between mt-1">
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>0h</span>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>6h</span>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>12h</span>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>18h</span>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>24h</span>
      </div>
    </div>
  );
}

// ── Cron Timeline Widget ──

interface CronTimelineWidgetProps {
  t: TFunction;
}

export function CronTimelineWidget({ t }: CronTimelineWidgetProps) {
  const [jobs, setJobs] = useState<api.CronJobGlobal[]>([]);

  useEffect(() => {
    api.getCronJobs().then(setJobs).catch(() => {});
  }, []);

  const enabledJobs = useMemo(() => jobs.filter((j) => j.enabled), [jobs]);

  if (enabledJobs.length === 0) return null;

  const now = Date.now();

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex items-center justify-between mb-3">
        <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
          {t({ ko: "크론잡 타임라인", en: "Cron Timeline", ja: "クロンタイムライン", zh: "定时任务时间线" })}
        </h3>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {enabledJobs.length} {t({ ko: "활성", en: "active", ja: "アクティブ", zh: "活跃" })}
        </span>
      </div>
      <div className="space-y-1.5 max-h-60 overflow-y-auto">
        {enabledJobs
          .sort((a, b) => {
            const aNext = a.state?.nextRunAtMs ?? Infinity;
            const bNext = b.state?.nextRunAtMs ?? Infinity;
            return aNext - bNext;
          })
          .map((job) => {
            const lastRun = job.state?.lastRunAtMs;
            const nextRun = job.state?.nextRunAtMs;
            const isOk = job.state?.lastStatus === "ok";
            const isOverdue = nextRun != null && nextRun < now;

            return (
              <div
                key={job.id}
                className="flex items-center gap-2 px-2.5 py-1.5 rounded-lg"
                style={{ background: "var(--th-bg-surface)" }}
              >
                <span
                  className={`w-2 h-2 rounded-full shrink-0 ${isOk ? "bg-emerald-400" : "bg-amber-400"}`}
                />
                <div className="flex-1 min-w-0">
                  <div className="text-xs font-medium truncate" style={{ color: "var(--th-text)" }}>
                    {job.description_ko || job.name}
                  </div>
                  <div className="text-xs flex gap-2" style={{ color: "var(--th-text-muted)" }}>
                    <span>{job.agentId}</span>
                    {lastRun && (
                      <span>
                        {t({ ko: "최근", en: "Last", ja: "最近", zh: "最近" })}: {new Date(lastRun).toLocaleTimeString("ko-KR", { hour: "2-digit", minute: "2-digit" })}
                      </span>
                    )}
                  </div>
                </div>
                {nextRun && (
                  <span
                    className="text-xs px-1.5 py-0.5 rounded shrink-0"
                    style={{
                      background: isOverdue ? "rgba(239,68,68,0.15)" : "rgba(99,102,241,0.15)",
                      color: isOverdue ? "#f87171" : "#a5b4fc",
                    }}
                  >
                    {isOverdue ? "⏰ " : ""}
                    {new Date(nextRun).toLocaleTimeString("ko-KR", { hour: "2-digit", minute: "2-digit" })}
                  </span>
                )}
              </div>
            );
          })}
      </div>
    </div>
  );
}

// ── Streak Counter Widget ──

interface StreakWidgetProps {
  agents: Agent[];
  t: TFunction;
}

export function StreakWidget({ agents, t }: StreakWidgetProps) {
  const [streaks, setStreaks] = useState<api.AgentStreak[]>([]);
  const workingCount = agents.filter((a) => a.status === "working").length;
  const totalXp = agents.reduce((s, a) => s + a.stats_xp, 0);

  useEffect(() => {
    api.getStreaks().then((d) => setStreaks(d.streaks)).catch(() => {});
  }, []);

  const topStreak = streaks.length > 0 ? streaks[0] : null;

  return (
    <div
      className="rounded-2xl border p-4 text-center"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #f97316 10%), var(--th-surface))",
      }}
    >
      <div className="text-3xl mb-1">{workingCount > 0 ? "🔥" : "💤"}</div>
      <div className="text-2xl font-bold" style={{ color: "var(--th-text)" }}>
        {workingCount}
      </div>
      <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
        {t({ ko: "현재 가동 중", en: "Currently Active", ja: "現在稼働中", zh: "当前活跃" })}
      </div>
      {topStreak && topStreak.streak > 1 && (
        <div className="text-xs mt-2" style={{ color: "#f97316" }}>
          🏅 {topStreak.avatar_emoji} {topStreak.name} — {topStreak.streak}{t({ ko: "일 연속", en: "d streak", ja: "日連続", zh: "天连续" })}
        </div>
      )}
      <div className="text-xs mt-1" style={{ color: "var(--th-text-muted)" }}>
        {t({ ko: "총 XP", en: "Total XP", ja: "合計XP", zh: "总XP" })}: {totalXp.toLocaleString()}
      </div>
    </div>
  );
}

// ── Achievement Wall Widget ──

interface AchievementWidgetProps {
  t: TFunction;
}

export function AchievementWidget({ t }: AchievementWidgetProps) {
  const [achievements, setAchievements] = useState<api.Achievement[]>([]);

  useEffect(() => {
    api.getAchievements().then((d) => setAchievements(d.achievements)).catch(() => {});
  }, []);

  if (achievements.length === 0) return null;

  const badgeIcon: Record<string, string> = {
    xp_100: "⭐", xp_500: "🌟", xp_1000: "💫", xp_5000: "🏅",
    tasks_10: "🐝", tasks_50: "👑", tasks_100: "🎖️",
    streak_7: "🔥", streak_30: "💎",
  };

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #eab308 10%), var(--th-surface))" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        🏆 {t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" })}
      </h3>
      <div className="space-y-1.5 max-h-48 overflow-y-auto">
        {achievements.slice(0, 15).map((ach) => (
          <div
            key={ach.id}
            className="flex items-center gap-2 px-2 py-1.5 rounded-lg"
            style={{ background: "var(--th-bg-surface)" }}
          >
            <span className="text-base">{badgeIcon[ach.type] || "🎯"}</span>
            <div className="flex-1 min-w-0">
              <div className="text-xs font-medium truncate" style={{ color: "var(--th-text)" }}>
                {ach.avatar_emoji} {ach.agent_name_ko || ach.agent_name}
              </div>
              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {ach.name} — {ach.description}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── Weekly MVP Widget ──

interface MvpWidgetProps {
  agents: Agent[];
  t: TFunction;
  isKo: boolean;
}

export function MvpWidget({ agents, t, isKo }: MvpWidgetProps) {
  if (agents.length === 0) return null;
  const mvp = agents.reduce((best, a) => (a.stats_xp > best.stats_xp ? a : best), agents[0]);
  const lvInfo = getAgentLevel(mvp.stats_xp);
  const title = getAgentTitle(mvp.stats_xp, isKo);

  return (
    <div
      className="rounded-2xl border p-4 text-center"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 88%, #eab308 12%), var(--th-surface))",
      }}
    >
      <div className="text-2xl mb-1">🏆</div>
      <div className="text-lg font-bold" style={{ color: "var(--th-text)" }}>
        {mvp.avatar_emoji} {mvp.alias || mvp.name_ko || mvp.name}
      </div>
      <div className="text-xs mt-1" style={{ color: "#eab308" }}>
        Lv.{lvInfo.level} {title}
      </div>
      <div className="text-xs mt-0.5" style={{ color: "var(--th-text-muted)" }}>
        {mvp.stats_xp.toLocaleString()} XP — {t({ ko: "최다 XP", en: "Top XP", ja: "最多XP", zh: "最多XP" })}
      </div>
    </div>
  );
}

// ── Activity Feed Widget ──

interface ActivityFeedWidgetProps {
  agents: Agent[];
  t: TFunction;
}

interface ActivityEvent {
  id: string;
  type: string;
  agent_name: string;
  agent_emoji: string;
  description: string;
  time: number;
}

export function ActivityFeedWidget({ agents, t }: ActivityFeedWidgetProps) {
  const [events, setEvents] = useState<ActivityEvent[]>([]);

  // Listen to WebSocket events via CustomEvent dispatched by useDashboardSocket
  useEffect(() => {
    let eventId = 0;
    const handler = (e: Event) => {
      const data = (e as CustomEvent).detail as { type: string; payload: unknown };
      if (!data.type || data.type === "connected") return;

      let description = "";
      let agentName = "";
      let agentEmoji = "🔔";

      const payload = data.payload as Record<string, unknown>;

      switch (data.type) {
        case "agent_status": {
          const a = payload as { name?: string; name_ko?: string; avatar_emoji?: string; status?: string; alias?: string };
          agentName = a.alias as string || a.name_ko as string || a.name as string || "Agent";
          agentEmoji = a.avatar_emoji as string || "🤖";
          description = `상태 → ${a.status}`;
          break;
        }
        case "agent_created": {
          const a = payload as { name_ko?: string; name?: string; avatar_emoji?: string };
          agentName = a.name_ko as string || a.name as string || "New Agent";
          agentEmoji = a.avatar_emoji as string || "🆕";
          description = "새 에이전트 입사";
          break;
        }
        case "new_message": {
          const m = payload as { sender_name_ko?: string; sender_name?: string; sender_avatar?: string; content?: string; sender_type?: string };
          agentName = m.sender_type === "ceo" ? "CEO" : (m.sender_name_ko as string || m.sender_name as string || "Agent");
          agentEmoji = m.sender_type === "ceo" ? "👑" : (m.sender_avatar as string || "💬");
          description = String(m.content || "").slice(0, 50);
          break;
        }
        default: {
          agentName = "System";
          agentEmoji = "📡";
          description = data.type.replace(/_/g, " ");
        }
      }

      if (description) {
        setEvents((prev) => [
          { id: `evt-${++eventId}`, type: data.type, agent_name: agentName, agent_emoji: agentEmoji, description, time: Date.now() },
          ...prev,
        ].slice(0, 30));
      }
    };

    window.addEventListener("pcd-ws-event", handler);
    return () => window.removeEventListener("pcd-ws-event", handler);
  }, []);

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        📡 {t({ ko: "실시간 피드", en: "Live Feed", ja: "リアルタイム", zh: "实时动态" })}
      </h3>
      {events.length === 0 ? (
        <div className="text-center py-4 text-xs" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "이벤트 대기 중...", en: "Waiting for events...", ja: "イベント待機中...", zh: "等待事件..." })}
        </div>
      ) : (
        <div className="space-y-1.5 max-h-48 overflow-y-auto">
          {events.map((evt) => (
            <div
              key={evt.id}
              className="flex items-center gap-2 px-2 py-1.5 rounded-lg"
              style={{ background: "var(--th-bg-surface)" }}
            >
              <span className="text-sm">{evt.agent_emoji}</span>
              <div className="flex-1 min-w-0">
                <div className="text-xs truncate" style={{ color: "var(--th-text)" }}>
                  <span className="font-medium">{evt.agent_name}</span>
                  <span className="mx-1" style={{ color: "var(--th-text-muted)" }}>·</span>
                  {evt.description}
                </div>
              </div>
              <span className="text-xs shrink-0" style={{ color: "var(--th-text-muted)" }}>
                {new Date(evt.time).toLocaleTimeString("ko-KR", { hour: "2-digit", minute: "2-digit", second: "2-digit" })}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Skill Trend Chart (simple sparkline) ──

interface SkillTrendWidgetProps {
  t: TFunction;
}

export function SkillTrendWidget({ t }: SkillTrendWidgetProps) {
  const [trend, setTrend] = useState<Array<{ day: string; count: number }> | null>(null);

  useEffect(() => {
    fetch("/api/skills-trend?days=14")
      .then((r) => r.json())
      .then((d) => setTrend(d.trend))
      .catch(() => {});
  }, []);

  if (!trend || trend.length === 0) return null;

  const days = trend.map((entry) => entry.day);
  const dailyTotals = trend.map((entry) => entry.count);
  const max = Math.max(1, ...dailyTotals);

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        {t({ ko: "스킬 사용 추이 (14일)", en: "Skill Usage Trend (14d)", ja: "スキル使用推移 (14日)", zh: "技能使用趋势 (14天)" })}
      </h3>
      <div className="flex items-end gap-[3px] h-12">
        {dailyTotals.map((total, i) => (
          <div
            key={days[i]}
            className="flex-1 rounded-t"
            style={{
              height: `${Math.max(4, (total / max) * 100)}%`,
              background: `rgba(245,158,11,${0.3 + (total / max) * 0.5})`,
              minWidth: 0,
            }}
            title={`${days[i]}: ${total} calls`}
          />
        ))}
      </div>
      <div className="flex justify-between mt-1">
        {days.length > 0 && (
          <>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {days[0].slice(5)}
            </span>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {days[days.length - 1].slice(5)}
            </span>
          </>
        )}
      </div>
    </div>
  );
}
