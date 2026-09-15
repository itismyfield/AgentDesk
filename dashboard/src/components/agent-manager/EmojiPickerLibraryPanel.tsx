import { useEffect, useRef } from "react";
import EmojiPickerReact, {
  EmojiStyle,
  Theme,
  type EmojiClickData,
} from "emoji-picker-react";

interface EmojiPickerLibraryPanelProps {
  height: number;
  onSelect: (emoji: string) => void;
  width: number;
  value?: string;
}

export default function EmojiPickerLibraryPanel({
  height,
  onSelect,
  width,
  value,
}: EmojiPickerLibraryPanelProps) {
  const containerRef = useRef<HTMLDivElement>(null);

  const handleEmojiClick = (emojiData: EmojiClickData) => {
    onSelect(emojiData.emoji);
  };

  // emoji-picker-react (v4) renders each emoji as `button.epr-emoji` whose
  // visible text is the native emoji. The library exposes no selected state, so
  // we expose a complete `aria-pressed` state for every button.
  // aria-pressed is used to indicate the toggle state on an implicit role=button.
  // The match is exact (ignoring the FE0F variation selector) so composed
  // sequences that merely contain the same codepoint are not tagged.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const normalize = (text: string) => text.replace(/\uFE0F/g, "").trim();
    const target = value ? normalize(value) : null;

    const syncSelected = () => {
      container.querySelectorAll("button.epr-emoji").forEach((button) => {
        const isSelected = target !== null && normalize(button.textContent ?? "") === target;
        button.setAttribute("aria-pressed", isSelected ? "true" : "false");
      });
    };

    // Apply once for the emojis already mounted (observers do not replay the
    // mutations that happened before observe()), then keep in sync as the
    // library mounts/unmounts buttons during search, scroll and lazy loading.
    syncSelected();
    const observer = new MutationObserver(syncSelected);
    observer.observe(container, { childList: true, subtree: true });
    return () => observer.disconnect();
  }, [value]);

  return (
    <div ref={containerRef}>
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
    </div>
  );
}
