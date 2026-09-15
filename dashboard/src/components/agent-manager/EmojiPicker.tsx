import { Suspense, lazy, useEffect, useRef, useState, type SyntheticEvent } from "react";
import { useI18n } from "../../i18n";

const EmojiPickerLibraryPanel = lazy(() => import("./EmojiPickerLibraryPanel"));

const SPRITE_ICON_FALLBACK_SRC = "/sprites/1-D-1.png";

export function StackedSpriteIcon({ sprites }: { sprites: [number, number] }) {
  const handleImageError = (event: SyntheticEvent<HTMLImageElement>) => {
    const image = event.currentTarget;
    image.onerror = null;
    image.src = SPRITE_ICON_FALLBACK_SRC;
  };

  return (
    <span className="relative inline-flex items-center" style={{ width: 22, height: 16 }}>
      <img
        src={`/sprites/${sprites[0]}-D-1.png`}
        alt=""
        className="absolute left-0 top-0 w-4 h-4 rounded-full object-cover"
        style={{ imageRendering: "pixelated", opacity: 0.85 }}
        onError={handleImageError}
      />
      <img
        src={`/sprites/${sprites[1]}-D-1.png`}
        alt=""
        className="absolute left-1.5 top-px w-4 h-4 rounded-full object-cover"
        style={{ imageRendering: "pixelated", zIndex: 1 }}
        onError={handleImageError}
      />
    </span>
  );
}

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

  const btnSize = size === "sm" ? "w-10 h-10 text-lg" : "w-14 h-10 text-xl";
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
            ? tr({ ko: `선택된 이모지: ${value}, 이모지 변경`, en: `Selected emoji: ${value}, change emoji` })
            : tr({ ko: "이모지 선택기 열기", en: "Open emoji picker" }))
        }
      >
        {value || "❓"}
      </button>
      {open && (
        <div
          role="dialog"
          aria-label={dialogLabel || tr({ ko: "이모지 선택", en: "Choose an emoji" })}
          className="absolute left-0 top-full z-[60] mt-1 overflow-hidden rounded-xl shadow-2xl"
          style={{
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
            border: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
          }}
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
