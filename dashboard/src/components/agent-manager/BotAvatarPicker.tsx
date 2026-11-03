import { BOT_AVATARS, botAvatarFor, botAvatarStill } from "../agent-avatar/botAvatar";
import PickerPopover from "./PickerPopover";

/** Picks the agent's bot avatar; the value stays the shape's emoji in `avatar_emoji`. */
export default function BotAvatarPicker({
  id,
  agentId,
  value,
  color,
  isKo,
  tr,
  onChange,
}: {
  id?: string;
  agentId: string;
  value: string;
  color: string;
  /** The enclosing form's language, so labels match the fields around it. */
  isKo: boolean;
  tr: (ko: string, en: string) => string;
  onChange: (emoji: string) => void;
}) {
  const current = botAvatarFor({ id: agentId, avatar_emoji: value });
  const nameOf = (avatar: typeof current) => (isKo ? avatar.nameKo : avatar.definition.name);

  return (
    <PickerPopover
      id={id}
      triggerClassName="w-14 h-11"
      triggerLabel={tr(`선택된 아바타: ${nameOf(current)}, 아바타 변경`, `Selected avatar: ${nameOf(current)}, change avatar`)}
      dialogLabel={tr("아바타 선택", "Choose an avatar")}
      trigger={<img src={botAvatarStill(current.definition, color)} alt="" width={32} height={32} draggable={false} />}
    >
      {(close) => (
        <div className="grid max-h-[min(70vh,26rem)] w-[min(92vw,21rem)] grid-cols-5 gap-1 overflow-y-auto p-2">
          {BOT_AVATARS.map((avatar) => {
            const selected = avatar.definition.id === current.definition.id;
            return (
              <button
                key={avatar.definition.id}
                type="button"
                aria-pressed={selected}
                aria-label={nameOf(avatar)}
                title={nameOf(avatar)}
                onClick={() => {
                  onChange(avatar.emoji);
                  close();
                }}
                className={`flex aspect-square min-h-11 items-center justify-center rounded-lg border ${
                  selected
                    ? "border-[var(--th-accent-primary)] bg-[var(--th-accent-primary-soft)]"
                    : "border-transparent hover:bg-[var(--th-bg-surface-hover)]"
                }`}
              >
                <img src={botAvatarStill(avatar.definition, color)} alt="" width={40} height={40} draggable={false} />
              </button>
            );
          })}
        </div>
      )}
    </PickerPopover>
  );
}
