# Issue tracker

Issues and specs for this repository live as GitHub issues. Use the `gh` CLI for all operations; inside a clone it infers the repository from `git remote -v`.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, adding `--json` and `--jq` when the output is consumed by an agent.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments`.
- **Comment on an issue**: `gh issue comment <number> --body "..."`.
- **Apply or remove labels**: `gh issue edit <number> --add-label "..."` or `--remove-label "..."`.
- **Close an issue**: `gh issue close <number> --comment "..."`.

Pull requests share the issue number space. Resolve a bare `#42` with `gh pr view 42` before falling back to `gh issue view 42`.
