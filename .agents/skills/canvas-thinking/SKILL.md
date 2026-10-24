---
name: canvas-thinking
description: "Explore engineering ideas with the open rs-canvas drawing: explain selected elements, compare designs, sketch flows or failure cases, revise diagrams, or revisit workspace decisions. A session that starts a new discussion opens its own workspace; questions about an existing drawing stay on that drawing. Use for shared-whiteboard discussions and references to this drawing; ordinary repository code changes use the normal coding workflow."
---

Use rs-canvas as shared working material for the user's idea. Read canvas_status
for workspace context, selection and saved discussion; read only the relevant
selection, handles or frame. Match the user's pace with a small sketch and a
concrete engineering tradeoff or failure case. Ask briefly when a reference or
constraint is ambiguous.

Give a new canvas discussion its own workspace. Before the first sketch, delegate
`canvas_workspace_create` to the canvas agent with a short, topic-based name
(lowercase letters, digits and dashes), then read `canvas_status` there and stay in
that workspace for the rest of the session. If the name already exists, add a
short unique suffix. Do not create a workspace when the session is about an
existing drawing, selection or named workspace — work on the one the user is
pointing at.

For drawing, editing, arranging, reverting or saving discussion notes, delegate
to the canvas agent. In Codex, spawn the custom agent type canvas; include the
workspace context, observed revision, relevant handles, desired change and
constraints. Codex's allowlist excludes mutations; in OpenCode the mutating
tools ask for approval. Keep discussion, read tools and gentle presentation on
the primary agent.

Preserve alternatives while exploring. Verify the canvas agent's returned changed
lines and transaction with a scoped `canvas_read`; ask for `canvas_screenshot`
only when the rendered result itself matters (text wrapping, ink, exact camera
framing) or the user asks to see it. A stale context requires rereading the
workspace, not forcing the write. Playback cancellation means the user took over;
it does not undo the drawing. Revert can conflict with later edits and is retained
only during this document lifetime.

When decisions become useful context, delegate merging constraints, agreed decisions
with their reasons, and open questions into canvas_status.discussion. Speculation
belongs in open questions until agreed. These notes appear in the Agent panel.

If the canvas server is unavailable, report the connection failure and continue
reasoning in text. The project dev server must be running (`npm run dev` in web/);
do not start a second server on the same port or edit the live mirror file as a
substitute for the tools.
