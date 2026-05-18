import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { StatusBadge } from "./StatusBadge";
import { SYSTEM_HEALTH_TONES } from "../../theme/statusTokens";

describe("StatusBadge", () => {
  it("renders a named tone with its background color", () => {
    const html = renderToStaticMarkup(<StatusBadge tone="healthy">정상</StatusBadge>);
    expect(html).toContain("정상");
    expect(html).toContain(SYSTEM_HEALTH_TONES.healthy.bg);
  });

  it("falls back to 'unknown' tone for an unrecognized tone name", () => {
    const html = renderToStaticMarkup(
      // @ts-expect-error — exercising the runtime fallback
      <StatusBadge tone="not-a-real-tone">??</StatusBadge>,
    );
    expect(html).toContain(SYSTEM_HEALTH_TONES.unknown.bg);
  });

  it("accepts a custom StatusTone object", () => {
    const html = renderToStaticMarkup(
      <StatusBadge
        tone={{ accent: "#abcdef", bg: "rgba(1,2,3,0.5)", text: "#fedcba" }}
      >
        custom
      </StatusBadge>,
    );
    expect(html).toContain("rgba(1,2,3,0.5)");
    expect(html).toContain("#fedcba");
  });

  it("renders the pulse dot when pulse is true", () => {
    const html = renderToStaticMarkup(
      <StatusBadge tone="critical" pulse>
        live
      </StatusBadge>,
    );
    expect(html).toContain("data-pulse=\"true\"");
    expect(html).toContain("adkStatusPulse");
  });

  it("does NOT render an aria-live region by default", () => {
    const html = renderToStaticMarkup(<StatusBadge tone="healthy">정상</StatusBadge>);
    expect(html).not.toContain("role=\"status\"");
    expect(html).not.toContain("aria-live");
  });

  it("opts into role=status + aria-live=polite when announce is set", () => {
    const html = renderToStaticMarkup(
      <StatusBadge tone="critical" announce>
        장애
      </StatusBadge>,
    );
    expect(html).toContain("role=\"status\"");
    expect(html).toContain("aria-live=\"polite\"");
  });
});
