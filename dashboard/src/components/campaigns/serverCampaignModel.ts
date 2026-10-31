import type {
  ServerCampaignCommand, ServerCampaignDefinition, ServerCampaignEvent, ServerCampaignView,
} from "../../api/serverCampaigns";
import type { Tr } from "./campaignPresentation";

type View = ServerCampaignView;
type NodeView = View["nodes"][number];
type Runtime = NodeView["runtime"];
type Reason = View["reasons"][number]["code"];
export type CommandName = View["commands"][number]["command"];
type Label = [string, string];

// Each table maps one server enum value to words; none of them infers a state from other fields.
export const DISPLAY_LABELS = {
  stopping: ["중단 중", "Stopping"],
  stopped: ["중단됨", "Stopped"],
  prepared: ["준비됨", "Prepared"],
  succeeded: ["성공 완료", "Succeeded"],
  running: ["실행 중", "Running"],
  waiting: ["대기 중", "Waiting"],
  unable_to_progress: ["진행 불가", "Unable to progress"],
  needs_inspection: ["상태 확인 필요", "Needs inspection"],
} satisfies Record<View["display"], Label>;
const STOP_MODES = {
  immediate: ["즉시", "immediately"],
  finish_stage: ["마무리", "finish current stage"],
} satisfies Record<NonNullable<View["stop_mode"]>, Label>;
export const DISPOSITION_LABELS = {
  pending: ["시작 전", "Not started"],
  assigned: ["배정 중", "Assigning"],
  running: ["실행 중", "Running"],
  verifying: ["결과 확인 중", "Checking results"],
  failed: ["실패", "Failed"],
  blocked: ["차단", "Blocked"],
  interrupted: ["사용자 중단", "Stopped by user"],
  complete: ["완료", "Complete"],
} satisfies Record<Runtime["disposition"], Label>;
export const STAGE_LABELS = {
  requirements: ["요구사항", "Requirements"],
  design: ["설계", "Design"],
  design_review: ["설계 리뷰", "Design review"],
  implementation: ["구현", "Implementation"],
  review: ["리뷰", "Review"],
  landing: ["착지", "Landing"],
  main_verification: ["main 확인", "Main verification"],
  final_verification: ["최종 확인", "Final verification"],
  complete: ["완료", "Complete"],
} satisfies Record<Runtime["stage"], Label>;
export const REASON_LABELS = {
  execution_unavailable: ["실행 기능 미제공", "execution not available yet"],
  review_resources: ["리뷰 세션 자원 대기", "waiting for review session resources"],
  host_resources: ["호스트 자원 대기", "waiting for host resources"],
  owned_elsewhere: ["다른 경로가 소유", "owned by another path"],
  predecessor_main: ["선행 main 반영 대기", "waiting for predecessors in main"],
  predecessor_failed: ["선행 실패", "a predecessor failed"],
  observation_unknown: ["관측 불명", "observation unknown"],
  state_needs_inspection: ["상태 확인 필요", "state needs inspection"],
} satisfies Record<Reason, Label>;
export const COMMAND_LABELS = {
  edit_definition: ["정의 편집", "Edit definition"],
  start: ["시작", "Start"],
  stop_immediate: ["즉시 중단", "Stop now"],
  stop_finish_stage: ["현재 단계 마무리 후 중단", "Stop after current stage"],
  resume: ["재개", "Resume"],
  prepare_retry: ["다시 시도 준비", "Prepare retry"],
} satisfies Record<CommandName, Label>;
export const REJECTION_LABELS: Record<string, Label> = {
  invalid_definition: ["정의가 올바르지 않습니다", "The definition is not valid"],
  stale_definition: ["정의가 다른 곳에서 바뀌었습니다", "The definition changed elsewhere"],
  stale_run: ["대상 실행이 바뀌었습니다", "The target run changed"],
  stale_binding: ["대상 시도가 바뀌었습니다", "The target attempt changed"],
  invalid_state: ["지금 상태에서는 할 수 없습니다", "Not allowed in the current state"],
  execution_unavailable: ["실행 기능이 아직 없습니다", "Execution is not available yet"],
  sessions_not_gone: ["살아 있는 세션이 남아 있습니다", "Live sessions remain"],
  unreconciled_actions: ["확인되지 않은 외부 동작이 남아 있습니다", "Unreconciled actions remain"],
  fence_incomplete: ["중단 확정이 끝나지 않았습니다", "The stop is not confirmed yet"],
  stale_fence: ["중단 확정 기준이 바뀌었습니다", "The stop fence changed"],
  idempotency_conflict: ["같은 요청 키를 다른 내용에 썼습니다", "The request key was used for another request"],
  missing_change_reason: ["요구사항이나 수용조건을 바꾸려면 근거 링크와 변경 설명이 필요합니다", "Changing requirements or acceptance needs a reference link and an explanation"],
  completed_node_changed: ["완료한 작업은 바꿀 수 없습니다", "A completed task cannot change"],
  already_exists: ["같은 ID의 캠페인이 이미 있습니다", "A campaign with this ID already exists"],
  disconnected: ["연결이 끊겨 최신 상태를 확인할 수 없습니다", "Disconnected: the latest state is unknown"],
  not_offered: ["서버가 제공하지 않는 명령입니다", "The server does not offer this command"],
};

