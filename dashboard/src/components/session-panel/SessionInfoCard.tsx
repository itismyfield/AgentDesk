import { useEffect, useRef } from "react";
import type { Agent, Department, DispatchedSession } from "../../types";
import { getRankTier } from "../dashboard/model";
import { useI18n } from "../../i18n";
import { SurfaceActionButton, SurfaceCard, SurfaceMetaBadge } from "../common/SurfacePrimitives";
import TooltipLabel from "../common/TooltipLabel";
import {
  formatDuration,
  formatTimeAgo,
  isStaleIdleSession,
  linkedAgentLabel,
  normalizeTimestamp,
  sessionDisplayName,
  sessionProviderLabel,
  sessionProviderTone,
  sessionSpriteNum,
} from "./SessionPanelModel";

export function SessionInfoCard({
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
