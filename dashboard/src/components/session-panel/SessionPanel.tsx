import { useState, useRef, useEffect } from "react";
import type { Agent, Department, DispatchedSession } from "../../types";
import { Monitor, MapPin, Clock, Wifi, WifiOff } from "lucide-react";
import { getRankTier } from "../dashboard/model";
import { useI18n } from "../../i18n";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
} from "../common/SurfacePrimitives";
import TooltipLabel from "../common/TooltipLabel";

const STALE_IDLE_MS = 7 * 24 * 60 * 60 * 1000;

function normalizeTimestamp(value: unknown): number | null {
  if (value == null) return null;

  const normalizeEpoch = (epoch: number): number | null => {
    if (!Number.isFinite(epoch) || epoch <= 0) return null;
    return epoch < 1e12 ? Math.trunc(epoch * 1000) : Math.trunc(epoch);
  };

  if (typeof value === "number") return normalizeEpoch(value);

  if (typeof value === "string") {
    const trimmed = value.trim();
    if (!trimmed) return null;

    const numeric = Number(trimmed);
    if (Number.isFinite(numeric)) return normalizeEpoch(numeric);

    const parsed = Date.parse(trimmed);
    return Number.isFinite(parsed) ? parsed : null;
  }

  if (value instanceof Date) {
    return normalizeEpoch(value.getTime());
  }

  return null;
}

function sessionSpriteNum(s: DispatchedSession): number {
  if (s.sprite_number != null && s.sprite_number > 0) return s.sprite_number;
  const idStr = String(s.id);
  let hash = 0;
  for (let i = 0; i < idStr.length; i += 1) {
    hash = (hash * 31 + idStr.charCodeAt(i)) >>> 0;
  }
  return (hash % 12) + 1;
}

/** Display name for a session; full key used as tooltip */
function sessionDisplayName(s: DispatchedSession): { label: string; full: string } {
  if (s.name) return { label: s.name, full: s.name };
  return { label: `Session ${s.session_key.slice(0, 8)}`, full: s.session_key };
}

function sessionLastActivityTs(s: DispatchedSession): number {
  const lastSeenAt = normalizeTimestamp(s.last_seen_at);
  if (lastSeenAt != null) return lastSeenAt;
  const connectedAt = normalizeTimestamp(s.connected_at);
  if (connectedAt != null) return connectedAt;
  return 0;
}

function isStaleIdleSession(s: DispatchedSession): boolean {
  return s.status === "idle" && Date.now() - sessionLastActivityTs(s) >= STALE_IDLE_MS;
}

function compareSessions(a: DispatchedSession, b: DispatchedSession): number {
  const rank = (session: DispatchedSession): number => {
    if (session.status === "working") return 0;
    if (session.status === "idle" && !isStaleIdleSession(session)) return 1;
    if (session.status === "idle") return 2;
    return 3;
  };

  const rankDiff = rank(a) - rank(b);
  if (rankDiff !== 0) return rankDiff;
  return sessionLastActivityTs(b) - sessionLastActivityTs(a);
}

function linkedAgentLabel(s: DispatchedSession, agents: Agent[]): string | null {
  const linked = agents.find((agent) => agent.id === s.linked_agent_id);
  if (linked) return linked.name_ko || linked.name;
  return s.linked_agent_id;
}

function sessionProviderLabel(provider?: string): string {
  switch (provider) {
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
    case "qwen":
      return "Qwen";
    case "claude":
      return "Claude";
    default:
      return provider || "Unknown";
  }
}

function sessionProviderTone(provider?: string): "neutral" | "info" | "accent" | "success" {
  switch (provider) {
    case "codex":
      return "info";
    case "gemini":
      return "accent";
    case "qwen":
      return "success";
    case "claude":
      return "accent";
    default:
      return "neutral";
  }
}

interface Props {
  sessions: DispatchedSession[];
  departments: Department[];
  agents: Agent[];
  onAssign: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
}

