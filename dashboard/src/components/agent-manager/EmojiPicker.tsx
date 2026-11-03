import { Suspense, lazy } from "react";
import { useI18n } from "../../i18n";
import PickerPopover from "./PickerPopover";

const EmojiPickerLibraryPanel = lazy(() => import("./EmojiPickerLibraryPanel"));

export default function EmojiPicker({
  id,
  value,
  onChange,
  size = "md",
  "aria-label": ariaLabel,
  dialogLabel,
}: {
  id?: string;
  value: string;
  onChange: (emoji: string) => void;
  size?: "sm" | "md";
  "aria-label"?: string;
  dialogLabel?: string;
}) {
  const { t: tr } = useI18n();
  const btnSize = size === "sm" ? "w-11 h-11 text-lg" : "w-14 h-11 text-xl";
  const pickerWidth = size === "sm" ? 300 : 336;
  const pickerHeight = size === "sm" ? 360 : 420;

  return (
    <PickerPopover
      id={id}
      triggerClassName={btnSize}
      triggerLabel={
        ariaLabel ||
        (value
          ? tr({ ko: `선택된 아이콘: ${value}, 아이콘 변경`, en: `Selected icon: ${value}, change icon` })
          : tr({ ko: "아이콘 선택기 열기", en: "Open icon picker" }))
      }
      dialogLabel={dialogLabel || tr({ ko: "아이콘 선택", en: "Choose an icon" })}
      trigger={value || "❓"}
    >
      {(close) => (
        <Suspense
          fallback={(
            <div
              className="flex items-center justify-center text-sm"
              style={{
                color: "var(--th-text-muted)",
                height: pickerHeight,
                width: pickerWidth,
              }}
            >
              Loading emoji...
            </div>
          )}
        >
          <EmojiPickerLibraryPanel
            height={pickerHeight}
            onSelect={(emoji) => {
              onChange(emoji);
              close();
            }}
            width={pickerWidth}
            value={value}
          />
        </Suspense>
      )}
    </PickerPopover>
  );
}
