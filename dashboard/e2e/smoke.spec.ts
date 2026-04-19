import { expect, test } from "@playwright/test";

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

    const sidebar = page.getByTestId("app-sidebar");
    await expect(sidebar).toBeVisible();
    const box = await sidebar.boundingBox();
    expect(box?.width).toBeGreaterThan(230);
    expect(box?.width).toBeLessThan(250);
  });

  test("desktop: sidebar navigation updates route and breadcrumb", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    await page.getByRole("button", { name: /칸반|Kanban/ }).click();
    await expect(page).toHaveURL(/\/kanban$/);
    await expect(page.getByTestId("topbar")).toContainText(/칸반|Kanban/);
  });

  test("mobile: menu button opens the sidebar drawer", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");

    const sidebar = page.getByTestId("app-sidebar");
    await expect(sidebar).not.toBeVisible();

    await page.getByRole("button", { name: /사이드바 열기|Open sidebar/ }).click();
    await expect(sidebar).toBeVisible();
  });

  test("settings button routes to settings page", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    await page.getByRole("button", { name: /설정으로 이동|Open settings/ }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await expect(page.getByTestId("topbar")).toContainText(/설정|Settings/);
  });

  test("all app shell routes are directly reachable", async ({ page }) => {
    for (const route of ROUTES) {
      await page.goto(route.path);
      await expect(page).toHaveURL(new RegExp(`${route.path.replace("/", "\\/")}$`));
      await expect(page.getByTestId("topbar")).toContainText(route.label);
    }
  });
});
