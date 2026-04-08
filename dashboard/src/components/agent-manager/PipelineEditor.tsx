import { useCallback, useEffect, useState } from "react";
import * as api from "../../api";
import type { PipelineStage, Agent, UiLanguage } from "../../types";
import { localeName } from "../../i18n";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repo: string;
  agents: Agent[];
  selectedAgentId?: string | null;
}

const FAILURE_OPTIONS = [
  { value: "fail", ko: "실패 처리", en: "Fail" },
  { value: "retry", ko: "재시도", en: "Retry" },
  { value: "previous", ko: "이전 스테이지", en: "Previous stage" },
  { value: "goto", ko: "지정 스테이지", en: "Go to stage" },
] as const;

interface StageEditor {
  stage_name: string;
  entry_skill: string;
  provider: string;
  agent_override_id: string;
  timeout_minutes: number;
  on_failure: "fail" | "retry" | "previous" | "goto";
  on_failure_target: string;
  max_retries: number;
  skip_condition: string;
  parallel_with: string;
  applies_to_agent_id: string;
  trigger_after: "ready" | "review_pass";
}

function emptyStage(): StageEditor {
  return {
    stage_name: "",
    entry_skill: "",
    provider: "",
    agent_override_id: "",
    timeout_minutes: 60,
    on_failure: "fail",
    on_failure_target: "",
    max_retries: 3,
    skip_condition: "",
    parallel_with: "",
    applies_to_agent_id: "",
    trigger_after: "ready",
  };
}

function stageFromApi(s: PipelineStage): StageEditor {
  return {
    stage_name: s.stage_name,
    entry_skill: s.entry_skill ?? "",
    provider: s.provider ?? "",
    agent_override_id: s.agent_override_id ?? "",
    timeout_minutes: s.timeout_minutes,
    on_failure: s.on_failure,
    on_failure_target: s.on_failure_target ?? "",
    max_retries: s.max_retries,
    skip_condition: s.skip_condition ?? "",
    parallel_with: s.parallel_with ?? "",
    applies_to_agent_id: s.applies_to_agent_id ?? "",
    trigger_after: s.trigger_after ?? "ready",
  };
}

