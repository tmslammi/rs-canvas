# Persist documents in a versioned binary envelope (postcard payload)

Persistence uses a binary document envelope while JSON remains the import, export and wire format. The envelope is independent of the document schema: it carries the schema version and codec id, so a reader can reject a future document before decoding its payload and a future codec can reuse the same container. This amends ADR-0003: the schema is still rs-canvas's own, but its persistence encoding is now binary.

## Envelope layout

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | magic bytes `RSCDOC\r\n` |
| 8 | 4 | document schema version, little-endian `u32` |
| 12 | 1 | codec id (`1` = postcard) |
| 13 | 4 | payload length, little-endian `u32` |
| 17 | n | codec payload |

The header is strict: fewer bytes than the header, a payload shorter or longer than the declared length, or an unknown codec id is rejected. Bytes without the full magic are treated as legacy UTF-8 JSON; there is no ambiguity because JSON cannot start with `RSCDOC\r\n`. The same bytes and paths are reused (`current-v2` IndexedDB keys, checkpoint keys, `--doc` and `workspaces/<name>.json` mirror files), so old autosaves, checkpoints and mirrors load directly and the next save overwrites them with the binary envelope. The `.json` mirror name is kept until a later migration so existing `--doc` arguments, workspace listings and user expectations keep working.

## Payload codec

The payload uses **postcard** through an explicit v1 schema (`crates/canvas-core/src/binary.rs`) rather than serializing the model directly. The JSON model relies on internally tagged enums, `flatten` and `skip_serializing_if`, which positional binary codecs do not support; direct postcard serialization fails on the flattened `Element`. The explicit schema has positional struct fields, variant indexes plus positional fields for enums, and a presence tag for every `Option`. A borrowed mirror encodes a live document without cloning; an owned mirror decodes straight into the model without an intermediate `Value` tree. Field and variant order is the v1 schema, and the conversion matches are exhaustive, so adding a model field or kind fails to compile until the binary schema is handled.

Measured alternatives on the 10k-element `benchmark_fixture` (native release, 2026-10-24, `cargo run --release -p canvas-core --example persistence_bench`, median of 9):

| Encoding | Bytes | Save | Load |
| --- | ---: | ---: | ---: |
| JSON pretty (previous save/export) | 6,452,077 | 9.6 ms | 30.1 ms |
| JSON compact (previous mirror/wire) | 3,881,065 | 6.1 ms | 29.5 ms |
| CBOR (`ciborium`, direct model) | 2,842,608 | 3.2 ms | 29.8 ms |
| MessagePack (`rmp-serde`, direct compact) | 2,378,831 | 2.0 ms | 8.3 ms |
| MessagePack (`rmp-serde`, named) | 3,270,854 | 3.1 ms | 11.2 ms |
| **postcard, explicit binary v1 schema** | **1,279,183** | **1.3 ms** | **5.1 ms** |

The CBOR and MessagePack rows are payload-only probe timings; the JSON rows are the previous save/load paths and the final row includes the envelope, schema checks and sanitizing. Against the previous pretty save/export path the binary format is **7.3x faster to save, 5.9x faster to load and 80.2% smaller**; against compact JSON it is **4.6x faster to save, 5.8x faster to load and 67.0% smaller**. That clears the acceptance target of 3x faster and at least 50% smaller against either JSON baseline.

## Versioning and migration rules

- `SCHEMA_VERSION` in `canvas-core` is the single document schema version. The writer stamps it in the envelope; the payload does not repeat it.
- Read order is: envelope magic → header integrity → schema version and codec id → payload. A version newer than the build is `UnsupportedVersion`; a version with no migration path is `Invalid`; an unknown codec id is rejected before payload decoding.
- Version 1 is the first binary schema. `decode_binary` dispatches on the envelope version and version 1 decodes through the frozen v1 schema. When the document schema bumps, add a frozen v1 reader (or an explicit v1 → v2 migration) instead of editing the v1 structs; never renumber variants, fields or codec ids.
- Both JSON and binary loads funnel into `finish_document`: normalize the schema version, check element ids against map keys, validate order keys, clear dangling references and normalize inverted rectangles. Hostile-file treatment is therefore identical for both formats.
- JSON import remains available (`from_json`, `from_value`) with the existing `schemaVersion` migration seam; JSON export remains available (`to_json`, `to_json_pretty`, `to_file_json`). `from_value` stays the wire entry point, and the MCP/agent surface continues to render JSON on demand.
- A binary document this build cannot decode can still be recovered by importing a JSON export from the build that wrote it. Binary migrations are preferred, but JSON is the documented escape hatch.
- `from_bytes` auto-detects legacy JSON through the absence of the magic. This is the transition rule for autosaves, unload checkpoints and server mirrors; no separate migration job or key rename is needed.

## Consequences

- Autosave writes a `Uint8Array` under `bytes` in IndexedDB instead of a JSON string under `json`; a legacy `json` record is still read and replaced on the next save.
- `localStorage` is string-only, so unload checkpoints encode the binary envelope as base64 with a `rsc-bin1:` prefix. A checkpoint without the prefix is old JSON and is decoded as UTF-8 bytes.
- The server mirror is written from `to_bytes` and loaded with `from_bytes`; rename and merge use the same path. Discussion notes and settings stay JSON because they are not documents.
- The envelope module (`canvas_core::envelope`) is intentionally reusable for #34: `encode`/`decode`/`is_binary`, `Codec`, and `Envelope { schema_version, codec, payload }`.
