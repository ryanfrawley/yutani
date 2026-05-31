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

This repo lives on a self-hosted **Gitea** instance (`git.frawley.co`), not
GitHub — so `gh` does not apply (and isn't installed). Push your worktree
branch over SSH, which is the only transport that works unattended here:

```sh
git push -u origin <branch>
```

**Opening the PR is not scriptable from this environment.** The Gitea server
requires a **mutual-TLS client certificate** that lives in the macOS keychain,
and only `git`'s Secure-Transport libcurl can present it — standalone `curl`
(LibreSSL) and `tea` cannot complete the API handshake, and there is no API
token on disk. So the Gitea REST API / `tea pr create` are unavailable.

To open the PR, use one of:

- **Browser automation** (the browser already trusts the client cert): drive
  Chrome via the `claude-in-chrome` tools to the compare page
  `https://git.frawley.co/ryan/yutani/compare/main...<branch>`, fill in the
  title/body, and submit.
- **Hand off to the user**: surface that compare URL and ask them to click
  *Create Pull Request*.

Either way, surface the resulting PR URL to the user. If a `tea` login is ever
configured (`tea login list` non-empty) and the mTLS issue is resolved,
`tea pr create --repo ryan/yutani --base main --head <branch> ...` becomes an
option too.

## Testing requirement

Always run the `unit-test-writer` agent after implementing new features or
significant code changes.
