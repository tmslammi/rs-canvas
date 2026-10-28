// Local persistence for one page: debounced IndexedDB autosave, the
// synchronous unload checkpoint, and boot restore.
//
// The wasm editor owns serialization; this module only decides *when* it
// happens and where the bytes go. Keeping serialization out of pointer and
// keyboard handlers is why the save is debounced here rather than in Rust.
//
// Checkpoints are the second chance: an IndexedDB write spawned during unload
// can be torn down before it commits, so pagehide/beforeunload also write the
// same binary document synchronously to localStorage. localStorage is
// string-only, so the bytes carry a `rsc-bin1:` prefix and base64 payload.

import {
  load_saved_document,
  list_saved_workspaces,
  save_workspace_bytes,
  delete_saved_workspace,
} from "./wasm/canvas_wasm.js";

const SAVE_DEBOUNCE_MS = 700;
const CHECKPOINT_PREFIX = "rsc-bin1:";
const DEFAULT_CHECKPOINT = "rs-canvas-unload-checkpoint";
const WORKSPACE_CHECKPOINT_PREFIX = "rs-canvas-unload-checkpoint:workspace:";

export const checkpointKey = (name) => name === "default"
  ? DEFAULT_CHECKPOINT
  : `${WORKSPACE_CHECKPOINT_PREFIX}${name}`;

const bytesToBase64 = (bytes) => {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
};

export const checkpointValue = (bytes) => CHECKPOINT_PREFIX + bytesToBase64(bytes);

export const checkpointBytes = (value) => value.startsWith(CHECKPOINT_PREFIX)
  ? Uint8Array.from(atob(value.slice(CHECKPOINT_PREFIX.length)), (c) => c.charCodeAt(0))
  : null;

export function writeCheckpoint(name, bytes) {
  try {
    localStorage.setItem(checkpointKey(name), checkpointValue(bytes));
  } catch {
    // Quota or privacy-mode refusal: the awaited IndexedDB save is durable.
  }
}

export function clearCheckpoint(name) {
  try {
    localStorage.removeItem(checkpointKey(name));
  } catch {
    // Storage can be unavailable.
  }
}

const readCheckpoint = (name) => localStorage.getItem(checkpointKey(name));

// IndexedDB lives in wasm (storage.rs); these are the page-side names for it.
export const loadSavedDocument = (name) => load_saved_document(name);
export const saveWorkspaceBytes = (name, bytes) => save_workspace_bytes(name, bytes);
export const deleteSavedWorkspace = (name) => delete_saved_workspace(name);
export const listSavedWorkspaces = async () => JSON.parse(await list_saved_workspaces());

/**
 * Wire the page's autosave to `editor` for `workspace`.
 *
 * `onRestored` is called with the toast message after an IndexedDB autosave
 * is loaded; checkpoint restores are silent because they are the same boot's
 * later copy of that save.
 */
export function createAutosave(editor, workspace, { onRestored } = {}) {
  let timer = 0;
  let paused = false;

  const schedule = () => {
    if (paused) return;
    window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      timer = 0;
      editor.flush_save();
    }, SAVE_DEBOUNCE_MS);
  };

  // Remote merges are already committed and visible; flush them immediately
  // instead of waiting out the debounce.
  const flushNow = () => {
    if (!paused) editor.flush_save();
  };

  // The debounce loses edits made in its last ~700ms if the page goes away
  // before the timer fires.
  const flushPending = () => {
    if (!timer) return;
    window.clearTimeout(timer);
    timer = 0;
    editor.flush_save();
  };

  // A workspace switch hands the page off: stop scheduling while the
  // navigation and durable save are in flight.
  const pause = () => {
    paused = true;
    window.clearTimeout(timer);
    timer = 0;
  };
  const resume = () => {
    paused = false;
  };

  // Unload path: write the synchronous checkpoint first, then ask IndexedDB
  // to catch up. Only pending edits need the checkpoint; if no debounce is
  // live, IndexedDB is already current.
  const checkpoint = () => {
    if (!timer) return;
    window.clearTimeout(timer);
    timer = 0;
    try {
      localStorage.setItem(checkpointKey(workspace), checkpointValue(editor.save_bytes()));
    } catch {
      // Quota or privacy-mode refusal: the async flush below may still land.
    }
    editor.flush_save();
  };

  const restore = async () => {
    try {
      const saved = await loadSavedDocument(workspace);
      if (saved) {
        editor.load_bytes(saved);
        onRestored?.("Restored your last drawing");
      }
    } catch (error) {
      console.warn("could not restore autosave", error);
    }

    // An unload checkpoint always post-dates whatever IndexedDB holds: apply
    // it over the top, then hand it back to async persistence.
    try {
      const value = readCheckpoint(workspace);
      if (value) {
        localStorage.removeItem(checkpointKey(workspace));
        const bytes = checkpointBytes(value);
        if (bytes) {
          editor.load_bytes(bytes);
          editor.flush_save();
        }
      }
    } catch (error) {
      console.warn("could not restore unload checkpoint", error);
    }
  };

  window.addEventListener("pagehide", checkpoint);
  window.addEventListener("beforeunload", checkpoint);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") flushPending();
  });

  // Test hook: let frame-time measurements on huge documents skip the
  // (deliberately) expensive serialization pass. Unlike `pause()`, this only
  // stops future scheduling; an already-armed timer still fires, matching the
  // hook's previous behaviour.
  window.__rsPauseAutosave = (value) => {
    paused = !!value;
  };

  return {
    schedule,
    flushNow,
    flushPending,
    pause,
    resume,
    checkpoint,
    restore,
    isPaused: () => paused,
  };
}
