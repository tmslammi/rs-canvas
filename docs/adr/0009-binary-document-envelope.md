# Persist documents in a versioned binary envelope (postcard payload)

Persistence uses a binary document envelope. JSON remains the explicit import and export format; the document wire is binary too (ADR-0010). The envelope is independent of the document schema: it carries the schema version and codec id, so a reader can reject a future document before decoding its payload and a future codec can reuse the same container. This amends ADR-0003: the schema is still rs-canvas's own, but persisted documents are always binary.

## Envelope layout

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | magic bytes `RSCDOC\r\n` |
| 8 | 4 | document schema version, little-endian `u32` |
| 12 | 1 | codec id (`1` = postcard) |
| 13 | 4 | payload length, little-endian `u32` |
| 17 | n | codec payload |

The header is strict: fewer bytes than the header, a payload shorter or longer than the declared length, an unknown codec id, or missing magic are rejected. Persistence has no second encoding: autosaves (`current-v2` IndexedDB keys), unload checkpoints, `--doc` and `workspaces/<name>.rsc` mirror files are binary documents.

## Payload codec

The payload uses **postcard** through an explicit v1 schema (`crates/canvas-core/src/binary.rs`) rather than serializing the model directly. The JSON model relies on internally tagged enums, `flatten` and `skip_serializing_if`, which positional binary codecs do not support; direct postcard serialization fails on the flattened `Element`. The explicit schema has positional struct fields, variant indexes plus positional fields for enums, and a presence tag for every `Option`.

Every persisted type has its own v1 mirror, including nested types: `PointV1`, `RectV1`, `StyleBorrowed`/`StyleOwned`, `ShapeKindV1`, `FillPatternV1`, `StrokeDashV1`, `RoutingV1` and `TextAlignV1`. Conversions are exhaustive and construct the live model without `..`, so adding or removing a model field, enum variant or element kind (or changing a field type) fails to compile here instead of silently changing what a version 1 payload means. Reordering the live model cannot change the payload because conversions match by name. A borrowed mirror encodes a live document without cloning; an owned mirror decodes straight into the model without an intermediate `Value` tree. Field and variant order in this module is the v1 schema, and a golden payload test pins the exact bytes (length plus FNV-1a hash) for a document covering every element kind, enum variant, style field and optional reference.

Measured on the deterministic 10k-element `benchmark_fixture` (native release, 2026-10-24, `cargo run --release -p canvas-core --example persistence_bench`; median of 5 runs, each itself a median of 9):

| Encoding | Bytes | Save | Load |
| --- | ---: | ---: | ---: |
| JSON pretty (previous save/export) | 6,393,554 | 10.75 ms | 32.76 ms |
| JSON compact (previous mirror/wire) | 3,822,542 | 7.02 ms | 30.77 ms |
| CBOR (`ciborium`, direct model) | 2,822,328 | 3.2 ms | 29.8 ms |
| MessagePack (`rmp-serde`, direct compact) | 2,358,447 | 2.0 ms | 8.3 ms |
| MessagePack (`rmp-serde`, named) | 3,250,470 | 3.1 ms | 11.2 ms |
| **postcard, explicit binary v1 schema** | **1,249,684** | **1.39 ms** | **5.58 ms** |

The CBOR and MessagePack rows are payload-only probe timings; the JSON rows are the previous save/load paths and the final row includes the envelope, schema checks and sanitizing. Against the previous pretty save/export path the binary format is **7.7x faster to save, 5.9x faster to load and 80.5% smaller**; against compact JSON it is **5.1x faster to save, 5.5x faster to load and 67.3% smaller**. That clears the acceptance target of 3x faster and at least 50% smaller against either JSON baseline.

## Versioning and migration rules

- `SCHEMA_VERSION` in `canvas-core` is the single document schema version. The writer stamps it in the envelope; the payload does not repeat it.
- Read order is: envelope magic → header integrity → schema version and codec id → payload. A version newer than the build is `UnsupportedVersion`; a version with no migration path is `Invalid`; an unknown codec id is rejected before payload decoding.
- Version 1 is the first binary schema. `decode_binary` dispatches on the envelope version and version 1 decodes through the frozen v1 schema. When the document schema bumps, add a frozen v1 reader (or an explicit v1 → v2 migration) instead of editing the v1 structs; never renumber variants, fields or codec ids.
- JSON imports and binary loads funnel into `finish_document`: normalize the schema version, check element ids against map keys, validate order keys, clear dangling references and normalize inverted rectangles. Hostile-file treatment is therefore identical for both formats.
- JSON import remains available (`from_json`, `from_value`) with the existing `schemaVersion` migration seam; JSON export remains available (`to_json`, `to_json_pretty`, `to_file_json`). `from_value` stays the workspace-management import entry point, and the MCP/agent surface continues to render JSON on demand.
- A binary document this build cannot decode can still be recovered by importing a JSON export from the build that wrote it. Binary migrations are preferred, but JSON import is the documented escape hatch.
- `from_bytes` accepts only the binary envelope. Bytes without the magic are rejected as `NotBinary`.

## Consequences

- Autosave writes a `Uint8Array` under `bytes` in IndexedDB. Loads read that record only; there is no JSON fallback.
- `localStorage` is string-only, so unload checkpoints encode the binary envelope as base64 with a `rsc-bin1:` prefix. A value without the prefix is not a checkpoint and is ignored.
- The server mirror is written from `to_bytes` and loaded with `from_bytes`; rename and merge use the same path. Discussion notes and settings stay JSON because they are not documents.
- The envelope module (`canvas_core::envelope`) is intentionally reusable for #34: `encode`/`decode`/`is_binary`, `Codec`, and `Envelope { schema_version, codec, payload }`.
