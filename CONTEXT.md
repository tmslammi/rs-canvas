# rs-canvas

A browser drawing tool for freehand sketching and cloud-architecture diagrams, in a hand-drawn style. Local-first and single-player today, designed so multiplayer can arrive later without reworking the document model.

## Language

**Document**:
One drawing: everything that sits on the canvas and gets saved to a file. The unit that autosaves locally, and that multiplayer will eventually sync.
_Avoid_: Scene, board, drawing, canvas

**Element**:
A single object placed in a document: a shape, freehand stroke, text, arrow or frame.
_Avoid_: Item, node, object

**Frame**:
A container element that owns the elements placed inside it, such as a VPC or region box. Moving or deleting the frame moves or deletes its children.
_Avoid_: Container, group, box

**Connector**:
An arrow whose endpoints bind to elements and re-route when those elements move. Elbow routing is the default for architecture diagrams.
_Avoid_: Edge, link

**Hand-drawn rendering**:
The visual language of the tool: strokes are drawn with rough, sketch-like variation rather than crisp geometry, and text uses a handwriting font.
_Avoid_: Sketchy, cartoon, rough style

**View state**:
Everything on screen that is not part of the document: camera/zoom, selection, hover, in-progress edits. Never persisted, never shared.
_Avoid_: UI state, session

**Delta**:
A single reversible change to a document — add, update or remove an element. Every edit goes through one; undo replays its inverse.
_Avoid_: Mutation, patch, operation

**Patch**:
The wire projection of a gesture's deltas: the full elements that changed, tombstones included, merged element by element with last-writer-wins. What the page and the server exchange between snapshots.
_Avoid_: Changeset, delta batch
