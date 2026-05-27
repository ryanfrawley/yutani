# Claude Code Instructions

## Always work in a separate worktree

**All code changes must be made in a dedicated git worktree, never directly
in the primary checkout.** Multiple Claude sessions and the user may share one
working directory and its HEAD, so editing, branching, or committing there
races against concurrent work — a branch can be renamed or HEAD moved out from
under an in-flight commit, landing it on the wrong branch (this has happened).

Before touching any file, create an isolated worktree on a fresh branch:

```sh
git worktree add ../yutani-<branch> -b <branch>   # branches off the current HEAD
```

Do all edits, builds, commits, and pushes from that worktree. When invoked
via the Agent tool, prefer `isolation: "worktree"`. Clean up with
`git worktree remove` once the branch is pushed and the PR is open.

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
