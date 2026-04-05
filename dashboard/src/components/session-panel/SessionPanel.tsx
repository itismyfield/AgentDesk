import { useState, useRef, useEffect } from "react";
import type { Agent, Department, DispatchedSession } from "../../types";
import { Monitor, MapPin, Clock, Wifi, WifiOff } from "lucide-react";
import { getRankTier } from "../dashboard/model";
import { useI18n } from "../../i18n";
import TooltipLabel from "../common/TooltipLabel";

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

interface Props {
  sessions: DispatchedSession[];
  departments: Department[];
  agents: Agent[];
  onAssign: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
}

export function SessionPanel({ sessions, departments, agents, onAssign }: Props) {
  const active = sessions.filter((s) => s.status !== "disconnected");
  const disconnected = sessions.filter((s) => s.status === "disconnected");
  const [infoSession, setInfoSession] = useState<DispatchedSession | null>(null);
  const { t, language } = useI18n();
  const isKo = language === "ko";

  return (
    <div className="space-y-4 min-w-0">
      <div className="flex items-center gap-3">
        <Monitor className="text-indigo-400 shrink-0" size={24} />
        <h1 className="text-xl sm:text-2xl font-bold truncate">{t({ ko: "파견 인력", en: "Dispatched Sessions" })}</h1>
        <span className="bg-emerald-600 text-white text-xs px-2 py-0.5 rounded-full shrink-0">
          {active.length} {t({ ko: "활성", en: "Active" })}
        </span>
      </div>

      <p className="text-th-text-muted text-sm">
        {t({
          ko: "AgentDesk 세션이 감지되면 파견 인력으로 등록됩니다. 각 세션을 부서에 배치하여 오피스에서 시각화할 수 있습니다.",
          en: "Detected AgentDesk sessions are registered as dispatched staff. Assign each session to a department to visualize them in the office.",
        })}
      </p>

      {active.length === 0 && disconnected.length === 0 && (
        <div className="text-center py-12 text-th-text-muted">
          <Monitor size={48} className="mx-auto mb-4 opacity-30" />
          <p>{t({ ko: "현재 활성 세션이 없습니다", en: "No active sessions" })}</p>
          <p className="text-sm mt-1">{t({ ko: "AgentDesk 세션이 실행되면 자동으로 표시됩니다", en: "Sessions will appear automatically when AgentDesk starts" })}</p>
        </div>
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
          <h2 className="text-sm font-semibold text-th-text-muted mb-3 flex items-center gap-2">
            <WifiOff size={14} />
            {t({ ko: "종료된 세션", en: "Disconnected" })} ({disconnected.length})
          </h2>
          <div className="space-y-2 opacity-60">
            {disconnected.slice(0, 10).map((s) => (
              <div
                key={s.id}
                className="bg-th-bg-surface/50 rounded-lg px-3 sm:px-4 py-3 flex items-center gap-2 sm:gap-3 cursor-pointer hover:bg-surface-hover/70 transition-colors min-w-0"
                onClick={() => setInfoSession(s)}
              >
                <div className="w-7 h-7 rounded-lg overflow-hidden bg-th-card-bg shrink-0">
                  <img
                    src={`/sprites/${sessionSpriteNum(s)}-D-1.png`}
                    alt={s.name || ""}
                    className="w-full h-full object-cover"
                    style={{ imageRendering: "pixelated" }}
                  />
                </div>
                <TooltipLabel
                  className="flex-1 text-sm text-th-text-muted min-w-0"
                  text={sessionDisplayName(s).label}
                  tooltip={sessionDisplayName(s).full}
                />
                <span className="text-xs text-th-text-muted shrink-0">
                  {s.model || "unknown"}
                </span>
                {s.last_seen_at && (
                  <span className="text-xs text-th-text-muted shrink-0 whitespace-nowrap">
                    {formatTimeAgo(s.last_seen_at, isKo)}
                  </span>
                )}
              </div>
            ))}
          </div>
        </>
      )}

      {infoSession && (
        <SessionInfoCard session={infoSession} departments={departments} onClose={() => setInfoSession(null)} />
      )}
    </div>
  );
}

