import { useMemo } from "react";
import type { CSSProperties } from "react";
import type { Agent } from "../types";

const SPRITE_FALLBACK_NUMBER = 1;
const SPRITE_DEFAULT_DIRECTION = "D";
const SPRITE_DEFAULT_VARIATION = "1";

/** Map agent IDs to sprite numbers (stable order, same as OfficeView) */
export function buildSpriteMap(agents: Agent[]): Map<string, number> {
  const map = new Map<string, number>();
  // 1) sprite_number가 DB에 지정된 에이전트 우선
  for (const a of agents) {
    if (a.sprite_number != null && a.sprite_number > 0) map.set(a.id, a.sprite_number);
  }
  // 2) DORO fallback (sprite_number 미지정시)
  const doro = agents.find((a) => a.name === "DORO");
  if (doro && !map.has(doro.id)) map.set(doro.id, 13);
  // 3) 나머지: 자동 할당 (1-12 순환)
  const rest = [...agents].filter((a) => !map.has(a.id)).sort((a, b) => a.id.localeCompare(b.id));
  rest.forEach((a, i) => map.set(a.id, (i % 12) + 1));
  return map;
}

/** Hook: memoized sprite map from agents array */
export function useSpriteMap(agents: Agent[]): Map<string, number> {
  return useMemo(() => buildSpriteMap(agents), [agents]);
}

/** Get the sprite number for an agent by ID */
export function getSpriteNum(agents: Agent[], agentId: string): number | undefined {
  return buildSpriteMap(agents).get(agentId);
}

function spriteSrc(spriteNumber: number): string {
  return `/sprites/${spriteNumber}-${SPRITE_DEFAULT_DIRECTION}-${SPRITE_DEFAULT_VARIATION}.png`;
}

function resolveSpriteNumber(
  agent: AgentLike | undefined,
  agents: Agent[] | undefined,
  spriteMap: Map<string, number> | undefined,
): number {
  if (!agent) return SPRITE_FALLBACK_NUMBER;
  if (agent.sprite_number != null && agent.sprite_number > 0) return agent.sprite_number;
  if (spriteMap?.has(agent.id)) return spriteMap.get(agent.id) as number;
  if (agents && agents.length > 0) {
    const map = buildSpriteMap(agents);
    if (map.has(agent.id)) return map.get(agent.id) as number;
  }
  // Codex review (PR #1258, 4th pass): the previous hash fallback could
  // disagree with buildSpriteMap (sorted-id + DORO special case), so the
  // same agent showed different sprites on different pages when the
  // caller didn't pass agents/spriteMap. Use the canonical fallback
  // sprite instead — agent identity stays stable, even if it shares the
  // sprite with the default-portrait pool.
  return SPRITE_FALLBACK_NUMBER;
}

type AgentLike = Pick<Agent, "id" | "name"> & {
  sprite_number?: number | null;
};

interface AgentAvatarProps {
  agent: AgentLike | undefined;
  agents?: Agent[];
  spriteMap?: Map<string, number>;
  size?: number;
  className?: string;
  rounded?: "full" | "xl" | "2xl";
  imageFit?: "cover" | "contain";
  imagePosition?: CSSProperties["objectPosition"];
}

const ROUNDED_CLASS: Record<NonNullable<AgentAvatarProps["rounded"]>, string> = {
  full: "rounded-full",
  xl: "rounded-xl",
  "2xl": "rounded-2xl",
};

/** Sprite-based agent portrait. Pulls from /sprites/{n}-D-1.png. */
export default function AgentAvatar({
  agent,
  agents,
  spriteMap,
  size = 28,
  className = "",
  rounded = "full",
  imageFit = "cover",
  imagePosition = "center",
}: AgentAvatarProps) {
  const spriteNumber = resolveSpriteNumber(agent, agents, spriteMap);
  const label = agent?.name ?? "Agent";
  const wrapperClass = `inline-flex shrink-0 items-center justify-center overflow-hidden bg-[color-mix(in_srgb,var(--th-bg-surface)_88%,transparent)] ${ROUNDED_CLASS[rounded]} ${className}`.trim();
  return (
    <span
      className={wrapperClass}
      style={{ width: size, height: size }}
      aria-label={label}
      role="img"
    >
      <img
        src={spriteSrc(spriteNumber)}
        alt=""
        width={size}
        height={size}
        loading="lazy"
        draggable={false}
        style={{
          width: "100%",
          height: "100%",
          objectFit: imageFit,
          objectPosition: imagePosition,
          imageRendering: "pixelated",
        }}
      />
    </span>
  );
}
