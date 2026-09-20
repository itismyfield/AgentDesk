import { useCallback, useEffect, useRef, useState } from "react";
import { getCampaigns, type Campaign } from "../../api/campaigns";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue, writeLocalStorageValue } from "../../lib/useLocalStorage";
import { WidgetState } from "../common/WidgetState";
import { NODE_STATUSES, campaignProgress } from "./campaignModel";
import { Badge, type Tr } from "./campaignPresentation";
import CampaignExplorer from "./CampaignExplorer";
import type { CampaignDraft } from "./CampaignNodeDetails";
import "./campaigns.css";

export default function CampaignsPanel({ language }: { language: string }) {
  const tr: Tr = useCallback((ko, en) => language === "ko" ? ko : en, [language]);
  const [campaigns, setCampaigns] = useState<Campaign[]>([]);
  const [drafts, setDrafts] = useState<Record<string, CampaignDraft>>({});
  const onDraftChange = (key: string, draft: CampaignDraft | null, expected?: CampaignDraft) => setDrafts((current) => {
    // A save may finish after this task was reopened and its draft changed.
    if (expected && current[key] !== expected) return current;
    const next = { ...current };
    if (draft) next[key] = draft; else delete next[key];
    return next;
  });
  const [selectedId, setSelectedId] = useState<string | null>(() => readLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, null, { validate: (value): value is string | null => value === null || typeof value === "string" }));
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshedAt, setRefreshedAt] = useState<number | null>(null);
  const mounted = useRef(false);
  const requestId = useRef(0);
  const refresh = useCallback(async () => {
    const id = ++requestId.current;
    setLoading(true);
    try {
      const values = await getCampaigns();
      if (!mounted.current || id !== requestId.current) return;
      setCampaigns((current) => values.map((value) => {
        const previous = current.find((candidate) => candidate.id === value.id);
        return previous && previous.revision > value.revision ? previous : value;
      })); setError(null); setRefreshedAt(Date.now());
      setSelectedId((current) => values.some((value) => value.id === current) ? current : values.find((value) => value.status === "active")?.id ?? values[0]?.id ?? null);
    } catch (cause) {
      if (mounted.current && id === requestId.current) setError(cause instanceof Error ? cause.message : "Unable to load campaigns");
    } finally { if (mounted.current && id === requestId.current) setLoading(false); }
  }, []);
  useEffect(() => {
    mounted.current = true;
    void refresh();
    const interval = window.setInterval(() => { if (document.visibilityState === "visible") void refresh(); }, 15_000);
    return () => { mounted.current = false; requestId.current++; window.clearInterval(interval); };
  }, [refresh]);
  useEffect(() => { writeLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, selectedId); }, [selectedId]);
  const campaign = campaigns.find((value) => value.id === selectedId);
  const progress = campaign ? campaignProgress(campaign.nodes) : null;
  const onSaved = (updated: Campaign) => {
    requestId.current++; // A poll started before this save must not replace its result.
    setLoading(false); setError(null); setRefreshedAt(Date.now());
    setCampaigns((current) => current.map((value) => value.id === updated.id && updated.revision >= value.revision ? updated : value));
  };
  return <section className="campaigns-panel">
    <header className="campaign-row"><div><h2>{tr("캠페인", "Campaigns")}</h2><p>{tr("그룹별 작업을 살펴보고, 선택한 작업의 다음 행동부터 이어가세요.", "Browse tasks by group and pick up the next action.")}</p></div><button disabled={loading} onClick={() => void refresh()}>{loading ? tr("불러오는 중…", "Refreshing…") : tr("새로고침", "Refresh")}</button></header>
    {error && <WidgetState kind={campaigns.length ? "stale" : "error"} title={tr("캠페인을 갱신하지 못했습니다.", "Could not refresh campaigns.")} description={error + (campaigns.length ? tr(" · 이전 내용을 표시 중입니다.", " · Showing the previous snapshot.") : "")} action={<button onClick={() => void refresh()}>{tr("다시 시도", "Retry")}</button>} />}
    {!campaigns.length && !error && <WidgetState kind={loading ? "loading" : "empty"} title={loading ? tr("캠페인을 불러오고 있습니다.", "Loading campaigns.") : tr("등록된 캠페인이 없습니다.", "No campaigns yet.")} description={loading ? undefined : tr("캠페인이 등록되면 작업 흐름과 진행 상황이 여기에 표시됩니다.", "Registered campaigns and their progress will appear here.")} />}
    {campaign && progress && <div className="campaign-main">
      <div className="campaign-row campaign-summary-header">
        <label className="campaign-selector"><span>{tr("캠페인 선택", "Select campaign")}</span><select aria-label={tr("캠페인 선택", "Select campaign")} value={campaign.id} onChange={(event) => setSelectedId(event.target.value)}>{campaigns.map((value) => <option key={value.id} value={value.id}>{value.title}</option>)}</select></label>
        <Badge status={campaign.status} tr={tr} />
        <span className="campaign-summary-round">{tr("라운드", "Round")} {campaign.round}</span>
        <strong>{progress.percent}% <span className="campaign-muted">{progress.counts.completed}/{progress.total}</span></strong>
      </div>
      {campaign.description && <p className="campaign-description">{campaign.description}</p>}
      <progress className="campaign-progress" max={100} value={progress.percent} aria-label={tr("작업 완료율", "Task completion")} />
      <div className="campaign-row"><div className="campaign-counts">{NODE_STATUSES.map((status) => <span key={status}><Badge status={status} tr={tr} /> {progress.counts[status]}</span>)}</div>{refreshedAt && <span className="campaign-freshness">{tr("확인", "Checked")} {new Date(refreshedAt).toLocaleTimeString(language)}</span>}</div>
      {campaign.nodes.length ? <CampaignExplorer key={campaign.id} campaign={campaign} tr={tr} onSaved={onSaved} drafts={drafts} onDraftChange={onDraftChange} /> : <WidgetState kind="empty" title={tr("아직 작업이 없습니다.", "No tasks yet.")} />}
    </div>}
  </section>;
}
