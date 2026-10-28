# Own versioned JSON format, not third-party whiteboard compatibility

Documents and autosave use rs-canvas's own versioned JSON schema (`schemaVersion` plus migrations), not a third-party whiteboard format. Copying another tool's format was rejected for the initial version: it would make rs-canvas inherit their evolving schema and versioning, and rs-canvas constructs — frames that own children, bound elbow connectors, the custom glyph library — do not map cleanly onto it. Import/export interop with third-party whiteboard formats remains a possible adapter once the format stabilizes.

Persistence encoding moved to the binary envelope of ADR-0009, and the document wire is binary (ADR-0010). The schema, versioning and migration rules stay rs-canvas's own; JSON remains the import and export format.
