import { useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { AgentWarning } from "../../agent-insights";
import { localeName } from "../../i18n";
import { AgentMonitor } from "../../kit/pro/agent-monitor";
import type { Agent, Department } from "../../types";
import { Select } from "../../kit/free/select-clean";
import AgentPersona from "../AgentPersona";
import { Chip } from "../common/KitPrimitives";
import { monitorLabels } from "../home/HomeSummaryCards";
import { agentStateLabel, type HomeAgentState, type MonitorView } from "../home/homeModel";
import type { AttentionReason } from "./agentsAttention";
import type { Translator } from "./types";
import { AgentDetailSection, SectionWell, type DetailFold } from "./AgentInfoCardSections";
import { AgentStatusBadge } from "./AgentRow";

const WARNING_TONE = { error: "err", warning: "warn", info: "info" } as const;

/** Long free text: four lines, then a "show all" toggle (always offered past 140 characters). */
export function ClampedText({ text, tr }: { text: string; tr: Translator }) {
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

/** "Now": the state in home's words, what the agent does, what waits on it, and on desktop a running turn as the kit
 * monitor whose log button opens the recent turns. */
export function AgentNowSection({ agent, state, tr, isKo, summary, monitor, onOpenLog, reasons, warnings }: {
  agent: Agent;
  state?: HomeAgentState;
  tr: Translator;
  isKo: boolean;
  summary: string | null;
  monitor: MonitorView | null;
  onOpenLog: () => void;
  reasons: AttentionReason[];
  warnings: AgentWarning[];
}) {
  const labels = useMemo(() => monitorLabels(tr), [tr]);
  const source = agent.status === "working" && agent.activity_source === "agentdesk" ? tr("AgentDesk 작업", "AgentDesk") : null;
  const term = "text-(--fg-muted)";
  return (
    <AgentDetailSection title={tr("지금", "Now")} testId="agent-detail-now">
      <dl className="m-0 grid min-w-0 grid-cols-[5.5rem_minmax(0,1fr)] items-start gap-x-3 gap-y-1.5 text-[13px] leading-5">
        <dt className={term}>{tr("상태", "Status")}</dt>
        <dd className="m-0 flex min-w-0 flex-wrap items-center gap-1.5">
          <AgentStatusBadge status={agent.status} state={state} tr={tr} />
          {source ? <span className="text-xs text-(--fg-muted)">{source}</span> : null}
        </dd>
        {!monitor && summary ? (
          <>
            <dt className={term}>{tr("하는 일", "Doing")}</dt>
            <dd className="m-0 min-w-0 text-(--fg-dim) [overflow-wrap:anywhere]">{summary}</dd>
          </>
        ) : null}
        {reasons.length > 0 ? (
          <>
            <dt className={term}>{tr("걸린 일", "Waiting")}</dt>
            <dd className="m-0 flex min-w-0 flex-col gap-1">
              {reasons.map((reason) => (
                <span key={reason.key} className="flex min-w-0 items-center gap-1.5">
                  <Chip tone={reason.tone}>{reason.chip}</Chip>
                  <span className="min-w-0 truncate text-xs text-(--fg-muted)" title={reason.detail}>{reason.detail}</span>
                </span>
              ))}
            </dd>
          </>
        ) : null}
      </dl>
      {monitor ? (
        <AgentMonitor
          status="running"
          agent={{ name: agentStateLabel("working", tr), role: monitor.role ?? undefined, mark: <AgentPersona agent={agent} state="working" size={28} /> }}
          current={{ label: monitor.label, kind: monitor.kind }}
          started={monitor.started ?? undefined}
          onOpenLog={onOpenLog}
          labels={labels}
          style={{ width: "100%" }}
        />
      ) : null}
      <div className="flex flex-wrap items-center gap-1.5">
        <Chip className="tabular-nums">{tr(`완료 ${agent.stats_tasks_done.toLocaleString()}건`, `${agent.stats_tasks_done.toLocaleString()} done`)}</Chip>
        {warnings.map((warning) => (
          <Chip key={warning.code} tone={WARNING_TONE[warning.severity]} dot>{isKo ? warning.ko : warning.en}</Chip>
        ))}
      </div>
    </AgentDetailSection>
  );
}

/** Run settings: department, main Provider and account save as soon as they change; the device below has its own save. */
export function AgentRunSettingsSection({
  tr,
  locale,
  fold,
  summary,
  departments,
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
  children,
}: {
  tr: Translator;
  locale: string;
  fold: DetailFold;
  summary: string;
  departments: Department[];
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
  children: ReactNode;
}) {
  const savingLabel = tr("저장 중...", "Saving...");
  return (
    <AgentDetailSection title={tr("실행 설정", "Run settings")} summary={fold.foldable ? summary : undefined} fold={fold} testId="agent-detail-run">
      <div className="@container min-w-0">
        <div className="grid min-w-0 gap-3 @md:grid-cols-2">
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
        </div>
      </div>
      {children}
    </AgentDetailSection>
  );
}
