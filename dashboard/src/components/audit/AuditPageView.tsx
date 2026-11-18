import { useId } from "react";
import { useSearchParams } from "react-router";
import { Anchor, FileText } from "lucide-react";
import { PageFrame } from "../../app/PageFrame";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";

type AuditTab = "prompts" | "anchors";

/** Audit route: tabs for system prompts and Memento anchors. Both stay empty until the server API (#8249) lands. */
export default function AuditPageView({ isKo }: { isKo: boolean }) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const [searchParams, setSearchParams] = useSearchParams();
  const tab: AuditTab = searchParams.get("tab") === "anchors" ? "anchors" : "prompts";
  const panelId = useId();
  const tabs = [
    {
      value: "prompts" as const,
      label: tr("시스템 프롬프트", "System prompts"),
      text: tr("에이전트가 받은 시스템 프롬프트를 출처별로 보여 줄 거예요.", "This will show the system prompt each agent received, split by source."),
      icon: <FileText aria-hidden />,
    },
    {
      value: "anchors" as const,
      label: tr("메멘토 앵커", "Memento anchors"),
      text: tr("세션에 실린 메멘토 앵커와 출처를 보여 줄 거예요.", "This will show the Memento anchors loaded into each session and where they came from."),
      icon: <Anchor aria-hidden />,
    },
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
            onChange={(next) => setSearchParams(next === "prompts" ? {} : { tab: next }, { replace: true })}
            aria-label={tr("감사 대상", "Audit target")}
            options={tabs.map(({ value, label }) => ({ value, label, controls: panelId }))}
          />
        </div>
      }
    >
      <section id={panelId} role="tabpanel" aria-label={active.label} data-testid={`audit-panel-${active.value}`}>
        <EmptyState icon={active.icon} title={tr("서버 API 준비 중(#8249)", "Server API in progress (#8249)")} text={active.text} />
      </section>
    </PageFrame>
  );
}
