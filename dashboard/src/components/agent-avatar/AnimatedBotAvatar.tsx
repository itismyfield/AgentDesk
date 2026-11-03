import { useEffect, useRef } from "react";
import type { AvatarDefinition, AvatarMotion } from "open-bot-avatar";
import { mountAvatar, type MountedAvatar } from "open-bot-avatar/dom";

/** Live avatar (idle motion, blinking, moods); the library owns this element's children. */
export default function AnimatedBotAvatar({
  definition,
  color,
  mood,
  size,
}: {
  definition: AvatarDefinition;
  color: string;
  mood: AvatarMotion;
  size: number;
}) {
  const host = useRef<HTMLSpanElement>(null);
  const avatar = useRef<MountedAvatar | null>(null);

  useEffect(() => {
    if (!host.current) return;
    const mounted = mountAvatar(host.current, { definition, color, mood, label: "" });
    avatar.current = mounted;
    return () => {
      mounted.destroy();
      avatar.current = null;
    };
    // Mount once; later prop changes go through update() below.
  }, []);

  useEffect(() => {
    avatar.current?.update({ definition, color, mood });
  }, [definition, color, mood]);

  return <span ref={host} aria-hidden className="block" style={{ width: size, height: size }} />;
}
