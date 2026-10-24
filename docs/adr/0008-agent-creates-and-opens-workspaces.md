# The agent may create and open a workspace; the user still follows the page

ADR-0007 kept workspace creation outside the agent: a workspace existed when a page or a file named it, and every tool acted on the user's current workspace. That rule is right for drawing, but wrong for the start of a task. "Start a new board for the auth flow" is a normal thing to ask an agent, and it should end with a fresh, empty workspace open on screen — not with an agent that has to ask the user to use the workspace picker first.

`canvas_workspace_create` creates the named workspace and asks the page showing the current workspace to switch to it. The page saves its outgoing document locally and reloads into `?ws=<name>`; the ordinary page-activity rule then makes the new workspace current, and the agent waits briefly for that page to connect before returning. With no page connected the workspace is created but not made current: the agent still cannot draw somewhere nobody is watching.

The follow-the-user invariant is unchanged. Creation is a visible navigation that the user watches happen, not a hidden session binding: the agent still never owns routing state, and there is still no per-call workspace argument. What ADR-0007 rejected — an agent that draws on a workspace while the user looks at another one — remains rejected here.

## Considered options

- **Create only, let the user switch manually** — rejected: the agent cannot fill the workspace it just created, so "make me a board for X" would leave the drawing on the old workspace and an empty workspace in the picker.
- **Bind the MCP session to the new workspace while the page stays put** — rejected: this is the hidden-drawing failure ADR-0007 rejected. It is also surprising: a later tool call would silently target a canvas the user is not looking at.
- **A `workspace` argument on every tool call** — still rejected: every call site must remember it, and cross-workspace handle mix-ups become silent errors.
- **Rename and delete over MCP** — not included. Creation is the missing start-of-task verb; destructive management stays in the page where the confirmation UI lives.

## Consequences

- `canvas_workspace_create` validates the workspace name, refuses `default` and existing names, and writes an empty document immediately when the workspace has a backing file, so an explicit creation survives a restart before its first edit.
- Created workspaces are listed even when empty; lazily named, never-used husks remain hidden.
- The page handles a new `workspace_changed` action, `switch`: save the outgoing document locally, then navigate to the new workspace. Existing rename and delete actions keep their behaviour.
- All other tools remain current-workspace only. After a failed page switch the tool reports `switched: false`; the agent must read `canvas_status` before drawing.