export function SessionPanel({ sessions, departments, agents, onAssign }: Props) {
  const active = [...sessions.filter((s) => s.status !== "disconnected")].sort(compareSessions);
  const disconnected = [...sessions.filter((s) => s.status === "disconnected")].sort(compareSessions);
  const workingCount = active.filter((s) => s.status === "working").length;
  const staleIdleCount = active.filter((s) => isStaleIdleSession(s)).length;
  const [showDisconnected, setShowDisconnected] = useState(false);
  const [infoSession, setInfoSession] = useState<DispatchedSession | null>(null);
  const { t, language } = useI18n();
  const isKo = language === "ko";

  return (
    <div className="space-y-4 min-w-0">
      <div className="flex flex-wrap items-center gap-2 sm:gap-3">
        <Monitor className="shrink-0" size={24} style={{ color: "var(--th-accent-primary)" }} />
        <h1 className="text-xl sm:text-2xl font-bold truncate">{t({ ko: "파견 인력", en: "Dispatched Sessions" })}</h1>
        <SurfaceMetaBadge tone="success" className="shrink-0">
          {active.length} {t({ ko: "활성", en: "Active" })}
        </SurfaceMetaBadge>
        <SurfaceMetaBadge tone="info" className="shrink-0">
          {workingCount} {t({ ko: "작업 중", en: "Working" })}
        </SurfaceMetaBadge>
        {staleIdleCount > 0 && (
          <SurfaceMetaBadge tone="warn" className="shrink-0">
            {staleIdleCount} {t({ ko: "stale", en: "stale" })}
          </SurfaceMetaBadge>
        )}
      </div>

      <p className="text-th-text-muted text-sm">
        {t({
          ko: "AgentDesk 세션이 감지되면 파견 인력으로 등록됩니다. 각 세션을 부서에 배치하여 오피스에서 시각화할 수 있습니다.",
          en: "Detected AgentDesk sessions are registered as dispatched staff. Assign each session to a department to visualize them in the office.",
        })}
      </p>

      {active.length === 0 && disconnected.length === 0 && (
        <SurfaceEmptyState className="py-12 text-center">
          <Monitor size={48} className="mx-auto mb-4 opacity-30" />
          <p>{t({ ko: "현재 활성 세션이 없습니다", en: "No active sessions" })}</p>
          <p className="mt-1 text-sm">{t({ ko: "AgentDesk 세션이 실행되면 자동으로 표시됩니다", en: "Sessions will appear automatically when AgentDesk starts" })}</p>
        </SurfaceEmptyState>
      )}

      {/* Active sessions */}
      {active.length > 0 && (
        <div className="space-y-3">
          {active.map((s) => (
            <SessionCard
              key={s.id}
              session={s}
              departments={departments}
              agents={agents}
              onAssign={onAssign}
              onSelect={() => setInfoSession(s)}
            />
          ))}
        </div>
      )}

      {/* Disconnected sessions */}
      {disconnected.length > 0 && (
        <>
          <div className="flex items-center justify-between gap-3">
            <h2 className="text-sm font-semibold text-th-text-muted flex items-center gap-2">
              <WifiOff size={14} />
              {t({ ko: "종료된 세션", en: "Disconnected" })} ({disconnected.length})
            </h2>
            <SurfaceActionButton
              onClick={() => setShowDisconnected((prev) => !prev)}
              tone="neutral"
              compact
              className="shrink-0"
            >
              {showDisconnected
                ? t({ ko: "숨기기", en: "Hide" })
                : t({ ko: "표시", en: "Show" })}
            </SurfaceActionButton>
          </div>
          {showDisconnected && (
            <div className="space-y-2 opacity-60">
              {disconnected.slice(0, 10).map((s) => {
                const lastSeenAt = normalizeTimestamp(s.last_seen_at);

                return (
                  <button
                    key={s.id}
                    type="button"
                    className="block w-full text-left"
                    onClick={() => setInfoSession(s)}
                  >
                    <SurfaceListItem
                      className="transition-opacity hover:opacity-100"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                        background: "color-mix(in srgb, var(--th-bg-surface) 84%, transparent)",
                      }}
                      trailing={
                        lastSeenAt ? (
                          <span className="text-[11px] whitespace-nowrap" style={{ color: "var(--th-text-muted)" }}>
                            {formatTimeAgo(lastSeenAt, isKo)}
                          </span>
                        ) : undefined
                      }
                    >
                      <div className="flex items-center gap-3 min-w-0">
                        <div className="w-7 h-7 rounded-lg overflow-hidden bg-th-card-bg shrink-0">
                          <img
                            src={`/sprites/${sessionSpriteNum(s)}-D-1.png`}
                            alt={s.name || ""}
                            className="w-full h-full object-cover"
                            style={{ imageRendering: "pixelated" }}
                          />
                        </div>
                        <div className="min-w-0 flex-1">
                          <TooltipLabel
                            className="min-w-0 text-sm text-th-text-muted"
                            text={sessionDisplayName(s).label}
                            tooltip={sessionDisplayName(s).full}
                          />
                          <div className="mt-1 flex flex-wrap gap-2">
                            <SurfaceMetaBadge>{s.model || "unknown"}</SurfaceMetaBadge>
                            <SurfaceMetaBadge>{t({ ko: "연결 종료", en: "Disconnected" })}</SurfaceMetaBadge>
                          </div>
                        </div>
                      </div>
                    </SurfaceListItem>
                  </button>
                );
              })}
            </div>
          )}
        </>
      )}

      {infoSession && (
        <SessionInfoCard
          session={infoSession}
          departments={departments}
          agents={agents}
          onClose={() => setInfoSession(null)}
        />
      )}
    </div>
  );
}

