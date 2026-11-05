# Security

## Threat model

`rexctl` is a client of a Rex server's control API. Anyone who can reach that API can drive the
server's terminals: type into any pane, read any screen, and create or close sessions. `rexctl`
adds nothing to that access and can't take any of it away. Its guard rails limit what a caller
of `rexctl` can do, such as a bot that relays requests from a chat. They don't protect a Rex
server from any other client.

Treat network access to the Rex port as shell access to the Rex host, for the user Rex runs as.

### What the guard rails cover

- `send` types only plain text, only into sessions `rexctl` created (unless `--allow-foreign`),
  and only at Claude Code's input prompt. It never answers a permission prompt or a menu, and it
  never types into a sign-in prompt.
- `close` closes only sessions `rexctl` created. No flag overrides that.
- `rexctl` connects only to endpoints listed in `allowed_endpoints`.
- Every `new`, `send`, and `close` is logged to the audit log, including refused ones. The log
  records a message's SHA-256 and length, never its text.

The README's [guard rails](README.md#guard-rails) section has the details.

### What they don't cover

- Other clients of the Rex server, including the Rex app and `rex` CLI, and any program that
  speaks the control API directly.
- What Claude Code does with a message. A message that `send` delivers is a prompt with the
  full power of that Claude Code session, and Claude Code's own permission settings apply.
- A caller who can edit `rexctl`'s config, registry, or audit log. Those files are created `0600`
  in `0700` directories, so this means the same user or root.
- `--allow-foreign`, which lets a caller send to any session on the server. Don't give a caller
  you don't fully trust a way to pass it.

## Deploying safely

- Never expose the Rex port to the public internet.
- If `rexctl` runs on the same machine as Rex, use the Unix socket (`unix://…`) and no TCP
  listener at all.
- If `rexctl` runs on another machine, put both on a tailnet and publish Rex with Tailscale
  Serve, so `rexctl` connects with `https://`. Then write tailnet ACLs that let only the relay's
  machine reach the Rex machine's port. Tag the relay machine, such as `tag:rex-relay`, and grant
  that tag access to the Rex machine. Grant nothing broader.
- If you use Rex's built-in Tailscale, check its `allow_tailnet_members` setting. When it's off,
  Rex lets in only the Tailscale user who signed the device in. When it's on, Rex lets in anyone
  your tailnet's ACLs allow. Turn it on only if the relay connects as a different identity, such
  as a tagged machine, and only after the ACLs above are in place.
- List exactly one endpoint in `allowed_endpoints`, the one the relay is meant to use.
- Run the relay as a dedicated user, so a compromised caller can't read your own files through
  the relay's account.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/mbriggs/rex-relay/security/advisories/new)
for this repository. Don't open a public issue. Include the `rexctl` version, the command you ran,
and what happened.
