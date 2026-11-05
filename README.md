# rex-relay

`rexctl` drives [Claude Code](https://claude.com/claude-code) panes in
[Rex](https://www.superlogical.com/rex) sessions from the command line, so that another program
(a bot, a script, a CI job) can hand Claude Code a message and wait for the outcome — without
ever answering a permission prompt on the user's behalf.

It talks to Rex's control API: a WebSocket at `GET /api/control` with subprotocol
`rex.control.v1`, API revision 8. Revision 8 exposes each program's OSC 7501 status (Working,
Needs approval, Question, Needs sign-in, Needs input, Error, Done), which `rexctl` turns into
exit codes.

```console
$ rexctl new fix-ci --cwd /srv/app --purpose "Fix the failing CI job"
session:…	grokbot-fix-ci	block:…
$ rexctl send grokbot-fix-ci "The CI job fails on main. Find out why and fix it."
rexctl: sent 49 bytes to grokbot-fix-ci block:…
> The CI job fails on main. Find out why and fix it.
● I'll start by looking at the workflow…
…
rexctl: Needs approval: Allow Bash(cargo test)?
$ echo $?
10
```

## Install

Requires a Rust toolchain (stable).

```sh
cargo install --path .        # installs rexctl into ~/.cargo/bin
# or
cargo build --release         # target/release/rexctl
```

`https://` endpoints, such as a Rex server published with Tailscale Serve, use TLS (rustls,
with the Mozilla root store from `webpki-roots`). `http://` and `unix://` endpoints are
supported for local servers.

## Configuration

`~/.config/rex-relay/config.toml` (or `$XDG_CONFIG_HOME/rex-relay/config.toml`, or the file named
by `REX_RELAY_CONFIG` or `--config`). See [`config.example.toml`](config.example.toml):

```toml
endpoint = "https://rex-host.example.ts.net"
allowed_endpoints = ["https://rex-host.example.ts.net"]
label_prefix = "grokbot-"
max_message_bytes = 32768
timeout = "30m"
# audit_log = "~/.local/state/rex-relay/audit.jsonl"
# registry = "~/.local/state/rex-relay/owned.json"
```

The endpoint comes from `--endpoint`, else `REX_SERVER`, else the config's `endpoint`. There is
no built-in default. Whichever it is, it must be listed in `allowed_endpoints`, or `rexctl`
refuses before opening a connection. Entries are compared in canonical form (scheme, lowercase
host, port), so `wss://rex-host.example.ts.net:443/` matches `https://rex-host.example.ts.net`.
URLs with credentials, queries or paths other than `/api/control` are rejected.

## Commands

Global options: `--endpoint URL`, `--config PATH`, `--json`.

`<session>` is a session id (`session:…`) or a session's *current* label, resolved when the
command runs. Claude Code renames the sessions it runs in, so the label you created a session with
may no longer work; the id always does. A label that matches more than one session is an error
(exit 1); use the id.

| Command | What it does |
| --- | --- |
| `rexctl list` | Lists sessions. `*` (or `"owned": true`) marks sessions rexctl created, followed by their purpose. |
| `rexctl status <session> [--block ID]` | Shows the Claude Code pane's status and message. |
| `rexctl read <session> [--lines N] [--block ID]` | Prints the pane's last N lines (default 100; 0 for all). |
| `rexctl new <label> [--cwd DIR] [--cmd claude] [--purpose TEXT]` | Creates a session with initial label `<prefix><label>`, whose one pane runs `--cmd` (default `claude`, split on whitespace) in `--cwd`, and records it in the ownership registry. |
| `rexctl send <session> "msg"` | Pastes the message into the Claude Code pane, presses Enter once, then streams the pane's new output until Claude Code stops. Owned sessions only, unless `--allow-foreign`. |
| `rexctl wait <session>` | Streams the pane's new output until Claude Code stops working. Returns at once if it isn't working. |
| `rexctl close <session>` | Closes a session rexctl created, and removes it from the registry. There is no override: other sessions are never closed. |

### Ownership registry

`new` records every session it creates in `~/.local/state/rex-relay/owned.json` (or
`$XDG_STATE_HOME/rex-relay/owned.json`, or the config's `registry`):

```json
{
  "version": 1,
  "sessions": [
    {
      "endpoint": "https://rex-host.example.ts.net",
      "session_id": "session:…",
      "label": "grokbot-fix-ci",
      "created_at": "2026-11-04T12:00:00.000Z",
      "purpose": "Fix the failing CI job"
    }
  ]
}
```

A session is *owned* when its id is listed for the endpoint in use; `label` is only the label it
was created with. Changes take an exclusive lock on `owned.json.lock` and replace the file
atomically (write to a temporary file, fsync, rename); the file is `0600`. A registry that can't
be parsed is an error, never treated as empty. `close` on a registered id that the server no
longer has removes it from the registry without sending anything.

`send` and `wait` options: `--timeout DURATION` (default `30m`, or the config's), `--quiet`
(don't stream output), `--poll DURATION` (how often to read the pane, default `1s`).
`send` also takes `--answer`, `--allow-foreign`, `--max-bytes N`, and `--block ID`.

### Which pane

A session can have several terminal panes. `rexctl` uses the one whose program is Claude Code,
recognised by its OSC 7501 status records (`app`/`effective_app` = `claude`), its foreground
process (`claude`, or the npm package's `claude-code/cli.js`), or a terminal title containing
"Claude Code". If several qualify, the focused pane of the active window wins; otherwise
`rexctl` lists the candidates and asks for `--block`. `--block` takes a block id, the id without
`block:`, or a unique prefix of it.

### How `send` delivers

The message is written as one bracketed paste (`ESC[200~ … ESC[201~`), so newlines stay inside
the message, followed after a short pause by a single carriage return (Enter). `send` refuses if
the pane hasn't enabled bracketed paste (mode 2004), which means it isn't sitting at Claude
Code's prompt.

After Enter, `send` follows the pane's `program_status_changed` events (re-reading the status
every few seconds in case one is missed) and ends at the first stop after the message: Done,
Needs approval, Question, Needs input, Needs sign-in, or Error. The status the pane had before the
message doesn't count.

### Output

Text mode prints the pane's new lines on stdout as they settle, and status notes on stderr.
Claude Code redraws the bottom of the screen constantly, so the bottom 8 lines are held back
until they scroll up or the command ends.

`--json` prints one JSON object per line on stdout:

```json
{"type":"sent","session_id":"session:…","session_label":"grokbot-fix-ci","block_id":"block:…","bytes":49}
{"type":"status","status":"working","label":"Working","msg":null}
{"type":"output","line":"● I'll start by looking at the workflow…"}
{"type":"result","status":"needs_approval","label":"Needs approval","msg":"Allow Bash(cargo test)?","exit_code":10}
```

A timeout ends with `{"type":"result","status":"timeout","last_status":"working","exit_code":15}`,
and failures with `{"type":"error","message":"…","exit_code":1}`. `list`, `status`, `read`, `new`
and `close` print `session`, `status`, `text`, `created` and `closed` objects.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Done (or the command succeeded). `wait` also returns 0 for an idle pane. |
| 1 | Error: connection, protocol, session or pane not found, pane closed. |
| 2 | Usage error. |
| 3 | Refused by a guard rail. Nothing was typed. |
| 10 | Needs approval: Claude Code is waiting for a permission decision. |
| 11 | Question: Claude Code asked a question. |
| 12 | Needs input: blocked, without saying on what. |
| 13 | Needs sign-in: waiting for a login or credential. |
| 14 | Error: the program reported an error. |
| 15 | Timeout: still working when `--timeout` ran out. |

## Guard rails

1. **Ownership by id.** `send` refuses any session whose id isn't in the ownership registry for
   the endpoint in use, unless `--allow-foreign`; the check happens before rexctl attaches to the
   session. Labels prove nothing: Claude Code renames sessions, and anyone can name a session
   `grokbot-…`. The prefix (default `grokbot-`) is only the initial label `new` gives.
2. **Closing owned sessions only.** `close` closes a session only if its id is in the registry;
   there is no flag to override that. A refused `close` sends no close request. Foreign sessions
   can never be closed or killed through rexctl.
3. **No auto-answers.** `rexctl` never answers an approval or question by itself. `send` refuses
   while the pane is in Needs approval or Question unless `--answer` says the message is the
   answer, and always refuses while it is in Needs sign-in: it never types into a credential
   prompt.
4. **Text only, bounded.** There are no raw-key or kill commands, and `close` is limited as
   above. Messages may contain
   printable text, newline and tab; any other control character (ESC, CR, Ctrl-C, DEL, C1
   controls, NUL) is refused. Messages larger than `max_message_bytes` (default 32 KiB) are
   refused; `--max-bytes` can lower that limit for one call but not raise it.
5. **Audit log.** Every mutating call (`new`, `send`, `close`), refused ones included, appends a
   JSON line
   to `~/.local/state/rex-relay/audit.jsonl` (or `$XDG_STATE_HOME/rex-relay/`, or the config's
   `audit_log`): timestamp, command, canonical endpoint, session, block, the message's SHA-256
   and length (never its text), and the result. A delivered `send` writes a `sent` entry, then
   one with the outcome (`done`, `needs_approval`, `timeout`, …). The file is created `0600` in a
   `0700` directory; if it can't be opened, nothing is sent. No credentials are logged.
6. **Endpoint allowlist.** Only endpoints in `allowed_endpoints` are contacted; there is no
   hardcoded endpoint.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`cargo test` runs unit tests and end-to-end tests of the `rexctl` binary against a scripted mock
of the control endpoint (`tests/mock_server.rs`). Tests never contact a real server.

`vendor/rexrs` is a vendored copy of the [rexrs](https://github.com/a10y/rexrs) client with API
revision 8 patches; see [`vendor/rexrs/NOTICE`](vendor/rexrs/NOTICE).

## Live-test checklist

Run these by hand against a real Rex server you control, in a throwaway directory. Use a
dedicated config whose `allowed_endpoints` holds only that server.

- [ ] `rexctl --endpoint https://rex-host.example.ts.net list` without that endpoint in the
      allowlist exits 3 and opens no connection.
- [ ] With it allowed, `rexctl list` shows the sessions; an `https://` endpoint connects over
      TLS (Tailscale Serve).
- [ ] `SID=$(rexctl --json new smoke --cwd /tmp/rex-relay-smoke --purpose "live smoke test" | jq -r .session_id)`
      creates `grokbot-smoke`, Claude Code starts in the pane, `owned.json` gains the id with
      its purpose, and the audit log gains a `created` entry.
- [ ] `rexctl list` marks the session `*` and shows the purpose.
- [ ] `rexctl status "$SID"` finds the Claude Code pane and shows its status.
- [ ] `rexctl send "$SID" "Reply with the word pong."` streams the reply and exits 0 (Done). The
      audit log gains `sent` and `done` entries with the hash, not the text.
- [ ] After Claude Code renames the session, `rexctl list` still marks it `*`, and `send` by its
      new label works.
- [ ] A multi-line message arrives as one prompt (bracketed paste) and is submitted once.
- [ ] `rexctl send "$SID" "Create hello.txt containing hi."` exits 10 at the permission prompt
      (with default permissions); a second plain `send` then exits 3; nothing was approved.
- [ ] Answer the prompt in Rex by hand, then `rexctl wait "$SID"` exits 0 once done.
- [ ] A prompt that makes Claude Code ask a question exits 11.
- [ ] `--timeout 5s` on a long task exits 15, and `--json` ends with a `timeout` result.
- [ ] Create a session by hand in Rex and label it `grokbot-fake`: `rexctl send grokbot-fake "hi"`
      exits 3 (not in the registry); with `--allow-foreign` it sends; `rexctl close grokbot-fake`
      exits 3 and the session stays open.
- [ ] A message with an escape character (`$'\e[A'`) or over 32 KiB exits 3.
- [ ] In a session with a shell pane and a Claude Code pane, `send` picks Claude Code;
      `--block` with the shell's id is refused (no bracketed paste).
- [ ] `rexctl read "$SID" --lines 20` prints the pane's last 20 lines.
- [ ] `rexctl close "$SID"` closes the session, removes it from `owned.json`, and logs `closed`.
      Close `grokbot-fake` in Rex by hand.
