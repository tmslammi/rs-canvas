import init, { Editor, load_saved_json } from "./wasm/canvas_wasm.js";
import { createStylePanel } from "./stylePanel.js";
import { createAgentPanel } from "./agentPanel.js";
import { createChatPanel } from "./chatPanel.js";
import { connectAgentBridge } from "./agentBridge.js";

const TOOLS = [
  { name: "select", label: "Select", glyph: "↖", shortcut: "V" },
  { name: "hand", label: "Hand", glyph: "✋", shortcut: "H" },
  { name: "rectangle", label: "Rectangle", glyph: "▭", shortcut: "R" },
  { name: "diamond", label: "Diamond", glyph: "◇", shortcut: "D" },
  { name: "ellipse", label: "Ellipse", glyph: "◯", shortcut: "O" },
  { name: "freehand", label: "Draw", glyph: "✎", shortcut: "P" },
  { name: "line", label: "Line", glyph: "╱", shortcut: "L" },
  { name: "arrow", label: "Arrow", glyph: "→", shortcut: "A" },
  { name: "text", label: "Text", glyph: "T", shortcut: "T" },
  { name: "frame", label: "Frame", glyph: "⬚", shortcut: "F" },
  { name: "eraser", label: "Eraser", glyph: "⌦", shortcut: "E" },
];

async function loadFonts() {
  const face = new FontFace("ComicShanns", "url(/fonts/ComicShanns-Regular.ttf)");
  await face.load();
  document.fonts.add(face);
}

