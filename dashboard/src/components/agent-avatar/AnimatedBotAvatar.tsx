import { useEffect, useRef } from "react";
import type { AvatarMotion } from "open-bot-avatar";
import { mountAvatar, type MountedAvatar } from "open-bot-avatar/dom";
import type { BotAvatar } from "./botAvatar";

/** Live avatar (idle motion, blinking, moods); the library owns this element's children. */
export default function AnimatedBotAvatar({
  avatar: { definition, color, eyeColor },
  mood,
  size,
}: {
  avatar: BotAvatar;
  mood: AvatarMotion;
  size: number;
}) {
  const host = useRef<HTMLSpanElement>(null);
  const avatar = useRef<MountedAvatar | null>(null);

  useEffect(() => {
    if (!host.current) return;
    const mounted = mountAvatar(host.current, { definition, color, eyeColor, mood, label: "" });
    avatar.current = mounted;
    return () => {
      mounted.destroy();
      avatar.current = null;
    };
    // Mount once; later prop changes go through update() below.
  }, []);

  useEffect(() => {
    avatar.current?.update({ definition, color, eyeColor, mood });
  }, [definition, color, eyeColor, mood]);

  return <span ref={host} aria-hidden className="block" style={{ width: size, height: size }} />;
}
