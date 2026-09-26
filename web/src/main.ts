import init, { Editor } from "./wasm/canvas_wasm.js";
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
];

async function boot(): Promise<void> {
  await init();

  const scene = document.getElementById("scene") as HTMLCanvasElement;
  const overlay = document.getElementById("overlay") as HTMLCanvasElement;
  const toolbar = document.getElementById("toolbar") as HTMLElement;
  const editor = new Editor(scene, overlay);

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

  const refreshToolbar = (): void => {
    const active = editor.active_tool();
    for (const [name, button] of toolButtons) {
      button.classList.toggle("active", name === active);
    }
    undoButton.disabled = !editor.can_undo();
    redoButton.disabled = !editor.can_redo();
  };

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
  refreshToolbar();

  window.__rsEditor = editor;
  window.__rsReady = true;
}

boot().catch((error) => {
  console.error("rs-canvas failed to boot", error);
});
