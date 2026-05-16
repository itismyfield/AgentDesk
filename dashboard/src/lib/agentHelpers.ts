import type { Agent } from "../types";

export type AgentLabelTranslator = (ko: string, en: string) => string;

export interface AgentTaskSummary {
  label: string;
  value: string;
}

export function getCurrentTaskSummary(
  agent: Agent,
  tr: AgentLabelTranslator,
): AgentTaskSummary {
  if (agent.current_task_id) {
    return {
      label: tr("현재 작업", "Current Task"),
      value: agent.current_task_id,
    };
  }
  if (agent.workflow_pack_key) {
    return {
      label: tr("워크플로우", "Workflow"),
      value: agent.workflow_pack_key,
    };
  }
  if (agent.session_info) {
    return {
      label: tr("세션", "Session"),
      value: agent.session_info,
    };
  }
  if (agent.personality) {
    return {
      label: tr("메모", "Notes"),
      value: agent.personality,
    };
  }
  return {
    label: tr("상태", "Status"),
    value: tr("대기 중", "Standing by"),
  };
}
