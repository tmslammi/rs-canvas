import init, { Editor, glyph_catalog, load_saved_json } from "./wasm/canvas_wasm.js";
import { createLibraryPanel } from "./libraryPanel";
import { createStylePanel } from "./stylePanel";
import "./styles.css";

declare global {
  interface Window {
    __rsReady?: boolean;
    __rsEditor?: Editor;
  }
}

interface ToolbarTool {
  name: string;
  label: string;
  glyph: string;
  shortcut: string;
}

const TOOLS: ToolbarTool[] = [
  { name: "select", label: "Select", glyph: "↖", shortcut: "V" },
  { name: "hand", label: "Hand", glyph: "✋", shortcut: "H" },
  { name: "rectangle", label: "Rectangle", glyph: "▭", shortcut: "R" },
  { name: "diamond", label: "Diamond", glyph: "◇", shortcut: "D" },
  { name: "ellipse", label: "Ellipse", glyph: "◯", shortcut: "O" },
  { name: "freehand", label: "Draw", glyph: "✎", shortcut: "P" },
  { name: "line", label: "Line", glyph: "╱", shortcut: "L" },
  { name: "arrow", label: "Arrow", glyph: "→", shortcut: "A" },
  { name: "text", label: "Text", glyph: "T", shortcut: "T" },
];

async function loadFonts(): Promise<void> {
  const face = new FontFace("ComicShanns", "url(/fonts/ComicShanns-Regular.ttf)");
  await face.load();
  document.fonts.add(face);
}

