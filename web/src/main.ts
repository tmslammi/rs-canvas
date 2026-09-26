import init, { Editor } from "./wasm/canvas_wasm.js";
import "./styles.css";

declare global {
  interface Window {
    __rsReady?: boolean;
    __rsEditor?: Editor;
  }
}

async function boot(): Promise<void> {
  await init();

  const scene = document.getElementById("scene") as HTMLCanvasElement;
  const editor = new Editor(scene);

  const updateCursor = (): void => {
    scene.style.cursor = editor.cursor();
  };

  const redraw = (): void => {
    editor.resize();
    editor.render();
  };

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
    editor.pointer_down(event.offsetX, event.offsetY, event.button);
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
    editor.pointer_move(event.offsetX, event.offsetY);
    updateCursor();
  });

  const endPointer = (event: PointerEvent): void => {
    activePointers.delete(event.pointerId);
    if (activePointers.size < 2) {
      pinchDistance = 0;
    }
    editor.pointer_up(event.offsetX, event.offsetY, event.button);
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

  window.addEventListener("keydown", (event) => {
    if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) {
      return;
    }
    if (editor.key_down(event.key, event.shiftKey)) {
      event.preventDefault();
    }
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

  window.__rsEditor = editor;
  window.__rsReady = true;
}

boot().catch((error) => {
  console.error("rs-canvas failed to boot", error);
});
