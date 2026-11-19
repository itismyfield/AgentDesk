import { describe, expect, it } from "vitest";
import { PRIMARY_ROUTES, findRouteByPath } from "./routes";
import { MOBILE_PRIMARY_ROUTE_IDS } from "./shellNavigationConfig";

describe("v3 route contract", () => {
  it("lists the menu in rail order without ops", () => {
    expect(PRIMARY_ROUTES.map((route) => route.id)).toEqual(["home", "campaigns", "agents", "routines", "audit", "stats", "settings"]);
    expect(MOBILE_PRIMARY_ROUTE_IDS).toEqual(["home", "campaigns", "agents", "audit"]);
    expect(Object.fromEntries(PRIMARY_ROUTES.map((route) => [route.shortcutKey, route.id]))).toEqual({
      "1": "home", "2": "campaigns", "3": "agents", "4": "routines", "5": "audit", "6": "stats", "9": "settings",
    });
  });

  it("keeps ops and its alias reachable by URL", () => {
    expect(findRouteByPath("/ops")?.id).toBe("ops");
    expect(findRouteByPath("/control")?.id).toBe("ops");
    expect(findRouteByPath("/audit/")?.id).toBe("audit");
  });
});