async function boot() {
  await init();
  await loadFonts();

  const scene = document.getElementById("scene");
  const grid = document.getElementById("grid");
  const overlay = document.getElementById("overlay");
  const toolbar = document.getElementById("toolbar");
  const stylePanelHost = document.getElementById("style-panel");
  const editor = new Editor(scene, overlay, grid);
  const stylePanel = createStylePanel(stylePanelHost, editor);

  const toast = document.createElement("div");
  toast.id = "toast";
  document.body.appendChild(toast);
  let toastTimer = 0;
  const showToast = (message, isError = false) => {
    toast.textContent = message;
    toast.classList.toggle("error", isError);
    toast.classList.add("visible");
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => toast.classList.remove("visible"), 3200);
  };

  // -- grid and snapping (per-browser preferences) ------------------------

  const GRID_KEY = "rs-canvas-grid";
  const SNAP_KEY = "rs-canvas-snap";
  const GRID_SIZE_KEY = "rs-canvas-grid-size";
  const ZOOM_KEY = "rs-canvas-zoom-sensitivity";
  const GRID_SIZES = [10, 20, 40, 80];
  const ZOOM_SENSITIVITIES = [0.5, 1, 2, 4];

  const gridDivider = document.createElement("div");
  gridDivider.className = "divider";

  const gridButton = document.createElement("button");
  gridButton.textContent = "#";
  gridButton.title = "Grid — G";
  const gridSizeButton = document.createElement("button");
  const snapButton = document.createElement("button");
  snapButton.textContent = "🧲";
  snapButton.title = "Snap to grid — ⇧G";
  const zoomButton = document.createElement("button");

  const updateGridSizeButton = () => {
    const size = Math.round(editor.grid_size());
    gridSizeButton.textContent = String(size);
    gridSizeButton.title = `Grid size — ${size} world units (click to change)`;
  };

  const updateZoomButton = () => {
    const value = editor.zoom_sensitivity();
    zoomButton.textContent = `${value}×`;
    zoomButton.title = `Zoom speed — ${value}× (⌘/Ctrl + wheel)`;
  };

  const persist = (key, value) => {
    try {
      localStorage.setItem(key, value ? "1" : "0");
    } catch {
      // Private mode: the preference just won't survive a reload.
    }
  };

  const setGridVisible = (visible, remember = true) => {
    editor.set_grid_visible(visible);
    gridButton.classList.toggle("active", visible);
    if (remember) persist(GRID_KEY, visible);
  };

  const setSnapEnabled = (enabled, remember = true) => {
    editor.set_snap_enabled(enabled);
    snapButton.classList.toggle("active", enabled);
    if (remember) persist(SNAP_KEY, enabled);
  };

  const setGridSize = (size, remember = true) => {
    editor.set_grid_size(size);
    updateGridSizeButton();
    if (remember) {
      try {
        localStorage.setItem(GRID_SIZE_KEY, String(Math.round(editor.grid_size())));
      } catch {
        // Preference is optional.
      }
    }
  };

  gridButton.addEventListener("click", () => {
    const next = !editor.grid_visible();
    setGridVisible(next);
    showToast(next ? "Grid on" : "Grid off");
  });
  gridSizeButton.addEventListener("click", () => {
    const current = Math.round(editor.grid_size());
    const index = GRID_SIZES.indexOf(current);
    const next = GRID_SIZES[(index + 1) % GRID_SIZES.length];
    setGridSize(next);
    showToast(`Grid size ${next}`);
  });
  snapButton.addEventListener("click", () => {
    const next = !editor.snap_enabled();
    setSnapEnabled(next);
    showToast(next ? "Snap to grid on" : "Snap to grid off");
  });
  zoomButton.addEventListener("click", () => {
    const current = editor.zoom_sensitivity();
    const index = ZOOM_SENSITIVITIES.indexOf(current);
    const next = ZOOM_SENSITIVITIES[(index + 1) % ZOOM_SENSITIVITIES.length];
    editor.set_zoom_sensitivity(next);
    updateZoomButton();
    try {
      localStorage.setItem(ZOOM_KEY, String(next));
    } catch {
      // Preference is optional.
    }
    showToast(`Zoom speed ${next}×`);
  });

  try {
    setGridVisible(localStorage.getItem(GRID_KEY) === "1", false);
    setSnapEnabled(localStorage.getItem(SNAP_KEY) === "1", false);
    const storedSize = Number(localStorage.getItem(GRID_SIZE_KEY));
    if (GRID_SIZES.includes(storedSize)) {
      setGridSize(storedSize, false);
    } else {
      updateGridSizeButton();
    }
    const storedZoom = Number(localStorage.getItem(ZOOM_KEY));
    if (ZOOM_SENSITIVITIES.includes(storedZoom)) {
      editor.set_zoom_sensitivity(storedZoom);
    }
    updateZoomButton();
  } catch {
    // Preferences are optional.
    updateGridSizeButton();
    updateZoomButton();
  }

  let updateCursor = () => {
    const cursor = editor.cursor();
    if (scene.style.cursor !== cursor) {
      scene.style.cursor = cursor;
    }
  };

  const redraw = () => {
    editor.resize();
    editor.render();
  };

  // -- toolbar ------------------------------------------------------------

  const toolButtons = new Map();
  for (const tool of TOOLS) {
    const button = document.createElement("button");
    button.textContent = tool.glyph;
    button.title = `${tool.label} — ${tool.shortcut}`;
    button.addEventListener("click", () => {
      if (editor.active_tool() === tool.name) return;
      editor.set_tool(tool.name);
      refreshToolbar();
      updateCursor();
    });
    button.addEventListener("dblclick", () => {
      editor.toggle_tool_lock();
      refreshToolbar();
      showToast(
        editor.is_tool_locked()
          ? `${tool.label} locked — keeps drawing`
          : `${tool.label} unlocked`,
      );
    });
    toolbar.appendChild(button);
    toolButtons.set(tool.name, button);
  }

  const divider = document.createElement("div");
  divider.className = "divider";
  toolbar.appendChild(divider);

  const undoButton = document.createElement("button");
  undoButton.textContent = "↶";
  undoButton.title = "Undo — ⌘Z";
  undoButton.addEventListener("click", () => {
    editor.undo();
    refreshToolbar();
  });

  const redoButton = document.createElement("button");
  redoButton.textContent = "↷";
  redoButton.title = "Redo — ⇧⌘Z";
  redoButton.addEventListener("click", () => {
    editor.redo();
    refreshToolbar();
  });

  toolbar.appendChild(undoButton);
  toolbar.appendChild(redoButton);

  // -- files --------------------------------------------------------------

  const fileDivider = document.createElement("div");
  fileDivider.className = "divider";
  toolbar.appendChild(fileDivider);

  const newButton = document.createElement("button");
  newButton.textContent = "✚";
  newButton.title = "New drawing";
  newButton.addEventListener("click", () => {
    editor.new_document();
    refreshToolbar();
    showToast("New drawing");
  });

  const seedButton = document.createElement("button");
  seedButton.textContent = "🌱";
  seedButton.title = "Seed demo";
  seedButton.addEventListener("click", () => {
    editor.seed_demo();
    refreshToolbar();
    updateCursor();
    showToast("Demo architecture loaded — a VPC with GKE, Cloud SQL, Cloud Storage and Pub/Sub");
  });

  toolbar.appendChild(newButton);
  toolbar.appendChild(seedButton);
  toolbar.appendChild(gridDivider);
  toolbar.appendChild(gridButton);
  toolbar.appendChild(gridSizeButton);
  toolbar.appendChild(snapButton);
  toolbar.appendChild(zoomButton);

  // -- agent settings -----------------------------------------------------

  const agentPanel = createAgentPanel(document.getElementById("app"), {
    onChange: (patch) => bridge?.patchSettings(patch),
  });
  const chatPanel = createChatPanel(document.getElementById("app"));
  const agentDivider = document.createElement("div");
  agentDivider.className = "divider";
  const agentButton = document.createElement("button");
  agentButton.textContent = "◎";
  agentButton.title = "Agent settings";
  agentButton.addEventListener("click", () => agentPanel.toggle());
  const chatButton = document.createElement("button");
  chatButton.textContent = "✉";
  chatButton.title = "Chat";
  chatButton.addEventListener("click", () => chatPanel.toggle());
  toolbar.appendChild(agentDivider);
  toolbar.appendChild(agentButton);
  toolbar.appendChild(chatButton);

  const refreshToolbar = () => {
    const active = editor.active_tool();
    const locked = editor.is_tool_locked();
    for (const [name, button] of toolButtons) {
      const isActive = name === active;
      button.classList.toggle("active", isActive);
      button.classList.toggle("locked", isActive && locked);
    }
    undoButton.disabled = !editor.can_undo();
    redoButton.disabled = !editor.can_redo();
    stylePanel.refresh();
  };

  // -- text editor overlay ------------------------------------------------

  const textEditor = document.getElementById("text-editor");
  let editingId = null;
  let editingSingleLine = false;

  const closeTextEditor = () => {
    textEditor.classList.remove("visible", "single-line");
    textEditor.blur();
    editingId = null;
    editingSingleLine = false;
  };

  const commitTextEditor = () => {
    if (!editingId) return;
    const id = editingId;
    const content = textEditor.value;
    closeTextEditor();
    editor.commit_text(id, content);
    refreshToolbar();
    updateCursor();
  };

  editor.set_action_handler((json) => {
    const action = JSON.parse(json);
    if (action.type === "document_changed") {
      scheduleAutosave();
      return;
    }
    if (action.type !== "open_text_editor") return;
    editingId = action.id;
    editingSingleLine = !!action.single_line;
    textEditor.value = action.text;
    textEditor.style.left = `${action.x}px`;
    textEditor.style.top = `${action.y}px`;
    textEditor.style.width = `${action.width}px`;
    textEditor.style.height = `${action.height}px`;
    textEditor.style.fontSize = `${action.font_size}px`;
    textEditor.style.lineHeight = "1.3";
    textEditor.style.textAlign = action.align;
    // Frame names paint a band above the border; an opaque editor hides the
    // canvas-drawn name instead of ghosting through it.
    textEditor.classList.toggle("single-line", !!action.single_line);
    textEditor.classList.add("visible");
    textEditor.focus();
    textEditor.setSelectionRange(action.text.length, action.text.length);
  });

  // Autosave is debounced in the shell so serializing a large document never
  // happens inside a pointer or keyboard handler.
  let autosaveTimer = 0;
  let autosavePaused = false;
  let bridge = null;
  const scheduleAutosave = () => {
    if (autosavePaused) return;
    window.clearTimeout(autosaveTimer);
    autosaveTimer = window.setTimeout(() => {
      autosaveTimer = 0;
      editor.flush_save();
      bridge?.publishDocument();
    }, 700);
  };
  // Test hook: let frame-time measurements on huge documents skip the
  // (deliberately) expensive serialization pass.
  window.__rsPauseAutosave = (paused) => {
    autosavePaused = !!paused;
  };
  // The debounce loses edits made in its last ~700ms if the page goes away
  // before the timer fires.
  const flushPendingAutosave = () => {
    if (autosaveTimer) {
      window.clearTimeout(autosaveTimer);
      autosaveTimer = 0;
      editor.flush_save();
      bridge?.publishDocument();
    }
  };
  // IndexedDB writes spawned during unload can be torn down before they
  // commit, so the unload path also writes a synchronous checkpoint.
  const UNLOAD_CHECKPOINT = "rs-canvas-unload-checkpoint";
  const checkpointAutosave = () => {
    if (!autosaveTimer) return; // nothing pending — IndexedDB is already current
    window.clearTimeout(autosaveTimer);
    autosaveTimer = 0;
    try {
      localStorage.setItem(UNLOAD_CHECKPOINT, editor.save_json());
    } catch {
      // Quota or privacy-mode refusal: the async flush below may still land.
    }
    editor.flush_save();
  };
  window.addEventListener("pagehide", checkpointAutosave);
  window.addEventListener("beforeunload", checkpointAutosave);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") flushPendingAutosave();
  });

  textEditor.addEventListener("input", () => {
    textEditor.style.height = "auto";
    textEditor.style.height = `${Math.max(textEditor.scrollHeight, 24)}px`;
  });

  textEditor.addEventListener("keydown", (event) => {
    const composing = event.isComposing || event.keyCode === 229;
    if (composing) return;
    if (event.key === "Escape" || (event.key === "Enter" && editingSingleLine)) {
      event.preventDefault();
      commitTextEditor();
    }
  });

  textEditor.addEventListener("blur", () => {
    if (editingId) commitTextEditor();
  });

  scene.addEventListener("dblclick", (event) => {
    if (editingId) commitTextEditor();
    editor.double_click(event.offsetX, event.offsetY);
    refreshToolbar();
    updateCursor();
  });

  // -- pointer input ------------------------------------------------------

  const modifiers = (event) => ({
    shift: event.shiftKey,
    alt: event.altKey,
    ctrl: event.ctrlKey,
    meta: event.metaKey,
  });

  const activePointers = new Map();
  let pinchDistance = 0;

  scene.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    if (editingId) {
      commitTextEditor();
    }
    // preventDefault above also suppresses the focus change, so a form
    // control (opacity slider, library search) would otherwise keep focus
    // forever and swallow every canvas shortcut. Blur it explicitly.
    const focused = document.activeElement;
    if (focused instanceof HTMLElement && focused !== document.body) {
      focused.blur();
    }
    if (event.pointerType === "touch") {
      activePointers.set(event.pointerId, { x: event.offsetX, y: event.offsetY });
    }
    try {
      scene.setPointerCapture(event.pointerId);
    } catch {
      // pointer capture is best-effort
    }
    editor.set_pressure(event.pressure);
    editor.pointer_down(event.offsetX, event.offsetY, event.button, modifiers(event));
    refreshToolbar();
    updateCursor();
  });

  scene.addEventListener("pointermove", (event) => {
    if (activePointers.has(event.pointerId)) {
      activePointers.set(event.pointerId, { x: event.offsetX, y: event.offsetY });
    }
    if (activePointers.size === 2) {
      const [a, b] = [...activePointers.values()];
      const distance = Math.hypot(a.x - b.x, a.y - b.y);
      const center = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
      if (pinchDistance > 0 && distance > 0) {
        editor.zoom_at(center.x, center.y, distance / pinchDistance);
      }
      pinchDistance = distance;
      return;
    }
    editor.set_pressure(event.pressure);
    editor.pointer_move(event.offsetX, event.offsetY, modifiers(event));
    updateCursor();
  });

  const endPointer = (event) => {
    activePointers.delete(event.pointerId);
    if (activePointers.size < 2) {
      pinchDistance = 0;
    }
    editor.pointer_up(event.offsetX, event.offsetY, event.button, modifiers(event));
    refreshToolbar();
    updateCursor();
  };
  scene.addEventListener("pointerup", endPointer);
  scene.addEventListener("pointercancel", endPointer);

  // Wheel panning shifts existing pixels; snap to a crisp full render once
  // the gesture pauses.
  let viewSettleTimer = 0;
  const settleView = () => {
    window.clearTimeout(viewSettleTimer);
    viewSettleTimer = window.setTimeout(() => editor.render(), 140);
  };

  scene.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      editor.wheel(
        event.offsetX,
        event.offsetY,
        event.deltaX,
        event.deltaY,
        event.ctrlKey,
      );
      settleView();
      updateCursor();
    },
    { passive: false },
  );

  scene.addEventListener("contextmenu", (event) => event.preventDefault());

  // -- keyboard -----------------------------------------------------------

  window.addEventListener("keydown", (event) => {
    if (
      event.target instanceof HTMLInputElement ||
      event.target instanceof HTMLTextAreaElement
    ) {
      return;
    }
    if (
      !event.metaKey &&
      !event.ctrlKey &&
      !event.altKey &&
      event.key.toLowerCase() === "g"
    ) {
      event.preventDefault();
      if (event.shiftKey) {
        setSnapEnabled(!editor.snap_enabled());
      } else {
        setGridVisible(!editor.grid_visible());
      }
      return;
    }
    const handled = editor.key_down(
      event.key,
      event.shiftKey,
      event.altKey,
      event.ctrlKey,
      event.metaKey,
    );
    if (handled) {
      event.preventDefault();
    }
    refreshToolbar();
    updateCursor();
  });

  window.addEventListener("keyup", (event) => {
    editor.key_up(event.key);
    updateCursor();
  });

  window.addEventListener("blur", () => {
    editor.blur();
    updateCursor();
  });

  window.addEventListener("resize", redraw);

  redraw();

  try {
    const saved = await load_saved_json();
    if (saved) {
      editor.load_json(saved);
      showToast("Restored your last drawing");
    }
  } catch (error) {
    console.warn("could not restore autosave", error);
  }

  // An unload checkpoint always post-dates whatever IndexedDB holds: apply
  // it over the top, then hand it back to async persistence.
  try {
    const checkpoint = localStorage.getItem(UNLOAD_CHECKPOINT);
    if (checkpoint) {
      localStorage.removeItem(UNLOAD_CHECKPOINT);
      editor.load_json(checkpoint);
      editor.flush_save();
    }
  } catch (error) {
    console.warn("could not restore unload checkpoint", error);
  }

  refreshToolbar();

  // Connect only after the local autosave and checkpoint are in place: the
  // server hands over its mirror, and the two reconcile.
  bridge = connectAgentBridge(editor, {
    onRemoteChange: () => editor.flush_save(),
    onSettingsChange: (settings) => agentPanel.setSettings(settings),
  });

  window.__rsEditor = editor;
  window.__rsReady = true;
}

boot().catch((error) => {
  console.error("rs-canvas failed to boot", error);
});
