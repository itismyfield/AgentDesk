import { SurfaceCard, SurfaceMetricPill } from "../common/SurfacePrimitives";

interface KanbanPipelineHooksCardProps {
  ctx: any;
}

export default function KanbanPipelineHooksCard({ ctx }: KanbanPipelineHooksCardProps) {
  const { pipelineHookEntries, pipelineHookNames, selectedRepo, SURFACE_CHIP_STYLE, tr } = ctx;

  return (
    <>
      {selectedRepo && pipelineHookEntries.length > 0 && (
        <SurfaceCard
          data-testid="kanban-pipeline-hooks"
          className="rounded-[24px] p-4"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-info) 16%, var(--th-border) 84%)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
          }}
        >
          <div className="flex flex-wrap items-start gap-3">
            <div className="min-w-0 flex-1">
              <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("Pipeline Hooks", "Pipeline Hooks")}
              </div>
              <div className="mt-1 text-sm" style={{ color: "var(--th-text-primary)" }}>
                {tr(
                  "현재 repo pipeline에 연결된 hook을 보드 바로 아래에서 확인합니다.",
                  "Review the currently enabled repo pipeline hooks directly under the board.",
                )}
              </div>
            </div>
            <SurfaceMetricPill
              tone="info"
              label={tr("활성 Hook", "Enabled Hooks")}
              value={pipelineHookEntries.length}
              className="min-w-[112px]"
            />
          </div>

          <div className="mt-4 flex flex-wrap gap-2">
            {pipelineHookNames.map((hookName: string) => (
              <span
                key={hookName}
                className="rounded-full border px-2 py-1 text-xs"
                style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}
              >
                {hookName}
              </span>
            ))}
          </div>

          <div className="mt-4 flex flex-wrap gap-2">
            {pipelineHookEntries.map((entry: any) => (
              <span
                key={`${entry.state}:${entry.phase}:${entry.hook}`}
                className="rounded-full border px-2 py-1 text-xs"
                style={{
                  ...SURFACE_CHIP_STYLE,
                  color: entry.phase === "on_enter" ? "#93c5fd" : "#fbbf24",
                }}
              >
                {entry.state} · {entry.phase === "on_enter" ? tr("진입", "Enter") : tr("이탈", "Exit")} · {entry.hook}
              </span>
            ))}
          </div>
        </SurfaceCard>
      )}
    </>
  );
}
