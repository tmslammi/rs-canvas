// The canvas chat: a right-side drawer that talks to the local OpenCode
// service through canvas-server. The session it uses already has the canvas
// MCP tools from the project config, so the agent draws while it answers.
// The model is pinned server-side (DeepSeek 4.1 Flash for now).

export function createChatPanel(host) {
  const panel = document.createElement("div");
  panel.id = "chat-panel";
  panel.classList.add("hidden");
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-label", "Canvas chat");

  const header = document.createElement("div");
  header.className = "chat-header";
  const title = document.createElement("span");
  title.className = "chat-title";
  title.textContent = "Canvas chat";
  const modelLabel = document.createElement("span");
  modelLabel.className = "chat-model-label";
  modelLabel.textContent = "DeepSeek 4.1 Flash";
  modelLabel.title = "Pinned chat model";
  const closeButton = document.createElement("button");
  closeButton.className = "chat-close";
  closeButton.textContent = "×";
  closeButton.title = "Close chat";
  header.append(title, modelLabel, closeButton);

  const messages = document.createElement("div");
  messages.className = "chat-messages";

  const form = document.createElement("form");
  form.className = "chat-input";
  const input = document.createElement("textarea");
  input.placeholder = "Talk to the agent…";
  input.rows = 2;
  const send = document.createElement("button");
  send.type = "submit";
  send.textContent = "Send";
  form.append(input, send);

  panel.append(header, messages, form);
  host.appendChild(panel);

  let pending = false;
  let open = false;

  const bubble = (kind, text) => {
    const element = document.createElement("div");
    element.className = `chat-bubble ${kind}`;
    element.textContent = text;
    messages.appendChild(element);
    messages.scrollTop = messages.scrollHeight;
    return element;
  };

  const setPending = (value) => {
    pending = value;
    send.disabled = value;
    input.disabled = value;
  };

  const sendMessage = async (text) => {
    bubble("user", text);
    const thinking = bubble("assistant pending", "…");
    setPending(true);
    try {
      const response = await fetch("/chat", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text }),
      });
      const body = await response.json();
      thinking.remove();
      if (!response.ok) {
        bubble("error", body.error ?? "the chat failed");
        return;
      }
      bubble("assistant", body.reply ?? "");
    } catch (error) {
      thinking.remove();
      bubble("error", `the chat failed: ${error.message ?? error}`);
    } finally {
      setPending(false);
      input.focus();
    }
  };

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (!text || pending) return;
    input.value = "";
    sendMessage(text);
  });

  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      form.requestSubmit();
    }
  });

  closeButton.addEventListener("click", () => toggle(false));

  const toggle = (value) => {
    open = value ?? !open;
    panel.classList.toggle("hidden", !open);
    host.classList.toggle("chat-open", open);
    if (open) input.focus();
  };

  return { toggle, isOpen: () => open };
}
