import { lazy, Suspense } from "react";
import type { Translator } from "./types";
import { AgentDetailSection, SectionLoading, type DetailFold } from "./AgentInfoCardSections";

const TurnTranscriptPanel = lazy(() => import("./TurnTranscriptPanel"));

/** Recent turns, read only while open: eight turns weigh about 160KB. */
export function AgentInfoTurnsSection({ agentId, tr, isKo, fold }: { agentId: string; tr: Translator; isKo: boolean; fold: DetailFold }) {
  return (
    <AgentDetailSection title={tr("최근 턴", "Recent turns")} fold={fold} testId="agent-detail-turns">
      <Suspense fallback={<SectionLoading label={tr("불러오는 중...", "Loading...")} />}>
        <TurnTranscriptPanel source={{ type: "agent", id: agentId }} tr={tr} isKo={isKo} embedded />
      </Suspense>
    </AgentDetailSection>
  );
}
