import { defineConfig } from "@playwright/test";

// The application intentionally does not depend on Node ambient types.
declare const process: { env: Record<string, string | undefined> };

const port = Number(process.env.PTY_BROWSER_PORT || 18748);
const dist = process.env.PTY_BROWSER_DIST || "dist";
const quotedDist = "'" + dist.replaceAll("'", "'\"'\"'") + "'";
export default defineConfig({
  outputDir: process.env.PTY_BROWSER_OUTPUT || "test-results",
  testDir: "./browser-tests",
  fullyParallel: false,
  workers: 1,
  timeout: 20_000,
  expect: { timeout: 5_000 },
  use: { baseURL: `http://127.0.0.1:${port}`, viewport: { width: 744, height: 1133 }, trace: "retain-on-failure" },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }, { name: "webkit", use: { browserName: "webkit" } }],
  webServer: {
    command: `npx vite preview --host 127.0.0.1 --port ${port} --strictPort --outDir ${quotedDist}`,
    url: `http://127.0.0.1:${port}`,
    reuseExistingServer: false,
  },
});
