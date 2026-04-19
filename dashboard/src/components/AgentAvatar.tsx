import { useMemo } from "react";
import type { CSSProperties } from "react";
import type { Agent } from "../types";
import { resolveAvatarSeed } from "../lib/pixel-avatar";
import PixelAvatar from "./PixelAvatar";

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

interface AgentAvatarProps {
  agent: Agent | undefined;
  agents?: Agent[];
  spriteMap?: Map<string, number>;
  size?: number;
  className?: string;
  rounded?: "full" | "xl" | "2xl";
  imageFit?: "cover" | "contain";
  imagePosition?: CSSProperties["objectPosition"];
}

/** Procedural 8x8 avatar — sprite maps remain reserved for the Pixi office scene. */
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
  void agents;
  void spriteMap;
  void imageFit;
  void imagePosition;
  return (
    <PixelAvatar
      seed={resolveAvatarSeed(agent)}
      size={size}
      className={className}
      rounded={rounded}
      label={agent?.name ?? "Agent"}
    />
  );
}
