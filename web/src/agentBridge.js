// The page's end of the agent bridge: a WebSocket to the canvas server.
//
// View commands (camera focus, highlights) arrive here and are applied by the
// wasm editor; document snapshots travel both ways and are merged by the
// merge-ready model. When the page is served statically — no server behind
// it — the bridge keeps retrying quietly, so the app still works offline and
// in the e2e suite.

const RETRY_MIN_MS = 500;
const RETRY_MAX_MS = 15000;

const reducedMotion = () =>
  window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches ?? false;

export function connectAgentBridge(editor, hooks = {}) {
  let retry = RETRY_MIN_MS;
  let pumping = false;
  let socket = null;
  // Servers can disable document sync (plain static serving for tests); the
  // hello frame says so before any snapshot is sent.
  let syncEnabled = true;
  let settings = null;

  // Hand the server the page's current document; the server reconciles it
  // with its mirror and answers with the merged result if anything differs.
  const publishDocument = () => {
    if (!syncEnabled) return;
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(
        JSON.stringify({ type: "snapshot", json: editor.snapshot_json() }),
      );
    }
  };

  const sendPresentDone = (id, cancelled) => {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify({ type: "present_done", id, cancelled }));
    }
  };

  const drainPresentOutcomes = () => {
    const raw = editor.presentation_outcomes();
    if (!raw) return;
    for (const outcome of JSON.parse(raw)) {
      sendPresentDone(outcome.id, outcome.cancelled);
    }
  };

  // The wasm editor exposes its animation as `advance_animation(now) -> more`;
  // the shell owns the requestAnimationFrame loop so the core stays clock-free.
  // The timestamp is `Date.now()` to match the core's clock, not rAF's origin.
  const pumpAnimations = () => {
    if (pumping) return;
    pumping = true;
    const step = () => {
      if (editor.advance_animation(Date.now())) {
        requestAnimationFrame(step);
      } else {
        pumping = false;
        drainPresentOutcomes();
      }
    };
    requestAnimationFrame(step);
  };

  const handle = (message) => {
    if (message.type === "hello") {
      syncEnabled = message.sync !== false;
      if (message.settings) {
        settings = message.settings;
        hooks.onSettingsChange?.(settings);
      }
      return;
    }
    if (message.type === "settings" && message.settings) {
      settings = message.settings;
      hooks.onSettingsChange?.(settings);
      return;
    }
    if (message.type === "view") {
      if (reducedMotion()) message.animate = false;
      if (editor.apply_view_command(JSON.stringify(message))) {
        pumpAnimations();
      }
      return;
    }
    if (message.type === "snapshot" && typeof message.json === "string") {
      if (!syncEnabled) return;
      if (editor.reconcile_json(message.json)) {
        hooks.onRemoteChange?.();
      }
      return;
    }
    if (message.type === "present") {
      // Reduced motion means no staged reveal: everything is already there.
      if (reducedMotion()) {
        sendPresentDone(message.id, false);
        return;
      }
      if (editor.start_presentation(JSON.stringify(message))) {
        pumpAnimations();
      } else {
        sendPresentDone(message.id, false);
      }
      return;
    }
    if (message.type === "screenshot") {
      let dataUrl = null;
      try {
        dataUrl = editor.export_viewport_png(message.scale ?? 1);
      } catch (error) {
        console.warn("[agent] screenshot failed", error);
      }
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(
          JSON.stringify({
            type: "screenshot_done",
            id: message.id,
            data_url: dataUrl,
          }),
        );
      }
      return;
    }
    console.debug("[agent]", message);
  };

  const connect = () => {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    socket = new WebSocket(`${scheme}://${location.host}/ws`);

    socket.addEventListener("open", () => {
      retry = RETRY_MIN_MS;
      window.__rsAgentConnected = true;
      console.info("[agent] bridge connected");
      publishDocument();
    });

    socket.addEventListener("message", (event) => {
      try {
        handle(JSON.parse(event.data));
      } catch {
        // Malformed frames are dropped; the bridge stays up.
      }
    });

    // An error is always followed by a close, which drives the retry.
    socket.addEventListener("close", () => {
      window.__rsAgentConnected = false;
      window.setTimeout(connect, retry);
      retry = Math.min(retry * 2, RETRY_MAX_MS);
    });
  };

  connect();

  // Report transient view state (camera, viewport, selection) so the agent
  // can see what the user is looking at. Hints only flow when the server
  // asked for sync; they are never persisted.
  let lastHint = "";
  window.setInterval(() => {
    if (!syncEnabled || !socket || socket.readyState !== WebSocket.OPEN) return;
    const hint = editor.view_state_json();
    if (hint === lastHint) return;
    lastHint = hint;
    socket.send(JSON.stringify({ type: "hint", ...JSON.parse(hint) }));
  }, 400);

  // Send a settings patch to the server, which enforces and persists it.
  const patchSettings = (patch) => {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify({ type: "settings", patch }));
    }
  };

  return { publishDocument, patchSettings };
}
