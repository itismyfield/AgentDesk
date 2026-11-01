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
    const handler = (e: MouseEvent) => {
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
    document.addEventListener("mousedown", handler);
    document.addEventListener("keydown", keyHandler, true);
    return () => {
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
            ? tr({ ko: `선택된 아이콘: ${value}, 아이콘 변경`, en: `Selected icon: ${value}, change icon` })
            : tr({ ko: "아이콘 선택기 열기", en: "Open icon picker" }))
        }
      >
        {value || "❓"}
      </button>
      {open && (
        <div
          role="dialog"
          aria-label={dialogLabel || tr({ ko: "아이콘 선택", en: "Choose an icon" })}
          className="fixed left-1/2 top-1/2 z-[60] -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface shadow-xl"
        >
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
