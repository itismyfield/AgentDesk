import { useId, type ReactNode } from "react";
import { ArrowLeft, X } from "lucide-react";
import type { Agent } from "../../types";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import type { HomeAgentState } from "../home/homeModel";
import type { Translator } from "./types";
import type { AttentionReason } from "./agentsAttention";
import { AgentOverlay } from "./AgentOverlay";
import { AgentInfoCardContent } from "./AgentInfoCardContent";
import type { AgentDetailShared } from "./useAgentDetailData";

interface AgentInfoCardProps {
  agent: Agent;
  state?: HomeAgentState;
  isKo: boolean;
  locale: string;
  tr: Translator;
  onClose: () => void;
  onAgentUpdated?: () => void;
  /** Label of the phone back button; the agents page says "back to list". */
  backLabel?: string;
  actions?: ReactNode;
  /** The page's shared reads and the agent's attention reasons, when the agents page opens it. */
  shared?: AgentDetailShared;
  reasons?: AttentionReason[];
}

/** Agent detail as an overlay: a centred card from 900px, a full-screen page with a back button below it. */
export default function AgentInfoCard({
  agent,
  state,
  isKo,
  locale,
  tr,
  onClose,
  onAgentUpdated,
  backLabel,
  actions,
  shared,
  reasons,
}: AgentInfoCardProps) {
  const isMobile = useIsMobile();
  const titleId = useId();
  const primaryName = localeName(locale, agent);
  const detailLabel = tr("에이전트 상세", "Agent details");

  return (
    <AgentOverlay
      variant="drawer"
      ariaLabel={`${primaryName} — ${detailLabel}`}
      onClose={onClose}
      widthClassName="sm:max-w-2xl"
      testId="agent-detail-overlay"
      header={
        isMobile ? (
          <div className="flex min-h-11 items-center gap-2">
            <Button variant="secondary" size="md" icon={<ArrowLeft size={15} aria-hidden />} onClick={onClose} data-autofocus>
              {backLabel ?? tr("돌아가기", "Back")}
            </Button>
            <span className="ml-auto truncate text-xs text-(--fg-muted)">{detailLabel}</span>
          </div>
        ) : (
          <div className="flex items-center justify-between gap-3">
            <span className="text-xs font-medium text-(--fg-muted)">{detailLabel}</span>
            <IconButton icon={<X size={15} />} label={tr("닫기", "Close")} onClick={onClose} />
          </div>
        )
      }
    >
      <AgentInfoCardContent
        key={agent.id}
        agent={agent}
        state={state}
        isKo={isKo}
        locale={locale}
        tr={tr}
        onAgentUpdated={onAgentUpdated}
        actions={actions}
        titleId={titleId}
        shared={shared}
        reasons={reasons}
      />
    </AgentOverlay>
  );
}
