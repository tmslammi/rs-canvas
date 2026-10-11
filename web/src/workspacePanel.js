const CATALOGUE_KEY = "rs-canvas-workspaces";

export const validWorkspaceName = (name) =>
  typeof name === "string" && /^[a-z0-9][a-z0-9-]{0,39}$/.test(name);

export function workspaceCatalogue() {
  try {
    let names = [];
    try { names = JSON.parse(localStorage.getItem(CATALOGUE_KEY) || "[]"); }
    catch { /* Checkpoints can still recover names from a damaged catalogue. */ }
    const saved = Array.isArray(names) ? names.filter(validWorkspaceName) : [];
    const checkpoints = Object.keys(localStorage)
      .filter((key) => key.startsWith("rs-canvas-unload-checkpoint:workspace:"))
      .map((key) => key.slice("rs-canvas-unload-checkpoint:workspace:".length))
      .filter(validWorkspaceName);
    return [...new Set([...saved, ...checkpoints])];
  } catch { return []; }
}

export function updateWorkspaceCatalogue(remove, add) {
  const names = new Set(workspaceCatalogue());
  if (remove) names.delete(remove);
  if (add && validWorkspaceName(add)) names.add(add);
  localStorage.setItem(CATALOGUE_KEY, JSON.stringify([...names]));
}

