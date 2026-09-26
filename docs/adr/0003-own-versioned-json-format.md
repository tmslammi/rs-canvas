# Own versioned JSON format, not Excalidraw compatibility

Documents and autosave use rs-canvas's own versioned JSON schema (`schemaVersion` plus migrations), not Excalidraw's format. Excalidraw compatibility was rejected for v1: it would make rs-canvas inherit their evolving schema and versioning, and rs-canvas constructs — frames that own children, bound elbow connectors, the custom glyph library — do not map cleanly onto it. Import/export interop with `.excalidraw` files stays on the roadmap as an adapter once the format stabilizes.
