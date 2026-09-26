# Own versioned JSON format, not third-party whiteboard compatibility

Documents and autosave use rs-canvas's own versioned JSON schema (`schemaVersion` plus migrations), not a third-party whiteboard format. Copying another tool's format was rejected for v1: it would make rs-canvas inherit their evolving schema and versioning, and rs-canvas constructs — frames that own children, bound elbow connectors, the custom glyph library — do not map cleanly onto it. Import/export interop with third-party whiteboard formats stays on the roadmap as an adapter once the format stabilizes.
