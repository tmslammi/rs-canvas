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
  let stopped = false;
  // Servers can disable document sync (plain static serving for tests); the
  // hello frame says so before any snapshot is sent.
  let syncEnabled = true;
  let lastHint = "";
  const publishHint = (force = false) => {
    if (!syncEnabled || !socket || socket.readyState !== WebSocket.OPEN) return;
    const hint = editor.view_state_json();
    if (!force && hint === lastHint) return;
    lastHint = hint;
    socket.send(JSON.stringify({ type: "hint", ...JSON.parse(hint) }));
  };

  // Hand the server the page's current document; the server reconciles it
  // with its mirror and answers with the merged result if anything differs.
  // Snapshots only travel on connect and resync — steady state is patches.
  const publishDocument = () => {
    if (!syncEnabled) return;
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(editor.snapshot_frame_bytes());
    }
  };

  // Deltas committed since the last drain travel as one binary patch per
  // gesture. Drain even when disconnected or when the server disabled sync,
  // so the wasm outbox cannot grow; the snapshot sent on reconnect catches
  // the server up.
  const publishChanges = () => {
    if (!syncEnabled || !socket || socket.readyState !== WebSocket.OPEN) {
      editor.discard_outbound();
      return;
    }
    const frame = editor.take_outbound_frame_bytes();
    if (!frame?.byteLength) return;
    socket.send(frame);
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

  // Document frames are binary, routed by their wire kind and decoded by wasm.
  // No string is built and JSON.parse/stringify never sees them.
  const handleBinary = (bytes) => {
    if (!syncEnabled) return;
    const kind = editor.wire_frame_kind(bytes);
    if (kind === 1) {
      if (editor.reconcile_bytes(bytes)) hooks.onRemoteChange?.();
      return;
    }
    if (kind === 2 && editor.merge_patch_bytes(bytes)) {
      hooks.onRemoteChange?.();
    }
  };

  const handle = (message) => {
    if (message.type === "workspace_changed") {
      void hooks.onWorkspaceChange?.(message);
      return;
    }
    if (message.type === "hello") {
      syncEnabled = message.sync !== false;
      hooks.onDiscussionChange?.(message.discussion, message.discussion_persisted);
      if (message.settings) {
        hooks.onSettingsChange?.(message.settings);
      }
      publishHint(true);
      // The upload waits for hello so a server with document sync disabled
      // receives nothing.
      publishDocument();
      return;
    }
    if (message.type === "synced") {
      // The acknowledgement carries the revision no correction patch needed
      // to repeat, so the next snapshot has an accurate base.
      if (Number.isSafeInteger(message.rev) && message.rev >= 0) {
        editor.set_wire_revision(message.rev);
      }
      return;
    }
    if (message.type === "discussion") {
      hooks.onDiscussionChange?.(message.discussion, message.discussion_persisted);
      return;
    }
    if (message.type === "settings" && message.settings) {
      hooks.onSettingsChange?.(message.settings);
      return;
    }
    if (message.type === "view") {
      if (reducedMotion()) message.animate = false;
      if (editor.apply_view_command(JSON.stringify(message))) {
        pumpAnimations();
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
  };

  const connect = () => {
    if (stopped) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const workspace = hooks.workspace || "default";
    const params = new URLSearchParams({ ws: workspace });
    socket = new WebSocket(`${scheme}://${location.host}/ws?${params}`);

    // Binary frames must arrive as ArrayBuffer, never Blob: the hot path
    // hands the bytes to wasm without async blob reads.
    socket.binaryType = "arraybuffer";

    socket.addEventListener("open", () => {
      retry = RETRY_MIN_MS;
      lastHint = "";
      window.__rsAgentConnected = true;
      // Register this page and its view immediately: the agent's
      // current-workspace resolution follows page activity. Hints are control
      // frames and stay JSON. The document upload happens on hello.
      publishHint(true);
    });

    socket.addEventListener("message", (event) => {
      if (typeof event.data === "string") {
        try {
          handle(JSON.parse(event.data));
        } catch {
          // Malformed frames are dropped; the bridge stays up.
        }
        return;
      }
      if (event.data instanceof ArrayBuffer) {
        handleBinary(new Uint8Array(event.data));
        return;
      }
      // Defensive: some hosts ignore binaryType. Still no string round trip.
      if (event.data instanceof Blob) {
        void event.data
          .arrayBuffer()
          .then((buffer) => handleBinary(new Uint8Array(buffer)));
      }
    });

    // An error is always followed by a close, which drives the retry.
    socket.addEventListener("close", () => {
      window.__rsAgentConnected = false;
      if (!stopped) window.setTimeout(connect, retry);
      retry = Math.min(retry * 2, RETRY_MAX_MS);
    });
  };

  connect();

  // Report transient view state (camera, viewport, selection) so the agent
  // can see what the user is looking at. Hints only flow when the server
  // asked for sync; they are never persisted.
  window.setInterval(() => {
    if (document.visibilityState === "visible") publishHint();
  }, 400);
  window.addEventListener("focus", () => publishHint(true));
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") publishHint(true);
  });

  // Send a settings patch to the server, which enforces and persists it.
  const patchSettings = (patch) => {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify({ type: "settings", patch }));
    }
  };

  const close = () => {
    stopped = true;
    window.__rsAgentConnected = false;
    socket?.close();
  };
  return { publishChanges, patchSettings, close };
}