export function rejectionText(code: string, tr: Tr): string {
  return REJECTION_LABELS[code] ? tr(...REJECTION_LABELS[code]) : code;
}

export function displayText(view: Pick<View, "display" | "stop_mode" | "failed_count" | "blocked_count">, tr: Tr): string {
  const parts = [tr(...DISPLAY_LABELS[view.display])];
  if (view.display === "stopping" && view.stop_mode) parts[0] += ` · ${tr(...STOP_MODES[view.stop_mode])}`;
  if (view.failed_count > 0) parts.push(tr(`실패 ${view.failed_count}`, `${view.failed_count} failed`));
  if (view.blocked_count > 0) parts.push(tr(`차단 ${view.blocked_count}`, `${view.blocked_count} blocked`));
  return parts.join(" · ");
}

/** Accepted, confirmed and finished are read from the server view, never from a command response. */
export function stopPhase(view: View): "accepted" | "confirmed" | "stopped" | null {
  if (view.control === "stopped") return "stopped";
  if (view.control !== "stopping") return null;
  return view.stop_confirmed ? "confirmed" : "accepted";
}

/** Sessions the server could not observe: shown as unknown, never as gone or running. */
export function unknownObservations(view: View) {
  return view.sessions.flatMap(({ identity, observation }) => observation.state === "unknown"
    ? [{ host: identity.gate.host, session: identity.session, reason: observation.reason, lastObservedAt: observation.last_observed_at }]
    : []);
}

/** The node's own wait reason and every campaign reason the server addressed to this node. */
export function nodeReasons({ id, runtime }: NodeView, reasons: View["reasons"]): Reason[] {
  const codes = [runtime.wait_reason, ...reasons.filter((reason) => reason.target === id).map((reason) => reason.code)];
  return [...new Set(codes.filter((code): code is Reason => code !== null))];
}

/** live: the last poll answered; stale: it failed and the shown snapshot is older. */
export type Connection = "live" | "stale";

export function commandAvailability(view: View, command: CommandName, connection: Connection): { enabled: boolean; reason: string | null } {
  const entry = view.commands.find((candidate) => candidate.command === command);
  if (!entry) return { enabled: false, reason: "not_offered" };
  if (entry.rejection) return { enabled: false, reason: entry.rejection };
  // A stale snapshot may no longer allow a start or an edit; a stop is safe to resend under its key.
  if (connection !== "live" && command !== "stop_immediate" && command !== "stop_finish_stage") return { enabled: false, reason: "disconnected" };
  return { enabled: true, reason: null };
}

export interface StopIntent {
  key: string;
  campaignId: string;
  command: Extract<ServerCampaignCommand, { command: "stop" }>;
  /** sending: in flight; unconfirmed: no answer arrived, so the outcome is being checked. */
  status: "sending" | "unconfirmed";
}

/** A stop targets the run and the definition revision only, never an event sequence.
 * Only an unanswered stop with the same body is resent under its key. */
export function stopRequest(previous: StopIntent | null, view: View, mode: "immediate" | "finish_stage", newKey: () => string): StopIntent {
  const command = {
    command: "stop" as const, expected_definition_revision: view.definition_revision, expected_run: view.run_id ?? "", mode,
  };
  const resend = previous?.status === "unconfirmed" && previous.campaignId === view.campaign_id
    && JSON.stringify(previous.command) === JSON.stringify(command);
  return { key: resend ? previous.key : newKey(), campaignId: view.campaign_id, command, status: "sending" };
}

