# One command layer owns committed document edits

Committed edits run through `canvas-core::commands`. A command takes the document and a `Transaction`, applies reversible `Delta`s, and returns the affected ids or a typed result. The browser's `Editor` keeps gesture previews, but every commit and every agent tool uses the same commands, element defaults, label layout, delete closure and transaction journal. The server commits that journal directly: `Workspace` records the changed ids and broadcasts `Patch::from_deltas`, instead of cloning the document before the edit and diffing it afterwards. Revert stores the same bounded journal and replays its freshened inverse.

This keeps ADR-0002's single Delta seam and ADR-0005's page-authoritative mirror, while removing the parallel server mutation path that had already drifted (agent stickies used hachure while hand-drawn stickies were solid).

## Consequences

- `canvas_bulk` remains a batched builder with an additions-only record path; it still shares the command layer's element defaults.
- Typed, revisioned binary wire frames landed independently in the binary-sync work. The tombstone ledger that removes deletion-by-absence landed in ADR-0011; splitting `Editor`/`Workspace` remains a follow-up. This decision only fixes where committed edits live.

## Considered options

- **Keep server-side `doc_ops`/`verbs` mutators** — rejected: three implementations of the same semantics, with visible default drift.
- **Derive deltas by diffing before/after documents in `apply_transaction`** — rejected: O(document) per edit, loses provenance, and cannot distinguish a no-op update from a delete-and-readd.
- **Move gesture previews into commands too** — rejected: drags need cheap live preview state; commands own only the committed result.
