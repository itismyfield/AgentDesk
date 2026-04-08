import { useCallback, useEffect, useState } from "react";
import * as api from "../../api";
import type { Agent, UiLanguage } from "../../types";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repo?: string;
  agents: Agent[];
  selectedAgentId?: string | null;
}

type EditLevel = "repo" | "agent";

/** Editor for repo/agent-level pipeline config overrides. */
export default function PipelineOverrideEditor({
  tr,
  locale,
  repo,
  agents,
  selectedAgentId,
}: Props) {
  const [expanded, setExpanded] = useState(false);
  const [level, setLevel] = useState<EditLevel>("repo");
  const [json, setJson] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);

  const fetchOverride = useCallback(async () => {
    setError(null);
    setSuccess(false);
    try {
      if (level === "repo" && repo) {
        const result = await api.getRepoPipeline(repo);
        const config = result?.pipeline_config;
        setJson(config && config !== null ? JSON.stringify(config, null, 2) : "");
      } else if (level === "agent" && selectedAgentId) {
        const result = await api.getAgentPipeline(selectedAgentId);
        const config = result?.pipeline_config;
        setJson(config && config !== null ? JSON.stringify(config, null, 2) : "");
      } else {
        setJson("");
      }
    } catch {
      setJson("");
    }
  }, [level, repo, selectedAgentId]);

  useEffect(() => {
    void fetchOverride();
  }, [fetchOverride]);

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    setSuccess(false);
    try {
      const config = json.trim() ? JSON.parse(json) : null;
      if (level === "repo" && repo) {
        await api.setRepoPipeline(repo, config);
      } else if (level === "agent" && selectedAgentId) {
        await api.setAgentPipeline(selectedAgentId, config);
      }
      setSuccess(true);
      setTimeout(() => setSuccess(false), 3000);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
    } finally {
      setSaving(false);
    }
  };

  const handleClear = async () => {
    setSaving(true);
    setError(null);
    try {
      if (level === "repo" && repo) {
        await api.setRepoPipeline(repo, null);
      } else if (level === "agent" && selectedAgentId) {
        await api.setAgentPipeline(selectedAgentId, null);
      }
      setJson("");
      setSuccess(true);
      setTimeout(() => setSuccess(false), 3000);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  if (!repo) return null;

  return (
    <section
      className="rounded-2xl border p-3 sm:p-4 space-y-3"
      style={{
        borderColor: "rgba(251,191,36,0.35)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      <button
        onClick={() => setExpanded((p) => !p)}
        className="flex items-center gap-2"
      >
        <span className="text-sm" style={{ color: "var(--th-text-muted)" }}>
          {expanded ? "\u25BE" : "\u25B8"}
        </span>
        <h3
          className="text-sm font-semibold"
          style={{ color: "var(--th-text-heading)" }}
        >
          {tr("파이프라인 오버라이드 편집", "Pipeline Override Editor")}
        </h3>
      </button>

      {expanded && (
        <div className="space-y-3">
          {/* Level selector */}
          <div className="flex gap-2">
            <button
              onClick={() => setLevel("repo")}
              className="px-3 py-1 rounded-lg text-xs"
              style={{
                backgroundColor:
                  level === "repo"
                    ? "rgba(99,102,241,0.3)"
                    : "rgba(30,41,59,0.5)",
                color: level === "repo" ? "#818cf8" : "var(--th-text-muted)",
                border:
                  level === "repo"
                    ? "1px solid rgba(99,102,241,0.5)"
                    : "1px solid transparent",
              }}
            >
              {tr("레포 오버라이드", "Repo Override")}
            </button>
            <button
              onClick={() => setLevel("agent")}
              disabled={!selectedAgentId}
              className="px-3 py-1 rounded-lg text-xs"
              style={{
                backgroundColor:
                  level === "agent"
                    ? "rgba(99,102,241,0.3)"
                    : "rgba(30,41,59,0.5)",
                color: level === "agent" ? "#818cf8" : "var(--th-text-muted)",
                border:
                  level === "agent"
                    ? "1px solid rgba(99,102,241,0.5)"
                    : "1px solid transparent",
                opacity: selectedAgentId ? 1 : 0.4,
              }}
            >
              {tr("에이전트 오버라이드", "Agent Override")}
            </button>
          </div>

          {/* JSON editor */}
          <textarea
            value={json}
            onChange={(e) => setJson(e.target.value)}
            rows={12}
            className="w-full rounded-lg p-3 text-xs font-mono resize-y"
            style={{
              backgroundColor: "var(--th-bg-surface)",
              color: "var(--th-text-primary)",
              border: "1px solid rgba(148,163,184,0.2)",
            }}
            placeholder={`{
  "states": [...],
  "transitions": [...],
  "hooks": {...},
  "gates": {...},
  "clocks": {...},
  "timeouts": {...}
}`}
          />

          {/* Error/success messages */}
          {error && (
            <div
              className="text-xs px-3 py-2 rounded-lg"
              style={{
                backgroundColor: "rgba(239,68,68,0.15)",
                color: "#f87171",
                border: "1px solid rgba(239,68,68,0.3)",
              }}
            >
              {error}
            </div>
          )}
          {success && (
            <div
              className="text-xs px-3 py-2 rounded-lg"
              style={{
                backgroundColor: "rgba(34,197,94,0.15)",
                color: "#4ade80",
              }}
            >
              {tr("저장 완료", "Saved successfully")}
            </div>
          )}

          {/* Action buttons */}
          <div className="flex gap-2">
            <button
              onClick={handleSave}
              disabled={saving}
              className="px-4 py-1.5 rounded-lg text-xs font-medium"
              style={{
                backgroundColor: "rgba(99,102,241,0.3)",
                color: "#818cf8",
                border: "1px solid rgba(99,102,241,0.5)",
                opacity: saving ? 0.5 : 1,
              }}
            >
              {saving
                ? tr("저장 중...", "Saving...")
                : tr("저장", "Save")}
            </button>
            <button
              onClick={handleClear}
              disabled={saving || !json.trim()}
              className="px-4 py-1.5 rounded-lg text-xs font-medium"
              style={{
                backgroundColor: "rgba(239,68,68,0.15)",
                color: "#f87171",
                border: "1px solid rgba(239,68,68,0.3)",
                opacity: saving || !json.trim() ? 0.4 : 1,
              }}
            >
              {tr("초기화 (부모 상속)", "Clear (inherit parent)")}
            </button>
          </div>

          <p
            className="text-xs"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr(
              "비워두면 부모 파이프라인을 상속합니다. 저장 시 병합된 파이프라인의 유효성을 자동 검증합니다.",
              "Leave empty to inherit parent pipeline. Merged pipeline is validated on save.",
            )}
          </p>
        </div>
      )}
    </section>
  );
}
