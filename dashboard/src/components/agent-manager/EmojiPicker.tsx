import { Suspense, lazy, useEffect, useRef, useState } from "react";
import { useI18n } from "../../i18n";

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
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const { t: tr } = useI18n();

  useEffect(() => {
    if (!open) return;
    const handler = (e: PointerEvent | MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
        buttonRef.current?.focus();
      }
    };
    const keyHandler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        setOpen(false);
        buttonRef.current?.focus();
      }
    };
    document.addEventListener("pointerdown", handler);
    document.addEventListener("mousedown", handler);
    document.addEventListener("keydown", keyHandler, true);
    return () => {
      document.removeEventListener("pointerdown", handler);
      document.removeEventListener("mousedown", handler);
      document.removeEventListener("keydown", keyHandler, true);
    };
  }, [open]);

  const btnSize = size === "sm" ? "w-11 h-11 text-lg" : "w-14 h-11 text-xl";
  const pickerWidth = size === "sm" ? 300 : 336;
  const pickerHeight = size === "sm" ? 360 : 420;
  const handleEmojiSelect = (emoji: string) => {
    onChange(emoji);
    setOpen(false);
    buttonRef.current?.focus();
  };

  return (
    <div className="relative" ref={ref}>
      <button
        id={id}
        ref={buttonRef}
        type="button"
        onClick={() => setOpen(!open)}
        className={`${btnSize} rounded-lg border flex items-center justify-center transition-all hover:scale-105 hover:shadow-md`}
        style={{ background: "var(--th-input-bg)", borderColor: "var(--th-input-border)" }}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={
          ariaLabel ||
          (value
            ? tr({
                ko: `선택된 아이콘: ${value}, 아이콘 변경`,
                en: `Selected icon: ${value}, change icon`,
                ja: `選択中のアイコン: ${value}、アイコンを変更`,
                zh: `已选图标: ${value}，更改图标`,
              })
            : tr({
                ko: "아이콘 선택기 열기",
                en: "Open icon picker",
                ja: "アイコン選択を開く",
                zh: "打开图标选择器",
              }))
        }
      >
        {value || "❓"}
      </button>
      {open && (
        <div
          role="dialog"
          aria-label={
            dialogLabel ||
            tr({ ko: "아이콘 선택", en: "Choose an icon", ja: "アイコンを選択", zh: "选择图标" })
          }
          className="fixed left-1/2 top-1/2 z-[110] max-h-[calc(100dvh-2rem)] max-w-[calc(100vw-1.5rem)] -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface shadow-xl"
        >
          <Suspense
            fallback={(
              <div
                className="flex items-center justify-center text-sm"
                style={{
                  color: "var(--th-text-muted)",
                  height: pickerHeight,
                  width: pickerWidth,
                  maxWidth: "calc(100vw - 2rem)",
                  maxHeight: "calc(100dvh - 4rem)",
                }}
              >
                {tr({
                  ko: "이모지 불러오는 중…",
                  en: "Loading emoji…",
                  ja: "絵文字を読み込み中…",
                  zh: "正在加载表情…",
                })}
              </div>
            )}
          >
            <EmojiPickerLibraryPanel
              height={pickerHeight}
              onSelect={handleEmojiSelect}
              width={pickerWidth}
              value={value}
            />
          </Suspense>
        </div>
      )}
    </div>
  );
}