function SessionCard({
  session: s,
  departments,
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

  const statusColor = s.status === "working" ? "bg-emerald-500" : "bg-amber-500";

  return (
    <div className="bg-th-bg-surface rounded-lg p-3 sm:p-4 border border-th-border">
      <div className="flex items-start gap-3">
        {/* Avatar + status */}
        <div className="relative cursor-pointer shrink-0" onClick={onSelect}>
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
        </div>

        {/* Info */}
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 min-w-0">
            <span className="cursor-pointer hover:text-indigo-400 transition-colors min-w-0" onClick={onSelect}>
              <TooltipLabel
                className="font-medium"
                text={sessionDisplayName(s).label}
                tooltip={sessionDisplayName(s).full}
              />
            </span>
            <Wifi size={14} className="text-emerald-400 shrink-0" />
          </div>

          <div className="flex flex-wrap items-center gap-2 text-xs text-th-text-muted mt-1">
            {s.model && (
              <span className="bg-th-card-bg px-1.5 py-0.5 rounded shrink-0">
                {s.model}
              </span>
            )}
            <span
              className="px-1.5 py-0.5 rounded shrink-0"
              style={{
                background: s.provider === "codex" ? "var(--th-badge-sky-bg)"
                  : s.provider === "gemini" ? "var(--th-badge-blue-bg)"
                  : s.provider === "qwen" ? "var(--th-badge-emerald-bg)"
                  : "var(--th-badge-violet-bg)",
                color: s.provider === "codex" ? "var(--th-badge-sky-text)"
                  : s.provider === "gemini" ? "var(--th-badge-blue-text)"
                  : s.provider === "qwen" ? "var(--th-badge-emerald-text)"
                  : "var(--th-badge-violet-text)",
              }}
            >
              {s.provider === "codex" ? "Codex" : s.provider === "gemini" ? "Gemini" : s.provider === "qwen" ? "Qwen" : "Claude"}
            </span>
            {s.stats_xp > 0 && (
              <span className="px-1.5 py-0.5 rounded shrink-0" style={{ background: "var(--th-badge-amber-bg)", color: "var(--th-badge-amber-text)" }}>
                ⭐ {s.stats_xp} XP
              </span>
            )}
            {s.session_info && (
              <span className="truncate max-w-full sm:max-w-[300px]" title={s.session_info}>{s.session_info}</span>
            )}
          </div>

          {s.connected_at && (
            <div className="flex items-center gap-1 text-xs text-th-text-muted mt-1">
              <Clock size={10} className="shrink-0" />
              <span className="whitespace-nowrap">{t({ ko: "접속", en: "Connected" })}: {formatTimeAgo(s.connected_at, isKo)}</span>
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
          className="bg-th-card-bg text-sm rounded px-2 py-1 border border-th-border text-th-text-primary flex-1 min-w-[120px]"
        >
          <option value="">{t({ ko: "미배정", en: "Unassigned" })}</option>
          {departments.map((d) => (
            <option key={d.id} value={d.id}>
              {d.icon} {d.name_ko || d.name}
            </option>
          ))}
        </select>
        <button
          onClick={handleAssign}
          disabled={assigning || selectedDept === (s.department_id || "")}
          className="bg-indigo-600 hover:bg-indigo-500 disabled:opacity-40 text-white text-xs px-3 py-1.5 rounded transition-colors shrink-0"
        >
          {assigning ? "..." : t({ ko: "배치", en: "Assign" })}
        </button>
      </div>

      {/* Current department badge */}
      {s.department_id && s.department_name_ko && (
        <div className="mt-2 sm:ml-11">
          <span
            className="text-xs px-2 py-0.5 rounded-full text-white"
            style={{ backgroundColor: s.department_color || "#6366f1" }}
          >
            {t({ ko: `${s.department_name_ko}에 배치됨`, en: `Assigned to ${s.department_name_ko}` })}
          </span>
        </div>
      )}
    </div>
  );
}

function formatTimeAgo(ts: number, isKo = true): string {
  const diff = Date.now() - ts;
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
  onClose,
}: {
  session: DispatchedSession;
  departments: Department[];
  onClose: () => void;
}) {
  const overlayRef = useRef<HTMLDivElement>(null);
  const spriteNum = sessionSpriteNum(s);
  const dept = departments.find((d) => d.id === s.department_id);
  const tier = getRankTier(s.stats_xp);
  const isDisconnected = s.status === "disconnected";
  const uptime = s.connected_at ? Date.now() - s.connected_at : 0;
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

  return (
    <div
      ref={overlayRef}
      className="fixed inset-0 z-50 flex items-center justify-center p-4"
      style={{ background: "rgba(0,0,0,0.6)" }}
      onClick={(e) => {
        if (e.target === overlayRef.current) onClose();
      }}
    >
      <div className="w-full max-w-md rounded-2xl bg-th-bg-primary border border-th-border shadow-2xl overflow-hidden" role="dialog" aria-modal="true" aria-label="Session details">
        {/* Header */}
        <div className="flex items-center gap-4 p-5 border-b border-th-border">
          <div className="relative shrink-0">
            <div className="w-14 h-14 rounded-xl overflow-hidden bg-th-card-bg">
              <img
                src={`/sprites/${spriteNum}-D-1.png`}
                alt={s.name || ""}
                className="w-full h-full object-cover"
                style={{ imageRendering: "pixelated" }}
              />
            </div>
            <span
              className={`absolute -bottom-0.5 -right-0.5 w-3.5 h-3.5 rounded-full border-2 border-th-bg-primary ${
                isDisconnected ? "bg-gray-500" : s.status === "working" ? "bg-emerald-500" : "bg-amber-500"
              }`}
            />
          </div>
          <div className="flex-1 min-w-0">
            <TooltipLabel
              className="font-bold text-base text-th-text-primary"
              text={sessionDisplayName(s).label}
              tooltip={sessionDisplayName(s).full}
            />
            <div className="flex items-center gap-2 mt-1.5 flex-wrap">
              <span
                className="text-xs px-2 py-0.5 rounded-full font-medium"
                style={{
                  background: isDisconnected ? "rgba(100,116,139,0.15)" :
                    s.status === "working" ? "rgba(16,185,129,0.15)" : "rgba(245,158,11,0.15)",
                  color: isDisconnected ? "#94a3b8" :
                    s.status === "working" ? "#34d399" : "#fbbf24",
                }}
              >
                {statusLabel[s.status] ?? s.status}
              </span>
              {dept && (
                <span
                  className="text-xs px-2 py-0.5 rounded-full text-white"
                  style={{ backgroundColor: s.department_color || "#6366f1" }}
                >
                  {s.department_name_ko || dept.name}
                </span>
              )}
              {!dept && (
                <span className="text-xs px-2 py-0.5 rounded-full bg-th-bg-surface text-th-text-muted">
                  {t({ ko: "미배정", en: "Unassigned" })}
                </span>
              )}
            </div>
          </div>
          <button
            onClick={onClose}
            className="w-11 h-11 rounded-lg flex items-center justify-center hover:bg-surface-hover transition-colors self-start text-th-text-muted"
            aria-label="Close"
          >
            ✕
          </button>
        </div>

        {/* Details */}
        <div className="px-5 py-3 space-y-2.5 border-b border-th-border">
          {s.model && (
            <InfoRow label={t({ ko: "모델", en: "Model" })} value={s.model} />
          )}
          {s.session_info && (
            <InfoRow label={t({ ko: "최근 도구", en: "Recent Tool" })} value={s.session_info} />
          )}
          <InfoRow label={t({ ko: "세션 키", en: "Session Key" })} value={s.session_key} mono />
          {s.connected_at > 0 && (
            <InfoRow label={t({ ko: "접속 시각", en: "Connected At" })} value={new Date(s.connected_at).toLocaleString(isKo ? "ko-KR" : "en-US")} />
          )}
          {s.connected_at > 0 && !isDisconnected && (
            <InfoRow label={t({ ko: "가동 시간", en: "Uptime" })} value={formatDuration(uptime, isKo)} />
          )}
          {s.last_seen_at && (
            <InfoRow label={t({ ko: "마지막 신호", en: "Last Seen" })} value={formatTimeAgo(s.last_seen_at, isKo)} />
          )}
        </div>

        {/* Stats */}
        <div className="px-5 py-3 flex items-center justify-between border-b border-th-border">
          <div className="flex items-center gap-3">
            <span
              className="text-xs px-2 py-0.5 rounded font-medium"
              style={{ background: `${tier.color}20`, color: tier.color }}
            >
              {tier.name}
            </span>
            <span className="text-xs text-th-text-muted">
              XP {s.stats_xp}
            </span>
          </div>
          <span className="text-xs font-mono text-th-text-muted">
            ID: {String(s.id).slice(0, 8)}
          </span>
        </div>

        {/* Footer */}
        <div className="flex justify-end px-5 py-3">
          <button
            onClick={onClose}
            className="px-3 py-1.5 rounded-lg text-xs font-medium border border-th-border text-th-text-muted hover:bg-surface-hover transition-colors"
          >
            {t({ ko: "닫기", en: "Close" })}
          </button>
        </div>
      </div>
    </div>
  );
}

function InfoRow({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-start gap-3">
      <span className="text-xs font-semibold uppercase tracking-widest text-th-text-muted w-20 shrink-0 pt-0.5">
        {label}
      </span>
      <span
        className={`text-xs text-th-text-primary break-all ${mono ? "font-mono" : ""}`}
      >
        {value}
      </span>
    </div>
  );
}
