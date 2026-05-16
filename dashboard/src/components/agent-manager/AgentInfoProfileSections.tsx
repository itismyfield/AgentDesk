import { formatElapsedCompact } from "../../agent-insights";
import { localeName } from "../../i18n";
import type { Agent, Department } from "../../types";
import type { AgentOfficeMembership, DiscordBinding } from "../../api/client";
import {
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type { Translator } from "./types";
import { inferBindingSource } from "./AgentInfoCardModel";

interface AgentInfoProfileSectionsProps {
  agent: Agent;
  departments: Department[];
  locale: string;
  isKo: boolean;
  tr: Translator;
  selectedDeptId: string;
  savingDept: boolean;
  onSaveDepartment: (nextDeptId: string) => void;
  selectedProvider: string;
  savingProvider: boolean;
  onSaveProvider: (nextProvider: string) => void;
  loadingOffices: boolean;
  officeMemberships: AgentOfficeMembership[];
  savingOfficeIds: Record<string, boolean>;
  onToggleOfficeMembership: (office: AgentOfficeMembership) => void;
  currentWorkSummary: string | null;
  currentWorkElapsedMs: number | null;
  currentWorkDetails: string[];
  discordBindings: DiscordBinding[];
  warnings: Array<{ code: string; severity: "info" | "warning" | "error"; ko: string; en: string }>;
}

export function AgentInfoProfileSections({
  agent,
  departments,
  locale,
  isKo,
  tr,
  selectedDeptId,
  savingDept,
  onSaveDepartment,
  selectedProvider,
  savingProvider,
  onSaveProvider,
  loadingOffices,
  officeMemberships,
  savingOfficeIds,
  onToggleOfficeMembership,
  currentWorkSummary,
  currentWorkElapsedMs,
  currentWorkDetails,
  discordBindings,
  warnings,
}: AgentInfoProfileSectionsProps) {
  const dbBindings = discordBindings.filter((binding) => inferBindingSource(binding) !== "role-map");

  return (
    <>
      <SurfaceSubsection title={tr("소속 부서", "Department")} className="min-w-0">
        <div className="min-w-0 flex flex-col items-stretch gap-2 sm:flex-row sm:items-center">
          <select
            value={selectedDeptId}
            onChange={(e) => onSaveDepartment(e.target.value)}
            disabled={savingDept}
            className="min-w-0 w-full rounded-xl border px-3 py-2 text-sm outline-none sm:flex-1"
            style={{
              background: "var(--th-input-bg)",
              borderColor: "var(--th-input-border)",
              color: "var(--th-text-primary)",
            }}
          >
            <option value="">{tr("— 미배정 —", "— Unassigned —")}</option>
            {departments.map((department) => (
              <option key={department.id} value={department.id}>
                {department.icon} {localeName(locale, department)}
              </option>
            ))}
          </select>
          <span className="self-start text-xs sm:shrink-0" style={{ color: "var(--th-text-muted)" }}>
            {savingDept ? tr("저장 중...", "Saving...") : null}
          </span>
        </div>
      </SurfaceSubsection>

      <SurfaceSubsection title={tr("메인 Provider", "Main Provider")} className="min-w-0">
        <div className="min-w-0 flex flex-col items-stretch gap-2 sm:flex-row sm:items-center">
          <select
            value={selectedProvider}
            onChange={(e) => onSaveProvider(e.target.value)}
            disabled={savingProvider}
            className="min-w-0 w-full rounded-xl border px-3 py-2 text-sm outline-none sm:flex-1"
            style={{
              background: "var(--th-input-bg)",
              borderColor: "var(--th-input-border)",
              color: "var(--th-text-primary)",
            }}
          >
            <option value="claude">Claude</option>
            <option value="codex">Codex</option>
            <option value="gemini">Gemini</option>
            <option value="qwen">Qwen</option>
          </select>
          <span className="self-start text-xs sm:shrink-0" style={{ color: "var(--th-text-muted)" }}>
            {savingProvider ? tr("저장 중...", "Saving...") : null}
          </span>
        </div>
      </SurfaceSubsection>

      <SurfaceSubsection title={tr("소속 오피스", "Offices")} className="min-w-0 md:col-span-2">
        {loadingOffices ? (
          <SurfaceNotice tone="neutral" compact>
            {tr("불러오는 중...", "Loading...")}
          </SurfaceNotice>
        ) : officeMemberships.length === 0 ? (
          <SurfaceEmptyState className="text-xs">
            {tr("등록된 오피스가 없습니다", "No offices")}
          </SurfaceEmptyState>
        ) : (
          <div className="flex flex-wrap gap-2">
            {officeMemberships.map((office) => {
              const assigned = office.assigned;
              const savingOffice = !!savingOfficeIds[office.id];

              return (
                <button
                  key={office.id}
                  onClick={() => onToggleOfficeMembership(office)}
                  disabled={savingOffice}
                  className="rounded-xl px-2.5 py-1.5 text-xs font-medium transition-all disabled:opacity-50"
                  style={
                    assigned
                      ? { background: office.color, color: "#ffffff" }
                      : {
                          background: "var(--th-bg-surface)",
                          color: "var(--th-text-secondary)",
                          border:
                            "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
                        }
                  }
                >
                  {office.icon} {localeName(locale, office)}
                </button>
              );
            })}
          </div>
        )}
      </SurfaceSubsection>

      <SurfaceSubsection title={tr("상태 요약", "Status Summary")} className="min-w-0 md:col-span-2">
        <div className="space-y-3">
          <SurfaceCard className="p-3">
            <div className="mb-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr("현재 작업", "Current Work")}
            </div>
            <div className="text-xs leading-relaxed" style={{ color: "var(--th-text-primary)" }}>
              {currentWorkSummary || tr("현재 작업 설명이 없습니다", "No current work detail")}
            </div>
          </SurfaceCard>
          <div className="flex flex-wrap gap-2">
            {currentWorkElapsedMs != null && (
              <SurfaceMetricPill
                label={tr("경과", "Elapsed")}
                tone="info"
                value={formatElapsedCompact(currentWorkElapsedMs, isKo)}
              />
            )}
            <SurfaceMetricPill label={tr("DB 경로", "DB routes")} tone="accent" value={`${dbBindings.length}`} />
          </div>
          {currentWorkDetails.length > 0 && (
            <div className="space-y-1">
              {currentWorkDetails.map((line, index) => (
                <div
                  key={`${line}:${index}`}
                  className="text-xs"
                  style={{ color: "var(--th-text-secondary)" }}
                >
                  • {line}
                </div>
              ))}
            </div>
          )}
        </div>
      </SurfaceSubsection>

      {warnings.length > 0 && (
        <SurfaceSubsection title={tr("이상 징후", "Warnings")} className="min-w-0 md:col-span-2">
          <div className="flex flex-wrap gap-2">
            {warnings.map((warning) => (
              <span
                key={warning.code}
                className="rounded-lg px-2 py-1 text-xs"
                style={{
                  background:
                    warning.severity === "error"
                      ? "rgba(239,68,68,0.14)"
                      : warning.severity === "warning"
                        ? "rgba(245,158,11,0.14)"
                        : "rgba(96,165,250,0.14)",
                  color:
                    warning.severity === "error"
                      ? "#fca5a5"
                      : warning.severity === "warning"
                        ? "#fcd34d"
                        : "#93c5fd",
                }}
              >
                {isKo ? warning.ko : warning.en}
              </span>
            ))}
          </div>
        </SurfaceSubsection>
      )}

      {agent.personality && (
        <SurfaceSubsection title={tr("성격", "Personality")} className="md:col-span-2">
          <div className="whitespace-pre-wrap text-xs leading-relaxed" style={{ color: "var(--th-text-secondary)" }}>
            {agent.personality}
          </div>
        </SurfaceSubsection>
      )}

      {agent.session_info && (
        <SurfaceSubsection title={tr("현재 작업", "Current Session")} className="md:col-span-2">
          <div className="text-xs leading-relaxed" style={{ color: "var(--th-text-secondary)" }}>
            {agent.session_info}
          </div>
        </SurfaceSubsection>
      )}
    </>
  );
}
