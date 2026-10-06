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
  note.textContent = "Live preferences; the agent reads them.";
  panel.appendChild(note);

  const render = () => {
    for (const [value, button] of cameraRow.buttons) {
      button.classList.toggle("active", value === settings.camera);
    }
    for (const [value, button] of pacingRow.buttons) {
      button.classList.toggle("active", value === settings.pacing);
    }
  };
  render();

  host.appendChild(panel);
  return {
    toggle: () => panel.classList.toggle("hidden"),
    setSettings: (next) => {
      settings = {
        camera: next?.camera ?? "gentle",
        pacing: next?.pacing ?? "normal",
      };
      render();
    },
  };
}
