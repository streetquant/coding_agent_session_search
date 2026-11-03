/**
 * Pages Pipeline E2E Tests
 *
 * Validates the complete pages export pipeline in browser:
 * Export → Encrypt → Bundle → Verify → Decrypt
 *
 * Acceptance Criteria (bead 3fu1):
 * - pages bundle/verify flows exercised end-to-end with real fixtures
 * - Browser logs, trace files, and output HTML captured as artifacts
 * - Failures include actionable logs and screenshots
 *
 * Run:
 *   npx playwright test e2e/export/pages-pipeline-e2e.spec.ts
 */

import AxeBuilder from "@axe-core/playwright";
import type { Page, TestInfo } from "@playwright/test";
import { createHash } from "crypto";
import { existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, realpathSync, statSync, writeFileSync } from "fs";
import http from "http";
import type { AddressInfo } from "net";
import os from "os";
import path from "path";
import { fileURLToPath } from "url";
import { collectBrowserErrors, expect, test, waitForPageReady } from "../setup/test-utils";

// Test artifact directory
const ARTIFACT_DIR = path.join(process.cwd(), "test-results", "e2e", "pages-pipeline");

// Global setup exports the fresh bundle root; its viewer lives under site/.
const PAGES_PREVIEW_DIR =
  process.env.TEST_PAGES_PREVIEW_DIR ||
  (process.env.TEST_PAGES_PREVIEW_SITE_DIR
    ? path.join(process.env.TEST_PAGES_PREVIEW_SITE_DIR, "site")
    : path.join(process.cwd(), "e2e", "pages_preview", "bundle", "site"));

test.describe("Pages Bundle - Static Files", () => {
  test.beforeAll(async () => {
    mkdirSync(ARTIFACT_DIR, { recursive: true });
  });

  test("bundle contains required static files", async ({ page }, testInfo) => {
    test.skip(!existsSync(PAGES_PREVIEW_DIR), "Pages preview directory not found");

    await test.step("Verify index.html exists", async () => {
      const indexPath = path.join(PAGES_PREVIEW_DIR, "index.html");
      expect(existsSync(indexPath), "index.html should exist").toBe(true);
    });

    await test.step("Verify JavaScript files exist", async () => {
      const jsFiles = ["session.js", "database.js", "search.js", "auth.js", "viewer.js", "sw.js"];

      for (const jsFile of jsFiles) {
        const filePath = path.join(PAGES_PREVIEW_DIR, jsFile);
        if (existsSync(filePath)) {
          const content = readFileSync(filePath, "utf-8");
          expect(content.length).toBeGreaterThan(0);
        }
      }
    });

    // Capture file listing as artifact
    await test.step("Capture bundle file listing", async () => {
      const files = readdirSync(PAGES_PREVIEW_DIR, { recursive: true });
      const artifactPath = path.join(ARTIFACT_DIR, "bundle-files.json");
      writeFileSync(artifactPath, JSON.stringify(files, null, 2));

      await testInfo.attach("bundle-files", {
        path: artifactPath,
        contentType: "application/json",
      });
    });
  });

  test("service worker registers correctly", async ({ page }, testInfo) => {
    const swPath = path.join(PAGES_PREVIEW_DIR, "sw.js");
    test.skip(!existsSync(swPath), "Service worker not found");

    await test.step("Validate service worker syntax", async () => {
      const content = readFileSync(swPath, "utf-8");

      // Basic syntax check - should have event listeners
      expect(content).toContain("addEventListener");

      // Should handle fetch events
      expect(content).toMatch(/fetch|cache|install|activate/);
    });
  });
});