function SessionCard({
  session: s,
  departments,
  agents,
  onAssign,
  onSelect,
}: {
  session: DispatchedSession;
  departments: Department[];
  agents: Agent[];
  onAssign: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
  onSelect: () => void;
}) {
  const [assigning, setAssigning] = useState(false);
  const [selectedDept, setSelectedDept] = useState(s.department_id || "");
  const { t, language } = useI18n();
  const isKo = language === "ko";
  const connectedAt = normalizeTimestamp(s.connected_at);
  const staleIdle = isStaleIdleSession(s);
  const linkedAgent = linkedAgentLabel(s, agents);
  const statusTone = s.status === "working" ? "success" : staleIdle ? "warn" : "info";
  const statusLabel = s.status === "working"
    ? t({ ko: "작업 중", en: "Working" })
    : staleIdle
      ? t({ ko: "오래된 대기", en: "Stale Idle" })
      : t({ ko: "대기", en: "Idle" });
  const assignDisabled = assigning || selectedDept === (s.department_id || "");

  const handleAssign = async () => {
    setAssigning(true);
    try {
      await onAssign(s.id, {
        department_id: selectedDept || null,
      } as Partial<DispatchedSession>);
    } finally {
      setAssigning(false);
    }
  };

  const statusColor = s.status === "working" ? "bg-emerald-500" : staleIdle ? "bg-slate-500" : "bg-amber-500";

  return (
    <SurfaceCard
      className="rounded-2xl p-3 sm:p-4"
      style={{
        borderColor: staleIdle
          ? "color-mix(in srgb, var(--th-accent-warn) 24%, var(--th-border) 76%)"
          : s.status === "working"
            ? "color-mix(in srgb, var(--th-accent-primary) 20%, var(--th-border) 80%)"
            : "color-mix(in srgb, var(--th-border) 68%, transparent)",
        background: staleIdle
          ? "linear-gradient(180deg, color-mix(in srgb, var(--th-badge-amber-bg) 28%, var(--th-card-bg) 72%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)"
          : s.status === "working"
            ? "linear-gradient(180deg, color-mix(in srgb, var(--th-badge-emerald-bg) 24%, var(--th-card-bg) 76%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)"
            : "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
    >
      <div className="flex items-start gap-3">
        {/* Avatar + status */}
        <button type="button" className="relative shrink-0" onClick={onSelect}>
          <div className="w-10 h-10 rounded-xl overflow-hidden bg-th-card-bg">
            <img
              src={`/sprites/${sessionSpriteNum(s)}-D-1.png`}
              alt={s.name || ""}
              className="w-full h-full object-cover"
              style={{ imageRendering: "pixelated" }}
            />
          </div>
          <span
            className={`absolute -bottom-0.5 -right-0.5 w-3 h-3 rounded-full border-2 border-th-card-border ${statusColor}`}
          />
        </button>

        {/* Info */}
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 min-w-0">
            <TooltipLabel
              className="font-medium transition-opacity hover:opacity-80"
              text={sessionDisplayName(s).label}
              tooltip={sessionDisplayName(s).full}
              onClick={onSelect}
            />
            <Wifi size={14} className="text-emerald-400 shrink-0" />
            <SurfaceMetaBadge tone={statusTone} className="shrink-0">
              {statusLabel}
            </SurfaceMetaBadge>
          </div>

          <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-th-text-muted">
            {s.model && (
              <SurfaceMetaBadge className="shrink-0">
                {s.model}
              </SurfaceMetaBadge>
            )}
            <SurfaceMetaBadge tone={sessionProviderTone(s.provider)} className="shrink-0">
              {sessionProviderLabel(s.provider)}
            </SurfaceMetaBadge>
            {linkedAgent && (
              <SurfaceMetaBadge tone="info" className="shrink-0">
                {t({ ko: "연결", en: "Linked" })}: {linkedAgent}
              </SurfaceMetaBadge>
            )}
            {staleIdle && (
              <SurfaceMetaBadge tone="warn" className="shrink-0">
                {t({ ko: "7일+ stale", en: "7d+ stale" })}
              </SurfaceMetaBadge>
            )}
            {s.stats_xp > 0 && (
              <SurfaceMetaBadge tone="warn" className="shrink-0">
                ⭐ {s.stats_xp} XP
              </SurfaceMetaBadge>
            )}
          </div>

          {s.session_info && (
            <p className="mt-2 truncate text-xs leading-relaxed text-th-text-muted" title={s.session_info}>
              {s.session_info}
            </p>
          )}

          {connectedAt && (
            <div className="flex items-center gap-1 text-xs text-th-text-muted mt-1">
              <Clock size={10} className="shrink-0" />
              <span className="whitespace-nowrap">{t({ ko: "접속", en: "Connected" })}: {formatTimeAgo(connectedAt, isKo)}</span>
            </div>
          )}
        </div>
      </div>

      {/* Department assignment (mobile-safe row) */}
      <div className="mt-3 flex items-center gap-2 flex-wrap pl-0 sm:pl-11">
        <MapPin size={14} className="text-th-text-muted shrink-0" />
        <select
          value={selectedDept}
          onChange={(e) => setSelectedDept(e.target.value)}
          className="min-w-[148px] flex-1 rounded-xl border px-3 py-2 text-sm text-th-text-primary"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
          }}
        >
          <option value="">{t({ ko: "부서 미배정", en: "Dept Unassigned" })}</option>
          {departments.map((d) => (
            <option key={d.id} value={d.id}>
              {d.icon} {d.name_ko || d.name}
            </option>
          ))}
        </select>
        <SurfaceActionButton
          onClick={handleAssign}
          disabled={assignDisabled}
          className="shrink-0"
        >
          {assigning ? "..." : t({ ko: "배치", en: "Assign" })}
        </SurfaceActionButton>
      </div>

      {/* Current department badge */}
      {s.department_id && s.department_name_ko && (
        <div className="mt-2 sm:ml-11">
          <SurfaceMetaBadge
            className="text-white"
            style={{ backgroundColor: s.department_color || "var(--th-accent-primary)" }}
          >
            {t({ ko: `${s.department_name_ko}에 배치됨`, en: `Assigned to ${s.department_name_ko}` })}
          </SurfaceMetaBadge>
        </div>
      )}
      {!s.department_id && (
        <div className="mt-2 sm:ml-11">
          <SurfaceMetaBadge>
            {t({ ko: "부서 미배정", en: "Dept Unassigned" })}
          </SurfaceMetaBadge>
        </div>
      )}
    </SurfaceCard>
  );
}

