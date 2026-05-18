import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { WidgetState } from "./WidgetState";
import { SYSTEM_HEALTH_TONES } from "../../theme/statusTokens";

describe("WidgetState", () => {
  it("uses the info tone and sets role='status' for loading", () => {
    const html = renderToStaticMarkup(<WidgetState kind="loading" title="동기화 중" />);
    expect(html).toContain("동기화 중");
    expect(html).toContain('role="status"');
    expect(html).toContain('data-widget-state="loading"');
    expect(html).toContain(SYSTEM_HEALTH_TONES.info.accent);
  });

  it("uses the critical tone and role='alert' for errors", () => {
    const html = renderToStaticMarkup(
      <WidgetState kind="error" title="실패" description="connection refused" />,
    );
    expect(html).toContain('role="alert"');
    expect(html).toContain(SYSTEM_HEALTH_TONES.critical.accent);
    expect(html).toContain("connection refused");
  });

  it("renders the empty state with the idle tone", () => {
    const html = renderToStaticMarkup(<WidgetState kind="empty" title="데이터 없음" />);
    expect(html).toContain(SYSTEM_HEALTH_TONES.idle.accent);
  });

  it("renders an action node when provided", () => {
    const html = renderToStaticMarkup(
      <WidgetState
        kind="error"
        title="failed"
        action={<button>retry</button>}
      />,
    );
    expect(html).toContain("retry");
  });

  it("respects a tone override", () => {
    const html = renderToStaticMarkup(
      <WidgetState kind="empty" title="warn override" tone="warning" />,
    );
    expect(html).toContain(SYSTEM_HEALTH_TONES.warning.accent);
  });
});
