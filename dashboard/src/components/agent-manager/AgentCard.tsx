import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import AgentAvatar from "../AgentAvatar";
import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";
import { STATUS_DOT } from "./constants";
import type { Translator } from "./types";

interface AgentCardProps {
  agent: Agent;
  spriteMap: Map<string, number>;
  isKo: boolean;
  locale: string;
  tr: Translator;
  departments: Department[];
  onEdit: () => void;
  confirmDeleteId: string | null;
  onDeleteClick: () => void;
  onDeleteConfirm: () => void;
  onDeleteCancel: () => void;
  saving: boolean;
}

export default function AgentCard({
  agent,
  spriteMap,
  isKo,
  locale,
  tr,
  departments,
  onEdit,
  confirmDeleteId,
  onDeleteClick,
  onDeleteConfirm,
  onDeleteCancel,
  saving,
}: AgentCardProps) {
  const isDeleting = confirmDeleteId === agent.id;
  const dept = departments.find((d) => d.id === agent.department_id);

  return (
    <SurfaceCard
      onClick={onEdit}
      className="group cursor-pointer rounded-3xl p-4 transition-all hover:-translate-y-0.5"
      draggable={false}
      onDragStart={(e) => e.preventDefault()}
      style={{
        background: "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
        touchAction: "pan-y",
        userSelect: "none",
      }}
    >
      <div className="flex items-start gap-3">
        <div className="relative shrink-0">
          <AgentAvatar agent={agent} spriteMap={spriteMap} size={44} rounded="xl" />
          <div
            className={`absolute -bottom-0.5 -right-0.5 w-3 h-3 rounded-full border-2 ${STATUS_DOT[agent.status] ?? STATUS_DOT.idle}`}
            style={{ borderColor: "var(--th-card-bg)" }}
          />
        </div>
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-1.5">
            <span className="font-semibold text-sm truncate" style={{ color: "var(--th-text-heading)" }}>
              {localeName(locale, agent)}
            </span>
            <span className="text-xs shrink-0" style={{ color: "var(--th-text-muted)" }}>
              {(() => {
                const primary = localeName(locale, agent);
                const sub = locale === "en" ? agent.name_ko || "" : agent.name;
                return primary !== sub ? sub : "";
              })()}
            </span>
          </div>
          {dept && (
            <div className="flex items-center gap-1.5 mt-1 flex-wrap">
              <span
                className="rounded-full px-2 py-1 text-[11px]"
                style={{
                  background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
                  color: "var(--th-text-muted)",
                  border: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
                }}
              >
                {dept.icon} {localeName(locale, dept)}
              </span>
            </div>
          )}
        </div>
      </div>

      <div
        className="mt-3 flex flex-wrap items-center justify-between gap-2 pt-2.5"
        style={{ borderTop: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)" }}
      >
        <div className="flex min-w-0 flex-1 items-center gap-2">
          {agent.personality && (
            <span
              className="max-w-[180px] truncate text-xs"
              style={{ color: "var(--th-text-muted)" }}
              title={agent.personality}
            >
              {agent.personality}
            </span>
          )}
        </div>
        <div
          className="flex shrink-0 items-center gap-1 opacity-0 transition-opacity group-hover:opacity-100"
          onClick={(e) => e.stopPropagation()}
        >
          {isDeleting ? (
            <>
              <SurfaceActionButton
                onClick={onDeleteConfirm}
                disabled={saving || agent.status === "working"}
                tone="danger"
                compact
              >
                {tr("해고", "Fire")}
              </SurfaceActionButton>
              <SurfaceActionButton
                onClick={onDeleteCancel}
                tone="neutral"
                compact
              >
                {tr("취소", "No")}
              </SurfaceActionButton>
            </>
          ) : (
            <SurfaceActionButton
              onClick={onDeleteClick}
              tone="neutral"
              compact
              style={{ minWidth: 28 }}
            >
              ✕
            </SurfaceActionButton>
          )}
        </div>
      </div>
    </SurfaceCard>
  );
}
