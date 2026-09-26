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

  const resizeAndRender = (): void => {
    editor.resize();
    editor.render();
  };

  window.addEventListener("resize", resizeAndRender);

  resizeAndRender();

  window.__rsEditor = editor;
  window.__rsReady = true;
}

boot().catch((error) => {
  console.error("rs-canvas failed to boot", error);
});
