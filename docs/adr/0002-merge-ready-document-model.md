# Document model is merge-ready from day one

Every document uses a merge-ready model even though the app is single-player today: stable immutable element IDs, per-element version + versionNonce last-writer-wins, fractional index for z-order, soft-delete tombstones, a strict document/view-state split, and a single seam where every edit is a reversible delta. This costs extra plumbing now, but it makes merge logic unit-testable today and lets a CRDT (e.g. Loro) slot in later without rewriting references, ordering or history.

## Considered options

- **Lean model (IDs + delta seam only)** — rejected: defers merge bugs into a future rewrite.
- **Integrate a CRDT immediately (Loro)** — rejected: adds ~1 MB of WASM and CRDT complexity to an app with no networking.
