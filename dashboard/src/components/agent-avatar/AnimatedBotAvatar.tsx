import { useEffect, useLayoutEffect, useRef } from "react";
import type { AvatarMotion } from "open-bot-avatar";
import { mountAvatar, type MountedAvatar } from "open-bot-avatar/dom";
import type { BotAvatar } from "./botAvatar";

/** Live avatar (idle motion, blinking, moods); the library owns this element's children. */
export default function AnimatedBotAvatar({
  avatar: { definition, color, eyeColor },
  mood,
  size,
  followPointer = false,
}: {
  avatar: BotAvatar;
  mood: AvatarMotion;
  size: number;
  followPointer?: boolean;
}) {
  const host = useRef<HTMLSpanElement>(null);
  const avatar = useRef<MountedAvatar | null>(null);

  useLayoutEffect(() => {
    if (!host.current) return;
    const mounted = mountAvatar(host.current, { definition, color, eyeColor, mood, followPointer, label: "" });
    avatar.current = mounted;
    return () => {
      mounted.destroy();
      avatar.current = null;
    };
    // Mount once before first paint; later prop changes go through update() below.
  }, []);

  useEffect(() => {
    avatar.current?.update({ definition, color, eyeColor, mood, followPointer });
  }, [definition, color, eyeColor, mood, followPointer]);

  return <span ref={host} aria-hidden className="block" style={{ width: size, height: size }} />;
}
