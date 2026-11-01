import type { ServerCampaignView } from "../../api/serverCampaigns";
import type { Tr } from "./campaignPresentation";
import { ActionMenu } from "../common/ActionMenu";
import {
  COMMAND_LABELS, STAGE_LABELS, commandAvailability, rejectionText, stopPhase,
  type CommandName, type Connection, type StopIntent,
} from "./serverCampaignModel";

export type ControlCommand = Exclude<CommandName, "edit_definition">;

const PHASES = {
  accepted: ["중단 요청 접수 · 확정 대기", "Stop accepted · waiting for confirmation"],
  confirmed: ["중단 확정 · 세션 종료와 외부 동작 대사 대기", "Stop confirmed · waiting for sessions to end and actions to reconcile"],
  // What may follow a stop is for the server's command list to say, not this label.
  stopped: ["중단됨", "Stopped"],
} satisfies Record<NonNullable<ReturnType<typeof stopPhase>>, [string, string]>;

/** One server command as a button; a refused or unavailable command says why. */
export function CommandButton({ view, command, connection, busy, onCommand, tr }: {
  view: ServerCampaignView; command: ControlCommand; connection: Connection; busy: boolean; onCommand: () => void; tr: Tr;
}) {
  const { enabled, reason } = commandAvailability(view, command, connection);
  return <span className="campaign-server-command">
    <button type="button" disabled={!enabled || busy} onClick={onCommand}>{tr(...COMMAND_LABELS[command])}</button>
    {reason && <small>{rejectionText(reason, tr)}</small>}
  </span>;
}

export default function ServerCampaignControls({ view, connection, stopIntent, busy, error, onCommand, tr }: {
  view: ServerCampaignView;
  connection: Connection;
  stopIntent: StopIntent | null;
  busy: boolean;
  error: string | null;
  onCommand: (command: ControlCommand) => void;
  tr: Tr;
}) {
  const phase = stopPhase(view);
  const active = view.nodes.filter((node) => ["assigned", "running", "verifying"].includes(node.runtime.disposition));
  const pendingStop = stopIntent?.campaignId === view.campaign_id ? stopIntent : null;
  const stopBusy = busy || pendingStop?.status === "sending";
  const immediate = commandAvailability(view, "stop_immediate", connection);
  return <section className="campaign-server-controls" aria-label={tr("캠페인 제어", "Campaign controls")}>
    <div className="campaign-actions">
      <CommandButton view={view} command="start" connection={connection} busy={busy} onCommand={() => onCommand("start")} tr={tr} />
      <CommandButton view={view} command="resume" connection={connection} busy={busy} onCommand={() => onCommand("resume")} tr={tr} />
    </div>
    <div className="campaign-server-stop grid gap-3 rounded-lg border border-th-border-subtle p-4">
      <h4>{tr("중단", "Stop")}</h4>
      <p className="campaign-muted">
        {tr("진행 단계", "Current stages")}: {active.length ? active.map((node) => `${node.id} ${tr(...STAGE_LABELS[node.runtime.stage])}`).join(", ") : tr("없음", "none")}
        {" · "}{tr(`관련 세션 ${view.sessions.length}개`, `${view.sessions.length} related sessions`)}
        {" · "}{tr(`미대사 외부 동작 ${view.unreconciled_actions.length}개`, `${view.unreconciled_actions.length} unreconciled actions`)}
      </p>
      <div className="campaign-actions">
        <CommandButton view={view} command="stop_finish_stage" connection={connection} busy={stopBusy} onCommand={() => onCommand("stop_finish_stage")} tr={tr} />
        <ActionMenu label={tr("다른 중단 방법", "More ways to stop")} items={[{
          key: "stop_immediate", danger: true, disabled: !immediate.enabled || stopBusy,
          label: immediate.reason ? `${tr(...COMMAND_LABELS.stop_immediate)} · ${rejectionText(immediate.reason, tr)}` : tr(...COMMAND_LABELS.stop_immediate),
          onSelect: () => onCommand("stop_immediate"),
          confirm: {
            title: tr("지금 바로 중단할까요?", "Stop right now?"),
            description: tr("진행 중인 세션을 기다리지 않고 멈춥니다. 하던 단계는 끝나지 않은 채로 남습니다.", "Stops without waiting for running sessions. The current stages stay unfinished."),
            confirmLabel: tr(...COMMAND_LABELS.stop_immediate), cancelLabel: tr("취소", "Cancel"),
          },
        }]} />
      </div>
      {phase && <p role="status">{tr(...PHASES[phase])}</p>}
      {pendingStop?.status === "unconfirmed" && <p role="status" className="campaign-filter-note">
        {tr("요청 결과 확인 중 · 같은 요청을 다시 보내도 한 번만 적용됩니다.", "Checking the request result · resending the same request applies it once.")}
        {" "}<button type="button" disabled={busy} onClick={() => onCommand(pendingStop.command.mode === "immediate" ? "stop_immediate" : "stop_finish_stage")}>{tr("같은 요청 다시 보내기", "Resend the same request")}</button>
      </p>}
    </div>
    {error && <p role="alert" className="campaign-filter-note">{error}</p>}
  </section>;
}
