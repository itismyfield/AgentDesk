import { useEffect, useId, useRef, useState, type TouchEvent as ReactTouchEvent } from "react";

interface TooltipLabelProps {
  text: string;
  tooltip: string;
  className?: string;
  onClick?: () => void;
}

export default function TooltipLabel({ text, tooltip, className, onClick }: TooltipLabelProps) {
  const [open, setOpen] = useState(false);
  const tooltipId = useId();
  const containerRef = useRef<HTMLSpanElement | null>(null);
  const lastTouchAtRef = useRef(0);

  useEffect(() => {
    if (!open) return;

    const handlePointerDown = (event: PointerEvent) => {
      if (containerRef.current?.contains(event.target as Node)) return;
      setOpen(false);
    };

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };

    document.addEventListener("pointerdown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("pointerdown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [open]);

  const openedFromRecentTouch = () => Date.now() - lastTouchAtRef.current < 750;
  const handleMouseEnter = () => {
    if (openedFromRecentTouch()) return;
    setOpen(true);
  };
  const handleMouseLeave = () => {
    if (openedFromRecentTouch()) return;
    setOpen(false);
  };
  const handleTouchStart = (event: ReactTouchEvent<HTMLElement>) => {
    lastTouchAtRef.current = Date.now();
    event.preventDefault();
    event.stopPropagation();
    setOpen((value) => !value);
  };
  const handleFocus = () => setOpen(true);
  const handleBlur = () => setOpen(false);

  return (
    <span
      ref={containerRef}
      className={`relative flex max-w-full min-w-0 items-center gap-1 ${className || ""}`}
    >
      {onClick ? (
        <button
          type="button"
          className="min-w-0 flex-1 truncate text-left"
          title={tooltip}
          aria-describedby={tooltipId}
          aria-expanded={open}
          onMouseEnter={handleMouseEnter}
          onMouseLeave={handleMouseLeave}
          onFocus={handleFocus}
          onBlur={handleBlur}
          onTouchStart={handleTouchStart}
          onClick={(e) => {
            e.stopPropagation();
            onClick();
          }}
        >
          {text}
        </button>
      ) : (
        <span
          className="min-w-0 flex-1 truncate"
          role="button"
          title={tooltip}
          aria-describedby={tooltipId}
          aria-expanded={open}
          tabIndex={0}
          onMouseEnter={handleMouseEnter}
          onMouseLeave={handleMouseLeave}
          onFocus={handleFocus}
          onBlur={handleBlur}
          onTouchStart={handleTouchStart}
        >
          {text}
        </span>
      )}
      <span
        className="text-xs shrink-0"
        style={{ color: "var(--th-text-muted)" }}
        title={tooltip}
        aria-hidden="true"
      >
        ⓘ
      </span>

      <span
        id={tooltipId}
        role="tooltip"
        className={`absolute left-0 top-full mt-1 max-w-[min(18rem,calc(100vw-3rem))] rounded-md px-2 py-1 text-[10px] whitespace-normal break-words transition-opacity ${open ? "z-50 opacity-100" : "pointer-events-none -z-10 opacity-0"}`}
        style={{
          background: "var(--th-card-bg)",
          color: "var(--th-text-primary)",
          border: "1px solid var(--th-border)",
          boxShadow: "0 4px 14px rgba(0,0,0,0.25)",
        }}
      >
        {tooltip}
      </span>
    </span>
  );
}
