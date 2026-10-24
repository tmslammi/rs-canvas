# Document snapshots and patches travel as binary wire frames

The page↔server document path uses WebSocket binary frames only. JSON carries control frames (hello, view, discussion, settings, hint, presentation, screenshot, workspace_changed, synced) because those are not document replication. This amends ADR-0005: agent edits still flow through the same Delta/merge seam, but the wire projection of a snapshot or patch never passes through a JSON string or an intermediate `Value` tree. It builds on ADR-0009: the snapshot payload is the existing document envelope, unchanged on disk.

## Frame layout

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | magic bytes `RSCWIRE\n` |
| 8 | 1 | wire version (`1`) |
| 9 | 1 | frame kind (`1` = snapshot, `2` = patch) |
| 10 | 8 | document revision, little-endian `u64` |
| 18 | 4 | payload length, little-endian `u32` |
| 22 | n | payload |

The frame magic is deliberately different from the document magic: a frame is not a document, and a receiver can reject the wrong bytes before decoding. The header is strict, like the document envelope: truncated headers/payloads, trailing bytes, an unknown wire version or kind, and a payload of the wrong kind are rejected before the payload is decoded.

- **Snapshot payload** is a complete `RSCDOC\r\n` document envelope. The document schema version and codec id stay inside it, so the on-disk format and versioning rules do not change.
- **Patch payload** is a bare sequence of full elements in the frozen v1 element layout (`crates/canvas-core/src/binary.rs`), tombstones included. It is decoded straight into `Vec<Element>` and merged element by element with the same last-writer-wins rules as every other patch.
- **Revision** is the sender's document revision. The server stamps its mirror revision. A page has no independent revision, so it echoes the last revision it applied (zero before the first server frame); the server ignores page revisions. Revisions are diagnostic/resync metadata only; merging never depends on them.

## Negotiation

There is none. Every page sends and receives document frames as binary; a JSON text frame on the socket is a control frame. The `hello` frame carries `sync` to disable document replication entirely (static serving and tests) but no encoding capability. A workspace builds exactly one binary frame per snapshot or patch and broadcasts it to every connected page. A direct resync is the same binary snapshot.

## Consequences

- The browser hands wasm a `Uint8Array` and sends the returned `Uint8Array`; it never stringifies a document and never parses a document frame. `reconcile_bytes`/`merge_patch_bytes` decode straight into the model, and `snapshot_frame_bytes`/`take_outbound_frame_bytes` encode straight from it.
- Mid-gesture deferral, origin suppression, resync snapshots, tombstones, revisions and agent edits keep their existing behaviour.
- JSON is the control-plane format and the explicit import/export format, never a document replication format.
- Measured 2026-10-24 (debug server, one Playwright page pair, 99,776-element connected board): a 14,069,805-byte snapshot upload reached the mirror in 866 ms; a fresh page decoded the same 14,069,805-byte download in 931 ms; a 375-byte agent patch reached two pages in 260 ms. A CPU profile of the incoming snapshot window had no `JSON.parse` or `JSON.stringify` samples; the functional probe additionally asserts no document-shaped JSON parse/stringify.
