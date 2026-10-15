# Making rs-canvas easier for agents to use

Investigation and proposed direction, 2026-10-10, against commit `e5191e7`.
This is a proposal, not an accepted ADR or an implemented feature. Repository
links below are the evidence for current behavior; proposed contracts are
illustrative. Protocol references are in
[agent-interface-sources.md](agent-interface-sources.md).

Implementation update, 2026-10-11: the first conversational milestone now has
semantic update/reconnect beats, owned frame creation, scoped/paginated reads,
workspace context guards, cursor-gap detection, transaction receipts and
guarded revert, glyph discovery, and inspectable persisted discussion notes.
The canvas agent brief and primary-agent guidance now describe the small
sketch → discuss → revise workflow. See [README](../../README.md) for current
usage. The gaps and test failures below describe the investigation baseline;
the server test baseline has since been restored. Full CLI/ops consolidation,
cropped captures, automatic layout diagnostics and retry deduplication remain
future work, rather than claims of this milestone.

## Recommendation

The user's clarified goal is **an engineering thinking partner**: someone to
throw rough ideas around with, who can sketch, question, compare and revise
alongside the user. The document is shared working material for a conversation.
Reliable agent editing enables that experience; it is not the product goal.

Optimize for a short loop: understand the idea → sketch just enough → discuss
one useful question or tradeoff → revise together. Give agents a small semantic
interface that owns geometry, labels, frame membership, connectors and
transactions. Let agents describe relationships; let Rust maintain them.
Live MCP collaboration comes first; the planned CLI is a secondary adapter.

## What the thinking partner should feel like

The user can start with an incomplete thought: “What if the workers pulled
jobs instead?” The agent follows the existing design, draws the changed flow,
and raises a concrete consequence: “That gives workers control over load.
Where should a job live while a worker is processing it?” It sketches the
failure case if that helps, then revises when the user says “Actually, assume
at-least-once delivery.” A rough idea should not require a finished specification.

The interaction should support these habits:

- **Shared attention.** Understand “this part,” the user's selection and recent
  edits. Point to the relevant element while explaining. Ask briefly when the
  reference is ambiguous instead of guessing.
- **Small contributions.** Draw the minimum useful sketch and make one useful
  observation. Match the user's pace; avoid turning every thought into a full
  architecture diagram or a long interrogation.
- **Engineering judgment.** Connect questions to the design at hand: failure
  modes, load, ownership, consistency, implementation cost and operational
  burden. Distinguish a hypothesis from a verified fact. Challenge a weak
  assumption with a concrete scenario, and adjust when the user supplies context.
- **Cheap alternatives.** Show option B beside option A, walk through the
  consequences, and keep the original recoverable. Explore before committing
  the document to one direction.
- **Visible reasoning.** Use arrows, annotations and short captions to explain
  flows and failure cases. Keep speculation recognizable without cluttering
  every element with metadata. Pause presentation when the user takes over.
- **Continuity.** Recall the constraints, open questions and reasons behind
  decisions when a workspace is revisited. Offer a short recap, with unresolved
  assumptions clearly separated from agreed decisions.

Continuity needs deliberate design: the drawing alone does not capture why a
choice was made. Begin with a small, inspectable workspace discussion note
(constraints, alternatives, decisions and open questions), maintained with
the conversation. Keep it separate from transient camera/selection hints and
from element merge metadata. Treat it as a proposed persisted artifact; decide
its ownership and update rules before implementing it. Store useful engineering
context rather than claiming unlimited memory of past conversations.

## First product milestone

Prove one end-to-end conversation in the existing app: sketch a producer,
queue and worker; discuss worker failure; compare acknowledgment before and
after processing; revise the preferred option; return later and recover the
reason for the choice. The user can interrupt, select an element, change an
assumption and undo an exploration throughout.

For that milestone, prioritize selected/scoped reads, update/reconnect,
alternative creation and safe reversal, then a concise canvas-agent workflow
brief. Prototype continuity with an explicit discussion note. The technical
roadmap below supplies the necessary foundations, but CLI distribution,
10k-element optimization and broad operation coverage should not delay testing
this conversational experience.

Evaluate whether the user can think aloud naturally, whether sketches advance
understanding, whether challenges are relevant, and whether interruptions and
corrections work. Count unnecessary clarification questions and unwanted
drawing changes alongside task success and latency. A polished diagram alone
is not evidence of a useful thinking partner.

