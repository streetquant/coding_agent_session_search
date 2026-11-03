import { defineConfig, devices } from "@playwright/test";
import path from "path";
import { fileURLToPath } from "url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
// A real native fixture is required for Pages acceptance. Ordinary HTML
// setup may create placeholders, so it cannot supply this proof mode.
const pagesProof = Boolean(process.env.CASS_PAGES_PROOF_FIXTURE);
const pagesProfile = Boolean(process.env.CASS_PAGES_PROFILE_SITE);
const pagesAcceptance = pagesProof || pagesProfile;

/**
 * Playwright configuration for HTML export E2E tests.
 * Tests verify that exported HTML files render correctly in real browsers.
 */
export default defineConfig({
  testDir: "./e2e",
  // Explicitly ignore test files outside e2e/ that use Playwright imports
  // but are meant to be run separately or are legacy tests
  // Note: accessibility tests previously ignored are now enabled (T4.5)
  testIgnore: ["**/html_export/**", "**/performance/**"],
  fullyParallel: !pagesAcceptance,
  grep: pagesAcceptance ? /Pages privacy proof|Pages loading profile/ : undefined,
  forbidOnly: !!process.env.CI,
  retries: pagesAcceptance ? 0 : process.env.CI ? 2 : 0,
  workers: pagesAcceptance || process.env.CI ? 1 : undefined,
  reporter: [
    ["list"],
    ["json", { outputFile: "e2e-results.json" }],
    ["html", { outputFolder: "e2e-report", open: "never" }],
    ["./e2e/reporters/jsonl-reporter.ts"],
  ],

  timeout: pagesAcceptance ? 180000 : 60000,
  expect: {
    timeout: 10000,
  },

  use: {
    baseURL: "file://",
    trace: pagesAcceptance ? "on" : "on-first-retry",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    // Use domcontentloaded for faster file:// URL navigation
    navigationTimeout: 30000,
    actionTimeout: 10000,
  },

  projects: [
    // Desktop browsers
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
      testIgnore: /mobile\/.*\.spec\.ts/,
    },
    {
      name: "firefox",
      use: { ...devices["Desktop Firefox"] },
      testIgnore: /mobile\/.*\.spec\.ts/,
    },
    {
      name: "webkit",
      use: { ...devices["Desktop Safari"] },
      testIgnore: /mobile\/.*\.spec\.ts/,
    },
    // Mobile devices - iOS (various screen sizes)
    {
      name: "iphone-12",
      use: { ...devices["iPhone 12"] },
    },
    {
      name: "iphone-13",
      use: { ...devices["iPhone 13"] },
    },
    {
      name: "iphone-14",
      use: { ...devices["iPhone 14"] },
    },
    // Mobile devices - Android
    {
      name: "pixel-5",
      use: { ...devices["Pixel 5"] },
    },
    {
      name: "pixel-7",
      use: { ...devices["Pixel 7"] },
    },
    {
      name: "galaxy-s9",
      use: { ...devices["Galaxy S9+"] },
    },
    // Low-end Android (320px width for edge case testing)
    {
      name: "low-end-android",
      use: {
        viewport: { width: 320, height: 568 },
        userAgent:
          "Mozilla/5.0 (Linux; Android 8.0; SM-G930F) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/80.0.3987.149 Mobile Safari/537.36",
        deviceScaleFactor: 2,
        isMobile: true,
        hasTouch: true,
      },
    },
    // Mobile-specific test projects (for targeted mobile testing)
    {
      name: "mobile-chrome",
      use: { ...devices["Pixel 5"] },
      testMatch: pagesAcceptance ? /export\/pages-pipeline-e2e\.spec\.ts/ : /mobile\/.*.spec.ts/,
    },
    {
      name: "mobile-safari",
      use: { ...devices["iPhone 12"] },
      testMatch: pagesAcceptance ? /export\/pages-pipeline-e2e\.spec\.ts/ : /mobile\/.*.spec.ts/,
    },
  ],

  globalSetup: pagesAcceptance ? undefined : path.join(__dirname, "e2e/setup/global-setup.ts"),
  globalTeardown: pagesAcceptance ? undefined : path.join(__dirname, "e2e/setup/global-teardown.ts"),
});
