import { defineConfig } from "@playwright/test";
import { testBase, testPort } from "./e2e/server-address";

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  fullyParallel: true,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL: testBase(0),
    viewport: { width: 1280, height: 800 },
  },
  webServer: [
    {
      command:
        `cargo run --manifest-path ../Cargo.toml -p canvas-server -- --web . --port ${testPort(0)} --no-sync`,
      url: testBase(0),
      reuseExistingServer: !process.env.CI && !process.env.RS_CANVAS_TEST_PORT,
      timeout: 120_000,
    },
    {
      // The agent spec gets its own server: view commands broadcast to every
      // connected page, so it must not share pages with other specs.
      command:
        `cargo run --manifest-path ../Cargo.toml -p canvas-server -- --web . --port ${testPort(1)}`,
      url: testBase(1),
      reuseExistingServer: !process.env.CI && !process.env.RS_CANVAS_TEST_PORT,
      timeout: 120_000,
    },
    {
      // Document sync needs a server of its own too: its spec asserts exact
      // element counts, and other specs' pages publish their documents here.
      command:
        `cargo run --manifest-path ../Cargo.toml -p canvas-server -- --web . --port ${testPort(2)}`,
      url: testBase(2),
      reuseExistingServer: !process.env.CI && !process.env.RS_CANVAS_TEST_PORT,
      timeout: 120_000,
    },
  ],
});
