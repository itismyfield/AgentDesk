import type { Agent } from "../../types";
import AgentAvatar from "../AgentAvatar";
import {
  MILESTONE_BY_ID,
  getRarityTheme,
  rarityLabel,
  type AchievementCardModel,
  type MilestoneId,
  type Rarity,
} from "../achievementsModel";

export function BadgeIcon({
  milestoneId,
  rarity,
  achieved,
}: {
  milestoneId: string;
  rarity: Rarity;
  achieved: boolean;
}) {
  const meta = MILESTONE_BY_ID.get(milestoneId as MilestoneId);
  const glyph = meta?.glyph ?? "?";
  const theme = getRarityTheme(rarity);

  return (
    <div style={{ width: 48, height: 48, position: "relative", flexShrink: 0 }}>
      <div
        style={{
          position: "absolute",
          inset: 0,
          background: achieved
            ? `linear-gradient(135deg, ${theme.accent}, color-mix(in srgb, ${theme.accent} 60%, black))`
            : "var(--bg-3)",
          clipPath: "polygon(50% 0, 100% 25%, 100% 75%, 50% 100%, 0 75%, 0 25%)",
          boxShadow: achieved
            ? `0 0 18px color-mix(in srgb, ${theme.accent} 28%, transparent)`
            : "none",
        }}
      />
      <div
        style={{
          position: "absolute",
          inset: 3,
          background: achieved
            ? "color-mix(in srgb, var(--bg-1) 28%, transparent)"
            : "var(--bg-2)",
          clipPath: "polygon(50% 0, 100% 25%, 100% 75%, 50% 100%, 0 75%, 0 25%)",
          display: "grid",
          placeItems: "center",
          color: achieved ? theme.accent : "var(--fg-faint)",
          fontSize: glyph.length > 1 ? 11 : 18,
          fontFamily: "var(--font-pixel)",
          fontWeight: 700,
        }}
      >
        {glyph}
      </div>
    </div>
  );
}

export function AchievementAvatar({
  agent,
  agents,
  emoji,
  label,
}: {
  agent?: Agent;
  agents?: Agent[];
  emoji?: string;
  label: string;
}) {
  // Sprite-first: when an agent reference is available, render the canonical
  // sprite. Emoji remains a fallback for leaderboard rows without a resolved Agent.
  if (agent) {
    return (
      <AgentAvatar
        agent={agent}
        agents={agents}
        size={28}
        rounded="full"
        className="border border-[color-mix(in_srgb,var(--line)_70%,transparent)]"
      />
    );
  }
  return (
    <div
      title={label}
      aria-label={label}
      style={{
        width: 28,
        height: 28,
        borderRadius: 999,
        display: "grid",
        placeItems: "center",
        background: "color-mix(in srgb, var(--bg-3) 88%, transparent)",
        border: "1px solid color-mix(in srgb, var(--line) 70%, transparent)",
        fontSize: 14,
      }}
    >
      {emoji || "🤖"}
    </div>
  );
}

export function AchievementCard({
  item,
  isKo,
  progressLabel,
  agents,
  onOpen,
}: {
  item: AchievementCardModel;
  isKo: boolean;
  progressLabel: string;
  agents: Agent[];
  onOpen: (item: AchievementCardModel) => void;
}) {
  const theme = getRarityTheme(item.rarity);

  return (
    <button
      type="button"
      data-testid={`achievement-card-${item.section}-${item.id}`}
      className="achievement-card"
      onClick={() => onOpen(item)}
      style={{
        position: "relative",
        overflow: "hidden",
        borderRadius: 18,
        border: `1px solid ${theme.border}`,
        background: "color-mix(in srgb, var(--bg-2) 94%, transparent)",
        padding: 16,
        textAlign: "left",
        opacity: item.section === "locked" ? 0.8 : 1,
        transition: "transform 0.16s ease, border-color 0.16s ease, background 0.16s ease",
      }}
    >
      <div
        style={{
          position: "absolute",
          right: 0,
          top: 0,
          width: 48,
          height: 48,
          background: `radial-gradient(circle at top right, color-mix(in srgb, ${theme.accent} 24%, transparent), transparent 72%)`,
        }}
      />
      <div className="flex items-start gap-3">
        <BadgeIcon
          milestoneId={item.milestoneId}
          rarity={item.rarity}
          achieved={item.section === "earned"}
        />
        <div className="min-w-0 flex-1">
          <div className="mb-1 flex items-center gap-2">
            <div className="truncate text-sm font-semibold" style={{ color: "var(--fg)" }}>
              {item.name}
            </div>
            <span
              className="shrink-0 rounded-full px-2 py-0.5 text-[10px] font-semibold"
              style={{
                marginLeft: "auto",
                background: theme.badgeBg,
                color: theme.badgeText,
              }}
            >
              {rarityLabel(item.rarity, isKo)}
            </span>
          </div>
          <div className="text-[11.5px] leading-5" style={{ color: "var(--fg-muted)" }}>
            {item.desc}
          </div>
          {item.agentName ? (
            <div
              className="mt-3 flex max-w-full items-center gap-2 text-[10.5px]"
              style={{ color: "var(--fg-faint)" }}
            >
              <AchievementAvatar
                agent={item.agent}
                agents={agents}
                emoji={item.avatarEmoji || "🤖"}
                label={item.agentName}
              />
              <span className="min-w-0 flex-1 truncate">{item.agentName}</span>
            </div>
          ) : null}
          {item.section !== "earned" && typeof item.progress === "number" ? (
            <div className="mt-3">
              <div
                className="overflow-hidden rounded-full"
                style={{
                  height: 4,
                  background: "color-mix(in srgb, var(--bg-3) 90%, transparent)",
                }}
              >
                <div
                  style={{
                    height: "100%",
                    width: `${Math.round(item.progress * 100)}%`,
                    background: theme.accent,
                  }}
                />
              </div>
              <div
                className="mt-2 text-[10px]"
                style={{ color: "var(--fg-faint)", fontFamily: "var(--font-mono)" }}
              >
                {Math.round(item.progress * 100)}% {progressLabel}
              </div>
            </div>
          ) : null}
          <div className="mt-3 flex items-center justify-between gap-3 text-[10.5px]">
            <span style={{ color: theme.badgeText, fontFamily: "var(--font-mono)" }}>
              +{item.xp} XP
            </span>
            <span style={{ color: "var(--fg-faint)" }}>
              {item.section === "earned"
                ? item.at
                : item.section === "progress"
                  ? item.remainingXp != null
                    ? isKo
                      ? `${item.remainingXp.toLocaleString()} XP 남음`
                      : `${item.remainingXp.toLocaleString()} XP left`
                    : null
                  : isKo
                    ? "세부 보기"
                    : "View details"}
            </span>
          </div>
        </div>
      </div>
    </button>
  );
}
