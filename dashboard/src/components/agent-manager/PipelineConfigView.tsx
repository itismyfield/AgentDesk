import { useCallback, useEffect, useState } from "react";
import * as api from "../../api";
import type { PipelineConfigFull, Agent, UiLanguage } from "../../types";
import { localeName } from "../../i18n";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repo?: string;
  agents: Agent[];
  selectedAgentId?: string | null;
}

/** Visualize the effective pipeline state machine with hierarchy indicators. */
export default function PipelineConfigView({
  tr,
  locale,
  repo,
  agents,
  selectedAgentId,
}: Props) {
  const [pipeline, setPipeline] = useState<PipelineConfigFull | null>(null);
  const [layers, setLayers] = useState<{
    default: boolean;
    repo: boolean;
    agent: boolean;
  }>({ default: true, repo: false, agent: false });
  const [expanded, setExpanded] = useState(false);
  const [loading, setLoading] = useState(true);

  const fetchPipeline = useCallback(async () => {
    try {
      const result = await api.getEffectivePipeline(
        repo,
        selectedAgentId ?? undefined,
      );
      setPipeline(result.pipeline);
      setLayers(result.layers);
    } catch {
      setPipeline(null);
    } finally {
      setLoading(false);
    }
  }, [repo, selectedAgentId]);

  useEffect(() => {
    setLoading(true);
    void fetchPipeline();
  }, [fetchPipeline]);

  if (loading || !pipeline) return null;

  const stateCount = pipeline.states.length;
  const transitionCount = pipeline.transitions.length;
  const hookCount = Object.values(pipeline.hooks).reduce(
    (acc, h) => acc + h.on_enter.length + h.on_exit.length,
    0,
  );

  const activeLayers = [
    layers.default && "default",
    layers.repo && "repo",
    layers.agent && "agent",
  ].filter(Boolean) as string[];

  return (
    <section
      className="rounded-2xl border p-3 sm:p-4 space-y-3"
      style={{
        borderColor: "rgba(99,102,241,0.35)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      <div className="flex items-center justify-between gap-2">
        <button
          onClick={() => setExpanded((p) => !p)}
          className="flex items-center gap-2"
        >
          <span
            className="text-sm"
            style={{ color: "var(--th-text-muted)" }}
          >
            {expanded ? "\u25BE" : "\u25B8"}
          </span>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text-heading)" }}
          >
            {tr("\uD30C\uC774\uD504\uB77C\uC778 \uC0C1\uD0DC\uBA38\uC2E0", "Pipeline State Machine")}
          </h3>
          <span
            className="text-xs px-2 py-0.5 rounded-full"
            style={{
              backgroundColor: "rgba(99,102,241,0.2)",
              color: "#818cf8",
            }}
          >
            {stateCount} {tr("\uC0C1\uD0DC", "states")} / {transitionCount}{" "}
            {tr("\uC804\uD658", "transitions")}
          </span>
          {activeLayers.length > 1 && (
            <span
              className="text-xs px-1.5 py-0.5 rounded-full"
              style={{
                backgroundColor: "rgba(251,191,36,0.15)",
                color: "#fbbf24",
              }}
            >
              {activeLayers.join(" \u2192 ")}
            </span>
          )}
        </button>
      </div>

      {expanded && (
        <div className="space-y-3">
          {/* States visualization */}
          <div className="space-y-1.5">
            <div
              className="text-xs font-medium uppercase tracking-wider"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("\uC0C1\uD0DC", "States")}
            </div>
            <div className="flex flex-wrap gap-1.5">
              {pipeline.states.map((s) => {
                const hookBindings = pipeline.hooks[s.id];
                const timeout = pipeline.timeouts[s.id];
                const hasHooks =
                  hookBindings &&
                  (hookBindings.on_enter.length > 0 ||
                    hookBindings.on_exit.length > 0);

                return (
                  <div
                    key={s.id}
                    className="group relative px-2.5 py-1.5 rounded-lg border text-xs"
                    style={{
                      borderColor: s.terminal
                        ? "rgba(34,197,94,0.4)"
                        : "rgba(148,163,184,0.25)",
                      backgroundColor: s.terminal
                        ? "rgba(34,197,94,0.08)"
                        : "var(--th-overlay-medium)",
                      color: s.terminal ? "#4ade80" : "var(--th-text-primary)",
                    }}
                  >
                    <span className="font-mono">{s.id}</span>
                    <span
                      className="ml-1 text-xs"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {s.label}
                    </span>
                    {hasHooks && (
                      <span className="ml-1 text-xs" style={{ color: "#818cf8" }}>
                        [{hookBindings.on_enter.length}h]
                      </span>
                    )}
                    {timeout && (
                      <span className="ml-1 text-xs" style={{ color: "#f59e0b" }}>
                        {timeout.duration}
                      </span>
                    )}

                    {/* Tooltip */}
                    {(hasHooks || timeout) && (
                      <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-1 hidden group-hover:block z-50">
                        <div
                          className="bg-th-bg-primary border border-th-border rounded px-2 py-1.5 text-xs whitespace-nowrap space-y-0.5"
                          style={{ color: "var(--th-text-secondary)" }}
                        >
                          {hasHooks && (
                            <div>
                              on_enter:{" "}
                              {hookBindings.on_enter.join(", ") || "none"}
                            </div>
                          )}
                          {hasHooks && hookBindings.on_exit.length > 0 && (
                            <div>
                              on_exit: {hookBindings.on_exit.join(", ")}
                            </div>
                          )}
                          {timeout && (
                            <div>
                              timeout: {timeout.duration} (
                              {timeout.on_exhaust ?? "none"})
                            </div>
                          )}
                        </div>
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          </div>

          {/* Transitions */}
          <div className="space-y-1.5">
            <div
              className="text-xs font-medium uppercase tracking-wider"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("\uC804\uD658 \uADDC\uCE59", "Transitions")}
            </div>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-1">
              {pipeline.transitions.map((t, i) => (
                <div
                  key={i}
                  className="flex items-center gap-1.5 text-xs px-2 py-1 rounded"
                  style={{ backgroundColor: "var(--th-overlay-subtle)" }}
                >
                  <span className="font-mono" style={{ color: "var(--th-text-primary)" }}>
                    {t.from}
                  </span>
                  <span style={{ color: "var(--th-text-muted)" }}>{"\u2192"}</span>
                  <span className="font-mono" style={{ color: "var(--th-text-primary)" }}>
                    {t.to}
                  </span>
                  <span
                    className="px-1 py-0.5 rounded text-xs"
                    style={{
                      backgroundColor:
                        t.type === "free"
                          ? "rgba(34,197,94,0.15)"
                          : t.type === "gated"
                            ? "rgba(251,191,36,0.15)"
                            : "rgba(239,68,68,0.15)",
                      color:
                        t.type === "free"
                          ? "#4ade80"
                          : t.type === "gated"
                            ? "#fbbf24"
                            : "#f87171",
                    }}
                  >
                    {t.type}
                  </span>
                  {t.gates && t.gates.length > 0 && (
                    <span
                      className="text-xs"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      [{t.gates.join(",")}]
                    </span>
                  )}
                </div>
              ))}
            </div>
          </div>

          {/* Layer info */}
          <div className="flex items-center gap-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
            <span>{tr("\uACC4\uCE35", "Layers")}:</span>
            {["default", "repo", "agent"].map((layer) => (
              <span
                key={layer}
                className="px-1.5 py-0.5 rounded"
                style={{
                  backgroundColor:
                    layers[layer as keyof typeof layers]
                      ? "rgba(99,102,241,0.15)"
                      : "transparent",
                  color: layers[layer as keyof typeof layers]
                    ? "#818cf8"
                    : "var(--th-text-muted)",
                  opacity: layers[layer as keyof typeof layers] ? 1 : 0.4,
                }}
              >
                {layer}
              </span>
            ))}
          </div>
        </div>
      )}
    </section>
  );
}
