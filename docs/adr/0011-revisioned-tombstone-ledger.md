# Deletion is an explicit revisioned record, never absence

A page snapshot now carries the server revision it was based on, and the page keeps a purge ledger for elements whose document entry disappeared locally (undoing a creation). The ledger's tombstones are folded into snapshots and autosaves, so the server never has to infer a deletion from a missing element. The server reconciles snapshots and patches through one `canvas-core` policy, returns the merge outcome, and sends the uploader a correction patch for every live element it omitted or every copy it lost. A small `synced` control frame acknowledges the applied revision, so the next snapshot has an accurate base even when no correction was needed. `Workspace::apply_page_frame` is that same path without a WebSocket, so connect/upload/edit/reconnect sequences are unit-testable.

This keeps ADR-0005's page-authoritative mirror and ADR-0002's single Delta/merge seam. The typed binary frames from ADR-0010 stay the carrier; only their snapshot revision field gained a defined meaning for page uploads.

## Scope

- Applies to the page↔server document path. MCP tools, view/presentation control frames, gesture previews and drag performance are unchanged.
- A page snapshot's revision is a base/catch-up hint. Element-level last-writer-wins still decides every merge, and a revision never authorizes deleting an absent element.
- A page snapshot must carry its base revision. A frame without `rev` is malformed; absence is never interpreted as a deletion.
- `PageSync`'s per-connection tombstone synthesis and the `mirror_leads` download heuristic are removed.

## Consequences

- An element absent from a snapshot but present in the mirror survives. If it is live and the page omitted it, the server sends it back as a correction instead of treating the omission as a delete.
- A stale reconnect copy loses to the newer mirror copy by the normal version/nonce rules, and the page receives the winner as a correction. The mirror is not overwritten.
- A purge made while disconnected reaches the mirror with the same tombstone the wire patch would have carried. Undo of a creation and ordinary deletes are both explicit on reconnect.
- Snapshot and patch index collisions run the same deterministic regeneration pass, so the two paths converge on the same z-order.
- A full snapshot is still sent for a lagged broadcast or a client that never uploads. It is a repair frame, not the correction path.
- The purge ledger's tombstones persist in autosaves, so a reload keeps an explicit purge. They are ordinary tombstones once loaded and merge like any other deleted element.

## Considered options

- **Keep per-connection previous snapshots and infer disappearances** — rejected: a new connection has no base, a purge made while disconnected is exactly the case it cannot see, and reconnect ordering can revive elements.
- **Make a page snapshot authoritative and delete every mirror element it omits** — rejected: a stale autosave or a partially loaded page would wipe server state, and it contradicts ADR-0002's union semantics.
- **Send a full snapshot after every upload** — rejected: it restores the download cost the connect handshake was designed to avoid; a targeted correction patch is proportional to what the page actually lost or omitted.
- **Persist the purge ledger as a separate sidecar** — rejected: the document is already the autosave unit; materialized tombstones survive reloads without a second storage format or a migration.
