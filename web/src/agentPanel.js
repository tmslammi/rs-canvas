// Agent settings: the user's live controls over what the agent may do with
// the canvas. Changes travel to the server, which enforces them and reports
// them to the agent through canvas_status.

const CAMERA_MODES = [
  { value: "off", label: "Off", hint: "no camera moves from the agent" },
  { value: "gentle", label: "Gentle", hint: "only when needed, zoom capped at 2×" },
  { value: "full", label: "Full", hint: "free framing" },
];
const PACINGS = [
  { value: "fast", label: "Fast" },
  { value: "normal", label: "Normal" },
  { value: "slow", label: "Slow" },
];

export function createAgentPanel(host, { onChange }) {
  const panel = document.createElement("div");
  panel.id = "agent-panel";
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-label", "Agent settings");
  panel.classList.add("hidden");

  const title = document.createElement("div");
  title.className = "agent-title";
  title.textContent = "Agent";
  panel.appendChild(title);

  let settings = { camera: "gentle", pacing: "normal" };

  const buildRow = (label, options, key) => {
    const section = document.createElement("div");
    section.className = "style-section";
    const labelEl = document.createElement("div");
    labelEl.className = "style-label";
    labelEl.textContent = label;
    section.appendChild(labelEl);
    const optionsEl = document.createElement("div");
    optionsEl.className = "style-options";
    const buttons = new Map();
    for (const option of options) {
      const button = document.createElement("button");
      button.textContent = option.label;
      if (option.hint) button.title = option.hint;
      button.addEventListener("click", () => {
        settings = { ...settings, [key]: option.value };
        onChange({ [key]: option.value });
        render();
      });
      optionsEl.appendChild(button);
      buttons.set(option.value, button);
    }
    section.appendChild(optionsEl);
    return { section, buttons };
  };

  const cameraRow = buildRow("Camera", CAMERA_MODES, "camera");
  const pacingRow = buildRow("Pacing", PACINGS, "pacing");
  panel.appendChild(cameraRow.section);
  panel.appendChild(pacingRow.section);

  const note = document.createElement("div");
  note.className = "agent-note";
  note.textContent = "Changes apply immediately and are shared with the agent.";
  panel.appendChild(note);

  const discussion = document.createElement("section");
  discussion.className = "agent-discussion";
  discussion.setAttribute("aria-label", "Workspace discussion");
  panel.appendChild(discussion);

  const setDiscussion = (record, persisted = false) => {
    discussion.replaceChildren();
    const heading = document.createElement("h3");
    heading.textContent = "Workspace discussion";
    discussion.appendChild(heading);
    let count = 0;
    for (const [key, title] of [["constraints", "Constraints"], ["decisions", "Decisions and reasons"], ["questions", "Open questions"]]) {
      const entries = record?.notes?.[key] ?? [];
      if (!entries.length) continue;
      const label = document.createElement("h4");
      label.textContent = title;
      const list = document.createElement("ul");
      for (const text of entries) {
        const entry = document.createElement("li");
        entry.textContent = text;
        list.appendChild(entry);
        count++;
      }
      discussion.append(label, list);
    }
    const hint = document.createElement("p");
    hint.className = "agent-note";
    hint.textContent = count
      ? (persisted ? "Saved with this workspace. Ask the agent to revise these notes." : "Kept for this server session. Ask the agent to revise these notes.")
      : "Ask the agent to remember constraints, decisions or open questions here.";
    discussion.appendChild(hint);
  };
  setDiscussion(null);

  const render = () => {
    for (const [value, button] of cameraRow.buttons) {
      const active = value === settings.camera;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
    }
    for (const [value, button] of pacingRow.buttons) {
      const active = value === settings.pacing;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
    }
  };
  render();

  host.appendChild(panel);
  return {
    setDiscussion,
    toggle: () => !panel.classList.toggle("hidden"),
    setSettings: (next) => {
      settings = {
        camera: next?.camera ?? "gentle",
        pacing: next?.pacing ?? "normal",
      };
      render();
    },
  };
}
