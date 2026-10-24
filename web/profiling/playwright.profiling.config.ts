import { defineConfig } from "@playwright/test";

// Profiling harness: one worker, one long-lived app server, CDP artifacts.
// Kept separate from the e2e suite (and off its 4173+ port range) so a
// profile run never contends with correctness tests.
const PORT = Number(process.env.RS_CANVAS_PROFILE_PORT ?? 4180);

export default defineConfig({
  testDir: ".",
  testMatch: /profile\.spec\.ts/,
  timeout: 900_000,
  workers: 1,
  fullyParallel: false,
  reporter: "list",
  outputDir: "./out/test-results",
  use: {
    baseURL: `http://localhost:${PORT}`,
    viewport: { width: 1280, height: 800 },
    launchOptions: {
      args: ["--enable-precise-memory-info"],
    },
  },
  webServer: {
    // cwd is this config's directory: serve the worktree's web/ on its own
    // port, without workspace sync (profiles must be reproducible).
    command: `cargo run --manifest-path ../../Cargo.toml -p canvas-server -- --web .. --port ${PORT} --no-sync`,
    url: `http://localhost:${PORT}`,
    // A server already on the profile port may belong to another checkout.
    reuseExistingServer: false,
    timeout: 300_000,
  },
});
