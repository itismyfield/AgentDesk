import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { getCurrentTaskSummary } from "../../lib/agentHelpers";
import { getFontFamilyForText } from "../../lib/fonts";
import { getProviderMeta } from "../../app/providerTheme";
import AgentAvatar from "../AgentAvatar";
import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";
import { STATUS_DOT } from "./constants";
import { getAgentLevel, getAgentTitle } from "./agentProgress";
import type { Translator } from "./types";

type AgentCardViewMode = "grid" | "list";

interface AgentCardProps {
  agent: Agent;
  spriteMap: Map<string, number>;
  isKo: boolean;
  locale: string;
  tr: Translator;
  departments: Department[];
  onOpen: () => void;
  onEdit: () => void;
  confirmDeleteId: string | null;
  onDeleteClick: () => void;
  onDeleteConfirm: () => void;
  onDeleteCancel: () => void;
  saving: boolean;
  topSkills: string[];
  viewMode: AgentCardViewMode;
}

export default function AgentCard({
  agent,
  spriteMap,
  isKo,
  locale,
  tr,
  departments,
  onOpen,
  onEdit,
  confirmDeleteId,
  onDeleteClick,
  onDeleteConfirm,
  onDeleteCancel,
  saving,
  topSkills,
  viewMode,
}: AgentCardProps) {
  const isDeleting = confirmDeleteId === agent.id;
  const dept = departments.find((d) => d.id === agent.department_id);
  const primaryLabel = localeName(locale, agent);
  const providerMeta = getProviderMeta(agent.cli_provider);
  const levelInfo = getAgentLevel(agent.stats_xp);
  const levelTitle = getAgentTitle(agent.stats_xp, isKo);
  const task = getCurrentTaskSummary(agent, tr);
  const contentFont = getFontFamilyForText(task.value, "sans");

  return (
    <SurfaceCard
      data-testid={`agents-card-${agent.id}`}
      onClick={onOpen}
      className={`group cursor-pointer rounded-[28px] p-4 transition-all hover:-translate-y-0.5 ${
        viewMode === "list" ? "flex flex-col gap-4 md:flex-row md:items-center" : ""
      }`}
      draggable={false}
      onDragStart={(event) => event.preventDefault()}
      style={{
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 92%, transparent) 100%)",
        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
        touchAction: "pan-y",
        userSelect: "none",
      }}
    >
      <div
        className={`min-w-0 ${
          viewMode === "list"
            ? "flex flex-1 flex-col gap-4 md:flex-row md:items-start"
            : "space-y-4"
        }`}
      >
        <div className="flex items-start gap-3">
          <div className="relative shrink-0">
            <AgentAvatar
              agent={agent}
              spriteMap={spriteMap}
              size={viewMode === "list" ? 56 : 52}
              rounded="xl"
            />
            <div
              className={`absolute -bottom-0.5 -right-0.5 h-3.5 w-3.5 rounded-full border-2 ${
                STATUS_DOT[agent.status] ?? STATUS_DOT.idle
              }`}
              style={{ borderColor: "var(--th-card-bg)" }}
            />
          </div>
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-2">
              <span
                className="truncate text-sm font-semibold"
                style={{
                  color: "var(--th-text-heading)",
                  fontFamily: getFontFamilyForText(primaryLabel, "pixel"),
                }}
              >
                {primaryLabel}
              </span>
              <span
                className="rounded-full border px-2 py-0.5 text-[11px] font-medium"
                style={{
                  borderColor: "rgba(250,204,21,0.32)",
                  background: "rgba(250,204,21,0.12)",
                  color: "#fde68a",
                }}
              >
                Lv.{levelInfo.level} {levelTitle}
              </span>
              <span
                className="rounded-full border px-2 py-0.5 text-[11px] font-medium"
                style={{
                  borderColor: providerMeta.border,
                  background: providerMeta.bg,
                  color: providerMeta.color,
                }}
              >
                {providerMeta.label}
              </span>
            </div>
            <div
              className="mt-1 text-xs"
              style={{ color: "var(--th-text-muted)" }}
            >
              {locale === "en" ? agent.name_ko || agent.name : agent.name}
            </div>
            <div className="mt-2 flex flex-wrap items-center gap-1.5">
              {dept ? (
                <span
                  className="rounded-full border px-2 py-1 text-[11px]"
                  style={{
                    background:
                      "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
                    color: "var(--th-text-muted)",
                    border:
                      "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
                  }}
                >
                  {dept.icon} {localeName(locale, dept)}
                </span>
              ) : null}
              <span
                className="rounded-full border px-2 py-1 text-[11px] font-medium"
                style={{
                  background: "rgba(59,130,246,0.12)",
                  borderColor: "rgba(59,130,246,0.22)",
                  color: "#93c5fd",
                }}
              >
                XP {agent.stats_xp.toLocaleString()}
              </span>
              <span
                className="rounded-full border px-2 py-1 text-[11px] font-medium"
                style={{
                  background: "rgba(16,185,129,0.12)",
                  borderColor: "rgba(16,185,129,0.22)",
                  color: "#6ee7b7",
                }}
              >
                {tr("완료", "Done")} {agent.stats_tasks_done.toLocaleString()}
              </span>
            </div>
          </div>
        </div>

        <div className="min-w-0 flex-1">
          <div
            className="rounded-2xl border px-3 py-3"
            style={{
              background:
                "color-mix(in srgb, var(--th-bg-surface) 84%, transparent)",
              borderColor:
                "color-mix(in srgb, var(--th-border) 68%, transparent)",
            }}
          >
            <div
              className="text-[11px] font-semibold uppercase tracking-[0.18em]"
              style={{ color: "var(--th-text-muted)" }}
            >
              {task.label}
            </div>
            <div
              className="mt-2 line-clamp-2 text-sm"
              style={{
                color: "var(--th-text-primary)",
                fontFamily: contentFont,
              }}
            >
              {task.value}
            </div>
          </div>

          <div className="mt-3 flex flex-wrap items-center gap-2">
            {topSkills.length > 0 ? (
              topSkills.map((skill) => (
                <span
                  key={`${agent.id}-${skill}`}
                  className="rounded-full border px-2 py-1 text-[11px]"
                  style={{
                    borderColor:
                      "color-mix(in srgb, var(--th-border) 70%, transparent)",
                    background:
                      "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                    color: "var(--th-text-secondary)",
                  }}
                >
                  {skill}
                </span>
              ))
            ) : (
              <span
                className="text-xs"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("최근 스킬 데이터 없음", "No recent skill data")}
              </span>
            )}
          </div>
        </div>
      </div>

      <div
        className={`flex shrink-0 items-center gap-1.5 ${
          viewMode === "list" ? "justify-end" : "justify-between pt-2"
        }`}
        onClick={(event) => event.stopPropagation()}
      >
        <SurfaceActionButton onClick={onEdit} tone="neutral" compact>
          {tr("편집", "Edit")}
        </SurfaceActionButton>
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
            <SurfaceActionButton onClick={onDeleteCancel} tone="neutral" compact>
              {tr("취소", "No")}
            </SurfaceActionButton>
          </>
        ) : (
          <SurfaceActionButton onClick={onDeleteClick} tone="neutral" compact>
            {tr("삭제", "Delete")}
          </SurfaceActionButton>
        )}
      </div>
    </SurfaceCard>
  );
}
