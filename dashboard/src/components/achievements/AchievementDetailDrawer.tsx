import { ChevronRight } from "lucide-react";
import type { I18nContextValue } from "../../i18n";
import type { Agent } from "../../types";
import { Drawer } from "../common/overlay";
import {
  formatExact,
  getRarityTheme,
  rarityLabel,
  type AchievementCardModel,
} from "../achievementsModel";
import { AchievementAvatar, BadgeIcon } from "./AchievementCard";

interface AchievementDetailDrawerProps {
  selectedCard: AchievementCardModel | null;
  isKo: boolean;
  locale: string;
  t: I18nContextValue["t"];
  agents: Agent[];
  onClose: () => void;
  onSelectAgent?: (agent: Agent) => void;
}

export function AchievementDetailDrawer({
  selectedCard,
  isKo,
  locale,
  t,
  agents,
  onClose,
  onSelectAgent,
}: AchievementDetailDrawerProps) {
  return (
    <Drawer
      open={Boolean(selectedCard)}
      onClose={onClose}
      title={selectedCard?.name}
      width="min(520px, 100vw)"
    >
      {selectedCard ? (
        <div className="space-y-5" data-testid="achievements-drawer">
          <div
            className="rounded-[1.25rem] border p-4"
            style={{
              borderColor: getRarityTheme(selectedCard.rarity).border,
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
            }}
          >
            <div className="flex items-start gap-3">
              <BadgeIcon
                milestoneId={selectedCard.milestoneId}
                rarity={selectedCard.rarity}
                achieved={selectedCard.section === "earned"}
              />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span
                    className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                    style={{
                      background: getRarityTheme(selectedCard.rarity).badgeBg,
                      color: getRarityTheme(selectedCard.rarity).badgeText,
                    }}
                  >
                    {rarityLabel(selectedCard.rarity, isKo)}
                  </span>
                  <span
                    className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                    style={{
                      background: "var(--th-overlay-medium, var(--bg-3))",
                      color: "var(--th-text-muted, var(--fg-muted))",
                    }}
                  >
                    {selectedCard.section === "earned"
                      ? t({ ko: "획득", en: "Unlocked" })
                      : selectedCard.section === "progress"
                        ? t({ ko: "진행 중", en: "In progress" })
                        : t({ ko: "잠김", en: "Locked" })}
                  </span>
                </div>
                <div
                  className="mt-3 text-sm leading-6"
                  style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
                >
                  {selectedCard.desc}
                </div>
                {selectedCard.agentName ? (
                  <button
                    type="button"
                    className="mt-4 inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-white/5"
                    style={{
                      borderColor: "var(--th-border-subtle, var(--line))",
                      color: "var(--th-text-primary, var(--fg))",
                    }}
                    onClick={() => {
                      if (selectedCard.agent && onSelectAgent) {
                        onSelectAgent(selectedCard.agent);
                      }
                    }}
                    disabled={!selectedCard.agent || !onSelectAgent}
                  >
                    <AchievementAvatar
                      agent={selectedCard.agent}
                      agents={agents}
                      emoji={selectedCard.avatarEmoji || "🤖"}
                      label={selectedCard.agentName}
                    />
                    <span>{selectedCard.agentName}</span>
                    <ChevronRight size={14} />
                  </button>
                ) : null}
              </div>
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            {[
              {
                label: t({ ko: "목표 XP", en: "Target XP" }),
                value: formatExact(selectedCard.xp, locale),
              },
              {
                label: t({ ko: "현재 XP", en: "Current XP" }),
                value:
                  typeof selectedCard.currentXp === "number"
                    ? formatExact(selectedCard.currentXp, locale)
                    : "-",
              },
              {
                label: t({ ko: "남은 XP", en: "XP left" }),
                value:
                  typeof selectedCard.remainingXp === "number"
                    ? formatExact(selectedCard.remainingXp, locale)
                    : "-",
              },
              {
                label: t({ ko: "상태", en: "Status" }),
                value:
                  selectedCard.section === "earned"
                    ? t({ ko: "획득", en: "Unlocked" })
                    : selectedCard.section === "progress"
                      ? t({ ko: "진행 중", en: "In progress" })
                      : t({ ko: "잠김", en: "Locked" }),
              },
            ].map((metric) => (
              <div
                key={metric.label}
                className="rounded-2xl border px-3 py-3"
                style={{
                  borderColor: "var(--th-border-subtle, var(--line))",
                  background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                }}
              >
                <div
                  className="text-[10.5px] font-semibold uppercase tracking-[0.08em]"
                  style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                >
                  {metric.label}
                </div>
                <div
                  className="mt-1 text-lg font-semibold"
                  style={{ color: "var(--th-text-heading, var(--fg))" }}
                >
                  {metric.value}
                </div>
              </div>
            ))}
          </div>

          {typeof selectedCard.progress === "number" ? (
            <div data-testid="achievements-progress">
              <div
                className="mb-2 text-[12px] font-medium"
                style={{ color: "var(--th-text-secondary, var(--fg))" }}
              >
                {t({ ko: "진행도", en: "Progress" })}
              </div>
              <div
                className="overflow-hidden rounded-full"
                style={{
                  height: 6,
                  background:
                    "color-mix(in srgb, var(--th-border-subtle, var(--line)) 68%, transparent)",
                }}
              >
                <div
                  style={{
                    width: `${Math.round(selectedCard.progress * 100)}%`,
                    height: "100%",
                    background: getRarityTheme(selectedCard.rarity).accent,
                  }}
                />
              </div>
            </div>
          ) : null}

          <div className="space-y-4">
            <div data-testid="achievements-details">
              <div
                className="mb-2 text-[12px] font-medium"
                style={{ color: "var(--th-text-secondary, var(--fg))" }}
              >
                {t({ ko: "세부", en: "Details" })}
              </div>
              <div className="space-y-2">
                {selectedCard.detailLines.map((line) => (
                  <div
                    key={line}
                    className="rounded-2xl border px-3 py-3 text-sm"
                    style={{
                      borderColor: "var(--th-border-subtle, var(--line))",
                      color: "var(--th-text-secondary, var(--fg-muted))",
                      background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                    }}
                  >
                    {line}
                  </div>
                ))}
              </div>
            </div>

            <div data-testid="achievements-timeline">
              <div
                className="mb-2 text-[12px] font-medium"
                style={{ color: "var(--th-text-secondary, var(--fg))" }}
              >
                {t({ ko: "타임라인", en: "Timeline" })}
              </div>
              <div className="space-y-2">
                {selectedCard.timelineLines.map((line, index) => (
                  <div
                    key={`${line}-${index}`}
                    className="flex items-start gap-3 rounded-2xl border px-3 py-3"
                    style={{
                      borderColor: "var(--th-border-subtle, var(--line))",
                      background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                    }}
                  >
                    <div
                      className="mt-1 h-2.5 w-2.5 rounded-full"
                      style={{
                        background:
                          index === 0
                            ? getRarityTheme(selectedCard.rarity).accent
                            : "var(--th-text-muted, var(--fg-muted))",
                      }}
                    />
                    <div
                      className="text-sm"
                      style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
                    >
                      {line}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </div>
        </div>
      ) : null}
    </Drawer>
  );
}