export function createWorkspacePanel(host, { current, onSwitch, onDuplicate, onRename, onDelete, localSavedNames }) {
  const root = document.createElement("div");
  root.id = "workspace-control";
  const toggle = document.createElement("button");
  toggle.id = "workspace-button";
  toggle.textContent = `Workspace: ${current}`;
  toggle.title = `Current workspace: ${current}`;
  toggle.setAttribute("aria-haspopup", "dialog");
  toggle.setAttribute("aria-expanded", "false");
  toggle.setAttribute("aria-controls", "workspace-picker");
  const picker = document.createElement("section");
  picker.id = "workspace-picker";
  picker.hidden = true;
  picker.setAttribute("role", "dialog");
  picker.setAttribute("aria-label", "Workspaces");
  const heading = document.createElement("h2");
  heading.textContent = "Workspaces";
  const close = document.createElement("button");
  close.textContent = "Close";
  close.setAttribute("aria-label", "Close workspaces");
  const header = document.createElement("div");
  header.className = "workspace-header";
  header.append(heading, close);
  const list = document.createElement("div");
  list.className = "workspace-list";
  const status = document.createElement("p");
  status.className = "workspace-status";
  status.setAttribute("role", "status");
  const form = document.createElement("form");
  const label = document.createElement("label");
  label.htmlFor = "workspace-name";
  label.textContent = "New workspace name";
  const input = document.createElement("input");
  input.id = "workspace-name";
  input.type = "text";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.setAttribute("aria-describedby", "workspace-name-hint workspace-error");
  const hint = document.createElement("p");
  hint.id = "workspace-name-hint";
  hint.textContent = "1–40 lowercase letters, digits or dashes. Start with a letter or digit.";
  const error = document.createElement("p");
  error.id = "workspace-error";
  error.setAttribute("role", "alert");
  const create = document.createElement("button");
  create.type = "submit";
  create.textContent = "Create workspace";
  const duplicate = document.createElement("button");
  duplicate.type = "button";
  duplicate.textContent = "Duplicate workspace";
  const createActions = document.createElement("div");
  createActions.className = "workspace-actions";
  createActions.append(create, duplicate);
  form.append(label, input, hint, error, createActions);

  const management = document.createElement("section");
  management.className = "workspace-management";
  const managementTitle = document.createElement("h3");
  managementTitle.textContent = `Manage ${current}`;
  const renameForm = document.createElement("form");
  const renameLabel = document.createElement("label");
  renameLabel.htmlFor = "workspace-rename";
  renameLabel.textContent = "Rename workspace name";
  const renameInput = document.createElement("input");
  renameInput.id = "workspace-rename";
  renameInput.value = current;
  renameInput.autocomplete = "off";
  renameInput.spellcheck = false;
  const rename = document.createElement("button");
  rename.type = "submit";
  rename.textContent = "Rename workspace";
  renameForm.append(renameLabel, renameInput, rename);
  const remove = document.createElement("button");
  remove.type = "button";
  remove.textContent = "Delete workspace";
  const managementNote = document.createElement("p");
  const confirmation = document.createElement("form");
  confirmation.hidden = true;
  const warning = document.createElement("p");
  warning.textContent = `Delete workspace ${current} and its saved drawing? This cannot be undone.`;
  const confirmLabel = document.createElement("label");
  confirmLabel.htmlFor = "workspace-confirm-delete";
  confirmLabel.textContent = "Confirm workspace name";
  const confirmInput = document.createElement("input");
  confirmInput.id = "workspace-confirm-delete";
  confirmInput.autocomplete = "off";
  const confirmDelete = document.createElement("button");
  confirmDelete.type = "submit";
  confirmDelete.textContent = "Delete permanently";
  confirmDelete.className = "danger";
  const cancelDelete = document.createElement("button");
  cancelDelete.type = "button";
  cancelDelete.textContent = "Cancel";
  const deleteActions = document.createElement("div");
  deleteActions.className = "workspace-actions";
  deleteActions.append(confirmDelete, cancelDelete);
  confirmation.append(warning, confirmLabel, confirmInput, deleteActions);
  management.append(managementTitle, renameForm, remove, managementNote, confirmation);
  picker.append(header, list, status, form, management);
  let remoteAvailable = false;
  let busy = false;
  const managementState = () => {
    rename.disabled = remove.disabled = renameInput.disabled = busy || current === "default" || !remoteAvailable;
    managementNote.textContent = current === "default"
      ? "Default is always available. Duplicate it to make a named workspace."
      : remoteAvailable ? "Close other tabs viewing this workspace before renaming or deleting it."
        : "Reconnect to the server before renaming or deleting. Creating and switching still work locally.";
  };
  managementState();
  root.append(picker, toggle);
  host.append(root);

  let names = new Set(["default", current]);
  const browserNames = workspaceCatalogue;
  const remember = (name) => {
    names.add(name);
    try {
      localStorage.setItem(CATALOGUE_KEY, JSON.stringify([...new Set([...browserNames(), name])]));
    } catch {
      // IndexedDB saves and the server listing can still recover the name.
    }
  };
  remember(current);
  const render = () => {
    list.replaceChildren();
    for (const name of [...names].sort((a, b) => a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b))) {
      const button = document.createElement("button");
      button.textContent = name;
      button.setAttribute("aria-label", `Switch to workspace ${name}`);
      if (name === current) {
        button.setAttribute("aria-current", "true");
        button.classList.add("current");
      }
      button.addEventListener("click", () => {
        if (name === current) dismiss();
        else void run(() => onSwitch(name));
      });
      list.append(button);
    }
  };
  let request = 0;
  const refresh = async () => {
    const generation = ++request;
    names = new Set(["default", current, ...browserNames()]);
    render();
    status.textContent = "Checking for other workspaces…";
    const local = Promise.resolve().then(() => localSavedNames?.()).catch(() => []);
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 3000);
    const remote = fetch("/api/workspaces", { signal: controller.signal, cache: "no-store" })
      .then((response) => {
        if (!response.ok) throw new Error("Workspace list unavailable");
        return response.json();
      })
      .then((data) => {
        if (!Array.isArray(data.workspaces)) throw new Error("Invalid workspace list");
        return data.workspaces.map((entry) => entry?.name).filter(validWorkspaceName);
      })
      .finally(() => clearTimeout(timeout));
    const [localResult, remoteResult] = await Promise.allSettled([local, remote]);
    if (generation !== request) return;
    if (localResult.status === "fulfilled" && Array.isArray(localResult.value)) {
      for (const name of localResult.value.filter(validWorkspaceName)) remember(name);
    }
    if (remoteResult.status === "fulfilled") {
      for (const name of remoteResult.value) names.add(name);
    }
    render();
    remoteAvailable = remoteResult.status === "fulfilled";
    managementState();
    status.textContent = remoteResult.status === "fulfilled"
      ? "Includes workspaces saved in this browser and on the server."
      : "Server unavailable. Your browser workspaces are still available.";
  };
  const dismiss = () => {
    if (busy) return;
    confirmation.hidden = true;
    picker.hidden = true;
    toggle.setAttribute("aria-expanded", "false");
    toggle.focus();
  };
  toggle.addEventListener("click", () => {
    if (!picker.hidden) return dismiss();
    picker.hidden = false;
    toggle.setAttribute("aria-expanded", "true");
    error.textContent = "";
    input.removeAttribute("aria-invalid");
    render();
    list.querySelector("[aria-current]")?.focus();
    void refresh();
  });
  close.addEventListener("click", dismiss);
  picker.addEventListener("keydown", (event) => {
    event.stopPropagation();
    if (event.key === "Escape") {
      event.preventDefault();
      dismiss();
    }
  });
  const run = async (action) => {
    if (busy) return;
    busy = true;
    picker.setAttribute("aria-busy", "true");
    for (const control of picker.querySelectorAll("button, input")) control.disabled = true;
    error.textContent = "";
    try { await action(); }
    catch (failure) { error.textContent = failure.message || String(failure); }
    finally {
      busy = false;
      picker.removeAttribute("aria-busy");
      for (const control of picker.querySelectorAll("button, input")) control.disabled = false;
      managementState();
    }
  };
  const checkName = (field) => {
    const name = field.value.trim();
    if (!validWorkspaceName(name)) {
      error.textContent = "Use 1–40 lowercase letters, digits or dashes, starting with a letter or digit.";
      field.setAttribute("aria-invalid", "true");
      field.focus();
      return null;
    }
    if (names.has(name)) {
      error.textContent = "That workspace already exists. Choose it from the list.";
      field.setAttribute("aria-invalid", "true");
      return null;
    }
    field.removeAttribute("aria-invalid");
    return name;
  };
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const name = checkName(input);
    if (name) void run(() => onSwitch(name));
  });
  duplicate.addEventListener("click", () => {
    const name = checkName(input);
    if (name) void run(() => onDuplicate(name));
  });
  renameForm.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!remoteAvailable || current === "default") return;
    const name = checkName(renameInput);
    if (name) void run(() => onRename(name));
  });
  remove.addEventListener("click", () => {
    if (!remoteAvailable || current === "default") return;
    confirmation.hidden = false;
    confirmInput.value = "";
    confirmInput.focus();
  });
  cancelDelete.addEventListener("click", () => {
    confirmation.hidden = true;
    remove.focus();
  });
  confirmation.addEventListener("submit", (event) => {
    event.preventDefault();
    if (confirmInput.value !== current) {
      error.textContent = `Type ${current} to confirm deletion.`;
      confirmInput.setAttribute("aria-invalid", "true");
      confirmInput.focus();
      return;
    }
    if (remoteAvailable && current !== "default") void run(onDelete);
  });
  window.addEventListener("storage", (event) => {
    if (event.key === CATALOGUE_KEY && !picker.hidden) void refresh();
  });
  return { isOpen: () => !picker.hidden, remember, refresh };
}
