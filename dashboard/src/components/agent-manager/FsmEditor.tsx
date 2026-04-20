import PipelineVisualEditor from "./PipelineVisualEditor";

import type { Agent, UiLanguage } from "../../types";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repo?: string;
  agents: Agent[];
  selectedAgentId?: string | null;
}

export default function FsmEditor(props: Props) {
  return <PipelineVisualEditor {...props} variant="fsm" />;
}
