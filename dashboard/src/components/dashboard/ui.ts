export function cx(...values: Array<string | false | null | undefined>) {
  return values.filter(Boolean).join(" ");
}

export const dashboardCard = {
  standard: "dash-card dash-card-pad-standard",
  hero: "dash-card dash-card-pad-hero",
  compact: "dash-card dash-card-pad-compact",
  accentStandard: "dash-card dash-card-accent dash-card-pad-standard",
  accentHero: "dash-card dash-card-accent dash-card-pad-hero",
  accentCompact: "dash-card dash-card-accent dash-card-pad-compact",
  nested: "dash-card dash-card-nested dash-card-pad-standard",
  nestedCompact: "dash-card dash-card-nested dash-card-pad-compact",
  interactiveNestedCompact: "dash-card dash-card-nested dash-card-surface-muted dash-card-hover-emphasis dash-card-pad-compact",
  smallCompact: "dash-card dash-card-small dash-card-pad-compact",
  accentNestedCompact: "dash-card dash-card-accent dash-card-nested dash-card-pad-compact",
} as const;

export const dashboardBadge = {
  default: "dash-badge",
  large: "dash-badge dash-badge-lg",
} as const;

export const dashboardButton = {
  sm: "dash-btn dash-btn-sm",
  md: "dash-btn dash-btn-md",
} as const;
