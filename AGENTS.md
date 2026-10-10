# rs-canvas

## Canvas drawing

Every canvas change — drawing, editing, arranging — goes through the `canvas`
subagent. Never call the mutating canvas tools (`canvas_draw`, `canvas_pen`,
`canvas_script`, `canvas_clear`, `canvas_annotate`, `canvas_emphasis`,
`canvas_tidy`) directly; they are denied on the primary agent. Read and
presentation tools (`canvas_status`, `canvas_read`, `canvas_screenshot`,
`canvas_focus`, `canvas_point`, `canvas_zoom`) stay available for discussion, so
brief the subagent fully when it is time to draw.

## Agent skills

### Issue tracker

Issues live as GitHub issues, managed via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical roles: needs-triage, needs-info, ready-for-agent, ready-for-human, wontfix. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
