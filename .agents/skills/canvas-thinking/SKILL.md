---
name: canvas-thinking
description: "Explore engineering ideas with the open rs-canvas drawing: explain selected elements, compare designs, sketch flows or failure cases, revise diagrams, or revisit workspace decisions. Use for shared-whiteboard discussions and references to this drawing; ordinary repository code changes use the normal coding workflow."
---

Use rs-canvas as shared working material for the user's idea. Read canvas_status
for workspace context, selection and saved discussion; read only the relevant
selection, handles or frame. Match the user's pace with a small sketch and a
concrete engineering tradeoff or failure case. Ask briefly when a reference or
constraint is ambiguous.

For drawing, editing, arranging, reverting or saving discussion notes, delegate
to the canvas agent. In Codex, spawn the custom agent type canvas; include the
workspace context, observed revision, relevant handles, desired change and
constraints. The primary agent's tool allowlist deliberately excludes mutations.
Keep discussion, read tools and gentle presentation on the primary agent.

Preserve alternatives while exploring. Verify the canvas agent's returned changed
lines and transaction; use a scoped read or screenshot when needed. A stale context
requires rereading the workspace, not forcing the write. Playback cancellation
means the user took over; it does not undo the drawing. Revert can conflict with
later edits and is retained only during this document lifetime.

When decisions become useful context, delegate merging constraints, agreed decisions
with their reasons, and open questions into canvas_status.discussion. Speculation
belongs in open questions until agreed. These notes appear in the Agent panel.

If the canvas server is unavailable, report the connection failure and continue
reasoning in text. The project dev server must be running (`npm run dev` in web/);
do not start a second server on the same port or edit the live mirror file as a
substitute for the tools.
