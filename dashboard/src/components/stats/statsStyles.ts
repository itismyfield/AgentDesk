export const STATS_SHELL_STYLES = `
  .stats-shell .page {
    padding: 24px 28px 48px;
    max-width: 1440px;
    width: 100%;
    margin: 0 auto;
    min-width: 0;
  }

  .stats-shell .page-header {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 24px;
  }

  .stats-shell .page-title {
    font-family: var(--font-display);
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.5px;
    line-height: 1.2;
    color: var(--th-text-heading);
  }

  .stats-shell .page-sub {
    margin-top: 4px;
    font-size: 13px;
    color: var(--th-text-muted);
    line-height: 1.6;
  }

  .stats-shell .page-controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
  }

  .stats-shell .grid {
    display: grid;
    gap: 14px;
  }

  .stats-shell .grid-4 {
    grid-template-columns: repeat(4, minmax(0, 1fr));
  }

  .stats-shell .grid-2 {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }

  .stats-shell .grid-feature {
    grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
  }

  .stats-shell .grid-extra {
    grid-template-columns: minmax(0, 1fr) minmax(0, 0.94fr);
  }

  .stats-shell .stack {
    display: grid;
    gap: 14px;
  }

  .stats-shell .card {
    background: var(--th-surface);
    border: 1px solid var(--th-border-subtle);
    border-radius: 18px;
    overflow: hidden;
    box-shadow: 0 10px 30px color-mix(in srgb, var(--th-shadow-color) 8%, transparent);
  }

  .stats-shell .card-head {
    padding: 14px 16px 0;
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }

  .stats-shell .card-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12.5px;
    font-weight: 500;
    color: var(--th-text-secondary);
    letter-spacing: -0.1px;
  }

  .stats-shell .card-body {
    padding: 10px 16px 16px;
  }

  .stats-shell .metric {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .stats-shell .metric-value {
    font-family: var(--font-display);
    font-size: 28px;
    font-weight: 600;
    letter-spacing: -1px;
    line-height: 1.1;
    font-variant-numeric: tabular-nums;
  }

  .stats-shell .metric-sub {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--th-text-muted);
    font-variant-numeric: tabular-nums;
  }

  .stats-shell .seg {
    display: inline-flex;
    border: 1px solid var(--th-border-subtle);
    border-radius: 10px;
    padding: 2px;
    background: color-mix(in srgb, var(--th-surface-alt) 80%, transparent);
  }

  .stats-shell .seg button {
    padding: 4px 10px;
    border-radius: 8px;
    border: 0;
    background: transparent;
    color: var(--th-text-muted);
    font-size: 11.5px;
    font-variant-numeric: tabular-nums;
    transition: background 0.16s ease, color 0.16s ease;
  }

  .stats-shell .seg button.active {
    background: var(--th-surface);
    color: var(--th-text-primary);
    box-shadow: 0 1px 2px color-mix(in srgb, var(--th-shadow-color) 10%, transparent);
  }

  .stats-shell .chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--th-border-subtle);
    background: color-mix(in srgb, var(--th-surface-alt) 86%, transparent);
    color: var(--th-text-secondary);
    font-size: 11px;
    font-weight: 500;
    font-variant-numeric: tabular-nums;
  }

  .stats-shell .chip-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border: 1px solid var(--th-border-subtle);
    border-radius: 999px;
    background: color-mix(in srgb, var(--th-surface-alt) 86%, transparent);
    color: var(--th-text-secondary);
    font-size: 11px;
    font-weight: 500;
    font-variant-numeric: tabular-nums;
    transition:
      background 0.16s ease,
      color 0.16s ease,
      border-color 0.16s ease;
  }

  .stats-shell .chip-btn:hover {
    background: var(--th-surface);
    color: var(--th-text-primary);
  }

  .stats-shell .delta {
    display: inline-flex;
    align-items: center;
    min-height: 20px;
    padding: 1px 5px;
    border-radius: 4px;
    font-family: var(--font-mono);
    font-size: 11px;
    letter-spacing: -0.2px;
  }

  .stats-shell .delta.up {
    color: var(--ok);
    background: color-mix(in oklch, var(--ok) 14%, transparent);
  }

  .stats-shell .delta.down {
    color: var(--err);
    background: color-mix(in oklch, var(--err) 14%, transparent);
  }

  .stats-shell .delta.flat {
    color: var(--th-text-muted);
    background: var(--th-overlay-subtle);
  }

  .stats-shell .bar-track {
    height: 6px;
    overflow: hidden;
    border-radius: 3px;
    background: var(--th-overlay-subtle);
  }

  .stats-shell .bar-fill {
    height: 100%;
    border-radius: 3px;
    transition: width 0.6s cubic-bezier(0.22, 1, 0.36, 1);
  }

  .stats-shell .list-section {
    margin-bottom: 10px;
    font-size: 10.5px;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--th-text-muted);
  }

  .stats-shell .list-card {
    border: 1px solid var(--th-border-subtle);
    border-radius: 14px;
    background: var(--th-bg-surface);
    padding: 12px;
  }

  .stats-shell .list-card.tight {
    padding: 10px 12px;
  }

  .stats-shell .stats-inline-alert {
    border-color: color-mix(in oklch, var(--warn) 30%, var(--th-border) 70%);
    background:
      linear-gradient(
        180deg,
        color-mix(in oklch, var(--warn) 8%, var(--th-surface) 92%) 0%,
        var(--th-surface) 100%
      );
  }

  @media (max-width: 1024px) {
    .stats-shell .page-header {
      align-items: flex-start;
      flex-direction: column;
    }

    .stats-shell .grid-2,
    .stats-shell .grid-feature,
    .stats-shell .grid-extra {
      grid-template-columns: minmax(0, 1fr);
    }
  }

  @media (max-width: 768px) {
    .stats-shell .page {
      padding: 16px 16px calc(9rem + env(safe-area-inset-bottom));
    }

    .stats-shell .grid-4 {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (max-width: 520px) {
    .stats-shell .grid-4 {
      grid-template-columns: minmax(0, 1fr);
    }
  }
`;
