// Workspace lifecycle for one page: URL naming, switching, rename/delete
// handoff, and the local checkpoints that survive the navigation.
//
// A switch is a full page navigation. Before leaving, the outgoing document is
// committed, flushed to IndexedDB and checkpointed; only then does the URL
// change and a new editor boot against the new workspace. Rename and delete
// also coordinate the server's management route, but the document itself
// always travels through the binary wire (the page uploads after navigating).

import { validWorkspaceName, updateWorkspaceCatalogue } from "./workspacePanel.js";
import {
  saveWorkspaceBytes,
  deleteSavedWorkspace,
  loadSavedDocument,
  writeCheckpoint,
  clearCheckpoint,
} from "./autosave.js";

const MESSAGE_KEY = "rs-canvas-workspace-message";

/**
 * Resolve `?ws=` for this tab. An invalid name falls back to `default` and is
 * removed from the URL so a copied link does not keep it.
 */
export function workspaceFromLocation() {
  const requested = new URLSearchParams(location.search).get("ws") || "default";
  if (validWorkspaceName(requested)) {
    return { requested, workspace: requested };
  }
  const url = new URL(location.href);
  url.searchParams.delete("ws");
  history.replaceState(null, "", url);
  return { requested, workspace: "default" };
}

/**
 * `prepare` commits the outgoing text edit, cancels gestures, publishes the
 * last local changes and returns the document bytes. It stays in the
 * composition root because it owns the input surface; this module owns the
 * storage and navigation around it.
 */
export function createWorkspaceSession({
  workspace,
  autosave,
  prepare,
  closeBridge,
  showToast,
}) {
  let leaving = false;

  const catalogue = (remove, add) => {
    try {
      updateWorkspaceCatalogue(remove, add);
    } catch {
      // IndexedDB still lists saved names.
    }
  };

  const workspaceUrl = (name) => {
    const url = new URL(location.href);
    if (name === "default") url.searchParams.delete("ws");
    else url.searchParams.set("ws", name);
    return url;
  };

  const navigateWorkspace = (name, message) => {
    leaving = true;
    autosave.pause();
    closeBridge();
    catalogue(null, name);
    if (message) {
      try {
        sessionStorage.setItem(MESSAGE_KEY, message);
      } catch {
        // Feedback is optional.
      }
    }
    location.assign(workspaceUrl(name));
  };

  const storeOutgoingWorkspace = async () => {
    const bytes = prepare();
    await saveWorkspaceBytes(workspace, bytes);
    writeCheckpoint(workspace, bytes);
    return bytes;
  };

  const managementRequest = async (path, options) => {
    const response = await fetch(`/api/workspaces/${encodeURIComponent(workspace)}${path}`, options);
    const body = await response.json().catch(() => ({}));
    if (!response.ok && response.status !== 404) {
      throw new Error(body.error || "Could not update workspace. Try again when the server is available.");
    }
    // A local autosave need not have a server mirror (for example after a restart).
    if (response.status === 404 && !body.error) {
      throw new Error("The server is unavailable. Reconnect before renaming or deleting.");
    }
  };

  const removeLocalWorkspace = async () => {
    await deleteSavedWorkspace(workspace);
    clearCheckpoint(workspace);
    catalogue(workspace, null);
  };

  const manageWorkspace = async (name) => {
    const bytes = await storeOutgoingWorkspace();
    if (name && await loadSavedDocument(name)) {
      throw new Error("That workspace already has a saved drawing. Choose another name.");
    }
    autosave.pause();
    leaving = true;
    let serverChanged = false;
    try {
      if (name) await saveWorkspaceBytes(name, bytes);
      await managementRequest(name ? "/rename" : "", name ? {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        // The page uploads its document over the binary wire after navigating;
        // the management route only moves the workspace.
        body: JSON.stringify({ name }),
      } : { method: "DELETE" });
      serverChanged = true;
      closeBridge();
      await removeLocalWorkspace();
      if (name) writeCheckpoint(name, bytes);
      navigateWorkspace(
        name || "default",
        name ? `Workspace renamed to ${name}` : `Workspace ${workspace} deleted`,
      );
    } catch (error) {
      if (serverChanged) {
        // The server's change is durable. Keep a local recovery copy if the
        // browser refuses deletion, and take the page to its new destination.
        navigateWorkspace(
          name || "default",
          "Workspace updated on the server. A local recovery copy could not be removed.",
        );
      } else {
        if (name) await deleteSavedWorkspace(name).catch(() => {});
        leaving = false;
        autosave.resume();
        throw error;
      }
    }
  };

  const switchTo = async (name) => {
    if (!validWorkspaceName(name)) throw new Error("Invalid workspace name.");
    await storeOutgoingWorkspace();
    navigateWorkspace(name);
  };

  const duplicate = async (name) => {
    if (await loadSavedDocument(name)) {
      throw new Error("That workspace already has a saved drawing. Choose another name.");
    }
    const bytes = await storeOutgoingWorkspace();
    await saveWorkspaceBytes(name, bytes);
    writeCheckpoint(name, bytes);
    navigateWorkspace(name, `Duplicated ${workspace} as ${name}`);
  };

  /**
   * The agent created, renamed or deleted a workspace. The server has already
   * committed the change; the page follows it without changing the document
   * protocol.
   */
  const handleServerChange = async (change) => {
    if (leaving || !validWorkspaceName(change.workspace)) return;
    if (change.action === "switch") {
      // The agent created and opened another workspace. Keep this browser's
      // autosave for the outgoing one, then follow the server's navigation.
      leaving = true;
      try {
        await storeOutgoingWorkspace();
      } catch {
        // The server mirror still holds the outgoing document.
      }
      navigateWorkspace(change.workspace, `Opened workspace ${change.workspace}`);
      return;
    }
    leaving = true;
    autosave.pause();
    closeBridge();
    try {
      if (change.action === "rename") {
        const bytes = prepare();
        await saveWorkspaceBytes(change.workspace, bytes);
        writeCheckpoint(change.workspace, bytes);
      }
      await removeLocalWorkspace();
      navigateWorkspace(change.workspace, "Workspace updated");
    } catch {
      navigateWorkspace(
        change.workspace,
        "Workspace updated on the server. A local recovery copy remains in this browser.",
      );
    }
  };

  /** Show the message a previous page left for this navigation, once. */
  const restoreMessage = () => {
    try {
      const message = sessionStorage.getItem(MESSAGE_KEY);
      if (message) {
        sessionStorage.removeItem(MESSAGE_KEY);
        showToast(message);
      }
    } catch {
      // Optional feedback.
    }
  };

  return {
    isLeaving: () => leaving,
    switchTo,
    duplicate,
    rename: manageWorkspace,
    remove: () => manageWorkspace(null),
    handleServerChange,
    restoreMessage,
  };
}
