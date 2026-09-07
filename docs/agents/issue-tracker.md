# Issue tracker: GitHub

Issues and specs live in GitHub Issues for `SoftARV/Dockyard`.
Use `gh` with `--repo SoftARV/Dockyard`; the Git remote uses an SSH alias.

## Operations

- Create: `gh issue create --repo SoftARV/Dockyard --title "..." --body-file <path>`
- Read: `gh issue view <number> --repo SoftARV/Dockyard --comments`
- Read structured data: `gh issue view <number> --repo SoftARV/Dockyard --json number,title,body,labels,comments`
- List: `gh issue list --repo SoftARV/Dockyard --state open --json number,title,body,labels`
- Comment: `gh issue comment <number> --repo SoftARV/Dockyard --body-file <path>`
- Label: `gh issue edit <number> --repo SoftARV/Dockyard --add-label "..."` or `--remove-label "..."`
- Close: `gh issue close <number> --repo SoftARV/Dockyard`

Write multiline bodies to a file and pass `--body-file`.

“Publish to the issue tracker” means create a GitHub issue.
“Fetch the relevant ticket” means read the issue, its labels, and its comments.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Wayfinding operations

- Map: one issue labelled `wayfinder:map`, containing Notes,
  Decisions-so-far, and Fog.
- Children: link tickets as GitHub sub-issues. If unavailable, use a task
  list in the map and `Part of #<map>` in each child.
- Types: `wayfinder:research`, `wayfinder:prototype`,
  `wayfinder:grilling`, or `wayfinder:task`.
- Blocking: use GitHub's native issue dependencies. If unavailable,
  record `Blocked by: #<number>` in the child.
- Frontier: choose the first open, unassigned child in map order whose
  blockers are all closed.
- Claim: assign the ticket to the driving developer.
- Resolve: comment with the result, close the child, and add a short
  finding with a link to the map's Decisions-so-far.
