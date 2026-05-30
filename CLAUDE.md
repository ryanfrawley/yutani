# Claude Code Instructions

## Always work in a separate worktree

**All code changes must be made in a dedicated git worktree, never directly
in the primary checkout.** Multiple Claude sessions and the user may share one
working directory and its HEAD, so editing, branching, or committing there
races against concurrent work — a branch can be renamed or HEAD moved out from
under an in-flight commit, landing it on the wrong branch (this has happened).

Before touching any file, create an isolated worktree on a fresh branch
**inside the repo**, under `.claude/worktrees/`:

```sh
git worktree add .claude/worktrees/<branch> -b <branch>   # branches off the current HEAD
```

The path MUST stay inside this repository. The tool sandbox is rooted at the
project directory, so a worktree created one level up (e.g. `../yutani-<branch>`)
falls outside the sandbox: `cd` into it gets reset after every command, and
`Read`/`Edit`/`Agent` calls against its paths are denied as out-of-root — which
silently breaks the whole session. Keeping it under `.claude/worktrees/` avoids
this. (`.claude/worktrees/` is already git-ignored.)

Do all edits, builds, commits, and pushes from that worktree. When invoked
via the Agent tool, prefer `isolation: "worktree"`. Clean up with
`git worktree remove .claude/worktrees/<branch>` once the branch is pushed and
the PR is open.

## Pushing branches and opening PRs

Push your worktree branch and open a pull request with the `gh` CLI:

```sh
git push -u origin <branch>
gh pr create --base main --title "<title>" --body "<markdown body>"
```

Surface the resulting PR URL to the user.

## Testing requirement

Always run the `unit-test-writer` agent after implementing new features or
significant code changes.
