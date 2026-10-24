# rs-canvas

## Canvas drawing

Every canvas change — drawing, editing, arranging — goes through the `canvas`
subagent by default. The primary agent's mutating canvas tools
(`canvas_draw`, `canvas_pen`, `canvas_script`, `canvas_bulk`,
`canvas_clear`, `canvas_annotate`, `canvas_emphasis`, `canvas_tidy`,
`canvas_revert`, `canvas_remember`, `canvas_workspace_create`) resolve to `ask`, so the primary may call them directly
only when the user explicitly asks for on-demand drawing, after approval.
Read and presentation tools (`canvas_status`, `canvas_read`,
`canvas_screenshot`, `canvas_focus`, `canvas_point`, `canvas_zoom`) stay
available for discussion, so brief the subagent fully when it is time to draw.

## Thinking together

Treat the document as shared working material for engineering ideas. A session
that starts a new discussion gets its own workspace: delegate
`canvas_workspace_create` with a short topic name before the first sketch, then
read `canvas_status` there and stay there for the session. If the user is
pointing at an existing drawing, selection or named workspace, work on that one
instead. Start with the user's selection and the workspace discussion in
`canvas_status`; use scoped reads for the part under discussion. Make one useful
observation or question at a time, grounded in a concrete flow, tradeoff or
failure case. Delegate a small sketch or revision when it helps the conversation,
with the read context, relevant handles, constraints and intended scope. Keep
agreed decisions separate from hypotheses; delegate saving their reasons and
open questions when they become useful context to return to.

## Agent skills

### Issue tracker

Issues live as GitHub issues, managed via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical roles: needs-triage, needs-info, ready-for-agent, ready-for-human, wontfix. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
