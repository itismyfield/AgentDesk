import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { Agent } from "../../types";
import {
  SurfaceActionButton,
  SurfaceNotice,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type { OfficeManualIntervention } from "./officeAgentState";

export function OfficeManualWarningOverlay({
  entries,
  isKo,
  onSelectAgent,
}: {
  entries: Array<{ agent: Agent; warning: OfficeManualIntervention; position: { x: number; y: number } }>;
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
}) {
  const [hoveredWarningId, setHoveredWarningId] = useState<string | null>(null);
  const [expandedWarningId, setExpandedWarningId] = useState<string | null>(null);
  const buttonRefs = useRef<Map<string, HTMLButtonElement | null>>(new Map());

  useEffect(() => {
    if (!expandedWarningId) return;
    if (!entries.some(({ warning }) => warning.cardId === expandedWarningId)) {
      setExpandedWarningId(null);
    }
  }, [entries, expandedWarningId]);

  /* Active overlay = the one being expanded (click) or hovered. We render
     exactly one tooltip via React portal to document.body so it can:
       1) escape the canvas's overflow:hidden / transform-containing
          ancestor (without portal the tooltip clips at the canvas edge);
       2) flip placement based on actual viewport space rather than a
          fixed `bottom-[calc(100%+...)]` that bleeds off the canvas top
          for warnings near the top row. */
  const activeId = expandedWarningId ?? hoveredWarningId;
  const activeEntry = useMemo(
    () => entries.find(({ warning }) => warning.cardId === activeId) ?? null,
    [entries, activeId],
  );
  const [tooltipPos, setTooltipPos] = useState<
    { top: number; left: number; placement: "top" | "bottom" } | null
  >(null);

  useLayoutEffect(() => {
    if (!activeEntry) {
      setTooltipPos(null);
      return;
    }
    const update = () => {
      const button = buttonRefs.current.get(activeEntry.warning.cardId);
      if (!button) return;
      const rect = button.getBoundingClientRect();
      const viewportW = window.innerWidth;
      const viewportH = window.innerHeight;
      const padding = 12;
      const tooltipW = Math.min(304, viewportW - padding * 2);
      // Estimated tooltip height — used only to pick top vs bottom; the
      // actual content auto-sizes via flex column, so we just need a
      // reasonable budget for the "is there enough room above" check.
      const tooltipBudgetH = 220;
      const spaceAbove = rect.top;
      const spaceBelow = viewportH - rect.bottom;
      const placement: "top" | "bottom" =
        spaceAbove >= tooltipBudgetH || spaceAbove >= spaceBelow ? "top" : "bottom";
      const centerX = rect.left + rect.width / 2;
      const left = Math.max(
        padding,
        Math.min(viewportW - tooltipW - padding, centerX - tooltipW / 2),
      );
      const top =
        placement === "top" ? Math.max(padding, rect.top - 12) : rect.bottom + 12;
      setTooltipPos({ top, left, placement });
    };
    update();
    const handle = () => update();
    window.addEventListener("scroll", handle, true);
    window.addEventListener("resize", handle);
    return () => {
      window.removeEventListener("scroll", handle, true);
      window.removeEventListener("resize", handle);
    };
  }, [activeEntry]);

  if (entries.length === 0) return null;

  const portalTarget = typeof document !== "undefined" ? document.body : null;
  const tooltipWidth = (() => {
    if (typeof window === "undefined") return 304;
    return Math.min(304, window.innerWidth - 24);
  })();

  return (
    <>
      <div className="pointer-events-none absolute inset-0 z-10">
        {entries.map(({ agent, warning, position }) => {
          const isOpen = activeId === warning.cardId;
          const agentLabel = agent.alias || agent.name_ko || agent.name;
          return (
            <div
              key={warning.cardId}
              className="absolute pointer-events-auto"
              style={{
                left: position.x + 16,
                top: position.y - 28,
                transform: "translate(-50%, -50%)",
              }}
              onMouseEnter={() => setHoveredWarningId(warning.cardId)}
              onMouseLeave={() =>
                setHoveredWarningId((current) =>
                  current === warning.cardId ? null : current,
                )
              }
              onFocusCapture={() => setHoveredWarningId(warning.cardId)}
              onBlurCapture={(event) => {
                if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
                setHoveredWarningId((current) =>
                  current === warning.cardId ? null : current,
                );
              }}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setExpandedWarningId((current) =>
                    current === warning.cardId ? null : current,
                  );
                  setHoveredWarningId((current) =>
                    current === warning.cardId ? null : current,
                  );
                }
              }}
            >
              {/* Compact "<!>" pill styled to match the in-canvas Pixi
                  nameplate (white 85% rounded-rect with subtle border)
                  so the warning tag reads as part of the same visual
                  language. The yellow glyph is the only colored
                  element, replacing the previous "● 수동" amber chip. */}
              <button
                ref={(el) => {
                  if (el) buttonRefs.current.set(warning.cardId, el);
                  else buttonRefs.current.delete(warning.cardId);
                }}
                type="button"
                className="relative inline-flex h-7 min-w-9 items-center justify-center rounded-[10px] border px-1.5 transition-colors focus:outline-none focus:ring-2"
                style={{
                  borderColor: "color-mix(in srgb, white 38%, var(--th-border) 62%)",
                  background:
                    "color-mix(in srgb, white 85%, var(--th-bg-surface) 15%)",
                  boxShadow: "0 1px 0 rgba(0,0,0,0.18)",
                }}
                aria-label={
                  isKo
                    ? `${agentLabel} 수동 개입 경고`
                    : `${agentLabel} manual intervention warning`
                }
                aria-expanded={isOpen}
                onClick={() =>
                  setExpandedWarningId((current) =>
                    current === warning.cardId ? null : warning.cardId,
                  )
                }
              >
                <span
                  aria-hidden="true"
                  className="absolute -bottom-[3px] left-1/2 h-2 w-2 -translate-x-1/2 rotate-45 border-b border-r"
                  style={{
                    borderColor: "color-mix(in srgb, white 38%, var(--th-border) 62%)",
                    background:
                      "color-mix(in srgb, white 85%, var(--th-bg-surface) 15%)",
                  }}
                />
                <span
                  className="font-mono text-[11px] font-bold leading-none"
                  style={{ color: "var(--th-accent-warn)" }}
                >
                  &lt;!&gt;
                </span>
              </button>
            </div>
          );
        })}
      </div>
      {activeEntry && tooltipPos && portalTarget
        ? createPortal(
            <div
              className="pointer-events-auto"
              style={{
                position: "fixed",
                left: tooltipPos.left,
                top: tooltipPos.placement === "bottom" ? tooltipPos.top : "auto",
                bottom:
                  tooltipPos.placement === "top"
                    ? `${
                        typeof window !== "undefined"
                          ? window.innerHeight - tooltipPos.top
                          : 0
                      }px`
                    : "auto",
                width: tooltipWidth,
                maxWidth: "calc(100vw - 1.5rem)",
                zIndex: 60,
              }}
              onMouseEnter={() => setHoveredWarningId(activeEntry.warning.cardId)}
              onMouseLeave={() =>
                setHoveredWarningId((current) =>
                  current === activeEntry.warning.cardId ? null : current,
                )
              }
            >
              <SurfaceSubsection
                title={activeEntry.warning.title}
                description={
                  isKo
                    ? `${
                        activeEntry.agent.alias ||
                        activeEntry.agent.name_ko ||
                        activeEntry.agent.name
                      }에게 연결된 카드에서 수동 개입이 필요합니다.`
                    : `Manual intervention is required for the card assigned to ${
                        activeEntry.agent.alias ||
                        activeEntry.agent.name_ko ||
                        activeEntry.agent.name
                      }.`
                }
                actions={
                  <SurfaceActionButton
                    tone="warn"
                    compact
                    className="pointer-events-auto rounded-full"
                    onClick={() => onSelectAgent?.(activeEntry.agent)}
                  >
                    {isKo ? "세부 보기" : "Open detail"}
                  </SurfaceActionButton>
                }
                className="rounded-[24px] p-3 sm:p-3"
                style={{
                  borderColor:
                    "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-badge-amber-bg) 52%, var(--th-card-bg) 48%) 0%, color-mix(in srgb, var(--th-card-bg) 92%, transparent) 100%)",
                  boxShadow:
                    "0 14px 32px -18px color-mix(in srgb, black 70%, transparent)",
                }}
              >
                <div className="space-y-2">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span
                      className="inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-semibold uppercase tracking-[0.14em]"
                      style={{
                        color: "var(--th-accent-warn)",
                        borderColor:
                          "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
                        background:
                          "color-mix(in srgb, var(--th-badge-amber-bg) 68%, var(--th-card-bg) 32%)",
                      }}
                    >
                      {isKo ? "수동 개입" : "Manual intervention"}
                    </span>
                    <span
                      className="text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {activeEntry.warning.issueNumber
                        ? `#${activeEntry.warning.issueNumber}`
                        : activeEntry.warning.status}
                    </span>
                  </div>
                  <SurfaceNotice tone="warn" compact className="items-start rounded-[18px]">
                    <div className="text-[11px] leading-5">
                      {isKo
                        ? "카드 상세에서 원인과 후속 조치를 확인할 수 있습니다."
                        : "Open the card detail to inspect the cause and next action."}
                    </div>
                  </SurfaceNotice>
                </div>
                {/* Body text uses normal whitespace handling so words wrap
                    on word boundaries; `break-words` keeps long
                    unbreakable tokens (URLs, IDs) from overflowing. The
                    previous `whitespace-pre-wrap` combined with the
                    overflow-clipped placement made the body collapse
                    into 1-character lines on warnings near the canvas
                    top. */}
                <div
                  className="mt-3 break-words text-xs leading-5"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {activeEntry.warning.reason ??
                    (isKo
                      ? "구체 사유는 카드 상세에서 확인할 수 있습니다."
                      : "Open the detail drawer to inspect the full reason.")}
                </div>
              </SurfaceSubsection>
            </div>,
            portalTarget,
          )
        : null}
    </>
  );
}
