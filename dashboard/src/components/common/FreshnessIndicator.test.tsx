import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { FreshnessIndicator } from "./FreshnessIndicator";
import { SYSTEM_HEALTH_TONES } from "../../theme/statusTokens";

describe("FreshnessIndicator", () => {
  it("renders the default '—' empty marker with the unknown tone when timestamp is null", () => {
    const html = renderToStaticMarkup(<FreshnessIndicator timestamp={null} />);
    expect(html).toContain("—");
    expect(html).toContain(SYSTEM_HEALTH_TONES.unknown.accent);
  });

  it("uses a caller-supplied emptyLabel when provided", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator timestamp={null} emptyLabel="No data" />,
    );
    expect(html).toContain("No data");
    expect(html).not.toContain("데이터 없음");
  });

  it("does NOT attach aria-live by default — even on the live ticking value", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator timestamp={Date.now() - 1_000} />,
    );
    expect(html).not.toContain("aria-live");
    expect(html).not.toContain("role=\"status\"");
  });

  it("renders '방금' with the healthy tone when timestamp is current", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator timestamp={Date.now()} />,
    );
    expect(html).toContain("방금");
    expect(html).toContain(SYSTEM_HEALTH_TONES.healthy.accent);
  });

  it("escalates to the warning tone past the stale threshold", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator
        timestamp={Date.now() - 45_000}
        staleAfterSeconds={30}
        criticalAfterSeconds={120}
      />,
    );
    expect(html).toContain("45초 전");
    expect(html).toContain(SYSTEM_HEALTH_TONES.warning.accent);
  });

  it("escalates to the critical tone past the critical threshold", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator
        timestamp={Date.now() - 5 * 60_000}
        staleAfterSeconds={30}
        criticalAfterSeconds={120}
      />,
    );
    expect(html).toContain("5분 전");
    expect(html).toContain(SYSTEM_HEALTH_TONES.critical.accent);
  });

  it("accepts seconds-since-epoch timestamps", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator timestamp={Math.floor(Date.now() / 1000)} />,
    );
    expect(html).toContain("방금");
  });

  it("omits the prefix label in compact mode", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator timestamp={Date.now()} compact />,
    );
    expect(html).not.toContain("업데이트");
  });

  it("snaps cleanly at the warn/critical boundaries", () => {
    const now = Date.now();
    const justUnderWarn = renderToStaticMarkup(
      <FreshnessIndicator
        timestamp={now - 29_500}
        staleAfterSeconds={30}
        criticalAfterSeconds={120}
      />,
    );
    expect(justUnderWarn).toContain(SYSTEM_HEALTH_TONES.healthy.accent);

    const justOverWarn = renderToStaticMarkup(
      <FreshnessIndicator
        timestamp={now - 30_500}
        staleAfterSeconds={30}
        criticalAfterSeconds={120}
      />,
    );
    expect(justOverWarn).toContain(SYSTEM_HEALTH_TONES.warning.accent);

    const justUnderCritical = renderToStaticMarkup(
      <FreshnessIndicator
        timestamp={now - 119_500}
        staleAfterSeconds={30}
        criticalAfterSeconds={120}
      />,
    );
    expect(justUnderCritical).toContain(SYSTEM_HEALTH_TONES.warning.accent);

    const justOverCritical = renderToStaticMarkup(
      <FreshnessIndicator
        timestamp={now - 120_500}
        staleAfterSeconds={30}
        criticalAfterSeconds={120}
      />,
    );
    expect(justOverCritical).toContain(SYSTEM_HEALTH_TONES.critical.accent);
  });

  it("attaches the live region on the first render when announceToneChange is on", () => {
    const html = renderToStaticMarkup(
      <FreshnessIndicator timestamp={Date.now()} announceToneChange />,
    );
    expect(html).toContain("role=\"status\"");
    expect(html).toContain("aria-live=\"polite\"");
  });
});
