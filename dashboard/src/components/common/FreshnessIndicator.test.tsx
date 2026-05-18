import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { FreshnessIndicator } from "./FreshnessIndicator";
import { SYSTEM_HEALTH_TONES } from "../../theme/statusTokens";

describe("FreshnessIndicator", () => {
  it("shows '데이터 없음' and uses the unknown tone when timestamp is null", () => {
    const html = renderToStaticMarkup(<FreshnessIndicator timestamp={null} />);
    expect(html).toContain("데이터 없음");
    expect(html).toContain(SYSTEM_HEALTH_TONES.unknown.accent);
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
});
