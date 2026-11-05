# rex-relay

`rexctl` drives [Claude Code](https://claude.com/claude-code) panes in
[Rex](https://www.superlogical.com/rex) sessions from the command line. Another program, such as
a bot, a script, or a CI job, can hand Claude Code a message and wait for the outcome. `rexctl`
never answers a permission prompt on the user's behalf.

It talks to Rex's control API: a WebSocket at `GET /api/control` with subprotocol
`rex.control.v1`, API revision 8. Revision 8 exposes each program's OSC 7501 status (Working,
Needs approval, Question, Needs sign-in, Needs input, Error, Done), which `rexctl` turns into
exit codes.

```console
$ rexctl new fix-ci --cwd /srv/app --purpose "Fix the failing CI job"
session:…	fix-ci	block:…
$ rexctl send fix-ci "The CI job fails on main. Find out why and fix it."
rexctl: sent 49 bytes to fix-ci block:…
> The CI job fails on main. Find out why and fix it.
● I'll start by looking at the workflow…
…
rexctl: Needs approval: Allow Bash(cargo test)?
$ echo $?
10
```

## Status

`rexctl` is at 0.1.0. It needs a Rex server that speaks control API revision 8. CI runs on Linux,
and development happens on macOS. Windows is untested, and `unix://` endpoints need a Unix
platform.

`send` decides whether Claude Code is at its input prompt by reading the pane's screen (see
[How `send` delivers](#how-send-delivers)). If a Claude Code release changes how the prompt
looks, `send` refuses with exit 3 until `src/prompt.rs` learns the new layout.

## Install

You need Rust 1.89 or later.

```sh
cargo install --locked --git https://github.com/mbriggs/rex-relay   # installs rexctl into ~/.cargo/bin
```

To build from a checkout instead, run `cargo build --release --locked`. The binary is
`target/release/rexctl`.

`https://` endpoints, such as a Rex server published with Tailscale Serve, use TLS (rustls,
with the Mozilla root store from `webpki-roots`). `http://` and `unix://` endpoints are
supported for local servers.

## Quick start

1. Allow your Rex server in `~/.config/rex-relay/config.toml`:

   ```toml
   endpoint = "https://rex-host.example.ts.net"
   allowed_endpoints = ["https://rex-host.example.ts.net"]
   ```

2. Create a session that runs Claude Code, and keep its id:

   ```sh
   SID=$(rexctl --json new smoke --cwd ~/src/project --purpose "try rexctl" | jq -r .session_id)
   ```

3. In Rex, answer Claude Code's first-run questions, such as whether to trust the folder.
   `rexctl` never answers them for you.

4. Send a message and wait for the result:

   ```sh
   rexctl send "$SID" "Reply with the word pong."
   echo $?   # 0 when Claude Code is done; see Exit codes for the rest
   ```

5. Close the session:

   ```sh
   rexctl close "$SID"
   ```

## Configuration

`~/.config/rex-relay/config.toml` (or `$XDG_CONFIG_HOME/rex-relay/config.toml`, or the file named
by `REX_RELAY_CONFIG` or `--config`). See [`config.example.toml`](config.example.toml):

```toml
endpoint = "https://rex-host.example.ts.net"
allowed_endpoints = ["https://rex-host.example.ts.net"]
max_message_bytes = 32768
timeout = "30m"
call_timeout = "10s"
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
| `rexctl new [label] [--cwd DIR] [--cmd claude] [--purpose TEXT]` | Creates a session with the initial label `label` (or one Rex picks), whose one pane runs `--cmd` (default `claude`, with arguments quoted as in a shell, though no shell runs it) in `--cwd`, and records it in the ownership registry. |
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
      "label": "fix-ci",
      "created_at": "2026-11-04T12:00:00.000Z",
      "purpose": "Fix the failing CI job"
    }
  ]
}
```

A session is *owned* when its id is listed for the endpoint in use. `label` is only the label it
was created with, and is left out when `new` got none. Changes take an exclusive lock on `owned.json.lock` and replace the file
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

Before typing, `send` checks that the pane is at Claude Code's input prompt. It refuses if the
pane hasn't enabled bracketed paste (mode 2004). It then reads the bottom of the pane's screen
and looks for the prompt: a `❯` line directly under a `────` rule, or `│ >` under `╭───╮` in
older Claude Code versions. If the screen shows a numbered menu instead, such as the
"Do you trust the files in this folder?" dialog or a permission request, `send` refuses. In a
menu, Enter picks the highlighted option. A screen that matches neither layout is refused too.

The message is written as one bracketed paste (`ESC[200~ … ESC[201~`), so newlines stay inside
the message, followed after a short pause by a single carriage return (Enter).

After Enter, `send` follows the pane's `program_status_changed` events and ends at the first stop
after the message: Done, Needs approval, Question, Needs input, Needs sign-in, or Error. The
status the pane had before the message doesn't count. Each report carries the time it was made,
so a new Done counts even when the pane already showed Done before the message.

### Lost connections

`send` and `wait` re-read the pane's status every 3 seconds, whether or not events arrive. That
read catches a missed event. Its round trip also shows whether the connection still works.

If any call to the server fails or takes longer than `call_timeout` (default 10 seconds),
`rexctl` drops the connection and opens a new one. It continues the same client on the server
(the welcome's reconnect token), joins the session again, re-reads the pane's status, and keeps
waiting. A Done reported while the connection was down still ends the wait. Failed attempts
repeat after 1, 2, 4, and then every 8 seconds.

`rexctl` never types anything after a reconnect. The message went out once, before the wait
began.

If the connection is still down when `--timeout` runs out, `rexctl` exits 16. In that case
Claude Code's state is unknown. Check it with `rexctl status` once the server is reachable.

### Output

Text mode prints the pane's new lines on stdout as they settle, and status notes on stderr.
Claude Code redraws the bottom of the screen constantly, so the bottom 8 lines are held back
until they scroll up or the command ends.

`--json` prints one JSON object per line on stdout:

```json
{"type":"sent","session_id":"session:…","session_label":"fix-ci","block_id":"block:…","bytes":49}
{"type":"status","status":"working","label":"Working","msg":null}
{"type":"output","line":"● I'll start by looking at the workflow…"}
{"type":"result","status":"needs_approval","label":"Needs approval","msg":"Allow Bash(cargo test)?","exit_code":10}
```

A timeout ends with `{"type":"result","status":"timeout","last_status":"working","exit_code":15}`,
a connection that couldn't be restored with
`{"type":"result","status":"lost_connection","last_status":"working","exit_code":16}`, and
failures with `{"type":"error","message":"…","exit_code":1}`. While waiting, `connection` objects
report a lost connection, a failed first reconnect attempt, and a restored connection:
`{"type":"connection","state":"lost","message":"…"}`, with `state` set to `lost`, `retrying`, or
`restored`. `list`, `status`, `read`, `new`
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
| 16 | Lost connection: the connection to the server broke during `send` or `wait` and couldn't be restored before `--timeout`. Claude Code's state is unknown. |

## Guard rails

1. **Ownership by id.** `send` refuses any session whose id isn't in the ownership registry for
   the endpoint in use, unless `--allow-foreign`; the check happens before rexctl attaches to the
   session. Labels prove nothing: Claude Code renames sessions, and anyone can give a session
   any label.
2. **Closing owned sessions only.** `close` closes a session only if its id is in the registry;
   there is no flag to override that. A refused `close` sends no close request. Foreign sessions
   can never be closed or killed through rexctl.
3. **No auto-answers.** `rexctl` never answers an approval or question by itself. `send` refuses
   while the pane is in Needs approval or Question unless `--answer` says the message is the
   answer. It always refuses while the pane is in Needs sign-in, so it never types into a
   credential prompt.
4. **Input prompt only.** `send` types only at Claude Code's input prompt, which it finds on the
   pane's screen. It refuses menus, such as the folder-trust dialog and permission requests, even
   with `--answer`, and it refuses screens it doesn't recognize.
5. **Text only, bounded.** There are no raw-key or kill commands, and `close` is limited as
   above. Messages may contain
   printable text, newline and tab; any other control character (ESC, CR, Ctrl-C, DEL, C1
   controls, NUL) is refused. Messages larger than `max_message_bytes` (default 32 KiB) are
   refused; `--max-bytes` can lower that limit for one call but not raise it.
6. **Audit log.** Every mutating call (`new`, `send`, `close`), refused ones included, appends a
   JSON line
   to `~/.local/state/rex-relay/audit.jsonl` (or `$XDG_STATE_HOME/rex-relay/`, or the config's
   `audit_log`): timestamp, command, canonical endpoint, session, block, the message's SHA-256
   and length (never its text), and the result. A delivered `send` writes a `sent` entry, then
   one with the outcome (`done`, `needs_approval`, `timeout`, …). The file is created `0600` in a
   `0700` directory; if it can't be opened, nothing is sent. No credentials are logged.
7. **Endpoint allowlist.** Only endpoints in `allowed_endpoints` are contacted; there is no
   hardcoded endpoint.

These rails limit what `rexctl` does. They don't limit other clients of the Rex server. Read
[SECURITY.md](SECURITY.md) before you expose a Rex server on a network.

## Development

The minimum supported Rust is 1.89, set by `rust-version` in `Cargo.toml` and checked in CI.

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
      creates `smoke`, Claude Code starts in the pane, `owned.json` gains the id with
      its purpose, and the audit log gains a `created` entry.
- [ ] `rexctl list` marks the session `*` and shows the purpose.
- [ ] `rexctl status "$SID"` finds the Claude Code pane and shows its status.
- [ ] `rexctl send "$SID" "Reply with the word pong."` streams the reply and exits 0 (Done). The
      audit log gains `sent` and `done` entries with the hash, not the text.
- [ ] After Claude Code renames the session, `rexctl list` still marks it `*`, and `send` by its
      new label works.
- [ ] A multi-line message arrives as one prompt (bracketed paste) and is submitted once.
- [ ] `rexctl new` in a directory Claude Code hasn't trusted yet, then an immediate
      `rexctl send`, exits 3 with "never picks menu options". The trust dialog stays open.
- [ ] `rexctl send "$SID" "Create hello.txt containing hi."` exits 10 at the permission prompt
      (with default permissions); a second plain `send` then exits 3; nothing was approved.
- [ ] Answer the prompt in Rex by hand, then `rexctl wait "$SID"` exits 0 once done.
- [ ] A prompt that makes Claude Code ask a question exits 11.
- [ ] `--timeout 5s` on a long task exits 15, and `--json` ends with a `timeout` result.
- [ ] During a long `send`, drop the network between `rexctl` and Rex for 20 seconds (such as
      `tailscale down` on the relay machine, then `tailscale up`). `rexctl` reports `connection
      lost`, then `connection restored`, and still exits 0 when Claude Code finishes. Claude Code
      received the message once.
- [ ] With the network down for longer than `--timeout`, `send` exits 16.
- [ ] Create a session by hand in Rex and label it `fake`: `rexctl send fake "hi"`
      exits 3 (not in the registry); with `--allow-foreign` it sends; `rexctl close fake`
      exits 3 and the session stays open.
- [ ] A message with an escape character (`$'\e[A'`) or over 32 KiB exits 3.
- [ ] In a session with a shell pane and a Claude Code pane, `send` picks Claude Code;
      `--block` with the shell's id is refused (no bracketed paste).
- [ ] `rexctl read "$SID" --lines 20` prints the pane's last 20 lines.
- [ ] `rexctl close "$SID"` closes the session, removes it from `owned.json`, and logs `closed`.
      Close `fake` in Rex by hand.

## Security

[SECURITY.md](SECURITY.md) describes the threat model and how to report a vulnerability.

## License

rex-relay is licensed under the [MIT License](LICENSE).

The vendored copy of rexrs in `vendor/rexrs` keeps its own license, MIT OR Apache-2.0, at your
option. See [`vendor/rexrs/NOTICE`](vendor/rexrs/NOTICE).
