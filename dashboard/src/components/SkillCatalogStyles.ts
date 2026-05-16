export const SKILL_CATALOG_SHELL_STYLES = `
  .skill-catalog-shell {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .skill-catalog-shell .page-header {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 14px;
    flex-wrap: wrap;
  }

  .skill-catalog-shell .page-title {
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.5px;
    line-height: 1.2;
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .page-sub {
    margin-top: 4px;
    max-width: 68ch;
    font-size: 13px;
    color: var(--th-text-muted);
    line-height: 1.65;
  }

  .skill-catalog-shell .card {
    border-radius: 18px;
    border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
    background: color-mix(in srgb, var(--th-card-bg) 94%, transparent);
    overflow: hidden;
  }

  .skill-catalog-shell .chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
    background: color-mix(in srgb, var(--th-bg-surface) 90%, transparent);
    font-size: 11px;
    font-weight: 500;
    color: var(--th-text-dim);
    font-variant-numeric: tabular-nums;
  }

  .skill-catalog-shell .chip .dot {
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: currentColor;
    opacity: 0.9;
  }

  .skill-catalog-shell .search-wrap {
    position: relative;
    width: min(100%, 260px);
  }

  .skill-catalog-shell.embedded .search-wrap {
    width: 100%;
  }

  .skill-catalog-shell .search-input {
    width: 100%;
    padding: 7px 10px 7px 30px;
    border-radius: 8px;
    border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
    background: color-mix(in srgb, var(--th-bg-surface) 88%, transparent);
    color: var(--th-text);
    font-size: 12.5px;
  }

  .skill-catalog-shell .metric-grid {
    display: grid;
    gap: 10px;
    grid-template-columns: repeat(4, minmax(0, 1fr));
  }

  .skill-catalog-shell .metric-card {
    padding: 12px 14px;
  }

  .skill-catalog-shell .metric-label {
    font-size: 10px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--th-text-muted);
    font-weight: 600;
  }

  .skill-catalog-shell .metric-value {
    margin-top: 8px;
    font-size: 20px;
    font-weight: 700;
    letter-spacing: -0.03em;
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .skill-tag-row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  .skill-catalog-shell .skill-tag {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border-radius: 999px;
    border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
    background: color-mix(in srgb, var(--th-bg-surface) 86%, transparent);
    font-size: 12px;
    color: var(--th-text-dim);
    transition: background 0.12s ease, border-color 0.12s ease, color 0.12s ease;
  }

  .skill-catalog-shell .skill-tag.active {
    border-color: color-mix(in srgb, var(--th-accent-info) 28%, var(--th-border) 72%);
    background: color-mix(in srgb, var(--th-accent-info) 12%, transparent);
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .skill-layout {
    display: grid;
    gap: 14px;
    grid-template-columns: minmax(0, 1.45fr) minmax(260px, 0.78fr);
  }

  .skill-catalog-shell .skill-grid {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 12px;
  }

  .skill-catalog-shell.embedded .skill-grid {
    grid-template-columns: 1fr;
  }

  .skill-catalog-shell .skill-card {
    padding: 14px;
  }

  .skill-catalog-shell .skill-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 10px;
  }

  .skill-catalog-shell .skill-name {
    font-size: 14px;
    font-weight: 600;
    color: var(--th-text-heading);
    line-height: 1.35;
    word-break: break-word;
  }

  .skill-catalog-shell .skill-desc {
    margin-top: 10px;
    font-size: 12.5px;
    line-height: 1.65;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .skill-foot {
    margin-top: 14px;
    display: grid;
    gap: 10px;
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }

  .skill-catalog-shell .skill-stat {
    border-radius: 14px;
    border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
    background: color-mix(in srgb, var(--th-card-bg) 88%, transparent);
    padding: 10px 12px;
  }

  .skill-catalog-shell .skill-stat-label {
    font-size: 10px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--th-text-muted);
    font-weight: 600;
  }

  .skill-catalog-shell .skill-stat-value {
    margin-top: 8px;
    font-size: 13px;
    font-weight: 600;
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .skill-usage {
    margin-top: 12px;
  }

  .skill-catalog-shell .skill-usage-meta {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-size: 11px;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .skill-usage-bar {
    margin-top: 8px;
    height: 5px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--th-border) 72%, transparent);
    overflow: hidden;
  }

  .skill-catalog-shell .skill-usage-fill {
    height: 100%;
    border-radius: inherit;
  }

  .skill-catalog-shell .highlights {
    padding: 14px;
    border-color: color-mix(in srgb, var(--th-accent-primary) 20%, var(--th-border) 80%);
    background: linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-primary-soft) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%);
  }

  .skill-catalog-shell .section-eyebrow {
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .section-copy {
    margin-top: 8px;
    font-size: 13px;
    line-height: 1.65;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .featured-list {
    margin-top: 14px;
    display: grid;
    gap: 10px;
  }

  .skill-catalog-shell .featured-item {
    border-radius: 14px;
    border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
    background: color-mix(in srgb, var(--th-card-bg) 88%, transparent);
    padding: 10px 12px;
  }

  @media (max-width: 1279px) {
    .skill-catalog-shell .skill-layout {
      grid-template-columns: 1fr;
    }

    .skill-catalog-shell .skill-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (max-width: 767px) {
    .skill-catalog-shell .metric-grid,
    .skill-catalog-shell .skill-grid,
    .skill-catalog-shell .skill-foot {
      grid-template-columns: 1fr;
    }

    .skill-catalog-shell .search-wrap {
      width: 100%;
    }
  }
`;
