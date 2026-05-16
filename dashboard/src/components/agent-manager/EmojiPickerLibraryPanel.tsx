import EmojiPickerReact, {
  EmojiStyle,
  Theme,
  type EmojiClickData,
} from "emoji-picker-react";

interface EmojiPickerLibraryPanelProps {
  height: number;
  onSelect: (emoji: string) => void;
  width: number;
}

export default function EmojiPickerLibraryPanel({
  height,
  onSelect,
  width,
}: EmojiPickerLibraryPanelProps) {
  const handleEmojiClick = (emojiData: EmojiClickData) => {
    onSelect(emojiData.emoji);
  };

  return (
    <EmojiPickerReact
      autoFocusSearch
      emojiStyle={EmojiStyle.NATIVE}
      height={height}
      lazyLoadEmojis
      onEmojiClick={handleEmojiClick}
      previewConfig={{ showPreview: false }}
      searchClearButtonLabel="Clear emoji search"
      searchPlaceholder="Search emoji"
      skinTonesDisabled={false}
      theme={Theme.DARK}
      width={width}
    />
  );
}