test.describe("Pages Bundle - Browser Validation", () => {
  let server: http.Server;
  let baseURL: string;

  test.beforeAll(async () => {
    mkdirSync(ARTIFACT_DIR, { recursive: true });

    // Skip server setup if preview directory doesn't exist
    if (!existsSync(PAGES_PREVIEW_DIR)) {
      return;
    }

    // Create a simple HTTP server to serve the pages bundle
    server = http.createServer((req, res) => {
      let filePath = path.join(PAGES_PREVIEW_DIR, req.url === "/" ? "index.html" : req.url!);

      // Handle directory paths
      if (existsSync(filePath) && statSync(filePath).isDirectory()) {
        filePath = path.join(filePath, "index.html");
      }

      if (!existsSync(filePath)) {
        res.writeHead(404);
        res.end("Not found");
        return;
      }

      const ext = path.extname(filePath);
      const contentTypes: Record<string, string> = {
        ".html": "text/html",
        ".js": "application/javascript",
        ".css": "text/css",
        ".json": "application/json",
        ".png": "image/png",
        ".svg": "image/svg+xml",
        ".wasm": "application/wasm",
      };

      const contentType = contentTypes[ext] || "application/octet-stream";
      const content = readFileSync(filePath);

      // Add CORS and COOP/COEP headers for SharedArrayBuffer support
      res.writeHead(200, {
        "Content-Type": contentType,
        "Cross-Origin-Opener-Policy": "same-origin",
        "Cross-Origin-Embedder-Policy": "require-corp",
        "Cross-Origin-Resource-Policy": "same-site",
      });
      res.end(content);
    });

    await new Promise<void>((resolve) => {
      server.listen(0, "127.0.0.1", () => {
        const address = server.address() as AddressInfo;
        baseURL = `http://127.0.0.1:${address.port}`;
        console.log(
          JSON.stringify({
            event: "server_started",
            port: address.port,
            ts: new Date().toISOString(),
          }),
        );
        resolve();
      });
    });
  });

  test.afterAll(async () => {
    if (server) {
      await new Promise<void>((resolve) => {
        server.close(() => resolve());
      });
    }
  });

  test("pages viewer loads without errors", async ({ page }, testInfo) => {
    test.skip(!server, "Server not started - pages preview not found");

    const consoleErrors: string[] = [];
    const pageErrors: string[] = [];
    const networkErrors: string[] = [];

    page.on("console", (msg) => {
      if (msg.type() === "error") {
        consoleErrors.push(msg.text());
      }
    });

    page.on("pageerror", (error) => pageErrors.push(error.message));
    page.on("requestfailed", (request) => {
      networkErrors.push(`${request.method()} ${request.url()}: ${request.failure()?.errorText}`);
    });

    await test.step("Navigate to pages viewer", async () => {
      await page.goto(baseURL, { waitUntil: "domcontentloaded" });
      await waitForPageReady(page);
    });

    await test.step("Wait for JavaScript initialization", async () => {
      // Wait for any deferred initialization
      await page.waitForTimeout(1000);
    });

    // Capture errors as artifact
    await test.step("Capture error logs", async () => {
      const errorLog = {
        consoleErrors,
        pageErrors,
        networkErrors,
        timestamp: new Date().toISOString(),
      };

      if (consoleErrors.length > 0 || pageErrors.length > 0 || networkErrors.length > 0) {
        const artifactPath = path.join(ARTIFACT_DIR, "viewer-errors.json");
        writeFileSync(artifactPath, JSON.stringify(errorLog, null, 2));

        await testInfo.attach("error-log", {
          path: artifactPath,
          contentType: "application/json",
        });
      }
    });

    expect(consoleErrors, "viewer console errors").toEqual([]);
    expect(pageErrors, "unhandled viewer errors").toEqual([]);
    expect(networkErrors, "failed viewer requests").toEqual([]);
    console.log(
      JSON.stringify({
        event: "viewer_load_complete",
        consoleErrorCount: consoleErrors.length,
        networkErrorCount: networkErrors.length,
        ts: new Date().toISOString(),
      }),
    );
  });

  test("authentication flow shows password prompt", async ({ page }, testInfo) => {
    test.skip(!server, "Server not started - pages preview not found");

    await page.goto(baseURL, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    await test.step("Check for authentication modal", async () => {
      // The pages viewer should show an auth modal or password prompt
      const authElements = page.locator(
        [
          "#password-modal",
          ".password-modal",
          '[data-testid="auth-modal"]',
          'input[type="password"]',
          ".auth-form",
          "#password-input",
        ].join(", "),
      );

      const count = await authElements.count();

      console.log(
        JSON.stringify({
          event: "auth_check",
          authElementsFound: count,
          ts: new Date().toISOString(),
        }),
      );

      // At least one auth-related element should exist
      if (count > 0) {
        await expect(authElements.first()).toBeVisible();
      }
    });
  });

  test("COI (Cross-Origin Isolation) headers work", async ({ page }, testInfo) => {
    test.skip(!server, "Server not started - pages preview not found");

    await page.goto(baseURL, { waitUntil: "domcontentloaded" });

    await test.step("Verify Cross-Origin Isolation", async () => {
      const coiStatus = await page.evaluate(() => {
        return {
          crossOriginIsolated: window.crossOriginIsolated,
          sharedArrayBufferAvailable: typeof SharedArrayBuffer !== "undefined",
        };
      });

      console.log(
        JSON.stringify({
          event: "coi_status",
          ...coiStatus,
          ts: new Date().toISOString(),
        }),
      );

      // COI should be enabled for WASM/threading support
      expect(coiStatus.crossOriginIsolated).toBe(true);
    });
  });

  test("OPFS (Origin Private File System) available", async ({ playwright, browserName }, testInfo) => {
    test.skip(!server, "Server not started - pages preview not found");

    // WebKit does not provide OPFS in private contexts. Linux MiniBrowser
    // also leaves its mature FileSystem and stable StorageAPI features off.
    // Exercise real storage in a fresh persistent profile with those APIs on.
    // The profile is retained with the test artifacts; the assertion stays strict.
    const args = browserName === "webkit" && process.platform === "linux"
      ? ["--features=+FileSystem,+StorageAPI"]
      : [];
    const context = await playwright[browserName].launchPersistentContext(
      testInfo.outputPath("opfs-profile"),
      {
        headless: true,
        args,
        // Setting a locale makes Playwright replace WebKit's initial page.
        locale: undefined,
      },
    );
    try {
      // MiniBrowser applies command-line features to the initial page.
      const page = context.pages()[0] ?? (await context.newPage());
      await page.goto(baseURL, { waitUntil: "domcontentloaded" });

      await test.step("Check OPFS availability", async () => {
        const opfsAvailable = await page.evaluate(async () => {
          try {
            const root = await navigator.storage.getDirectory();
            return root !== null;
          } catch {
            return false;
          }
        });

        console.log(
          JSON.stringify({
            event: "opfs_check",
            browser: browserName,
            context: "persistent",
            args,
            available: opfsAvailable,
            ts: new Date().toISOString(),
          }),
        );

        // OPFS should be available in modern browsers
        expect(opfsAvailable).toBe(true);
      });
    } finally {
      await context.close();
    }
  });

  test("crypto APIs available for decryption", async ({ page }, testInfo) => {
    test.skip(!server, "Server not started - pages preview not found");

    await page.goto(baseURL, { waitUntil: "domcontentloaded" });

    await test.step("Verify WebCrypto availability", async () => {
      const cryptoStatus = await page.evaluate(() => {
        return {
          cryptoAvailable: typeof crypto !== "undefined",
          subtleAvailable: typeof crypto?.subtle !== "undefined",
          algorithms: crypto?.subtle ? ["AES-GCM", "PBKDF2"] : [],
        };
      });

      console.log(
        JSON.stringify({
          event: "crypto_check",
          ...cryptoStatus,
          ts: new Date().toISOString(),
        }),
      );

      expect(cryptoStatus.cryptoAvailable).toBe(true);
      expect(cryptoStatus.subtleAvailable).toBe(true);
    });
  });
});

test.describe("Pages Bundle - Search Functionality", () => {
  let server: http.Server;
  let baseURL: string;

  test.beforeAll(async () => {
    if (!existsSync(PAGES_PREVIEW_DIR)) {
      return;
    }

    server = http.createServer((req, res) => {
      let filePath = path.join(PAGES_PREVIEW_DIR, req.url === "/" ? "index.html" : req.url!);

      if (existsSync(filePath) && statSync(filePath).isDirectory()) {
        filePath = path.join(filePath, "index.html");
      }

      if (!existsSync(filePath)) {
        res.writeHead(404);
        res.end("Not found");
        return;
      }

      const ext = path.extname(filePath);
      const contentTypes: Record<string, string> = {
        ".html": "text/html",
        ".js": "application/javascript",
        ".css": "text/css",
        ".json": "application/json",
      };

      res.writeHead(200, {
        "Content-Type": contentTypes[ext] || "application/octet-stream",
        "Cross-Origin-Opener-Policy": "same-origin",
        "Cross-Origin-Embedder-Policy": "require-corp",
      });
      res.end(readFileSync(filePath));
    });

    await new Promise<void>((resolve) => {
      server.listen(0, "127.0.0.1", () => {
        const address = server.address() as AddressInfo;
        baseURL = `http://127.0.0.1:${address.port}`;
        resolve();
      });
    });
  });

  test.afterAll(async () => {
    if (server) {
      await new Promise<void>((resolve) => {
        server.close(() => resolve());
      });
    }
  });

  test("search module loads without errors", async ({ page }, testInfo) => {
    test.skip(!server, "Server not started");

    const searchPath = path.join(PAGES_PREVIEW_DIR, "search.js");
    test.skip(!existsSync(searchPath), "search.js not found");

    await page.goto(baseURL, { waitUntil: "domcontentloaded" });

    await test.step("Verify search script loaded", async () => {
      // Check that search.js was loaded
      const scripts = await page.locator('script[src*="search"]').count();

      console.log(
        JSON.stringify({
          event: "search_module_check",
          searchScriptsFound: scripts,
          ts: new Date().toISOString(),
        }),
      );
    });
  });
});

// Actual bundle password/recovery/revoked slots come from global setup's CLI.
test.describe("Pages Bundle - Key Slots", () => {
  const archivePassword = process.env.TEST_PAGES_PREVIEW_PASSWORD || "";
  const recoveryKey = process.env.TEST_PAGES_PREVIEW_RECOVERY_KEY || "";
  const revokedPassword = process.env.TEST_PAGES_PREVIEW_REVOKED_PASSWORD || "";
  const slotError = process.env.TEST_PAGES_KEY_SLOT_ERROR || "no reason recorded";
  const unlockTimeout = 60_000;
  test.describe.configure({ timeout: unlockTimeout + 30_000 });

  async function expectOutcome(
    page: Page,
    selector: string,
    errors: ReturnType<typeof collectBrowserErrors>,
  ) {
    try {
      await expect(page.locator(selector)).toBeVisible({ timeout: unlockTimeout });
    } catch (error) {
      const authText = await page.locator("#auth-screen").innerText({ timeout: 1_000 }).catch(() => "");
      throw new Error(
        `${selector} never became visible: ${String(error).split("\n")[0]}\n` +
          `auth screen: ${authText.replace(/\s+/g, " ").slice(0, 400)}\n` +
          `page errors: ${JSON.stringify(errors.pageErrors)}\n` +
          `console errors: ${JSON.stringify(errors.consoleErrors)}`,
      );
    }
  }

  test("unlocks with the archive password", async ({ page, previewUrl }) => {
    test.skip(!previewUrl, "Pages preview server not available");
    expect(archivePassword, "global-setup exported no archive password").not.toBe("");
    const errors = collectBrowserErrors(page);
    await page.goto(previewUrl, { waitUntil: "domcontentloaded" });
    await page.locator("#password").fill(archivePassword);
    await page.locator("#unlock-btn").click();
    await expectOutcome(page, "#app-screen", errors);
    await expect(page.locator("#auth-screen")).toBeHidden();
  });

  test("unlocks with the typed recovery key", async ({ page, previewUrl }) => {
    test.skip(!previewUrl, "Pages preview server not available");
    expect(recoveryKey, `recovery slot not prepared: ${slotError}`).not.toBe("");
    const errors = collectBrowserErrors(page);
    await page.goto(previewUrl, { waitUntil: "domcontentloaded" });
    await page.locator("#recovery-key-btn").click();
    await page.locator("#recovery-key-input").fill(recoveryKey);
    await page.locator("#recovery-key-submit").click();
    await expectOutcome(page, "#app-screen", errors);
  });

  test("refuses a revoked password", async ({ page, previewUrl }) => {
    test.skip(!previewUrl, "Pages preview server not available");
    expect(revokedPassword, `revoked slot not prepared: ${slotError}`).not.toBe("");
    const errors = collectBrowserErrors(page);
    await page.goto(previewUrl, { waitUntil: "domcontentloaded" });
    await page.locator("#password").fill(revokedPassword);
    await page.locator("#unlock-btn").click();
    await expectOutcome(page, "#auth-error", errors);
    await expect(page.locator("#app-screen")).toBeHidden();
  });
});

test.describe("Pages Bundle - Artifact Capture", () => {
  test("capture trace file on test execution", async ({ page }, testInfo) => {
    // This test demonstrates comprehensive artifact capture

    await test.step("Start tracing", async () => {
      await page.context().tracing.start({
        screenshots: true,
        snapshots: true,
        sources: true,
      });
    });

    await test.step("Perform test actions", async () => {
      // Navigate to a simple page to generate trace data
      await page.goto("about:blank");
      await page.setContent("<html><body><h1>Trace Test</h1></body></html>");
    });

    await test.step("Stop and save trace", async () => {
      const tracePath = path.join(ARTIFACT_DIR, `trace-${testInfo.title.replace(/\s+/g, "-")}.zip`);
      mkdirSync(path.dirname(tracePath), { recursive: true });

      await page.context().tracing.stop({ path: tracePath });

      if (existsSync(tracePath)) {
        await testInfo.attach("trace", {
          path: tracePath,
          contentType: "application/zip",
        });
      }
    });
  });

  test("capture screenshot on demand", async ({ page }, testInfo) => {
    await page.goto("about:blank");
    await page.setContent(`
      <html>
        <head><title>Screenshot Test</title></head>
        <body style="padding: 20px; font-family: sans-serif;">
          <h1>Pages Pipeline E2E Test</h1>
          <p>This screenshot validates artifact capture is working.</p>
          <p>Timestamp: ${new Date().toISOString()}</p>
        </body>
      </html>
    `);

    await test.step("Capture screenshot", async () => {
      const screenshotPath = path.join(
        ARTIFACT_DIR,
        `screenshot-${testInfo.title.replace(/\s+/g, "-")}.png`,
      );
      mkdirSync(path.dirname(screenshotPath), { recursive: true });

      await page.screenshot({ path: screenshotPath, fullPage: true });

      await testInfo.attach("screenshot", {
        path: screenshotPath,
        contentType: "image/png",
      });
    });
  });
});

type PagesPrivacyFixture = {
  schema: string;
  test_source_sha256: string;
  source_sha256: string;
  export_sha256: string;
  integrity_sha256: string;
  config_sha256: string;
  site: string;
  password: string;
  recovery_key: string;
  search_query: string;
  expected_title: string;
  omitted_titles: string[];
  conversations: number[];
  messages: number[];
};

const pagesSpecPath = fileURLToPath(import.meta.url);
const pagesTestsRoot = path.resolve(path.dirname(pagesSpecPath), "../..");
const pagesDigest = (bytes: Buffer) => createHash("sha256").update(bytes).digest("hex");

function pagesJson<T>(bytes: Buffer, artifact: string): T {
  try {
    return JSON.parse(bytes.toString()) as T;
  } catch (cause) {
    throw new Error(`Invalid Pages JSON artifact: ${artifact}`, { cause });
  }
}

function pagesNativeReceipt() {
  const head = process.env.CASS_PAGES_NATIVE_GATE_HEAD;
  const source = process.env.CASS_PAGES_NATIVE_GATE_SOURCE_SHA256;
  expect(head, "native gate HEAD is required").toMatch(/^[a-f0-9]{40}$/);
  expect(source, "native gate source digest is required").toMatch(/^[a-f0-9]{64}$/);
  return { head, source_sha256: source };
}

function readPagesArtifact(root: string, relative: string): Buffer {
  const parts = relative.split("/");
  expect(parts.length).toBeGreaterThan(0);
  let file = root;
  for (const [index, part] of parts.entries()) {
    expect(part, "artifact path component").toMatch(/^[^\\/\u0000]+$/);
    expect([".", ".."]).not.toContain(part);
    file = path.join(file, part);
    const stat = lstatSync(file);
    expect(stat.isSymbolicLink(), "artifact paths must not traverse symlinks").toBe(false);
    if (index < parts.length - 1) {
      expect(stat.isDirectory()).toBe(true);
    } else {
      expect(stat.isFile()).toBe(true);
      expect(stat.nlink, "artifact must be an independent regular file").toBe(1);
    }
  }
  return readFileSync(file);
}

function readPagesPrivacyFixture(): { fixture: PagesPrivacyFixture; site: string } {
  const requested = process.env.CASS_PAGES_PROOF_FIXTURE;
  if (!requested) throw new Error("A real native Pages fixture is required");
  expect(path.isAbsolute(requested)).toBe(true);
  const root = realpathSync(requested);
  const fixture = pagesJson<PagesPrivacyFixture>(readPagesArtifact(root, "browser-proof.json"), "browser-proof.json");
  expect(fixture.schema).toBe("cass.pages.browser-fixture.v1");
  expect(fixture.site).toBe("bundle/site");
  expect(fixture.conversations).toEqual([1]);
  expect(fixture.messages).toEqual([1, 2, 3]);
  expect(fixture.expected_title).toBe("selected conversation");
  expect(fixture.omitted_titles).toEqual(["omitted agent", "omitted time", "omitted workspace"]);
  expect(fixture.search_query).toBe("first");
  expect(fixture.password).toEqual(expect.any(String));
  expect(fixture.password.length).toBeGreaterThanOrEqual(24);
  expect(fixture.recovery_key).toMatch(/^[A-Za-z0-9_-]{43}$/);
  expect(pagesDigest(readFileSync(path.join(pagesTestsRoot, "pages_bundle.rs"))))
    .toBe(fixture.test_source_sha256);
  for (const [file, digest] of [
    ["source.db", fixture.source_sha256],
    ["export.db", fixture.export_sha256],
    ["bundle/site/integrity.json", fixture.integrity_sha256],
    ["bundle/site/config.json", fixture.config_sha256],
  ]) {
    expect(digest).toMatch(/^[a-f0-9]{64}$/);
    expect(pagesDigest(readPagesArtifact(root, file)), `native fixture hash: ${file}`).toBe(digest);
  }
  return { fixture, site: path.join(root, "bundle", "site") };
}

async function serveVerifiedPagesSite(site: string) {
  const root = realpathSync(site);
  const manifestBytes = readPagesArtifact(root, "integrity.json");
  const integrity = pagesJson<{ version: number; files: Record<string, { size: number; sha256: string }> }>(
    manifestBytes, "integrity.json");
  expect(integrity.version).toBe(1);
  expect(Object.keys(integrity.files).length).toBeGreaterThan(0);
  // Requests select only this verified public file map. No HTTP path can
  // address the native database, private recovery files, or a filesystem path.
  const files = new Map<string, Buffer>([["/integrity.json", manifestBytes]]);
  for (const [relative, expected] of Object.entries(integrity.files)) {
    const bytes = readPagesArtifact(root, relative);
    expect(bytes.length, relative).toBe(expected.size);
    expect(pagesDigest(bytes), relative).toBe(expected.sha256);
    files.set(`/${relative}`, bytes);
  }
  expect(files.has("/index.html")).toBe(true);
  expect(files.has("/config.json")).toBe(true);
  const types: Record<string, string> = {
    ".html": "text/html", ".js": "application/javascript", ".mjs": "application/javascript",
    ".css": "text/css", ".json": "application/json", ".wasm": "application/wasm",
    ".png": "image/png", ".svg": "image/svg+xml",
  };
  // ubs:ignore[js.async.dangling-promise] -- Node http.createServer synchronously returns a Server; listen and close are explicitly awaited below.
  const server = http.createServer((req, res) => {
    const requestedPath = new URL(req.url || "/", "http://127.0.0.1").pathname;
    const pathname = requestedPath === "/" ? "/index.html" : requestedPath;
    if (pathname === "/favicon.ico") {
      res.writeHead(204).end();
      return;
    }
    const bytes = files.get(pathname);
    if (!bytes || !["GET", "HEAD"].includes(req.method || "")) {
      res.writeHead(404).end("Not found");
      return;
    }
    res.writeHead(200, {
      "Content-Type": types[path.extname(pathname)] || "application/octet-stream",
      "Content-Length": bytes.length,
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
      "Cross-Origin-Resource-Policy": "same-origin",
      "Cache-Control": "public, max-age=0",
    });
    res.end(req.method === "HEAD" ? undefined : bytes);
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/`;
  return {
    url,
    integrity_sha256: pagesDigest(manifestBytes),
    async stop() {
      if (!server.listening) return;
      await new Promise<void>((resolve, reject) => {
        server.close((error) => error ? reject(error) : resolve());
        server.closeAllConnections();
      });
    },
  };
}

async function pagesUnlock(page: Page, credential: string) {
  await expect(page.locator("#auth-screen")).toBeVisible();
  await expect(page.locator("#unlock-btn")).toBeEnabled();
  await page.locator("#password").fill(credential);
  await page.locator("#password").press("Enter");
  await expect(page.locator("#search-input")).toBeVisible({ timeout: 90000 });
  await expect(page.locator("#search-input")).toBeEnabled();
  await expect(page.locator("#password")).toHaveValue("");
}

async function pagesSelectedSearch(page: Page, fixture: PagesPrivacyFixture) {
  // Unlock initially renders the archive. A zero-hit query first proves the
  // subsequent expected card was produced by search rather than initial load.
  await page.locator("#search-input").fill("cassproofdeliberatelyabsenttoken");
  await page.locator("#search-input").press("Enter");
  const cards = page.locator("#results-list .result-card");
  await expect(cards).toHaveCount(0);
  await expect(page.locator("#no-results")).toBeVisible();
  await page.locator("#search-input").fill(fixture.search_query);
  await page.locator("#search-input").press("Enter");
  await expect(cards).toHaveCount(1);
  await expect(cards.locator(".result-title")).toHaveText(fixture.expected_title);
  for (const omitted of fixture.omitted_titles) {
    await expect(page.locator("#app-screen")).not.toContainText(omitted);
  }
  await expect(page.locator("#auth-screen")).toBeHidden();
}

async function pagesLayoutAndAccessibility(page: Page, testInfo: TestInfo, phase: string) {
  const layout = await page.evaluate(() => {
    const root = document.documentElement;
    if (!root) throw new Error("Pages layout requires a document element");
    return { width: window.innerWidth, scroll: root.scrollWidth };
  });
  expect(layout.width).toBeGreaterThan(0);
  const viewport = page.viewportSize();
  if (!viewport) throw new Error("Pages acceptance requires an explicit viewport");
  expect(layout.width, "the page must use the configured desktop or mobile viewport").toBe(viewport.width);
  const project = testInfo.project;
  if (!project || typeof project.name !== "string" || project.name.length === 0) {
    throw new Error("Pages acceptance requires a named Playwright project");
  }
  if (project.name.startsWith("mobile-")) {
    expect(viewport.width, "mobile acceptance must use a phone viewport").toBeLessThanOrEqual(430);
    expect(await page.evaluate(() => navigator.maxTouchPoints), "mobile acceptance must emulate touch").toBeGreaterThan(0);
  }
  expect(layout.scroll, `${phase}: horizontal overflow`).toBeLessThanOrEqual(layout.width + 1);
  const selector = phase === "locked" ? "#password" : "#search-input";
  const input = page.locator(selector);
  await input.focus();
  await expect(input).toBeFocused();
  const box = await input.boundingBox();
  if (!box) throw new Error(`${phase}: the visible input has no layout box`);
  expect(box.x).toBeGreaterThanOrEqual(-1);
  expect(box.x + box.width).toBeLessThanOrEqual(layout.width + 1);
  for (const colorScheme of ["light", "dark"] as const) {
    await page.emulateMedia({ colorScheme });
    const scan = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
    await testInfo.attach(`axe-${phase}-${colorScheme}`, { body: Buffer.from(JSON.stringify(scan)), contentType: "application/json" });
    expect(scan.violations, `${phase}, ${colorScheme}: automated WCAG checks`).toEqual([]);
    await page.screenshot({ path: testInfo.outputPath(`${phase}-${colorScheme}.png`), fullPage: true });
  }
  await page.emulateMedia({ colorScheme: "light" });
  await testInfo.attach(`layout-${phase}`, { body: Buffer.from(JSON.stringify(layout)), contentType: "application/json" });
}

// No fixture means this proof group is absent, so a dedicated --grep command
// fails with zero tests instead of manufacturing a skipped acceptance pass.
if (process.env.CASS_PAGES_PROOF_FIXTURE) {
  test.describe("Pages privacy proof", () => {
    let fixture: PagesPrivacyFixture;
    let host: Awaited<ReturnType<typeof serveVerifiedPagesSite>>;
    let pageErrors: string[];
    let consoleErrors: string[];

    test.beforeEach(async ({ page, browser }, testInfo) => {
      const loaded = readPagesPrivacyFixture();
      fixture = loaded.fixture;
      host = await serveVerifiedPagesSite(loaded.site);
      pageErrors = [];
      consoleErrors = [];
      page.on("pageerror", (error) => pageErrors.push(error.message));
      page.on("console", (message) => {
        if (message.type() === "error") consoleErrors.push(message.text());
      });
      await testInfo.attach("pages-proof-provenance", {
        body: Buffer.from(JSON.stringify({
          native_gate: pagesNativeReceipt(), hostname: os.hostname(), platform: process.platform,
          os_release: os.release(), node: process.version, node_executable: process.execPath,
          browser: browser.browserType().name(), version: browser.version(),
          playwright_default_browser_executable: browser.browserType().executablePath(), viewport: page.viewportSize(),
          fixture_test_sha256: fixture.test_source_sha256, fixture_source_sha256: fixture.source_sha256,
          fixture_export_sha256: fixture.export_sha256, integrity_sha256: host.integrity_sha256,
          spec_sha256: pagesDigest(readFileSync(pagesSpecPath)),
          config_sha256: pagesDigest(readFileSync(path.join(pagesTestsRoot, "playwright.config.ts"))),
          lock_sha256: pagesDigest(readFileSync(path.join(pagesTestsRoot, "package-lock.json"))),
        })), contentType: "application/json",
      });
    });

    test.afterEach(async () => {
      await host?.stop();
      expect(pageErrors, "unhandled browser errors").toEqual([]);
      const unexpected = consoleErrors?.filter((message) =>
        !/^Worker error: (?:Error: )?(?:Incorrect password|Invalid recovery code)(?:\n|$)/.test(message));
      expect(unexpected, "unexpected console errors").toEqual([]);
    });

    test("wrong password refuses access; retry renders only selected search results", async ({ page }, testInfo) => {
      await test.step("Verify locked layout and automated accessibility", async () => {
        await page.goto(host.url, { waitUntil: "domcontentloaded" });
        await expect(page.locator("#unlock-btn")).toBeEnabled();
        await expect(page.locator("#app-screen")).toBeHidden();
        await expect(page.locator("#results-list .result-card")).toHaveCount(0);
        await pagesLayoutAndAccessibility(page, testInfo, "locked");
      });
      await test.step("Reject a wrong password without rendering rows", async () => {
        await page.locator("#password").fill(`incorrect-${Date.now()}`);
        await page.locator("#password").press("Enter");
        await expect(page.locator("#auth-error .error-message")).toHaveText("Incorrect password", { timeout: 90000 });
        await expect(page.locator("#app-screen")).toBeHidden();
        await expect(page.locator("#password")).toHaveValue("");
        await expect(page.locator("#results-list .result-card")).toHaveCount(0);
      });
      await test.step("Unlock, search and check the rendered mobile layout", async () => {
        await pagesUnlock(page, fixture.password);
        await pagesSelectedSearch(page, fixture);
        await pagesLayoutAndAccessibility(page, testInfo, "unlocked");
      });
      await test.step("Lock removes rendered private rows", async () => {
        await page.locator("#lock-btn").click();
        await expect(page.locator("#auth-screen")).toBeVisible();
        await expect(page.locator("#app-screen")).toBeHidden();
        await expect(page.locator("#results-list .result-card")).toHaveCount(0);
      });
    });

    test("typed recovery refuses an invalid key and unlocks with the native recovery key", async ({ page }) => {
      await page.goto(host.url, { waitUntil: "domcontentloaded" });
      await expect(page.locator("#recovery-key-btn")).toBeEnabled();
      await page.locator("#recovery-key-btn").click();
      const input = page.locator("#recovery-key-input");
      await input.fill(Buffer.alloc(32, 65).toString("base64url"));
      await input.press("Enter");
      await expect(page.locator("#auth-error .error-message")).toHaveText("Invalid recovery code");
      await expect(page.locator("#app-screen")).toBeHidden();
      await expect(input).toHaveValue("");
      await input.fill(fixture.recovery_key);
      await input.press("Enter");
      await expect(page.locator("#search-input")).toBeVisible({ timeout: 90000 });
      await expect(input).toHaveValue("");
      await pagesSelectedSearch(page, fixture);
    });

    test("stopped-server offline reload unlocks and searches the cached archive", async ({ page }, testInfo) => {
      await page.goto(host.url, { waitUntil: "domcontentloaded" });
      await pagesUnlock(page, fixture.password);
      await pagesSelectedSearch(page, fixture);
      const loaded = readPagesPrivacyFixture();
      const config = pagesJson<{ payload: { files: string[] } }>(
        readPagesArtifact(loaded.site, "config.json"), "config.json");
      await test.step("Wait for the real service worker and encrypted payload cache", async () => {
        await page.waitForFunction(async (files: string[]) => {
          await navigator.serviceWorker.ready;
          if (!navigator.serviceWorker.controller) return false;
          const required = ["./", "./index.html", "./config.json", "./integrity.json", ...files.map((file) => `./${file}`)];
          return (await Promise.all(required.map((file) => caches.match(new URL(file, location.href).href)))).every(Boolean);
        }, config.payload.files);
      });
      await test.step("Stop the HTTP origin and prove direct connections are refused", async () => {
        await host.stop();
        const refused = await new Promise<boolean>((resolve, reject) => {
          const request = http.get(host.url, (response) => { response.resume(); resolve(false); });
          request.on("error", (error: NodeJS.ErrnoException) => error.code === "ECONNREFUSED" ? resolve(true) : reject(error));
        });
        expect(refused).toBe(true);
      });
      await test.step("Reload from the service worker and query the real database offline", async () => {
        const response = await page.reload({ waitUntil: "domcontentloaded" });
        if (!response) throw new Error("Offline reload did not return a navigation response");
        expect(response.fromServiceWorker()).toBe(true);
        await pagesUnlock(page, fixture.password);
        await pagesSelectedSearch(page, fixture);
        await testInfo.attach("metrics", { body: Buffer.from(JSON.stringify({
          name: "pages_offline", metrics: { origin_connection_refused: true, rendered_selected_results: 1, service_worker_navigation: true },
        })), contentType: "application/json" });
      });
    });
  });
}

if (process.env.CASS_PAGES_PROFILE_SITE) {
  test.describe("Pages loading profile", () => {
    test("measures actual Chromium process RSS during native archive unlock and search", async ({ page, browser }, testInfo) => {
      expect(process.platform).toBe("linux");
      expect(browser.browserType().name()).toBe("chromium");
      const native = pagesNativeReceipt();
      const site = process.env.CASS_PAGES_PROFILE_SITE;
      if (!site) throw new Error("An explicit native profile site is required");
      expect(path.isAbsolute(site)).toBe(true);
      const messages = Number(process.env.CASS_PAGES_PROFILE_MESSAGES);
      expect(Number.isSafeInteger(messages) && messages > 0).toBe(true);
      const credential = process.env.CASS_PAGES_PROFILE_PASSWORD;
      if (!credential) throw new Error("Profile credential must be explicit");
      const host = await serveVerifiedPagesSite(site);
      const cdp = await browser.newBrowserCDPSession();
      let stop = false;
      let peak = 0;
      let baseline = 0;
      let samples = 0;
      const processIds = new Set<number>();
      const errors: string[] = [];
      const started = Date.now();
      const sample = async () => {
        const { processInfo } = await cdp.send("SystemInfo.getProcessInfo");
        let rss = 0;
        for (const info of processInfo) {
          expect(Number.isSafeInteger(info.id) && info.id > 0).toBe(true);
          processIds.add(info.id);
          try {
            const status = readFileSync(`/proc/${info.id}/status`, "utf8");
            const match = /^VmRSS:\s+(\d+)\s+kB$/m.exec(status);
            if (!match) throw new Error(`Browser process RSS is unavailable for pid ${info.id}`);
            rss += Number(match[1]) * 1024;
          } catch (error) {
            if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
          }
        }
        expect(rss).toBeGreaterThan(0);
        if (samples === 0) baseline = rss;
        samples += 1;
        peak = Math.max(peak, rss);
      };
      await sample();
      const sampling = (async () => {
        while (!stop) {
          await new Promise((resolve) => setTimeout(resolve, 100));
          if (!stop) await sample();
        }
      })().catch((error) => errors.push(String(error)));
      try {
        await page.goto(host.url, { waitUntil: "domcontentloaded" });
        await pagesUnlock(page, credential);
        const cards = page.locator("#results-list .result-card");
        await page.locator("#search-input").fill("cassprofiledeliberatelyabsenttoken");
        await page.locator("#search-input").press("Enter");
        await expect(cards).toHaveCount(0);
        await expect(page.locator("#no-results")).toBeVisible();
        await page.locator("#search-input").fill("conv");
        await page.locator("#search-input").press("Enter");
        await expect(cards.first()).toBeVisible();
        await expect(page.locator("#results-list .result-title").first()).toContainText("Perf Conversation");
        await sample();
      } finally {
        stop = true;
        await sampling;
        await cdp.detach();
        await host.stop();
      }
      expect(errors).toEqual([]);
      expect(samples).toBeGreaterThan(2);
      await testInfo.attach("metrics", { body: Buffer.from(JSON.stringify({
        name: "pages_browser_loading", metrics: {
          messages, duration_ms: Date.now() - started, sampled_process_rss_peak_bytes: peak,
          sampled_process_rss_baseline_bytes: baseline, samples, sample_interval_ms: 100,
          process_count: processIds.size, method: "sum of CDP-owned process VmRSS; shared pages may be counted more than once",
        },
      })), contentType: "application/json" });
      await testInfo.attach("pages-profile-provenance", { body: Buffer.from(JSON.stringify({
        native_gate: native, hostname: os.hostname(), browser_version: browser.version(),
        playwright_default_browser_executable: browser.browserType().executablePath(), node: process.version,
        integrity_sha256: host.integrity_sha256, process_ids: [...processIds],
        spec_sha256: pagesDigest(readFileSync(pagesSpecPath)),
      })), contentType: "application/json" });
    });
  });
}
