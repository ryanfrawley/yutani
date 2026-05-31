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
GitHub — so `gh` does not apply (and isn't installed). The `origin` remote
**fetches over SSH (port 2222)** and **pushes over HTTPS**; `~/.gitconfig`
already wires the HTTPS side to present the `*.frawley.co` client cert from the
macOS keychain (via SecureTransport), so a plain push just works:

```sh
git push -u origin <branch>          # HTTPS push URL + keychain client cert
```

If the SSH port (2222) ever returns "Connection refused" on a fetch/push, fall
back to the HTTPS URL rather than waiting — both the push URL and the API work
over HTTPS.

### Opening the PR via the Gitea API

This **is** scriptable from here. Two non-obvious requirements that plain
`curl` doesn't handle by default:

- The API token lives in **git's credential helper**, not an env var — pull it
  with `git credential fill`.
- The system `/usr/bin/curl` must be told to use **SecureTransport** so it can
  read the client cert from the keychain by Common Name. Its default LibreSSL
  backend can't, and brewed curl (`/opt/homebrew/opt/curl/bin/curl`, OpenSSL)
  won't accept a keychain identity by name — so stick with `/usr/bin/curl` +
  `CURL_SSL_BACKEND=secure-transport` and `--cert "*.frawley.co"`.

```sh
TOKEN=$(printf 'protocol=https\nhost=git.frawley.co\n' \
  | git credential fill | sed -n 's/^password=//p')

cat > /tmp/pr.json <<EOF
{ "head": "<branch>", "base": "main", "title": "<title>", "body": "<markdown body>" }
EOF

CURL_SSL_BACKEND=secure-transport /usr/bin/curl -sS \
  --cert "*.frawley.co" \
  -H "Authorization: token $TOKEN" \
  -H "Content-Type: application/json" \
  -X POST -d @/tmp/pr.json \
  https://git.frawley.co/api/v1/repos/ryan/yutani/pulls
```

The response includes `number` and `html_url` — surface those to the user. The
same `CURL_SSL_BACKEND=secure-transport /usr/bin/curl --cert "*.frawley.co" -H
"Authorization: token $TOKEN"` recipe works for any other Gitea API call
(listing PRs, posting comments, merging, etc.). Endpoints are documented at
https://docs.gitea.com/api (Forgejo is API-compatible).

To **update an existing PR's branch**, just force-push the rebased branch
(`git push --force-with-lease origin <branch>`) — no API call needed.

## Testing requirement

Always run the `unit-test-writer` agent after implementing new features or
significant code changes.