## What already works

- The Rust core owns the document, merge rules and rendering. MCP accesses it
  natively; the browser uses WASM ([README](../../README.md),
  [ADR-0001](../adr/0001-rust-core-canvas2d-thin-ts-shell.md)).
- Workspace-scoped short handles and compact text reduce reference overhead
  ([aliases.rs](../../crates/canvas-server/src/aliases.rs),
  [describe.rs](../../crates/canvas-core/src/describe.rs)).
- `canvas_script` already batches drawing and connectors atomically. Without
  presentation fields it commits immediately; a separate silent batch tool
  is not needed merely to avoid playback
  ([script.rs](../../crates/canvas-server/src/script.rs:160),
  [mcp.rs](../../crates/canvas-server/src/mcp.rs:638)).
- Relative placement, bound labels, bound connectors, transient view hints,
  camera preferences, and screenshots provide useful collaboration primitives
  ([doc_ops.rs](../../crates/canvas-server/src/doc_ops.rs),
  [ADR-0006](../adr/0006-agent-reads-ephemeral-view-state.md),
  [mcp.rs](../../crates/canvas-server/src/mcp.rs)).
- Patch synchronization and compact reads already improved efficiency. The
  existing [protocol results](protocol-results.md) are historical benchmark
  evidence, not measurements repeated in this investigation.

## Gaps in the current interface

| Gap | Current evidence | Consequence |
| --- | --- | --- |
| No general update operation | MCP tool router and `ScriptBeat` expose create/connect/discussion verbs, but no text, size, membership or general style update | Agents cannot reliably refine drawings in place |
| Unbounded full reads; no selected/detail/search read | `ReadArgs` contains only `since` | A small question can require the whole document |
| Cursor truncation is silent | `changes_since` filters a 200-record log without checking retained coverage | A partial change history can look complete |
| Workspace and handle lifetime are implicit | Every call resolves the most recently active workspace; aliases are in-memory and workspace-local | `e1` can mean a different element after attention switches or the server restarts |
| Incomplete results | Draw/pen echo one compact line; most writes lack workspace, revision and dependent-element outcomes | Verification needs additional reads |
| Glyph discovery is absent from MCP | Core has `glyphs::catalog`; MCP exposes tools only, with no catalog tool/resource | Models must guess glyph IDs or read source |
| Several choices are unrestricted strings | Draw kind, side, emphasis, annotation and zoom arguments | The schema cannot fully guide valid choices |
| Placement guarantees are overstated | Script description says “never overlaps”; explicit coordinates bypass the search, and `inside` falls back to an occupied position | Agent expectations exceed actual behavior |
| Containment is not exposed | `inside` chooses coordinates; `around` computes frame bounds, but neither sets children's `frame_id` | Visual enclosure is mistaken for ownership |
| Screenshot provenance is implicit | A live request broadcasts to all pages in a workspace; timeout silently falls back to a whole-document headless image | The model may misinterpret which view it sees |
| Agent changes have no user undo entry | `agent-sync.spec.ts` explicitly checks this behavior | A mistaken agent edit lacks a convenient existing reversal path |

Sources: [MCP](../../crates/canvas-server/src/mcp.rs:202),
[change log](../../crates/canvas-server/src/state.rs:497),
[workspace resolution](../../crates/canvas-server/src/state.rs:655),
[placement](../../crates/canvas-server/src/doc_ops.rs:220),
[script frame creation](../../crates/canvas-server/src/script.rs:280),
[screenshots](../../crates/canvas-server/src/mcp.rs:869),
[undo test](../../web/e2e/agent-sync.spec.ts:192).

## Proposed interface

### 1. A shared semantic editing module

