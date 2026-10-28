# Editor is a composition root over session, gesture, view and projection

`canvas-core::Editor` had grown to ~7.7k lines and ~30 fields spanning committed
edits, in-flight gestures, view state and derived render caches, so ownership
was implicit and the shell could mutate caches directly. `Editor` is now a
composition root with four collaborators, each owning one kind of state:

- `DocumentSession` owns the committed document, transaction journal and
  undo/redo history, and the internal clipboard. It is the only place committed
  content is mutated, and every mutation is a reversible `Delta` (ADR-0002) or a
  command's transaction (ADR-0009). Remote merges and patches replace content
  without clearing the local journal; opening a file does clear it.
- `InteractionController` owns the in-flight gesture, its open transaction, the
  marquee, pressure, hover decorations and the drag-preview caches. Previews
  never write the document: `drag_working` is contiguous preview truth,
  translated in place, with scene slots resolved once per gesture. Release
  writes it back in one pass and commits one transaction; cancel just drops it.
- `ViewState` owns camera/navigation, tool and lock, style defaults, grid
  settings, viewport size, selection and the selection-bounds cache. It is never
  persisted or merged, keeping the document/view-state split strict and the
  agent's view commands transient (ADR-0005, ADR-0006).
- `Projection` owns the scene cache, routing obstacle index and hit index. They
  are derived state with private fields; the shell paints and routes through
  read-only query methods, so rebuild and preview invariants stay in one place.

`Effects` and `ShellAction` moved out to a small `shell` module: the messages
across the browser boundary do not need editor internals. `Editor` keeps its
public entry points and delegates, so the wasm façade changes only where it used
to reach into public cache or document fields.

## Considered options

- **Let the gesture controller mutate the committed document directly** — rejected: drag previews would become committed edits and cancel would need a restore path; ADR-0009 keeps gesture previews outside the command layer.
- **Share the session behind `Rc<RefCell<_>>`** — rejected: it hides the single-mutator invariant and adds runtime checks to the per-pointer-move path.
- **Keep projection caches as public fields for the paint loops** — rejected: the shell could mutate derived state out of band and skip the rebuild rules; query methods are just as cheap.
- **Split the shell-facing command methods out of `Editor` too** — deferred: commands still need selection, view and projection together; the composition root is the honest place for that orchestration.
