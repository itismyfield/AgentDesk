import type { CSSProperties, MouseEventHandler } from "react";

interface BackdropProps {
  onClick?: MouseEventHandler<HTMLDivElement>;
  zIndex?: number;
  style?: CSSProperties;
}

export function Backdrop({ onClick, zIndex = 50, style }: BackdropProps) {
  return (
    <div
      onClick={onClick}
      aria-hidden="true"
      className="fixed inset-0 bg-black/45 backdrop-blur-sm transition-opacity"
      style={{ zIndex, ...style }}
    />
  );
}
