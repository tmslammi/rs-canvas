# The agent may read ephemeral view state

CONTEXT.md says View state is never shared. The agent bridge narrows that rule deliberately: while a page is connected, it reports camera, viewport and selection to the local server, which exposes them through `canvas_status` as clearly-labelled transient hints. The exception exists because "make *this* nicer" only works if the agent can see what *this* is. Limits: hints are last-writer-wins from the most recent page, are never persisted, never merged, never part of the document, never written to the change log, and disappear when the page disconnects. Nothing in the document model or merge logic may depend on them. If a per-user toggle is ever needed, it belongs on the page, gating the hint messages at the source.

## Considered options

- **Share only the selection, not camera** — rejected: focus and zoom commands already imply camera awareness, and "zoom in on what I'm looking at" needs it.
- **Persist view state server-side** — rejected: it is session noise, not part of the drawing, and would leak into multiplayer later.
- **No sharing at all** — rejected: the agent would have to guess what "this" means, which is the difference between a collaborator and a vending machine.
