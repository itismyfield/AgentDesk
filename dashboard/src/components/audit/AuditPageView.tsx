import { useCallback, useId } from "react";
import { PageFrame } from "../../app/PageFrame";
import { useShellFocus } from "../../app/shellFocus";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import type { Agent } from "../../types";
import { AnchorAuditDetail, useAnchorCaptions, useAnchorWarnings } from "./AnchorAuditPanel";
import { AuditWorkspace } from "./AuditWorkspace";
import { PromptAuditDetail, usePromptCaptions } from "./PromptAuditPanel";
import { useAuditSelection, type AuditTab } from "./useAuditSelection";

interface AuditPageViewProps {
  isKo: boolean;
  agents: Agent[];
  isMobileViewport: boolean;
}

/** Audit route: an agent's system prompt by source and its Memento anchors. Both tabs share the agent and provider URL keys. */
export default function AuditPageView({ isKo, agents, isMobileViewport }: AuditPageViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const panelId = useId();
  const selection = useAuditSelection(agents, isMobileViewport);
  const { tab, agent, provider } = selection;
  const promptCaption = usePromptCaptions(selection.list, isKo, tr);
  const anchorCaption = useAnchorCaptions(selection.list, tr);
  const anchorWarning = useAnchorWarnings(selection.list);
  const tabs = [
    { value: "prompts" as const, label: tr("시스템 프롬프트", "System prompts") },
    { value: "anchors" as const, label: tr("메멘토 앵커", "Memento anchors") },
  ];
  const active = tabs.find((item) => item.value === tab) ?? tabs[0];
  // A phone detail leaves the tabs to the list and the prompt's anchor link, and its head takes the app top bar's row.
  const showTabs = !(isMobileViewport && agent);
  useShellFocus(!showTabs, "topBar");
  const description = tab === "anchors"
    ? tr("세션을 시작할 때 에이전트가 받은 기억이에요. 길이·중복·정본과 겹침을 확인해요.", "Memories an agent received at session start, checked for length and overlap.")
    : tr("에이전트가 받은 시스템 프롬프트를 출처별로 나눴어요. Discord /system-prompt와 같은 내용이에요.", "Splits the system prompt an agent received by source, matching Discord /system-prompt.");
  const detail = agent && provider ? {
    agent,
    provider,
    providers: selection.providers,
    onProvider: selection.selectProvider,
    onBack: isMobileViewport ? selection.back : undefined,
    phone: isMobileViewport,
    isKo,
    tr,
  } : null;

  return (
    <PageFrame
      testId="audit-page"
      title={tr("감사", "Audit")}
      description={description}
      actions={showTabs ? (
        // The kit control is an inline-size container, so its slot needs a definite width.
        <div className="w-full sm:w-80" data-testid="audit-tabs">
          <SegmentedControl<AuditTab>
            kind="tabs"
            variant="tray"
            fullWidth
            value={tab}
            onChange={selection.selectTab}
            aria-label={tr("감사 대상", "Audit target")}
            options={tabs.map(({ value, label }) => ({ value, label, controls: panelId }))}
          />
        </div>
      ) : null}
    >
      <section id={panelId} role={showTabs ? "tabpanel" : undefined} aria-label={active.label} data-testid={`audit-panel-${active.value}`}>
        <AuditWorkspace
          selection={selection}
          phone={isMobileViewport}
          isKo={isKo}
          tr={tr}
          caption={tab === "prompts" ? promptCaption : anchorCaption}
          warning={tab === "anchors" ? anchorWarning : undefined}
        >
          {detail && tab === "prompts" ? (
            <PromptAuditDetail key={`${detail.agent.id}:${detail.provider}`} {...detail} onOpenAnchors={() => selection.selectTab("anchors")} />
          ) : null}
          {detail && tab === "anchors" ? <AnchorAuditDetail key={`${detail.agent.id}:${detail.provider}`} {...detail} agents={agents} /> : null}
        </AuditWorkspace>
      </section>
    </PageFrame>
  );
}
