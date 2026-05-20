# Claude Code Instructions

## Always work in a separate worktree

**All code changes must be made in a dedicated git worktree, never directly
in the primary checkout (`/Users/ry/projects/terminal`).** Multiple Claude
sessions and the user share that one working directory and its HEAD, so
editing, branching, or committing there races against concurrent work — a
branch can be renamed or HEAD moved out from under an in-flight commit,
landing it on the wrong branch (this has happened).

Before touching any file, create an isolated worktree on a fresh branch:

```sh
git worktree add ../yutani-<branch> -b <branch>   # branches off the current HEAD
```

Do all edits, builds, commits, and pushes from that worktree. When invoked
via the Agent tool, prefer `isolation: "worktree"`. Clean up with
`git worktree remove` once the branch is pushed and the PR is open.

## Git remote / Forgejo PRs

This repo lives on a self-hosted Forgejo instance at `git.frawley.co`. The
`origin` remote is SSH on **port 2222** for fetch and HTTPS for push:

```
fetch: ssh://git@git.frawley.co:2222/ryan/yutani.git
push:  https://git.frawley.co/ryan/yutani.git
```

Two non-obvious things make CLI access tricky:

1. **The SSH port (2222) is not always reachable.** When `git fetch` or
   `git push` over SSH returns "Connection refused", fall back to HTTPS
   rather than waiting or asking. Both the push URL above and the API
   work over HTTPS.
2. **HTTPS requires a client TLS certificate** from the macOS Keychain.
   The cert is `*.frawley.co` (identity hash
   `CB9CBF31E01F807A4D57AD4166E05EFDE8E47038`). `~/.gitconfig` already
   wires git up to use it via SecureTransport, so plain `git push <https url>`
   just works. `curl` does **not** pick this up automatically.

### Pushing a branch

```sh
git push -u origin <branch>          # uses the HTTPS push URL + keychain cert
```

### Creating a PR via the Forgejo API

Two requirements `curl` doesn't handle by default:

- The token lives in git's credential helper, not an env var.
- The system `/usr/bin/curl` must be told to use SecureTransport (so it
  can read the client cert from the keychain by Common Name) — its
  default LibreSSL backend can't.

```sh
TOKEN=$(printf 'protocol=https\nhost=git.frawley.co\n' \
  | git credential fill | sed -n 's/^password=//p')

cat > /tmp/pr.json <<EOF
{
  "head": "<branch>",
  "base": "main",
  "title": "<title>",
  "body": "<markdown body>"
}
EOF

CURL_SSL_BACKEND=secure-transport /usr/bin/curl -sS \
  --cert "*.frawley.co" \
  -H "Authorization: token $TOKEN" \
  -H "Content-Type: application/json" \
  -X POST -d @/tmp/pr.json \
  https://git.frawley.co/api/v1/repos/ryan/yutani/pulls
```

The response includes `number` and `html_url` — surface those to the user.

The same `CURL_SSL_BACKEND=secure-transport /usr/bin/curl --cert "*.frawley.co" -H "Authorization: token $TOKEN"` recipe works for any other Forgejo API call (listing PRs, posting comments, merging, etc.). Endpoints are documented at https://docs.gitea.com/api (Forgejo is API-compatible).

Heads up: brewed curl (`/opt/homebrew/opt/curl/bin/curl`) is built against
OpenSSL and won't accept a keychain identity by name — stick with
`/usr/bin/curl` for these calls.

## Testing requirement

Per the global `~/.claude/CLAUDE.md`: always run the `unit-test-writer`
agent after implementing new features or significant code changes.