export default function PipelineEditor({ tr, locale, repo, agents, selectedAgentId }: Props) {
  const [stages, setStages] = useState<StageEditor[]>([]);
  const [savedStages, setSavedStages] = useState<PipelineStage[]>([]);
  const [saving, setSaving] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState(false);

  // Keep all stages for save merging, but only show filtered ones
  const [allRepoStages, setAllRepoStages] = useState<PipelineStage[]>([]);

  const fetchStages = useCallback(async () => {
    try {
      const result = await api.getPipelineStages(repo);
      setAllRepoStages(result);
      const filtered = selectedAgentId
        ? result.filter((s) => !s.applies_to_agent_id || s.applies_to_agent_id === selectedAgentId)
        : result;
      setSavedStages(filtered);
      setStages(filtered.map(stageFromApi));
    } catch {
      // silent
    } finally {
      setLoading(false);
    }
  }, [repo, selectedAgentId]);

  useEffect(() => {
    setLoading(true);
    void fetchStages();
  }, [fetchStages]);

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    try {
      const editedStages = stages
        .filter((s) => s.stage_name.trim())
        .map((s) => ({
          stage_name: s.stage_name.trim(),
          entry_skill: s.entry_skill.trim() || null,
          provider: s.provider.trim() || null,
          agent_override_id: s.agent_override_id || null,
          timeout_minutes: s.timeout_minutes,
          on_failure: s.on_failure,
          on_failure_target: s.on_failure_target.trim() || null,
          max_retries: s.max_retries,
          skip_condition: s.skip_condition.trim() || null,
          parallel_with: s.parallel_with.trim() || null,
          applies_to_agent_id: s.applies_to_agent_id || null,
          trigger_after: s.trigger_after,
        }));
      // When filtered by agent, preserve stages belonging to other agents
      const otherAgentStages = selectedAgentId
        ? allRepoStages
            .filter((s) => s.applies_to_agent_id && s.applies_to_agent_id !== selectedAgentId)
            .map((s) => ({
              stage_name: s.stage_name,
              entry_skill: s.entry_skill ?? null,
              provider: s.provider ?? null,
              agent_override_id: s.agent_override_id ?? null,
              timeout_minutes: s.timeout_minutes,
              on_failure: s.on_failure as "fail" | "retry" | "previous" | "goto",
              on_failure_target: s.on_failure_target ?? null,
              max_retries: s.max_retries,
              skip_condition: s.skip_condition ?? null,
              parallel_with: s.parallel_with ?? null,
              applies_to_agent_id: s.applies_to_agent_id ?? null,
              trigger_after: (s.trigger_after ?? "ready") as "ready" | "review_pass",
            }))
        : [];
      const input = [...editedStages, ...otherAgentStages];
      const result = await api.savePipelineStages(repo, input);
      setAllRepoStages(result);
      const filtered = selectedAgentId
        ? result.filter((s) => !s.applies_to_agent_id || s.applies_to_agent_id === selectedAgentId)
        : result;
      setSavedStages(filtered);
      setStages(filtered.map(stageFromApi));
    } catch (e) {
      setError(e instanceof Error ? e.message : tr("저장 실패", "Save failed"));
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    setSaving(true);
    setError(null);
    try {
      await api.deletePipelineStages(repo);
      setSavedStages([]);
      setStages([]);
    } catch (e) {
      setError(e instanceof Error ? e.message : tr("삭제 실패", "Delete failed"));
    } finally {
      setSaving(false);
    }
  };

  const addStage = () => setStages((prev) => [...prev, emptyStage()]);
  const removeStage = (idx: number) => setStages((prev) => prev.filter((_, i) => i !== idx));
  const moveStage = (idx: number, dir: -1 | 1) => {
    setStages((prev) => {
      const next = [...prev];
      const target = idx + dir;
      if (target < 0 || target >= next.length) return prev;
      [next[idx], next[target]] = [next[target], next[idx]];
      return next;
    });
  };
  const updateStage = (idx: number, patch: Partial<StageEditor>) => {
    setStages((prev) => prev.map((s, i) => (i === idx ? { ...s, ...patch } : s)));
  };

  const hasChanges = JSON.stringify(stages) !== JSON.stringify(savedStages.map(stageFromApi));

  if (loading) return null;

  return (
    <section
      className="rounded-2xl border p-3 sm:p-4 space-y-3"
      style={{
        borderColor: savedStages.length > 0 ? "rgba(14,165,233,0.35)" : "rgba(148,163,184,0.22)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      <div className="flex items-center justify-between gap-2">
        <button onClick={() => setExpanded((p) => !p)} className="flex items-center gap-2">
          <span className="text-sm" style={{ color: "var(--th-text-muted)" }}>
            {expanded ? "▾" : "▸"}
          </span>
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {tr("파이프라인", "Pipeline")}
          </h3>
          {savedStages.length > 0 && (
            <span className="text-xs px-2 py-0.5 rounded-full"
              style={{ backgroundColor: "rgba(14,165,233,0.2)", color: "#38bdf8" }}>
              {savedStages.length} {tr("스테이지", "stages")}
            </span>
          )}
        </button>
      </div>

      {expanded && (
        <div className="space-y-3">
          {error && (
            <div className="rounded-lg px-3 py-2 text-xs border"
              style={{ borderColor: "rgba(248,113,113,0.4)", color: "#fecaca", backgroundColor: "rgba(127,29,29,0.2)" }}>
              {error}
            </div>
          )}

          {stages.map((stage, idx) => (
            <div key={idx} className="rounded-xl border p-3 space-y-2"
              style={{ borderColor: "rgba(148,163,184,0.18)", backgroundColor: "var(--th-overlay-medium)" }}>
              <div className="flex items-center gap-2">
                <span className="text-xs font-mono w-5 text-center shrink-0"
                  style={{ color: "var(--th-text-muted)" }}>{idx + 1}</span>
                <input
                  className="flex-1 bg-transparent border-b text-xs px-1 py-0.5 outline-none"
                  style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-primary)" }}
                  placeholder={tr("스테이지 이름", "Stage name")}
                  value={stage.stage_name}
                  onChange={(e) => updateStage(idx, { stage_name: e.target.value })}
                />
                <div className="flex items-center gap-1">
                  {idx > 0 && (
                    <button onClick={() => moveStage(idx, -1)} className="text-xs px-1"
                      style={{ color: "var(--th-text-muted)" }}>↑</button>
                  )}
                  {idx < stages.length - 1 && (
                    <button onClick={() => moveStage(idx, 1)} className="text-xs px-1"
                      style={{ color: "var(--th-text-muted)" }}>↓</button>
                  )}
                  <button onClick={() => removeStage(idx)} className="text-xs px-1"
                    style={{ color: "#f87171" }}>✕</button>
                </div>
              </div>

              <div className="grid grid-cols-2 gap-2 text-xs">
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("스킬", "Skill")}</label>
                  <input
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    placeholder="e.g. claude-code-plan"
                    value={stage.entry_skill}
                    onChange={(e) => updateStage(idx, { entry_skill: e.target.value })}
                  />
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("프로바이더", "Provider")}</label>
                  <input
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    placeholder="claude / codex"
                    value={stage.provider}
                    onChange={(e) => updateStage(idx, { provider: e.target.value })}
                  />
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("담당 에이전트", "Agent override")}</label>
                  <select
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    value={stage.agent_override_id}
                    onChange={(e) => updateStage(idx, { agent_override_id: e.target.value })}
                  >
                    <option value="">{tr("카드 담당자", "Card assignee")}</option>
                    {agents.map((a) => (
                      <option key={a.id} value={a.id}>{a.avatar_emoji} {localeName(locale, a)}</option>
                    ))}
                  </select>
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("적용 에이전트", "Applies to agent")}</label>
                  <select
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    value={stage.applies_to_agent_id}
                    onChange={(e) => updateStage(idx, { applies_to_agent_id: e.target.value })}
                  >
                    <option value="">{tr("전체", "All agents")}</option>
                    {agents.map((a) => (
                      <option key={a.id} value={a.id}>{a.avatar_emoji} {localeName(locale, a)}</option>
                    ))}
                  </select>
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("트리거", "Trigger")}</label>
                  <select
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    value={stage.trigger_after}
                    onChange={(e) => updateStage(idx, { trigger_after: e.target.value as "ready" | "review_pass" })}
                  >
                    <option value="ready">{tr("카드 준비 시", "On card ready")}</option>
                    <option value="review_pass">{tr("리뷰 통과 후", "After review pass")}</option>
                  </select>
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("타임아웃(분)", "Timeout (min)")}</label>
                  <input
                    type="number"
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    value={stage.timeout_minutes}
                    min={1}
                    onChange={(e) => updateStage(idx, { timeout_minutes: Number(e.target.value) || 60 })}
                  />
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("실패 시", "On failure")}</label>
                  <select
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    value={stage.on_failure}
                    onChange={(e) => updateStage(idx, { on_failure: e.target.value as StageEditor["on_failure"] })}
                  >
                    {FAILURE_OPTIONS.map((opt) => (
                      <option key={opt.value} value={opt.value}>{tr(opt.ko, opt.en)}</option>
                    ))}
                  </select>
                </div>
                {stage.on_failure === "goto" && (
                  <div>
                    <label style={{ color: "var(--th-text-muted)" }}>{tr("이동 대상", "Goto target")}</label>
                    <select
                      className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                      style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                      value={stage.on_failure_target}
                      onChange={(e) => updateStage(idx, { on_failure_target: e.target.value })}
                    >
                      <option value="">{tr("선택", "Select")}</option>
                      {stages.filter((_, i) => i !== idx).map((s, i) => (
                        <option key={i} value={s.stage_name}>{s.stage_name}</option>
                      ))}
                    </select>
                  </div>
                )}
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("최대 재시도", "Max retries")}</label>
                  <input
                    type="number"
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    value={stage.max_retries}
                    min={0}
                    max={10}
                    onChange={(e) => updateStage(idx, { max_retries: Number(e.target.value) || 3 })}
                  />
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("스킵 조건", "Skip condition")}</label>
                  <input
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    placeholder="label:hotfix"
                    value={stage.skip_condition}
                    onChange={(e) => updateStage(idx, { skip_condition: e.target.value })}
                  />
                </div>
                <div>
                  <label style={{ color: "var(--th-text-muted)" }}>{tr("병렬 스테이지", "Parallel with")}</label>
                  <select
                    className="w-full bg-transparent border rounded px-1.5 py-0.5 outline-none text-xs"
                    style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
                    value={stage.parallel_with}
                    onChange={(e) => updateStage(idx, { parallel_with: e.target.value })}
                  >
                    <option value="">{tr("없음", "None")}</option>
                    {stages.filter((_, i) => i !== idx).map((s, i) => (
                      <option key={i} value={s.stage_name}>{s.stage_name}</option>
                    ))}
                  </select>
                </div>
              </div>
            </div>
          ))}

          <div className="flex items-center gap-2 flex-wrap">
            <button
              onClick={addStage}
              className="text-xs px-2.5 py-1 rounded-lg border font-medium"
              style={{ borderColor: "rgba(14,165,233,0.4)", color: "#38bdf8", backgroundColor: "rgba(14,165,233,0.1)" }}
            >
              + {tr("스테이지 추가", "Add stage")}
            </button>
            {hasChanges && (
              <button
                onClick={() => void handleSave()}
                disabled={saving}
                className="text-xs px-2.5 py-1 rounded-lg border font-medium"
                style={{ borderColor: "rgba(34,197,94,0.4)", color: "#4ade80", backgroundColor: "rgba(34,197,94,0.1)" }}
              >
                {saving ? "…" : tr("저장", "Save")}
              </button>
            )}
            {savedStages.length > 0 && (
              <button
                onClick={() => void handleDelete()}
                disabled={saving}
                className="text-xs px-2.5 py-1 rounded-lg border font-medium"
                style={{ borderColor: "rgba(239,68,68,0.3)", color: "#f87171", backgroundColor: "rgba(239,68,68,0.08)" }}
              >
                {tr("파이프라인 삭제", "Delete pipeline")}
              </button>
            )}
          </div>

          {stages.length === 0 && savedStages.length === 0 && (
            <div className="text-xs text-center py-3" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "파이프라인 미설정. 스테이지를 추가하면 카드가 자동으로 스테이지를 순서대로 진행합니다.",
                "No pipeline configured. Add stages to automate card progression.",
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