Build `canvas-core::ops`, as already proposed in
[issue #20](https://github.com/tmslammi/rs-canvas/issues/20). Move applicable
creation, reference resolution, label and connector behavior out of the server
into this module. MCP and CLI become adapters at the same seam; retain existing
tool names as compatibility adapters.

Start with add, update, connect, remove and layout operations. Updates should
rename, move, resize, restyle and reparent elements, and reconnect connectors.
Creation and updates must preserve owned labels, recursively handle frame
children, reroute affected connectors, and reflow connector labels. Explicitly
define removing an endpoint, selecting both a frame and its child, and changing
frame membership without changing world coordinates.

Use one tagged operation enum instead of a bag of optional action fields.
Resolve batch-local aliases before workspace handles; reserve the `e<number>`
namespace or reject alias collisions. Accept stable raw IDs too. Keep the
existing pen capability; add batch strokes if measured workflows need them.

Execute atomically and return outcomes plus touched IDs and reversible deltas.
The existing clone-and-commit script is a useful initial correctness model;
optimize touched-element work after measuring it. Do not return partially
applied success if a later operation fails.

**Decision to reconcile:** issue #20 proposes direct document mutation for CLI
operations. ADR-0002 specifies a reversible delta seam. Prefer a shared operation
implementation that produces deltas for either adapter, so live history,
recovery and file operations cannot drift. Record this decision before building
the module. Keep transport synchronization and persistence outside it.

### 2. Bounded observation and discovery

Extend `canvas_read` with explicit scopes: selected elements, handles, frame,
viewport/region, and label/kind search. Return a compact summary by default for
large documents; provide paginated element listings and exact detail on demand.
Keep a full-read option for compatibility and small drawings.

Summary: document bounds, live counts by kind, frames, selection handles,
revision, and a continuation cursor. Detail: exact geometry, style, owned-label
references, frame ownership, bindings and element versions. Stroke points are
opt-in. Summaries may round coordinates; detail must preserve precision. Full
styles should not be added to every compact line.

Read results must identify workspace, document identity, server epoch and
revision. Capture the document and its revision consistently under the document
lock: `canvas_read` currently loads revision before acquiring that lock.
Pagination should use a snapshot or reject a cursor when its revision changes.

For `since`, return complete current state of the touched elements initially;
field diffs can wait. Detect cursors older than retained history, from a future
revision, another document or an earlier epoch. Return `resync_required` with
the next read action. Never present a partial history as complete.

Expose the existing glyph catalog with search/category filters through a tool
or resource. Include supported operations, schema version and limits in compact
capability discovery. Resources suit reference material; keep essential
discovery available to clients that primarily support tools.

### 3. Context-safe transactions and actionable feedback

Return an opaque context token from reads. A write presents it to **assert**
that the current workspace and document epoch are still the ones observed.
If the user has switched workspaces, reject and request a reread. The token
must never route a write to an inactive workspace: preserve ADR-0007's rule
that the agent follows the user. This is a proposed addition to that ADR's
interface, not an agent-owned workspace selector.

Use per-element version preconditions for updates/removals. A global revision
guard is a simple first implementation but unnecessarily conflicts when a
human edits unrelated elements. Validate context, preconditions and operations
within the same serialized transaction.

Support a caller-supplied request ID. Cache committed outcomes by document
epoch and request ID, reject reuse with different inputs, and state the cache's
retention/lifetime. A timed-out create can then be retried without duplication
within that retention window. Do not promise retry safety across restart unless
the ledger is persisted.

Every write returns workspace/epoch, committed revision, transaction ID,
resolved aliases, compact changed lines, removed handles and warnings. Include
dependent labels/connectors/children in the touched set. Distinguish document
commit, page delivery, playback cancellation and persistence; cancelled
presentation does not undo the already committed drawing.

Errors should identify `code`, failing operation index, field/reference,
whether anything committed, and a repair action. Distinguish missing elements,
deleted elements, stale context, unsupported changes and unavailable rendering.
For a bad glyph, offer catalog search; for stale context, offer a new read.
Use strict object schemas, enums and documented ranges; explicitly disclose
any clamping. Keep tool-execution failures model-visible as MCP error results.

Add transaction revert after the editing contract is stable. Revert creates
fresh deltas/tombstones and checks affected element versions; it must not
restore a whole-document snapshot over intervening human edits. Expose the
same transaction action to the user. Do not silently insert remote edits into
the existing local undo stack.

### 4. Semantic layout and visual verification

Extend layout beyond `tidy`'s square grid: row/column, alignment, spacing,
fit-frame, and relative placement. Choose one simple layout contract initially;
return final positions and diagnostics. Keep layout deterministic and implement
it inside Rust rather than asking the model to calculate every coordinate.

Separate geometric enclosure from frame membership. Setting `frame` establishes
ownership; `around` can fit the frame to its children. Diagnose or reject
insufficient space instead of claiming every placement avoids overlaps.

Add scoped visual capture by element/frame/region with padding and a pixel
budget. Select the active page deliberately when several pages view the same
workspace. Report capture source (`live`/`headless`), requested region,
world-to-image mapping and revision provenance. For live captures use the
page's acknowledged revision, not an assumed mirror revision. Headless fallback
must be labeled. Keep camera commands separate from capture.

Offer geometric checks for unintended overlap, out-of-frame children,
dangling bindings and labels extending outside their owners. Allow intentional
overlap and enclosure; diagnostics should explain severity and scope. Browser
font metrics and the server's `ApproxMeasurer` differ, so geometric checks do
not replace a final image inspection for layout-heavy tasks.

### 5. Portable onboarding

Implement the CLI from issue #20 over the shared module: apply JSON operations
from stdin, list/search, glyph discovery, validate and render. Use atomic file
writes, stable JSON output/error contracts and explicit document paths. File
editing and the live bridge need distinct usage guidance: atomic rename alone
does not prevent competing writers, and directly editing a live mirror file
does not synchronize the browser.

Provide a portable skill with one tested creation example, one refinement
example, and recovery guidance. Replace “tools once each” in the existing
canvas subagent brief with read relevant context → batch edit → inspect
outcomes → verify scoped state/image → revise when needed. Update primary-agent
permissions when adding write tools so all live document changes still go
through the canvas subagent.

Publish protocol/client compatibility explicitly. Use MCP annotations and
output schemas where supported; structured results help integrations but do
not automatically reduce model tokens. Measure actual client-visible payloads.
Do not bundle a protocol-version migration with the editing redesign.

## Implementation order and acceptance checks

| Order | Deliverable | Acceptance check |
| --- | --- | --- |
| 0 | Restore server test compilation; establish representative tasks | Existing native tests run; record current task success and failures |
| 1 | Shared operations + MCP updates + catalog | Rename/move/reparent without replacing IDs; labels and connectors follow; failed batch changes nothing |
| 2 | Context guard + consistent revisions + cursor gap detection + result contract | Switch tabs or restart between read/write; stale handles never edit another document; truncated history requests resync |
| 3 | Scoped reads, detail, search, pagination, scoped screenshots | Inspect one frame in a 10k-element document with bounded output; identify the capture source |
| 4 | CLI and portable workflow skill | Same operation fixture yields equivalent document semantics through both adapters |
| 5 | Deterministic layout, diagnostics, safe retries and guarded revert | Repair crowded layouts; replay a request without duplicates; reject revert over later human changes |

Use real agent task evaluations alongside contract tests. Include: create a
three-tier diagram, rename one selected element, move a frame with children,
reconnect an existing connector, choose a glyph, repair crowding, resume after
more than 200 edits, handle a workspace switch, retry after a lost response,
and operate headlessly. Run against small and 10k-element documents with
unrelated content; evaluate more than one model class and repeat runs.

Measure verified task success, calls, actual input/output tokens, elapsed time,
repair attempts, unintended touched elements and visual defects. Treat zero
wrong-document edits, atomic failures and preserved bindings as correctness
requirements. Establish a baseline before setting numerical efficiency goals;
do not substitute heuristic token estimates for model/client measurements.

## Verification performed for this investigation

`cargo check -p canvas-server` passes. `cargo test -p canvas-core --lib`
passes all 223 tests. No browser tests or live drawing changes were performed.

`cargo test -p canvas-server` fails at test compilation with 55 errors. Tests
in `mcp.rs` still access former singleton fields such as `AppState.doc`,
`pages` and `to_pages`; several `state.rs` test helpers also pass mutex guards
without the required dereference. These were present before this note; no
application code was changed. Historical passing-test claims in
`protocol-results.md` do not establish the current workspace-refactor baseline.

## Defer

Binary formats, style interning and tombstone compaction remain separate
performance work. The open backlog (#33/#34) covers binary storage/wire work;
coordinate with it rather than prematurely freezing the storage format.
Embedding vectors, autonomous planning inside the app, agent chat UI and a
large collection of one-off tools are unnecessary for the first conversational
milestone. Test through the existing chat and live document before deciding
whether an embedded chat UI improves the experience. Prioritize a useful
engineering conversation backed by an interface the agent can learn and verify.
