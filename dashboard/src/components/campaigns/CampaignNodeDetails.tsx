import { ChevronDown } from "lucide-react";
import { useState } from "react";
import { updateCampaignNode, type Campaign, type CampaignNode, type CampaignNodeLive } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import { Select } from "../common/Select";
import { WidgetState } from "../common/WidgetState";
import { NODE_STATUSES, campaignTaskTitle, safeCampaignLink } from "./campaignModel";
import { legacyTask } from "./campaignOverviewModel";
import { Badge, LABELS, LiveChip, cardStatusLabel, dispatchLabel, queueLabel, sessionLabel, type Tr } from "./campaignPresentation";
import { RecentList, STATUS_CARD, WARN_CARD, useScreenTitle } from "./TaskScreen";

export interface CampaignDraft {
  campaign: Campaign;
  node: CampaignNode;
}
export default function CampaignNodeDetails({
  campaign,
  node,
  live,
  tr,
  onSaved,
  editing,
  onDraftChange,
}: {
  campaign: Campaign;
  node: CampaignNode;
  live?: CampaignNodeLive;
  tr: Tr;
  onSaved: (campaign: Campaign) => void;
  editing: CampaignDraft | null;
  onDraftChange: (draft: CampaignDraft | null, expected?: CampaignDraft) => void;
}) {
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const draft = editing?.node;
  const shown = legacyTask(node, live); // the status and blocker the overview shows
  const inScreen = useScreenTitle(campaignTaskTitle(node));
  const pr = safeCampaignLink(node.pr_url);
  const prNumber = pr && new URL(pr).pathname.match(/\/pull\/(\d+)/)?.[1];
  const patch = (changes: Partial<CampaignNode>) => {
    if (editing) onDraftChange({ ...editing, node: { ...editing.node, ...changes } });
  };
  const save = async () => {
    if (!editing || saving) return;
    setSaving(true);
    setError(null);
    try {
      const updated = await updateCampaignNode(editing.campaign, editing.node);
      onSaved(updated);
      onDraftChange(null, editing);
    } catch (cause) {
      setError(
        cause instanceof ApiRequestError && cause.status === 409
          ? tr(
              "다른 곳에서 이 캠페인을 수정했습니다. 취소 후 새로고침하여 최신 내용을 확인해 주세요.",
              "This campaign changed elsewhere. Cancel and refresh before editing again.",
            )
          : cause instanceof Error
            ? cause.message
            : tr("저장하지 못했습니다.", "Could not save changes."),
      );
    } finally {
      setSaving(false);
    }
  };
  return (
    <section className="campaign-node-detail" aria-label={tr("작업 상세", "Task details")}>
      {!inScreen && <div className="campaign-row"><h3>{node.title}</h3></div>}
      {draft ? (
        <form
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <p className="campaign-muted">
            {tr("다른 작업으로 이동하거나 상세를 닫아도 이 초안은 유지됩니다.", "This draft stays available when you switch tasks or close details.")}
          </p>
          <div className="campaign-edit-grid grid grid-cols-1 gap-3 text-sm sm:grid-cols-2">
            <label>
              {tr("상태", "Status")}
              <Select
                value={draft.status}
                onChange={(event) =>
                  patch({
                    status: event.target.value as CampaignNode["status"],
                  })
                }
              >
                {NODE_STATUSES.map((status) => (
                  <option key={status} value={status}>
                    {tr(...LABELS[status])}
                  </option>
                ))}
              </Select>
            </label>
            <label>
              {tr("단계", "Stage")}
              <input required value={draft.stage} onChange={(event) => patch({ stage: event.target.value })} />
            </label>
            <label>
              {tr("그룹", "Group")}
              <input
                maxLength={128}
                value={draft.group ?? ""}
                placeholder={tr("미분류", "Ungrouped")}
                onChange={(event) => patch({ group: event.target.value || null })}
              />
            </label>
            <label>
              {tr("라운드", "Round")}
              <input type="number" min={1} step={1} required value={draft.round} onChange={(event) => patch({ round: Number(event.target.value) })} />
            </label>
            <label>
              {tr("담당", "Assignee")}
              <input value={draft.assignee ?? ""} onChange={(event) => patch({ assignee: event.target.value || null })} />
            </label>
            <label>
              {tr("담당 세션", "Session")}
              <input value={draft.session_id ?? ""} onChange={(event) => patch({ session_id: event.target.value || null })} />
            </label>
            <label>
              {tr("프로바이더", "Provider")}
              <input value={draft.provider ?? ""} onChange={(event) => patch({ provider: event.target.value || null })} />
            </label>
          </div>
          <label>
            {tr("선행 작업 (여러 개 선택 가능)", "Dependencies (multiple selection)")}
            <select
              multiple
              size={Math.min(5, Math.max(2, campaign.nodes.length - 1))}
              value={draft.dependencies}
              onChange={(event) =>
                patch({
                  dependencies: Array.from(event.target.selectedOptions, (option) => option.value),
                })
              }
            >
              {campaign.nodes
                .filter((candidate) => candidate.id !== node.id)
                .map((candidate) => (
                  <option key={candidate.id} value={candidate.id}>
                    {candidate.title}
                  </option>
                ))}
            </select>
          </label>
          <label>
            {tr("다음 행동", "Next action")}
            <textarea rows={3} value={draft.next_action ?? ""} onChange={(event) => patch({ next_action: event.target.value || null })} />
          </label>
          <label>
            {tr("막힌 이유", "Blocker")}
            <textarea rows={2} value={draft.blocker ?? ""} onChange={(event) => patch({ blocker: event.target.value || null })} />
          </label>
          <label>
            {tr("한 줄 요지", "One-line gist")}
            <input value={draft.summary ?? ""} onChange={(event) => patch({ summary: event.target.value || null })} />
          </label>
          <label>
            {tr("기대효과", "Expected benefit")}
            <input value={draft.benefit ?? ""} onChange={(event) => patch({ benefit: event.target.value || null })} />
          </label>
          {error && <WidgetState kind="error" title={error} compact />}
          <div className="campaign-actions">
            <button type="submit" disabled={saving}>
              {saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}
            </button>
            <button
              type="button"
              disabled={saving}
              onClick={() => {
                onDraftChange(null);
                setError(null);
              }}
            >
              {tr("취소", "Cancel")}
            </button>
          </div>
        </form>
      ) : (
        <>
          {/* TaskPhone order: a status card, the progress record where server tasks list stages, evidence, then what it is for. */}
          {campaign.nodes_partial && <p role="status" className="campaign-muted">{tr("세부 내용과 검증 기록을 불러오는 중이에요…", "Loading details and verification records…")}</p>}
          <section aria-label={tr("지금 상태", "Now")} className={`${STATUS_CARD} ${shown.attention ? WARN_CARD : ""}`}>
            <span className="flex flex-wrap items-center gap-2"><Badge status={shown.status} tr={tr} /><LiveChip live={live} tr={tr} /></span>
            <p className="campaign-description text-[var(--fg)]">{node.stage || "—"} · {tr(`라운드 ${node.round}`, `round ${node.round}`)}</p>
            {shown.blocker && <p className={shown.attention ? "text-[var(--warn)]" : ""}>{shown.status === "pending" ? tr("대기 사유", "Waiting on") : tr("막힌 이유", "Blocker")}: {shown.blocker}</p>}
            {node.post_merge_hold && <p className="text-[var(--warn)]">{tr("보류: 머지 뒤 남은 일", "On hold: post-merge work")}: {node.post_merge_hold}</p>}
            {node.next_action && <p className="campaign-next !rounded-none !bg-transparent !p-0 whitespace-pre-wrap">{tr("다음 행동", "Next action")}: {node.next_action}</p>}
          </section>
          {/* A ledger keeps no fixed stages, so this shows only what it recorded. */}
          <section aria-label={tr("진행 기록", "Progress record")} className="grid min-w-0 gap-2">
            <h4 className="!mb-0">{tr("진행 기록", "Progress record")}</h4>
            <dl className="grid text-[13px] [&>div]:flex [&>div]:min-h-8 [&>div]:items-center [&>div]:justify-between [&>div]:gap-3 [&_dt]:shrink-0 [&_dd]:min-w-0 [&_dd]:text-right [&_dd]:text-[var(--fg)]">
              <div>
                <dt>{tr("선행 작업", "Dependencies")}</dt>
                <dd>
                  {node.dependencies.length
                    ? node.dependencies.map((id) => campaign.nodes.find((candidate) => candidate.id === id)?.title || id).join(" · ")
                    : tr("없음", "None")}
                </dd>
              </div>
              {pr && (
                <div>
                  <dt>PR</dt>
                  <dd><a href={pr} target="_blank" rel="noopener noreferrer">{prNumber ? `#${prNumber}` : tr("PR 보기", "View PR")} ↗</a></dd>
                </div>
              )}
              {node.head_sha && (
                <div>
                  <dt>{tr("커밋", "Commit")}</dt>
                  <dd><code title={node.head_sha}>{node.head_sha.slice(0, 7)}</code></dd>
                </div>
              )}
            </dl>
            {!!node.evidence_records?.length && (
              <ol aria-label={tr("단계 기록", "Stage record")}>
                {[...node.evidence_records].sort((a, b) => (Date.parse(a.recorded_at ?? "") || 0) - (Date.parse(b.recorded_at ?? "") || 0)).map((record, index) => (
                  <li key={index}>
                    {record.recorded_at && <time className="campaign-muted" dateTime={record.recorded_at}>{new Date(record.recorded_at).toLocaleString()} · </time>}
                    <strong>{record.summary}</strong>
                    {record.result && <p>{record.result}</p>}
                    {record.command && <code>{record.command}</code>}
                    {record.head_sha && <p title={record.head_sha}>Commit: {record.head_sha.slice(0, 7)}</p>}
                    {record.references.map((reference, refIndex) => (
                      <p key={refIndex}>{reference}</p>
                    ))}
                  </li>
                ))}
              </ol>
            )}
          </section>
          {node.evidence.length > 0 && <RecentList label={tr("검증 근거", "Evidence")} items={node.evidence} tr={tr} />}
          {(node.summary || node.benefit) && (
            <p className="campaign-description">
              {node.summary}
              {node.summary && node.benefit && "\n"}
              {node.benefit && `${tr("기대효과", "Benefit")}: ${node.benefit}`}
            </p>
          )}
          {!!node.acceptance?.length && (
            <div>
              <h4>{tr("완료 조건", "Acceptance criteria")}</h4>
              <ul>
                {node.acceptance.map((value, index) => (
                  <li key={index}>{value}</li>
                ))}
              </ul>
            </div>
          )}
          {node.details && (
            <details className="group min-w-0">
              <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 text-xs text-[var(--fg-muted)] [&::-webkit-details-marker]:hidden">
                <ChevronDown aria-hidden className="size-3.5 -rotate-90 transition-transform group-open:rotate-0 motion-reduce:transition-none" />{tr("작업 설명", "Task notes")}
              </summary>
              <p className="campaign-description">{node.details}</p>
            </details>
          )}
          {!!node.findings?.length && (
            <div>
              <h4>{tr("발견 사항", "Findings")}</h4>
              <ul>
                {node.findings.map((value, index) => (
                  <li key={index}>{value}</li>
                ))}
              </ul>
            </div>
          )}
          {safeCampaignLink(node.issue_url) && (
            <div className="campaign-actions">
              <a href={safeCampaignLink(node.issue_url)} target="_blank" rel="noopener noreferrer">
                {tr("이슈 보기", "View issue")} ↗
              </a>
            </div>
          )}
          {live && (
            <div className="campaign-live-detail">
              <h4>{tr("실제 실행 상태", "Live execution")}</h4>
              <dl className="campaign-detail-grid grid grid-cols-1 gap-4 text-sm sm:grid-cols-2 [&_dt]:mb-1 [&_dt]:text-xs [&_dd]:[overflow-wrap:anywhere]">
                <div>
                  <dt>{tr("이슈 카드 단계", "Issue card step")}</dt>
                  <dd>{cardStatusLabel(live.card_status, tr)}</dd>
                </div>
                <div>
                  <dt>{live.running ? tr("진행 중 디스패치", "Running dispatch") : tr("최근 디스패치", "Latest dispatch")}</dt>
                  <dd>{dispatchLabel(live, tr) ?? tr("없음", "None")}</dd>
                </div>
                {(live.running || live.dispatch_status) && (
                  <div>
                    <dt>{tr("세션", "Session")}</dt>
                    <dd>{sessionLabel(live, tr)}</dd>
                  </div>
                )}
                <div>
                  <dt>{tr("자동 큐", "Auto-queue")}</dt>
                  <dd>{queueLabel(live, tr) ?? tr("큐에 없음", "Not queued")}</dd>
                </div>
              </dl>
            </div>
          )}
          <dl className="campaign-detail-grid grid grid-cols-1 gap-4 text-sm sm:grid-cols-2 [&_dt]:mb-1 [&_dt]:text-xs [&_dd]:[overflow-wrap:anywhere]">
            <div>
              <dt>{tr("그룹", "Group")}</dt>
              <dd>{node.group || tr("미분류", "Ungrouped")}</dd>
            </div>
            <div>
              <dt>{tr("담당", "Assignee")}</dt>
              <dd>{node.assignee || tr("미배정", "Unassigned")}</dd>
            </div>
            <div>
              <dt>{tr("담당 세션", "Session")}</dt>
              <dd>{node.session_id || tr("연결 없음", "Not linked")}</dd>
            </div>
            <div>
              <dt>{tr("프로바이더", "Provider")}</dt>
              <dd>{node.provider || "—"}</dd>
            </div>
            <div>
              <dt>{tr("작업 기록 시각", "Task updated")}</dt>
              <dd>{node.updated_at ? new Date(node.updated_at).toLocaleString() : "—"}</dd>
            </div>
          </dl>
          <div className="campaign-actions">
            <button onClick={() => onDraftChange({ campaign, node: { ...node } })}>{tr("작업 수정", "Edit task")}</button>
          </div>
        </>
      )}
    </section>
  );
}
