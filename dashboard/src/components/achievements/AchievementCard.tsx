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
    <span className="grid h-10 w-10 shrink-0 place-items-center rounded-md border border-th-border-subtle bg-th-bg-primary font-mono text-sm font-semibold" style={{color:achieved ? theme.accent : "var(--th-text-muted)"}} aria-hidden="true">{glyph}</span>
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
  return (
    <button type="button" data-testid={`achievement-card-${item.section}-${item.id}`} className="achievement-card min-h-11 min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface p-4 text-left transition-colors hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)]" onClick={() => onOpen(item)}>
      <div className="flex min-w-0 items-start gap-3">
        <BadgeIcon milestoneId={item.milestoneId} rarity={item.rarity} achieved={item.section === "earned"} />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-baseline justify-between gap-x-2 gap-y-1">
            <h3 className="min-w-0 text-sm font-semibold leading-6 text-th-text-heading [overflow-wrap:anywhere]">{item.name}</h3>
            <span className="text-xs leading-5 text-th-text-muted">{rarityLabel(item.rarity,isKo)}</span>
          </div>
          <p className="mt-1 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{item.desc}</p>
          <div className="mt-3 flex min-w-0 flex-wrap items-center justify-between gap-2 text-xs leading-5 text-th-text-muted">
            {item.agentName ? <span className="flex min-w-0 items-center gap-2"><AchievementAvatar agent={item.agent} agents={agents} emoji={item.avatarEmoji} label={item.agentName} /><span className="min-w-0 [overflow-wrap:anywhere]">{item.agentName}</span></span> : null}
            <span className="tabular-nums">+{item.xp} XP</span>
          </div>
          {item.section !== "earned" && typeof item.progress === "number" ? <div className="mt-3"><div className="h-1 overflow-hidden rounded-full bg-th-border-subtle"><div className="h-full bg-[var(--th-accent-primary)]" style={{width:`${Math.round(item.progress*100)}%`}} /></div><p className="mt-1 text-xs tabular-nums text-th-text-muted">{Math.round(item.progress*100)}% {progressLabel}</p></div> : null}
          <div className="mt-1 text-right text-xs leading-5 tabular-nums text-th-text-muted"><span>{item.section === "earned" ? item.at : item.section === "progress" ? item.remainingXp != null ? isKo ? `${item.remainingXp.toLocaleString()} XP 남음` : `${item.remainingXp.toLocaleString()} XP left` : null : isKo ? "세부 보기" : "View details"}</span></div>
        </div>
      </div>
    </button>
  );
}
