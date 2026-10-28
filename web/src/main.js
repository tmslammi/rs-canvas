import init, { Editor } from "./wasm/canvas_wasm.js";
import { createStylePanel } from "./stylePanel.js";
import { createAgentPanel } from "./agentPanel.js";
import { connectAgentBridge } from "./agentBridge.js";
import { createWorkspacePanel } from "./workspacePanel.js";
import { createAutosave, listSavedWorkspaces } from "./autosave.js";
import { createWorkspaceSession, workspaceFromLocation } from "./workspaceSession.js";

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
  { name: "sticky", label: "Sticky note", glyph: "▤", shortcut: "N" },
  { name: "stickyStack", label: "Sticky stack", glyph: "☰", shortcut: "K" },
  { name: "frame", label: "Frame", glyph: "⬚", shortcut: "F" },
  { name: "eraser", label: "Eraser", glyph: "⌦", shortcut: "E" },
];

async function loadFonts() {
  const face = new FontFace("ComicShanns", "url(/fonts/ComicShanns-Regular.ttf)");
  try {
    await face.load();
    document.fonts.add(face);
  } catch (error) {
    console.warn("handwriting font unavailable; using the system font", error);
  }
}

async function boot() {
  await init();
  await loadFonts();

  const scene = document.getElementById("scene");
  const grid = document.getElementById("grid");
  const overlay = document.getElementById("overlay");
  const toolbar = document.getElementById("toolbar");
  const stylePanelHost = document.getElementById("style-panel");
  const { requested: requestedWorkspace, workspace } = workspaceFromLocation();
  document.title = `${workspace} — rs-canvas`;
  const editor = new Editor(scene, overlay, grid);
  editor.set_workspace(workspace);
  const stylePanel = createStylePanel(stylePanelHost, editor);

  const toast = document.createElement("div");
  toast.id = "toast";
  toast.setAttribute("role", "status");
  toast.setAttribute("aria-live", "polite");
  document.body.appendChild(toast);
  let toastTimer = 0;
  const showToast = (message, isError = false) => {
    toast.textContent = message;
    toast.classList.toggle("error", isError);
    toast.classList.add("visible");
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => toast.classList.remove("visible"), 3200);
  };

  // The page's local persistence: debounced IndexedDB saves, the unload
  // checkpoint and boot restore. Created before any input handler can emit
  // document changes.
  const autosave = createAutosave(editor, workspace, {
    onRestored: (message) => showToast(message),
  });

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
  gridButton.setAttribute("aria-label", "Toggle grid");
  const gridSizeButton = document.createElement("button");
  const snapButton = document.createElement("button");
  snapButton.textContent = "🧲";
  snapButton.title = "Snap to grid — ⇧G";
  snapButton.setAttribute("aria-label", "Toggle snap to grid");
  const zoomButton = document.createElement("button");
  zoomButton.setAttribute("aria-label", "Zoom speed");

  const updateGridSizeButton = () => {
    const size = Math.round(editor.grid_size());
    gridSizeButton.textContent = String(size);
    gridSizeButton.title = `Grid size — ${size} world units (click to change)`;
    gridSizeButton.setAttribute("aria-label", `Grid size ${size}`);
  };

  const updateZoomButton = () => {
    const value = editor.zoom_sensitivity();
    zoomButton.textContent = `${value}×`;
    zoomButton.title = `Zoom speed — ${value}× (⌘/Ctrl + wheel)`;
    zoomButton.setAttribute("aria-label", `Zoom speed ${value} times`);
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
    gridButton.setAttribute("aria-pressed", String(visible));
    if (remember) persist(GRID_KEY, visible);
  };

  const setSnapEnabled = (enabled, remember = true) => {
    editor.set_snap_enabled(enabled);
    snapButton.classList.toggle("active", enabled);
    snapButton.setAttribute("aria-pressed", String(enabled));
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
    button.setAttribute("aria-label", tool.label);
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
  undoButton.title = "Undo — Ctrl/Cmd+Z";
  undoButton.setAttribute("aria-label", "Undo");
  undoButton.addEventListener("click", () => {
    editor.undo();
    refreshToolbar();
  });

  const redoButton = document.createElement("button");
  redoButton.textContent = "↷";
  redoButton.title = "Redo — Shift+Ctrl/Cmd+Z";
  redoButton.setAttribute("aria-label", "Redo");
  redoButton.addEventListener("click", () => {
    editor.redo();
    refreshToolbar();
  });

  toolbar.appendChild(undoButton);
  toolbar.appendChild(redoButton);

  // -- document actions ---------------------------------------------------

  const actionDivider = document.createElement("div");
  actionDivider.className = "divider";
  toolbar.appendChild(actionDivider);

  const newButton = document.createElement("button");
  newButton.textContent = "✚";
  newButton.title = "New drawing";
  newButton.setAttribute("aria-label", "New drawing");
  newButton.addEventListener("click", () => {
    editor.new_document();
    refreshToolbar();
    showToast("New drawing");
  });

  const seedButton = document.createElement("button");
  seedButton.textContent = "🌱";
  seedButton.title = "Seed demo";
  seedButton.setAttribute("aria-label", "Load the demo architecture");
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
  const agentDivider = document.createElement("div");
  agentDivider.className = "divider";
  const agentButton = document.createElement("button");
  agentButton.textContent = "◎";
  agentButton.title = "Agent settings";
  agentButton.setAttribute("aria-label", "Agent settings");
  agentButton.setAttribute("aria-haspopup", "dialog");
  agentButton.setAttribute("aria-controls", "agent-panel");
  agentButton.setAttribute("aria-expanded", "false");
  agentButton.addEventListener("click", () => {
    agentButton.setAttribute("aria-expanded", String(agentPanel.toggle()));
  });
  toolbar.appendChild(agentDivider);
  toolbar.appendChild(agentButton);

  const refreshToolbar = () => {
    const active = editor.active_tool();
    const locked = editor.is_tool_locked();
    for (const [name, button] of toolButtons) {
      const isActive = name === active;
      button.classList.toggle("active", isActive);
      button.classList.toggle("locked", isActive && locked);
      button.setAttribute("aria-pressed", String(isActive));
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

  // The bridge is assigned once the local autosave is in place; the action
  // handler fires during boot, so the binding exists first.
  let bridge = null;

  editor.set_action_handler((json) => {
    const action = JSON.parse(json);
    if (action.type === "document_changed") {
      autosave.schedule();
      // Deferred: the handler runs inside the wasm call that produced the
      // change, and calling back into the editor re-entrantly would panic.
      queueMicrotask(() => {
        bridge?.publishChanges();
        refreshToolbar();
        updateCursor();
      });
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
    // Match the element's own ink: sticky notes are dark-on-pastel, while
    // point text is drawn in the canvas' light default.
    textEditor.style.color = action.color;
    // Bound text wraps to its rect; auto-width point text does not.
    textEditor.classList.toggle("wrap", !!action.wrap);
    // Frame names paint a band above the border; an opaque editor hides the
    // canvas-drawn name instead of ghosting through it.
    textEditor.classList.toggle("single-line", !!action.single_line);
    textEditor.classList.add("visible");
    textEditor.focus();
    textEditor.setSelectionRange(action.text.length, action.text.length);
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
  let pinching = false;

  scene.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    if (editingId) {
      commitTextEditor();
    }
    // preventDefault above also suppresses the focus change, so a form
    // control (the opacity slider) would otherwise keep focus
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
    if (activePointers.size >= 2) {
      if (!pinching) {
        // A second finger takes over navigation; discard the first finger's
        // incomplete drawing before it can become an accidental element.
        editor.pointer_cancel();
        pinching = true;
        const [a, b] = [...activePointers.values()];
        pinchDistance = Math.hypot(a.x - b.x, a.y - b.y);
        refreshToolbar();
        updateCursor();
      }
      return;
    }
    if (pinching) return;
    editor.set_pressure(event.pressure);
    editor.pointer_down(event.offsetX, event.offsetY, event.button, modifiers(event));
    refreshToolbar();
    updateCursor();
  });

  // Pointer moves arrive faster than frames (up to hundreds per second);
  // the core answers each in microseconds now, but every crossing still pays
  // a wasm round trip plus cursor/overlay work. Coalesce to one forward per
  // animation frame: only the latest position matters. Down/up stay immediate
  // so clicks never wait a frame.
  let pendingMove = null;
  let moveScheduled = false;
  const flushMove = () => {
    moveScheduled = false;
    if (!pendingMove) return;
    const event = pendingMove;
    pendingMove = null;
    if (pinching) return;
    editor.set_pressure(event.pressure);
    editor.pointer_move(event.offsetX, event.offsetY, modifiers(event));
    updateCursor();
  };
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
    // Wait until every finger has lifted after a pinch. The remaining finger
    // must not resume the drawing gesture that was cancelled on takeover.
    if (pinching) return;
    pendingMove = {
      pressure: event.pressure,
      offsetX: event.offsetX,
      offsetY: event.offsetY,
      shiftKey: event.shiftKey,
      altKey: event.altKey,
      ctrlKey: event.ctrlKey,
      metaKey: event.metaKey,
    };
    if (!moveScheduled) {
      moveScheduled = true;
      requestAnimationFrame(flushMove);
    }
  });

  const endPointer = (event) => {
    activePointers.delete(event.pointerId);
    if (pinching) {
      if (activePointers.size === 0) {
        pinching = false;
        pinchDistance = 0;
      }
      return;
    }
    if (activePointers.size < 2) {
      pinchDistance = 0;
    }
    if (event.type === "pointercancel") {
      editor.pointer_cancel();
    } else {
      editor.pointer_up(event.offsetX, event.offsetY, event.button, modifiers(event));
    }
    refreshToolbar();
    updateCursor();
  };
  scene.addEventListener("pointerup", endPointer);
  scene.addEventListener("pointercancel", endPointer);

  // A workspace switch ends the outgoing gesture/text edit, then waits for
  // durable local storage before changing the URL and connecting a new editor.
  const prepareWorkspace = () => {
    if (editingId) commitTextEditor();
    editor.pointer_cancel();
    activePointers.clear();
    pinching = false;
    pinchDistance = 0;
    bridge?.publishChanges();
    return editor.save_bytes();
  };
  const workspaceSession = createWorkspaceSession({
    workspace,
    autosave,
    prepare: prepareWorkspace,
    closeBridge: () => bridge?.close(),
    showToast,
  });
  const workspacePanel = createWorkspacePanel(document.getElementById("app"), {
    current: workspace,
    localSavedNames: listSavedWorkspaces,
    onSwitch: workspaceSession.switchTo,
    onDuplicate: workspaceSession.duplicate,
    onRename: workspaceSession.rename,
    onDelete: workspaceSession.remove,
  });

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
    if (workspacePanel.isOpen()) return;
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

  await autosave.restore();

  refreshToolbar();

  // Connect only after the local autosave and checkpoint are in place: the
  // server hands over its mirror, and the two reconcile.
  bridge = connectAgentBridge(editor, {
    workspace,
    onRemoteChange: () => {
      autosave.flushNow();
      refreshToolbar();
      updateCursor();
    },
    onSettingsChange: (settings) => agentPanel.setSettings(settings),
    onDiscussionChange: (notes, persisted) => agentPanel.setDiscussion(notes, persisted),
    onWorkspaceChange: workspaceSession.handleServerChange,
  });

  if (workspace !== requestedWorkspace) showToast("Invalid workspace name. Opened default instead.", true);
  workspaceSession.restoreMessage();
  window.__rsEditor = editor;
  window.__rsReady = true;
}

boot().catch((error) => {
  console.error("rs-canvas failed to boot", error);
});
