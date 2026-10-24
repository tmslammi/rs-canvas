# The agent drives the canvas over a local bridge

An MCP client talks to the canvas through an MCP server hosted by the same Rust process that serves the web app: MCP Streamable HTTP at `/mcp` for tools, a WebSocket at `/ws` for the live page. The server mirrors the document; the page owns the interactive copy, and edits flow between them through the existing Delta/merge seam, never through a new protocol. Agent camera and presentation commands are one-way view-state commands: transient, never persisted, never merged, and cancelled by any user input. The server binds `127.0.0.1` only — the bridge is a local development surface, not a deployed one.

## Considered options

- **python static server plus a separate MCP process** — rejected: two servers to keep in sync, and the Rust process can serve files, WebSocket and MCP while linking `canvas-core` natively.
- **Agent edits the saved JSON file, the page watches it** — rejected: no live merge, no stable ids in the conversation loop, and presentation needs a live page.
- **The Rust server owns the only document, page as a thin client** — rejected: direct manipulation would round-trip every pointer move; local-first interactivity belongs in the page.
