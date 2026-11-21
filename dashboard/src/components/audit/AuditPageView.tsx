import { useCallback, useId } from "react";
import { Anchor } from "lucide-react";
import { PageFrame } from "../../app/PageFrame";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import type { Agent } from "../../types";
import { AuditWorkspace } from "./AuditWorkspace";
import { PromptAuditDetail, usePromptCaptions } from "./PromptAuditPanel";
import { useAuditSelection, type AuditTab } from "./useAuditSelection";

interface AuditPageViewProps {
  isKo: boolean;
  agents: Agent[];
  isMobileViewport: boolean;
}

/** Audit route: an agent's system prompt by source, and (next) its Memento anchors. Both tabs share the agent and provider URL keys. */
export default function AuditPageView({ isKo, agents, isMobileViewport }: AuditPageViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const panelId = useId();
  const selection = useAuditSelection(agents, isMobileViewport);
  const { tab, agent, provider } = selection;
  const caption = usePromptCaptions(selection.list, isKo, tr);
  const tabs = [
    { value: "prompts" as const, label: tr("시스템 프롬프트", "System prompts") },
    { value: "anchors" as const, label: tr("메멘토 앵커", "Memento anchors") },
  ];
  const active = tabs.find((item) => item.value === tab) ?? tabs[0];

  return (
    <PageFrame
      testId="audit-page"
      title={tr("감사", "Audit")}
      description={tr("에이전트가 받은 시스템 프롬프트와 메멘토 앵커를 봐요.", "Review the system prompts and Memento anchors agents received.")}
      actions={
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
      }
    >
      <section id={panelId} role="tabpanel" aria-label={active.label} data-testid={`audit-panel-${active.value}`}>
        {tab === "prompts" ? (
          <AuditWorkspace selection={selection} phone={isMobileViewport} isKo={isKo} tr={tr} caption={caption}>
            {agent && provider ? (
              <PromptAuditDetail
                key={`${agent.id}:${provider}`}
                agent={agent}
                provider={provider}
                providers={selection.providers}
                onProvider={selection.selectProvider}
                onBack={isMobileViewport ? selection.back : undefined}
                phone={isMobileViewport}
                isKo={isKo}
                tr={tr}
              />
            ) : null}
          </AuditWorkspace>
        ) : (
          <EmptyState
            icon={<Anchor aria-hidden />}
            title={tr("앵커 화면은 준비 중이에요", "The anchor view is on its way")}
            text={tr("세션에 실린 메멘토 앵커와 출처를 보여 줄 거예요.", "This will show the Memento anchors loaded into each session and where they came from.")}
          />
        )}
      </section>
    </PageFrame>
  );
}
