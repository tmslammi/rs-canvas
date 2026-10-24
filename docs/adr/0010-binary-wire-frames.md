# Document snapshots and patches travel as binary wire frames

The page↔server document path uses WebSocket binary frames while control frames (hello, view, discussion, settings, presentation, screenshot, workspace_changed) stay JSON. This amends ADR-0005: agent edits still flow through the same Delta/merge seam, but the wire projection of a snapshot or patch no longer passes through a JSON string or an intermediate `Value` tree. It builds on ADR-0009: the snapshot payload is the existing document envelope, unchanged on disk.

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
- **Patch payload** is a bare sequence of full elements in the frozen v1 element layout (`crates/canvas-core/src/binary.rs`), tombstones included. It is decoded straight into `Vec<Element>` and merged element by element with the same last-writer-wins rules as the JSON patch.
- **Revision** is the sender's document revision. The server stamps its mirror revision. A page has no independent revision, so it echoes the last revision it applied (zero before the first server frame); the server ignores page revisions. Revisions are diagnostic/resync metadata only; merging never depends on them.

## Capability negotiation and fallback

The server advertises `"binary_sync": 1` in the JSON `hello` frame. A page that sees it uploads its snapshot as binary and uses binary for every subsequent snapshot/patch; a page that does not (an old server, or `?wire=json` in tests) keeps the existing JSON frames. A connection starts on the JSON path and is promoted when its first binary document frame arrives; the server therefore treats old pages, raw WebSocket clients and mixed-version pairs as legacy and replies in JSON. The server accepts JSON text frames and binary frames on the same socket.

A workspace tracks how many connected pages are binary. It builds the JSON encoding of a snapshot/patch only while a legacy page is connected, and the binary encoding only while a binary page is connected; when every page is binary the JSON pass is skipped entirely. Page joins are serialized with document-frame encoding/send, so a page cannot join between the capability check and the send and miss both encodings. A page sets its local binary flag before incrementing the capability count, so a broadcast that sees the count always has a reader for the bytes. A direct resync for one page uses that page's capability.

## Consequences

- The browser hands wasm a `Uint8Array` and sends the returned `Uint8Array`; it never stringifies a document and never parses a document frame. `reconcile_bytes`/`merge_patch_bytes` decode straight into the model, and `snapshot_frame_bytes`/`take_outbound_frame_bytes` encode straight from it.
- Mid-gesture deferral, origin suppression, resync snapshots, tombstones, revisions and agent edits keep their existing behaviour; only the frame encoding changes.
- JSON remains the fallback and the control-plane format, and the integration point for any future peer that cannot negotiate the binary wire.
- Measured 2026-10-24 (debug server, one Playwright page pair, 99,776-element connected board): a 14,069,805-byte snapshot upload reached the mirror in 866 ms; a fresh page decoded the same 14,069,805-byte download in 931 ms; a 375-byte agent patch reached two pages in 260 ms. A CPU profile of the incoming snapshot window had no `JSON.parse` or `JSON.stringify` samples; the functional probe additionally asserts no document-shaped JSON parse/stringify and no string-based wasm calls.