async function boot(): Promise<void> {
  await init();
  await loadFonts();

  const scene = document.getElementById("scene") as HTMLCanvasElement;
  const overlay = document.getElementById("overlay") as HTMLCanvasElement;
  const toolbar = document.getElementById("toolbar") as HTMLElement;
  const stylePanelHost = document.getElementById("style-panel") as HTMLElement;
  const libraryPanelHost = document.getElementById("library-panel") as HTMLElement;
  const editor = new Editor(scene, overlay);
  const stylePanel = createStylePanel(stylePanelHost, editor);
  createLibraryPanel(libraryPanelHost, editor, glyph_catalog());

  const libraryToggle = document.createElement("button");
  libraryToggle.textContent = "▦";
  libraryToggle.title = "Shapes library";
  libraryToggle.addEventListener("click", () => {
    libraryPanelHost.classList.toggle("visible");
    libraryToggle.classList.toggle("active", libraryPanelHost.classList.contains("visible"));
  });
  toolbar.appendChild(libraryToggle);

  const toast = document.createElement("div");
  toast.id = "toast";
  document.body.appendChild(toast);
  let toastTimer = 0;
  const showToast = (message: string, isError = false): void => {
    toast.textContent = message;
    toast.classList.toggle("error", isError);
    toast.classList.add("visible");
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => toast.classList.remove("visible"), 3200);
  };

  const updateCursor = (): void => {
    scene.style.cursor = editor.cursor();
  };

  const redraw = (): void => {
    editor.resize();
    editor.render();
  };

  // -- toolbar ------------------------------------------------------------

  const toolButtons = new Map<string, HTMLButtonElement>();
  for (const tool of TOOLS) {
    const button = document.createElement("button");
    button.textContent = tool.glyph;
    button.title = `${tool.label} — ${tool.shortcut}`;
    button.addEventListener("click", () => {
      editor.set_tool(tool.name);
      refreshToolbar();
      updateCursor();
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

  const openButton = document.createElement("button");
  openButton.textContent = "📂";
  openButton.title = "Open… (⌘O)";

  const saveButton = document.createElement("button");
  saveButton.textContent = "💾";
  saveButton.title = "Save (⌘S)";

  const pngButton = document.createElement("button");
  pngButton.textContent = "🖼";
  pngButton.title = "Export PNG";

  const svgButton = document.createElement("button");
  svgButton.textContent = "⇱";
  svgButton.title = "Export SVG";

  const copyButton = document.createElement("button");
  copyButton.textContent = "📋";
  copyButton.title = "Copy PNG";

  toolbar.appendChild(newButton);
  toolbar.appendChild(openButton);
  toolbar.appendChild(saveButton);
  toolbar.appendChild(pngButton);
  toolbar.appendChild(svgButton);
  toolbar.appendChild(copyButton);

  const downloadDataUrl = (dataUrl: string, filename: string): void => {
    const anchor = document.createElement("a");
    anchor.href = dataUrl;
    anchor.download = filename;
    anchor.click();
  };

  const exportStamp = (): string =>
    new Date().toISOString().slice(0, 16).replace(/[:T]/g, "-");

  pngButton.addEventListener("click", () => {
    try {
      const dataUrl = editor.export_png(2);
      downloadDataUrl(dataUrl, `rs-canvas-${exportStamp()}.png`);
      showToast("Exported PNG");
    } catch (error) {
      showToast(`Export failed: ${String(error)}`, true);
    }
  });

  svgButton.addEventListener("click", () => {
    const svg = editor.export_svg();
    const blob = new Blob([svg], { type: "image/svg+xml" });
    const url = URL.createObjectURL(blob);
    downloadDataUrl(url, `rs-canvas-${exportStamp()}.svg`);
    URL.revokeObjectURL(url);
    showToast("Exported SVG");
  });

  copyButton.addEventListener("click", async () => {
    try {
      const dataUrl = editor.export_png(2);
      const blob = await (await fetch(dataUrl)).blob();
      await navigator.clipboard.write([
        new ClipboardItem({ "image/png": blob }),
      ]);
      showToast("Copied PNG to clipboard");
    } catch (error) {
      showToast(`Copy failed: ${String(error)}`, true);
    }
  });

  const fileInput = document.createElement("input");
  fileInput.type = "file";
  fileInput.id = "file-input";
  fileInput.accept = ".json,application/json";
  fileInput.style.display = "none";
  document.body.appendChild(fileInput);

  const saveFile = (): void => {
    const json = editor.save_json();
    const blob = new Blob([json], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    const stamp = new Date().toISOString().slice(0, 16).replace(/[:T]/g, "-");
    anchor.href = url;
    anchor.download = `rs-canvas-${stamp}.json`;
    anchor.click();
    URL.revokeObjectURL(url);
    showToast("Saved");
  };

  openButton.addEventListener("click", () => fileInput.click());
  saveButton.addEventListener("click", () => saveFile());

  fileInput.addEventListener("change", async () => {
    const file = fileInput.files?.[0];
    if (!file) return;
    const text = await file.text();
    try {
      editor.load_json(text);
      refreshToolbar();
      updateCursor();
      showToast(`Opened ${file.name}`);
    } catch (error) {
      showToast(`Could not open: ${String(error)}`, true);
    }
    fileInput.value = "";
  });

  const refreshToolbar = (): void => {
    const active = editor.active_tool();
    for (const [name, button] of toolButtons) {
      button.classList.toggle("active", name === active);
    }
    undoButton.disabled = !editor.can_undo();
    redoButton.disabled = !editor.can_redo();
    stylePanel.refresh();
  };

  // -- text editor overlay ------------------------------------------------

  const textEditor = document.getElementById("text-editor") as HTMLTextAreaElement;
  let editingId: string | null = null;

  const closeTextEditor = (): void => {
    textEditor.classList.remove("visible");
    textEditor.blur();
    editingId = null;
  };

  const commitTextEditor = (): void => {
    if (!editingId) return;
    const id = editingId;
    const content = textEditor.value;
    closeTextEditor();
    editor.commit_text(id, content);
    refreshToolbar();
    updateCursor();
  };

  interface OpenTextEditorAction {
    type: string;
    id: string;
    x: number;
    y: number;
    width: number;
    height: number;
    font_size: number;
    text: string;
    align: string;
  }

  editor.set_action_handler((json: string) => {
    const action = JSON.parse(json) as OpenTextEditorAction;
    if (action.type !== "open_text_editor") return;
    editingId = action.id;
    textEditor.value = action.text;
    textEditor.style.left = `${action.x}px`;
    textEditor.style.top = `${action.y}px`;
    textEditor.style.width = `${action.width}px`;
    textEditor.style.height = `${action.height}px`;
    textEditor.style.fontSize = `${action.font_size}px`;
    textEditor.style.lineHeight = "1.3";
    textEditor.style.textAlign = action.align;
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
    if (event.key === "Escape" && !composing) {
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

  const modifiers = (event: PointerEvent | KeyboardEvent | WheelEvent) => ({
    shift: event.shiftKey,
    alt: event.altKey,
    ctrl: event.ctrlKey,
    meta: event.metaKey,
  });

  const activePointers = new Map<number, { x: number; y: number }>();
  let pinchDistance = 0;

  scene.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    if (editingId) {
      commitTextEditor();
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

  const endPointer = (event: PointerEvent): void => {
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
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
      event.preventDefault();
      saveFile();
      return;
    }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "o") {
      event.preventDefault();
      fileInput.click();
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

  refreshToolbar();

  window.__rsEditor = editor;
  window.__rsReady = true;
}

boot().catch((error) => {
  console.error("rs-canvas failed to boot", error);
});
