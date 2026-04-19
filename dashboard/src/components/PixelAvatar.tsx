import { buildPixelAvatarModel, normalizeAvatarSeed } from "../lib/pixel-avatar";

interface PixelAvatarProps {
  seed: number;
  size?: number;
  className?: string;
  rounded?: "full" | "xl" | "2xl";
  label?: string;
}

export default function PixelAvatar({
  seed,
  size = 28,
  className = "",
  rounded = "full",
  label,
}: PixelAvatarProps) {
  const model = buildPixelAvatarModel(normalizeAvatarSeed(seed));
  const roundedClass =
    rounded === "full"
      ? "rounded-full"
      : rounded === "xl"
        ? "rounded-xl"
        : "rounded-2xl";

  return (
    <div
      className={`${roundedClass} overflow-hidden bg-th-bg-surface flex-shrink-0 ${className}`}
      style={{ width: size, height: size }}
    >
      <svg
        viewBox="0 0 8 8"
        width={size}
        height={size}
        role="img"
        aria-label={label ?? "Pixel avatar"}
        shapeRendering="crispEdges"
        className="block h-full w-full"
        style={{ imageRendering: "pixelated" }}
      >
        <title>{label ?? "Pixel avatar"}</title>
        <rect width="8" height="8" fill={model.palette.background} />
        {model.pixels.map((pixel) => (
          <rect
            key={`${pixel.x}-${pixel.y}`}
            x={pixel.x}
            y={pixel.y}
            width="1"
            height="1"
            fill={pixel.color}
          />
        ))}
      </svg>
    </div>
  );
}