function formatTimeAgo(ts: unknown, isKo = true): string {
  const normalized = normalizeTimestamp(ts);
  if (normalized == null) return isKo ? "알 수 없음" : "Unknown";

  const diff = Math.max(0, Date.now() - normalized);
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return isKo ? `${sec}초 전` : `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return isKo ? `${min}분 전` : `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return isKo ? `${hr}시간 전` : `${hr}h ago`;
  const days = Math.floor(hr / 24);
  return isKo ? `${days}일 전` : `${days}d ago`;
}

function formatDuration(ms: number, isKo = true): string {
  const sec = Math.floor(ms / 1000);
  if (sec < 60) return isKo ? `${sec}초` : `${sec}s`;
  const min = Math.floor(sec / 60);
  const hr = Math.floor(min / 60);
  if (hr > 0) return isKo ? `${hr}시간 ${min % 60}분` : `${hr}h ${min % 60}m`;
  return isKo ? `${min}분` : `${min}m`;
}

function SessionInfoCard({
  session: s,
  departments,
  agents,
  onClose,
}: {
  session: DispatchedSession;
  departments: Department[];
  agents: Agent[];
  onClose: () => void;
}) {
  const overlayRef = useRef<HTMLDivElement>(null);
  const spriteNum = sessionSpriteNum(s);
  const dept = departments.find((d) => d.id === s.department_id);
  const tier = getRankTier(s.stats_xp);
  const isDisconnected = s.status === "disconnected";
  const connectedAt = normalizeTimestamp(s.connected_at);
  const lastSeenAt = normalizeTimestamp(s.last_seen_at);
  const uptime = connectedAt ? Math.max(0, Date.now() - connectedAt) : 0;
  const staleIdle = isStaleIdleSession(s);
  const linkedAgent = linkedAgentLabel(s, agents);
  const { t, language } = useI18n();
  const isKo = language === "ko";

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const statusLabel: Record<string, string> = {
    working: t({ ko: "작업 중", en: "Working" }),
    idle: t({ ko: "대기", en: "Idle" }),
    disconnected: t({ ko: "연결 종료", en: "Disconnected" }),
  };
  const statusTone = isDisconnected ? "neutral" : s.status === "working" ? "success" : staleIdle ? "warn" : "info";

  return (
    <div
      ref={overlayRef}
      className="fixed inset-0 z-50 flex items-center justify-center p-4"
      style={{ background: "rgba(0,0,0,0.6)" }}
      onClick={(e) => {
        if (e.target === overlayRef.current) onClose();
      }}
    >
      <div className="w-full max-w-lg" role="dialog" aria-modal="true" aria-label="Session details">
        <SurfaceCard
          className="overflow-hidden rounded-[28px] p-0 shadow-2xl"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 76%, transparent)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)",
          }}
        >
          <div className="max-h-[min(78vh,720px)] overflow-y-auto">
            <div className="flex items-start gap-4 border-b px-5 py-5 sm:px-6" style={{ borderColor: "var(--th-border-subtle)" }}>
              <div className="relative shrink-0">
                <div className="h-14 w-14 overflow-hidden rounded-2xl bg-th-card-bg">
                  <img
                    src={`/sprites/${spriteNum}-D-1.png`}
                    alt={s.name || ""}
                    className="h-full w-full object-cover"
                    style={{ imageRendering: "pixelated" }}
                  />
                </div>
                <span
                  className={`absolute -bottom-0.5 -right-0.5 h-3.5 w-3.5 rounded-full border-2 ${
                    isDisconnected ? "bg-gray-500" : s.status === "working" ? "bg-emerald-500" : "bg-amber-500"
                  }`}
                  style={{ borderColor: "var(--th-card-bg)" }}
                />
              </div>

              <div className="min-w-0 flex-1">
                <TooltipLabel
                  className="text-base font-bold text-th-text-primary"
                  text={sessionDisplayName(s).label}
                  tooltip={sessionDisplayName(s).full}
                />
                <div className="mt-2 flex flex-wrap items-center gap-2">
                  <SurfaceMetaBadge tone={statusTone}>{statusLabel[s.status] ?? s.status}</SurfaceMetaBadge>
                  {dept ? (
                    <SurfaceMetaBadge className="text-white" style={{ backgroundColor: s.department_color || "var(--th-accent-primary)" }}>
                      {s.department_name_ko || dept.name}
                    </SurfaceMetaBadge>
                  ) : (
                    <SurfaceMetaBadge>{t({ ko: "부서 미배정", en: "Dept Unassigned" })}</SurfaceMetaBadge>
                  )}
                  {staleIdle && <SurfaceMetaBadge tone="warn">{t({ ko: "7일+ stale", en: "7d+ stale" })}</SurfaceMetaBadge>}
                  {s.provider && (
                    <SurfaceMetaBadge tone={sessionProviderTone(s.provider)}>
                      {sessionProviderLabel(s.provider)}
                    </SurfaceMetaBadge>
                  )}
                </div>
              </div>

              <SurfaceActionButton tone="neutral" compact onClick={onClose} className="shrink-0">
                {t({ ko: "닫기", en: "Close" })}
              </SurfaceActionButton>
            </div>

            <div className="space-y-4 px-5 py-4 sm:px-6">
              <div
                className="rounded-2xl border px-4 py-4"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                  background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                }}
              >
                <div className="space-y-3">
                  {s.model && <InfoRow label={t({ ko: "모델", en: "Model" })} value={s.model} />}
                  {s.session_info && <InfoRow label={t({ ko: "최근 도구", en: "Recent Tool" })} value={s.session_info} />}
                  {linkedAgent && <InfoRow label={t({ ko: "연결 에이전트", en: "Linked Agent" })} value={linkedAgent} />}
                  <InfoRow label={t({ ko: "세션 키", en: "Session Key" })} value={s.session_key} mono />
                  {connectedAt && (
                    <InfoRow
                      label={t({ ko: "접속 시각", en: "Connected At" })}
                      value={new Date(connectedAt).toLocaleString(isKo ? "ko-KR" : "en-US")}
                    />
                  )}
                  {connectedAt && !isDisconnected && (
                    <InfoRow label={t({ ko: "가동 시간", en: "Uptime" })} value={formatDuration(uptime, isKo)} />
                  )}
                  {lastSeenAt && (
                    <InfoRow label={t({ ko: "마지막 신호", en: "Last Seen" })} value={formatTimeAgo(lastSeenAt, isKo)} />
                  )}
                </div>
              </div>

              <div className="grid gap-3 sm:grid-cols-2">
                <SurfaceCard
                  className="rounded-2xl p-4"
                  style={{
                    borderColor: `color-mix(in srgb, ${tier.color} 22%, var(--th-border) 78%)`,
                    background: `color-mix(in srgb, ${tier.color} 10%, var(--th-card-bg) 90%)`,
                  }}
                >
                  <div className="text-[10px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                    {t({ ko: "랭크", en: "Rank" })}
                  </div>
                  <div className="mt-2 flex flex-wrap items-center gap-2">
                    <SurfaceMetaBadge
                      className="font-medium"
                      style={{
                        borderColor: `color-mix(in srgb, ${tier.color} 28%, var(--th-border) 72%)`,
                        background: `color-mix(in srgb, ${tier.color} 16%, var(--th-card-bg) 84%)`,
                        color: tier.color,
                      }}
                    >
                      {tier.name}
                    </SurfaceMetaBadge>
                    <span className="text-sm font-medium text-th-text-primary">XP {s.stats_xp}</span>
                  </div>
                </SurfaceCard>

                <SurfaceCard
                  className="rounded-2xl p-4"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                    background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                  }}
                >
                  <div className="text-[10px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                    {t({ ko: "세션 식별", en: "Session Identity" })}
                  </div>
                  <div className="mt-2 space-y-2">
                    <div className="text-sm font-medium text-th-text-primary">{sessionProviderLabel(s.provider)}</div>
                    <div className="text-xs font-mono text-th-text-muted">
                      ID: {String(s.id).slice(0, 8)}
                    </div>
                  </div>
                </SurfaceCard>
              </div>
            </div>
          </div>

          <div className="flex justify-end border-t px-5 py-3 sm:px-6" style={{ borderColor: "var(--th-border-subtle)" }}>
            <SurfaceActionButton onClick={onClose} tone="neutral">
              {t({ ko: "닫기", en: "Close" })}
            </SurfaceActionButton>
          </div>
        </SurfaceCard>
      </div>
    </div>
  );
}

function InfoRow({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="grid gap-1 sm:grid-cols-[6.75rem_minmax(0,1fr)] sm:gap-3">
      <span className="pt-0.5 text-[10px] font-semibold uppercase tracking-[0.18em] text-th-text-muted">
        {label}
      </span>
      <span
        className={`text-sm leading-6 text-th-text-primary ${mono ? "break-all font-mono text-[11px]" : "break-words"}`}
      >
        {value}
      </span>
    </div>
  );
}
