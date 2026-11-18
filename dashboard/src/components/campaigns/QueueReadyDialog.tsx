import { useState } from "react";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import type { Agent, KanbanCard, UiLanguage } from "../../types";
import { Modal } from "../common/overlay/Modal";
import { Select } from "../common/Select";
import type { Tr } from "./campaignPresentation";
import { cardTitle } from "../agent-manager/kanban-utils";

/** Choose who takes an unassigned card before it moves to ready. The agent list stays a native select. */
export default function QueueReadyDialog({ card, agents, locale, tr, onCancel, onConfirm }: {
  card: KanbanCard; agents: Agent[]; locale: UiLanguage; tr: Tr; onCancel: () => void; onConfirm: (agentId: string) => void;
}) {
  const [agentId, setAgentId] = useState("");
  return <Modal open onClose={onCancel} title={tr("담당을 정하고 준비됨으로", "Assign, then move to ready")} size="sm"
    description={tr("준비됨 카드는 담당 에이전트가 이어받습니다.", "A ready card is picked up by its assigned agent.")}>
    <div className="grid gap-4">
      <p className="text-sm text-th-text-secondary [overflow-wrap:anywhere]">{card.github_issue_number ? `#${card.github_issue_number} ` : ""}{cardTitle(card.title)}</p>
      <label className="grid gap-1 text-xs text-th-text-muted">{tr("담당 에이전트", "Assigned agent")}
        <Select value={agentId} onChange={(event) => setAgentId(event.target.value)}>
          <option value="">{tr("선택하세요", "Choose an agent")}</option>
          {agents.map((agent) => <option key={agent.id} value={agent.id}>{localeName(locale, agent)}</option>)}
        </Select>
      </label>
      <div className="flex flex-wrap justify-end gap-2">
        <Button variant="secondary" onClick={onCancel}>{tr("취소", "Cancel")}</Button>
        <Button variant="primary" disabled={!agentId} onClick={() => onConfirm(agentId)}>{tr("준비됨으로 넘기기", "Move to ready")}</Button>
      </div>
    </div>
  </Modal>;
}
