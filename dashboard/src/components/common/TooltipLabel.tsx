import { useEffect, useState } from "react";

interface TooltipLabelProps {
  text: string;
  tooltip: string;
  className?: string;
  onClick?: () => void;
}

export default function TooltipLabel({ text, tooltip, className, onClick }: TooltipLabelProps) {
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (!open) return;
    const t = setTimeout(() => setOpen(false), 1800);
    return () => clearTimeout(t);
  }, [open]);

  return (
    <span className={`relative flex max-w-full min-w-0 items-center gap-1 ${className || ""}`}>
      {onClick ? (
        <button
          type="button"
          className="min-w-0 flex-1 truncate text-left"
          title={tooltip}
          aria-label={tooltip}
          onMouseEnter={() => setOpen(true)}
          onMouseLeave={() => setOpen(false)}
          onFocus={() => setOpen(true)}
          onBlur={() => setOpen(false)}
          onTouchStart={(e) => { e.stopPropagation(); setOpen((v) => !v); }}
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
          title={tooltip}
          aria-label={tooltip}
          tabIndex={0}
          onMouseEnter={() => setOpen(true)}
          onMouseLeave={() => setOpen(false)}
          onFocus={() => setOpen(true)}
          onBlur={() => setOpen(false)}
          onTouchStart={(e) => { e.stopPropagation(); setOpen((v) => !v); }}
        >
          {text}
        </span>
      )}
      <span
        className="text-xs shrink-0"
        style={{ color: "var(--th-text-muted)" }}
        title={tooltip}
      >
        ⓘ
      </span>

      {open && (
        <span
          className="absolute left-0 top-full z-30 mt-1 max-w-[min(18rem,calc(100vw-3rem))] rounded-md px-2 py-1 text-[10px] whitespace-normal break-words"
          style={{
            background: "var(--th-card-bg)",
            color: "var(--th-text-primary)",
            border: "1px solid var(--th-border)",
            boxShadow: "0 4px 14px rgba(0,0,0,0.25)",
          }}
        >
          {tooltip}
        </span>
      )}
    </span>
  );
}
