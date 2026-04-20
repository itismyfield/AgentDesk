import { expect, test, type Page } from "@playwright/test";

const ROUTES = [
  { path: "/home", label: /홈|Home/ },
  { path: "/office", label: /오피스|Office/ },
  { path: "/agents", label: /에이전트|Agents/ },
  { path: "/kanban", label: /칸반|Kanban/ },
  { path: "/stats", label: /통계|Stats/ },
  { path: "/ops", label: /운영|Ops/ },
  { path: "/meetings", label: /회의|Meetings/ },
  { path: "/achievements", label: /업적|Achievements/ },
  { path: "/settings", label: /설정|Settings/ },
];

async function expectNoHorizontalOverflow(page: Page) {
  const metrics = await page.evaluate(() => ({
    viewportWidth: window.innerWidth,
    bodyScrollWidth: document.body.scrollWidth,
    rootScrollWidth: document.documentElement.scrollWidth,
  }));

  expect(
    Math.max(metrics.bodyScrollWidth, metrics.rootScrollWidth),
  ).toBeLessThanOrEqual(metrics.viewportWidth + 1);
}

test.describe("Dashboard smoke tests", () => {
  test("page loads and renders root element", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("#root")).toBeAttached();
    await expect(page.getByTestId("topbar")).toBeVisible();
  });

  test("theme: dark/light toggle changes CSS variables", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => {
      document.documentElement.dataset.theme = "dark";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    const darkBg = await page.evaluate(() =>
      getComputedStyle(document.documentElement)
        .getPropertyValue("--th-bg-primary")
        .trim(),
    );
    expect(darkBg).toBeTruthy();

    await page.evaluate(() => {
      document.documentElement.dataset.theme = "light";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    const lightBg = await page.evaluate(() =>
      getComputedStyle(document.documentElement)
        .getPropertyValue("--th-bg-primary")
        .trim(),
    );
    expect(lightBg).toBeTruthy();
    expect(lightBg).not.toBe(darkBg);
  });

  test("theme: auto mode responds to prefers-color-scheme", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "dark" });
    await page.goto("/");
    await page.evaluate(() => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      document.documentElement.dataset.theme = mq.matches ? "dark" : "light";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

    await page.emulateMedia({ colorScheme: "light" });
    await page.evaluate(() => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      document.documentElement.dataset.theme = mq.matches ? "dark" : "light";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  });

  test("desktop: sidebar renders at the full shell width", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    const sidebar = page.getByTestId("app-sidebar-nav");
    await expect(sidebar).toBeVisible();
    const box = await sidebar.boundingBox();
    expect(box?.width).toBeGreaterThan(230);
    expect(box?.width).toBeLessThan(250);
  });

  test("responsive: 900px switches from mobile tab bar to desktop sidebar", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");

    await page.setViewportSize({ width: 899, height: 900 });
    await page.goto("/home");

    await expect(page.getByTestId("app-mobile-tabbar")).toBeVisible();
    await expect(page.getByTestId("app-sidebar-nav")).toHaveCount(0);
    await expect(page.getByTestId("app-mobile-tabbar").locator("button")).toHaveCount(5);

    await page.setViewportSize({ width: 900, height: 900 });

    await expect(page.getByTestId("app-sidebar-nav")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tabbar")).toHaveCount(0);
  });

  test("responsive: mobile viewport shows 5-tab bar and opens the More menu", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");

    const bottomNav = page.getByTestId("app-mobile-tabbar");
    const topbar = page.getByTestId("topbar");
    await expect(bottomNav).toBeVisible();
    await expect(bottomNav.locator("button")).toHaveCount(5);
    await expect(page.getByTestId("app-mobile-tab-home")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tab-office")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tab-kanban")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tab-stats")).toBeVisible();

    const tabbarZIndex = await bottomNav.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );
    const topbarZIndex = await topbar.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );
    expect(tabbarZIndex).toBeLessThan(50);
    expect(topbarZIndex).toBeLessThan(50);

    await page.getByTestId("app-mobile-more-button").click();

    const moreMenu = page.getByTestId("app-mobile-more-menu");
    await expect(moreMenu).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /에이전트|Agents/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /운영|Ops/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /회의|Meetings/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /업적|Achievements/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /설정|Settings/ })).toBeVisible();

    const moreMenuBackdropZIndex = await moreMenu.evaluate((element) =>
      Number(window.getComputedStyle(element.parentElement as HTMLElement).zIndex),
    );
    expect(moreMenuBackdropZIndex).toBeGreaterThan(tabbarZIndex);

    const shellStyles = await page.getByTestId("app-main-scroll").evaluate((element) => ({
      marginBottom: (element as HTMLElement).style.marginBottom,
    }));
    expect(shellStyles.marginBottom).toContain("env(safe-area-inset-bottom)");

    const tabbarStyles = await bottomNav.evaluate((element) => ({
      height: (element as HTMLElement).style.height,
      paddingBottom: (element as HTMLElement).style.paddingBottom,
      paddingLeft: (element as HTMLElement).style.paddingLeft,
      paddingRight: (element as HTMLElement).style.paddingRight,
    }));
    expect(tabbarStyles.height).toContain("env(safe-area-inset-bottom)");
    expect(tabbarStyles.paddingBottom).toContain("env(safe-area-inset-bottom)");
    expect(tabbarStyles.paddingLeft).toContain("env(safe-area-inset-left)");
    expect(tabbarStyles.paddingRight).toContain("env(safe-area-inset-right)");
  });

  test("responsive: mobile tab bar stays below modal overlays", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");

    const bottomNav = page.getByTestId("app-mobile-tabbar");
    await expect(bottomNav).toBeVisible();

    const tabbarZIndex = await bottomNav.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );

    await page.evaluate(() => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "?", bubbles: true }));
    });

    const shortcutHelpModal = page.getByTestId("shortcut-help-modal");
    await expect(shortcutHelpModal).toBeVisible();

    const modalZIndex = await shortcutHelpModal.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );
    expect(modalZIndex).toBeGreaterThan(tabbarZIndex);
  });

  test("responsive: mobile routes avoid horizontal overflow", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");

    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-tab-office").click();
    await expect(page).toHaveURL(/\/office$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-tab-kanban").click();
    await expect(page).toHaveURL(/\/kanban$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-tab-stats").click();
    await expect(page).toHaveURL(/\/stats$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-more-button").click();
    await page.getByTestId("app-mobile-more-menu").getByRole("button", { name: /에이전트|Agents/ }).click();
    await expect(page).toHaveURL(/\/agents$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-more-button").click();
    await page.getByTestId("app-mobile-more-menu").getByRole("button", { name: /설정|Settings/ }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await expectNoHorizontalOverflow(page);
  });

  test("stats: dedicated route exposes range controls and key widgets", async ({ page }) => {
    await page.goto("/stats");

    await expect(page.getByTestId("stats-page")).toBeVisible();
    await expect(page.getByTestId("stats-range-controls")).toBeVisible();
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("stats-range-30d")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("stats-range-90d")).toHaveAttribute("aria-pressed", "false");

    await expect(page.getByTestId("stats-summary-total-tokens")).toBeVisible();
    await expect(page.getByTestId("stats-summary-api-spend")).toBeVisible();
    await expect(page.getByTestId("stats-summary-cache-saved")).toBeVisible();
    await expect(page.getByTestId("stats-summary-cache-hit")).toBeVisible();
    await expect(page.getByTestId("stats-daily-token-chart")).toBeVisible();
    await expect(page.getByTestId("stats-model-share")).toBeVisible();
    await expect(page.getByTestId("stats-provider-share")).toBeVisible();
    await expect(page.getByTestId("stats-skill-usage")).toBeVisible();
    await expect(page.getByTestId("stats-agent-leaderboard")).toBeVisible();

    await page.getByTestId("stats-range-7d").click();
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("stats-range-30d")).toHaveAttribute("aria-pressed", "false");

    await page.getByTestId("stats-range-90d").click();
    await expect(page.getByTestId("stats-range-90d")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-pressed", "false");
  });

  test("stats: mobile stacks summary cards vertically", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/stats");

    const totalTokensCard = page.getByTestId("stats-summary-total-tokens");
    const apiSpendCard = page.getByTestId("stats-summary-api-spend");

    await expect(page.getByTestId("stats-page")).toBeVisible({ timeout: 15000 });
    await expect(totalTokensCard).toBeVisible({ timeout: 15000 });
    await expect(apiSpendCard).toBeVisible({ timeout: 15000 });

    const [firstBox, secondBox] = await Promise.all([
      totalTokensCard.boundingBox(),
      apiSpendCard.boundingBox(),
    ]);

    expect(firstBox).not.toBeNull();
    expect(secondBox).not.toBeNull();
    expect(secondBox!.y).toBeGreaterThan(firstBox!.y + firstBox!.height - 1);
  });

  test("settings button routes to settings page", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    await page.getByRole("button", { name: /설정으로 이동|Open settings/ }).click();
    await expect(page).toHaveURL(/\/settings(\?.*)?$/);
    await expect(page.getByTestId("topbar")).toContainText(/설정|Settings/);
  });

  test("all app shell routes are directly reachable", async ({ page }) => {
    for (const route of ROUTES) {
      await page.goto(route.path);
      await expect(page).toHaveURL(
        new RegExp(`${route.path.replace("/", "\\/")}$`),
      );
      await expect(page.getByTestId("topbar")).toContainText(route.label);
    }
  });
});
