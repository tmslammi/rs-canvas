import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  fullyParallel: true,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL: "http://localhost:4173",
    viewport: { width: 1280, height: 800 },
  },
  webServer: [
    {
      command:
        "cargo run --manifest-path ../Cargo.toml -p canvas-server -- --web . --port 4173 --no-sync",
      url: "http://localhost:4173",
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
    {
      // The agent spec gets its own server: view commands broadcast to every
      // connected page, so it must not share pages with other specs.
      command:
        "cargo run --manifest-path ../Cargo.toml -p canvas-server -- --web . --port 4174",
      url: "http://localhost:4174",
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
    {
      // Document sync needs a server of its own too: its spec asserts exact
      // element counts, and other specs' pages publish their documents here.
      command:
        "cargo run --manifest-path ../Cargo.toml -p canvas-server -- --web . --port 4175",
      url: "http://localhost:4175",
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
  ],
});
