# Workspace is a composition root over four collaborators

`canvas-server::Workspace` is split into four collaborators with disjoint
state ownership:

- `DocumentMirror` — the merged document, its revision, the bounded change log,
  element handles/aliases, and the agent transaction/revert journal.
- `PageHub` — connected-page presence, binary-wire accounting and the wire
  gate, the page broadcast sender, view hints, and presentation/screenshot
  request state.
- `WorkspaceStore` — name, document-lifetime epoch/generation, the persist
  path and debounced persist loop, creation and retirement.
- `DiscussionStore` — discussion notes and the path/flag that say whether they
  are persisted.

`Workspace` keeps its existing public operations and remains the only place
that sequences them. Absorbing a page frame is still: merge under the document
lock, record the change, broadcast winners/corrections, then queue a persist.
Publishing an agent edit is still: broadcast the patch, then queue a persist.
That ordering is the product behavior ADR-0011 depends on, and it does not
belong to any one collaborator.

## Consequences

- The document lock still comes before the change log, aliases and transaction
  journal. `PageHub` keeps its own wire gate; persistence keeps its own write
  lock. No collaborator reaches into another's state.
- Call sites use `Workspace::doc()`, `revision()`, `page_count()`,
  `broadcast()`, `discussion()` and similar delegations instead of public
  fields. Tests seed counts/revisions through test-only setters rather than
  poking atomics.
- Server-global page connection ids stay in `AppState`: they identify a
  socket process-wide, while origin skipping only compares ids within one
  workspace's hub.
- Workspace retirement stays lifecycle state in `WorkspaceStore`; the wakeup
  is the persist-loop notification, while connected pages are told through the
  `PageHub` broadcast.
- The `canvas-core::Editor` half of the issue is a separate stage.

## Considered options

- **An event bus or actor per workspace** — rejected: the existing async
  methods already carry the ordering, and an event loop would make the
  revision/broadcast/persist sequence harder to read and test.
- **Put frame workflows in `DocumentMirror` or `PageHub`** — rejected: each
  would need a reference to the other, recreating the god object behind a
  different name.
- **Duplicate shared state behind public fields to avoid call-site churn** —
  rejected: two handles to the same atomics is implicit coordination, the
  exact thing this split removes.