/** Null once the server shows this request's outcome: an event under its key, or a different run.
 * Any other change (another client's stop too) proves nothing, so the stop stays to be checked or resent. */
export function reconcileStop(intent: StopIntent | null, view: View, events: ServerCampaignEvent[] = []): StopIntent | null {
  if (!intent || intent.status !== "unconfirmed" || view.campaign_id !== intent.campaignId) return intent;
  if (events.some((event) => event.command_id === intent.key)) return null;
  return view.run_id !== intent.command.expected_run ? null : intent;
}

export interface CampaignLocation {
  mode: "legacy" | "server";
  campaign: string;
  node: string | null;
}

export function campaignLocationQuery({ mode, campaign, node }: CampaignLocation): string {
  const query = new URLSearchParams({ mode, campaign });
  if (node) query.set("node", node);
  return `?${query}`;
}

export function parseCampaignLocation(search: string): CampaignLocation | null {
  const query = new URLSearchParams(search);
  const mode = query.get("mode");
  const campaign = query.get("campaign");
  if ((mode !== "legacy" && mode !== "server") || !campaign) return null;
  return { mode, campaign, node: query.get("node") || null };
}

/** Feedback carries the link and what the screen showed; no tokens, logs or definitions. */
export function feedbackText(link: string, view: View, tr: Tr): string {
  return [
    link,
    `as_of ${view.as_of}`,
    `definition revision ${view.definition_revision}`,
    `event ${view.last_event_sequence}`,
    `display ${tr(...DISPLAY_LABELS[view.display])}`,
  ].join("\n");
}

/** Fields a newer definition changed against the draft's base, by task and acceptance ID. */
export function changedDefinitionFields(base: ServerCampaignDefinition, latest: ServerCampaignDefinition): string[] {
  return changedDefinitionValues(base, latest).map(({ field }) => field);
}

/** Each changed field with its value in the newer definition; a removed or empty value is "". */
export function changedDefinitionValues(base: ServerCampaignDefinition, latest: ServerCampaignDefinition): Array<{ field: string; value: string }> {
  const changed: Array<{ field: string; value: string }> = (["repository", "title", "goal", "profile_version"] as const)
    .filter((key) => base[key] !== latest[key]).map((key) => ({ field: key, value: latest[key] }));
  const after = new Map(latest.nodes.map((node) => [node.id, node]));
  const acValue = (ac: ServerCampaignDefinition["nodes"][number]["acceptance"][number]) => `${ac.description} (${ac.verification})`;
  for (const node of base.nodes) {
    const next = after.get(node.id);
    if (!next) { changed.push({ field: `${node.id} (removed)`, value: "" }); continue; }
    for (const key of ["title", "issue_url", "dependencies", "group"] as const) {
      if (JSON.stringify(node[key]) !== JSON.stringify(next[key])) {
        const value = next[key];
        changed.push({ field: `${node.id}.${key}`, value: Array.isArray(value) ? value.join(", ") : value ?? "" });
      }
    }
    const acceptance = new Map(next.acceptance.map((ac) => [ac.id, ac]));
    for (const ac of node.acceptance) {
      const nextAc = acceptance.get(ac.id);
      if (!nextAc) changed.push({ field: `${node.id}.acceptance.${ac.id} (removed)`, value: "" });
      else if (JSON.stringify(ac) !== JSON.stringify(nextAc)) changed.push({ field: `${node.id}.acceptance.${ac.id}`, value: acValue(nextAc) });
    }
    for (const ac of next.acceptance) if (!node.acceptance.some((candidate) => candidate.id === ac.id)) changed.push({ field: `${node.id}.acceptance.${ac.id} (added)`, value: acValue(ac) });
  }
  for (const [id, node] of after) if (!base.nodes.some((candidate) => candidate.id === id)) changed.push({ field: `${id} (added)`, value: nodeDefinitionText(node) });
  return changed;
}

/** One task's whole definition as read-only lines: title, link, group, dependencies and every acceptance. */
export function nodeDefinitionText(node: ServerCampaignDefinition["nodes"][number]): string {
  return [
    node.title, `issue_url: ${node.issue_url}`, `group: ${node.group ?? ""}`, `dependencies: ${node.dependencies.join(", ")}`,
    ...node.acceptance.map((ac) => `acceptance.${ac.id}: ${ac.description} (${ac.verification})`),
  ].join("\n");
}

export function newIdempotencyKey(): string {
  return globalThis.crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}
