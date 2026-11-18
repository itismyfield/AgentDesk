import { useId, useLayoutEffect, useRef, useState } from "react";
import { formatElapsedCompact } from "../../agent-insights";
import { localeName } from "../../i18n";
import type { Agent, Department } from "../../types";
import type { DiscordBinding } from "../../api/client";
import { Badge } from "../../kit/free/badge-clean";
import { Select } from "../../kit/free/select-clean";
import type { Translator } from "./types";
import { inferBindingSource } from "./AgentInfoCardModel";
import { AgentDetailSection, SectionWell } from "./AgentInfoCardSections";

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
  providerOptions: string[];
  providerLabels?: Record<string, string>;
  providerSelectDisabled?: boolean;
  providerNotice?: string | null;
  savingProvider: boolean;
  onSaveProvider: (nextProvider: string) => void;
  selectedAuthProfile: string;
  authProfileOptions: string[];
  savingAuthProfile: boolean;
  onSaveAuthProfile: (profileId: string) => void;
  currentWorkSummary: string | null;
  currentWorkElapsedMs: number | null;
  currentWorkDetails: string[];
  discordBindings: DiscordBinding[];
  warnings: Array<{ code: string; severity: "info" | "warning" | "error"; ko: string; en: string }>;
}

const WARNING_TONE = { error: "danger", warning: "warning", info: "info" } as const;

/** Long free text: four lines, then a "show all" toggle (always offered past 140 characters). */
function ClampedText({ text, tr }: { text: string; tr: Translator }) {
  const [expanded, setExpanded] = useState(false);
  const [clipped, setClipped] = useState(false);
  const body = useRef<HTMLParagraphElement>(null);
  const id = useId();
  useLayoutEffect(() => {
    const node = body.current;
    if (!node || expanded) return undefined;
    const measure = () => setClipped(node.scrollHeight - node.clientHeight > 1);
    measure();
    if (typeof ResizeObserver === "undefined") return undefined;
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, [text, expanded]);
  const toggle = text.length > 140 || clipped || expanded;
  return (
    <SectionWell className="flex flex-col items-start gap-1.5">
      <p
        ref={body}
        id={id}
        className={`m-0 whitespace-pre-wrap text-[13px] leading-5 text-(--fg-dim) ${expanded ? "" : "line-clamp-4"}`}
      >
        {text}
      </p>
      {toggle ? (
        <button
          type="button"
          aria-expanded={expanded}
          aria-controls={id}
          onClick={() => setExpanded((value) => !value)}
          className="-ml-2 inline-flex min-h-11 items-center rounded-(--r-sm) px-2 text-xs text-(--fg-muted) hover:bg-(--bg-2) hover:text-(--fg) focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent) sm:min-h-7"
        >
          {expanded ? tr("접기", "Show less") : tr("전체 보기", "Show all")}
        </button>
      ) : null}
    </SectionWell>
  );
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
  providerOptions,
  providerLabels,
  providerSelectDisabled,
  providerNotice,
  savingProvider,
  onSaveProvider,
  selectedAuthProfile,
  authProfileOptions,
  savingAuthProfile,
  onSaveAuthProfile,
  currentWorkSummary,
  currentWorkElapsedMs,
  currentWorkDetails,
  discordBindings,
  warnings,
}: AgentInfoProfileSectionsProps) {
  const dbBindings = discordBindings.filter((binding) => inferBindingSource(binding) !== "role-map");
  const savingLabel = tr("저장 중...", "Saving...");

  return (
    <>
      <AgentDetailSection title={tr("상태 요약", "Status Summary")}>
        <SectionWell>
          <div className="mb-1 text-xs text-(--fg-muted)">{tr("현재 작업", "Current Work")}</div>
          <p className="m-0 text-[13px] leading-5 text-(--fg-dim)">
            {currentWorkSummary || tr("현재 작업 설명이 없습니다", "No current work detail")}
          </p>
        </SectionWell>
        <div className="flex flex-wrap items-center gap-2">
          {currentWorkElapsedMs != null && (
            <Badge tone="info" size="md">
              {tr("경과", "Elapsed")} {formatElapsedCompact(currentWorkElapsedMs, isKo)}
            </Badge>
          )}
          <Badge tone="neutral" size="md">
            {tr("DB 경로", "DB routes")} <span className="tabular-nums">{dbBindings.length}</span>
          </Badge>
        </div>
        {currentWorkDetails.length > 0 && (
          <ul className="m-0 flex list-disc flex-col gap-1 pl-5 text-xs text-(--fg-dim)">
            {currentWorkDetails.map((line, index) => (
              <li key={`${line}:${index}`}>{line}</li>
            ))}
          </ul>
        )}
      </AgentDetailSection>

      {warnings.length > 0 && (
        <AgentDetailSection title={tr("이상 징후", "Warnings")}>
          <div className="flex flex-wrap gap-1.5">
            {warnings.map((warning) => (
              <Badge key={warning.code} tone={WARNING_TONE[warning.severity]} size="md" dot>
                {isKo ? warning.ko : warning.en}
              </Badge>
            ))}
          </div>
        </AgentDetailSection>
      )}

      {agent.personality && (
        <AgentDetailSection title={tr("성격", "Personality")}>
          <ClampedText key={agent.id} text={agent.personality} tr={tr} />
        </AgentDetailSection>
      )}

      {agent.session_info && (
        <AgentDetailSection title={tr("현재 작업", "Current Session")}>
          <p className="m-0 text-[13px] leading-5 text-(--fg-dim)">{agent.session_info}</p>
        </AgentDetailSection>
      )}

      <AgentDetailSection title={tr("실행 설정", "Run settings")}>
        <div className="grid min-w-0 gap-3">
          <Select
            label={tr("소속 부서", "Department")}
            value={selectedDeptId}
            onChange={(value) => onSaveDepartment(value)}
            disabled={savingDept}
            hint={savingDept ? savingLabel : undefined}
            options={[
              { value: "", label: tr("— 미배정 —", "— Unassigned —") },
              ...departments.map((department) => ({
                value: department.id,
                label: [department.icon, localeName(locale, department)].filter(Boolean).join(" "),
              })),
            ]}
          />
          <Select
            label={tr("Provider 계정", "Provider Account")}
            value={selectedAuthProfile}
            onChange={(value) => onSaveAuthProfile(value)}
            disabled={savingAuthProfile}
            hint={savingAuthProfile ? savingLabel : undefined}
            options={authProfileOptions.map((profileId) => ({
              value: profileId,
              label:
                profileId === "__primary__"
                  ? tr("Provider 기본 계정", "Provider primary account")
                  : profileId === "default"
                    ? tr("시스템 기본", "System default")
                    : profileId,
            }))}
          />
          <Select
            label={tr("메인 Provider", "Main Provider")}
            value={selectedProvider}
            onChange={(value) => onSaveProvider(value)}
            disabled={savingProvider || providerSelectDisabled}
            hint={savingProvider ? savingLabel : providerNotice ?? undefined}
            options={providerOptions.map((provider) => ({
              value: provider,
              label: providerLabels?.[provider] ?? provider,
            }))}
          />
        </div>
      </AgentDetailSection>
    </>
  );
}
