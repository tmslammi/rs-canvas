---
status: accepted
---

# Workspaces isolate concurrent canvas users; the agent follows the user

The server hosts many **workspaces** instead of one canvas. A workspace is a named, isolated drawing context — one document and the pages viewing it — and the unit of isolation between concurrent users of the same server. Pages pin a tab to a workspace with `?ws=name`; each tab is a live view of exactly that document, with its own autosave slot. The agent has no workspace of its own: every tool call targets the **current workspace** — the one the user's most recently active page is on, or `default` when no page is connected. The workspace is therefore context the page reports (like the camera and selection hints of ADR-0006), not routing state the agent owns: the agent can never draw somewhere the user isn't looking, and view commands stop hitting every open tab. `default` is the unnamed workspace, so existing URLs, `--doc`, the autosave key and single-canvas behaviour are unchanged. Aliases, revisions, change logs, view hints, page broadcasts and pending present/screenshot requests are scoped per workspace; agent settings stay global. Workspaces other than `default` persist beside the default document at `workspaces/<name>.json`.

## Considered options

- **Agent sessions bind to a workspace via a tool** (`canvas_workspace switch`) — rejected: the workspace is context on what the user is working on, not agent-owned routing state. Session binding lets an agent draw somewhere the user isn't looking — fatal to the main flow, which is watching the agent draw — and costs a tool, per-session state and presence bookkeeping to keep honest.
- **A `workspace` argument on every tool call** — rejected: stateless, but every call site must remember it; agents will forget, and cross-workspace handle mix-ups become silent errors. May return as an explicit override if a real "work on workspace B while I watch A" workflow appears.
- **One merged canvas with frames as workspace boundaries** — rejected: isolates nothing, just renames the problem.

## Consequences

- The singleton fields of `AppState` (document mirror, persist path/loop, revision, change log, aliases, view-hint slot, page broadcast, pending present/screenshot) move into a per-workspace struct; `AppState` becomes a map plus current-workspace resolution (the workspace of the most recent inbound page frame; the page also reports a hint when its tab regains focus, so "current" tracks the user's attention).
- With no page connected the current workspace is `default`, preserving today's headless flow: agent edits land in the `--doc` mirror and appear on next connect.
- A workspace needs no creation RPC: it exists when a page or a file names it. The switcher in the page chrome lists live and persisted workspaces from `GET /api/workspaces`.
- In-memory lifecycle mirrors today's rule per workspace: a workspace with no backing file drops its mirror when its last page disconnects.
