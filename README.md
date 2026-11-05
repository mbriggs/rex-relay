# rex-relay

`rex-relay` drives [Claude Code](https://claude.com/claude-code) panes in
[Rex](https://www.superlogical.com/rex) sessions from the command line. Another program, such as
a bot, a script, or a CI job, can hand Claude Code a message and wait for the outcome. `rex-relay`
never answers a permission prompt on the user's behalf.

It talks to Rex's control API: a WebSocket at `GET /api/control` with subprotocol
`rex.control.v1`, API revision 8. Revision 8 exposes each program's OSC 7501 status (Working,
Needs approval, Question, Needs sign-in, Needs input, Error, Done), which `rex-relay` turns into
exit codes.

```console
$ rex-relay new fix-ci --cwd /srv/app --purpose "Fix the failing CI job"
session:…	fix-ci	block:…
$ rex-relay send fix-ci "The CI job fails on main. Find out why and fix it."
rex-relay: sent 49 bytes to fix-ci block:…
> The CI job fails on main. Find out why and fix it.
● I'll start by looking at the workflow…
…
rex-relay: Needs approval: Allow Bash(cargo test)?
$ echo $?
10
```

## Status

`rex-relay` is at 0.1.0. It needs a Rex server that speaks control API revision 8. CI runs on Linux,
and development happens on macOS. Windows is untested, and `unix://` endpoints need a Unix
platform.

`send` decides whether Claude Code is at its input prompt by reading the pane's screen (see
[How `send` delivers](#how-send-delivers)). If a Claude Code release changes how the prompt
looks, `send` refuses with exit 3 until `src/prompt.rs` learns the new layout.

## Install

Releases have prebuilt binaries for Linux (x86_64, statically linked with musl) and macOS (Apple
silicon and Intel). To install the latest release, or update to it, run:

```sh
curl -fsSL https://raw.githubusercontent.com/mbriggs/rex-relay/main/install.sh | sh
```

The script downloads the archive for your platform, checks it against its SHA-256 checksum,
and installs `rex-relay` into `~/.local/bin`. To pick a release, set `REX_RELAY_VERSION=v0.1.0`. To
pick a directory, set `REX_RELAY_INSTALL_DIR`. To install by hand, download
`rex-relay-<target>.tar.gz` and `SHA256SUMS` from the
[releases page](https://github.com/mbriggs/rex-relay/releases), then check the archive with
`shasum -a 256 -c SHA256SUMS --ignore-missing`.

To build from source, you need Rust 1.89 or later:

```sh
cargo install --locked --git https://github.com/mbriggs/rex-relay   # installs rex-relay into ~/.cargo/bin
```

To build from a checkout instead, run `cargo build --release --locked`. The binary is
`target/release/rex-relay`.

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
   SID=$(rex-relay --json new smoke --cwd ~/src/project --purpose "try rex-relay" | jq -r .session_id)
   ```

3. In Rex, answer Claude Code's first-run questions, such as whether to trust the folder.
   `rex-relay` never answers them for you.

4. Send a message and wait for the result:

   ```sh
   rex-relay send "$SID" "Reply with the word pong."
   echo $?   # 0 when Claude Code is done; see Exit codes for the rest
   ```

5. Close the session:

   ```sh
   rex-relay close "$SID"
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
connect_timeout = "60s"
# audit_log = "~/.local/state/rex-relay/audit.jsonl"
# registry = "~/.local/state/rex-relay/owned.json"
```

The endpoint comes from `--endpoint`, else `REX_SERVER`, else the config's `endpoint`. There is
no built-in default. Whichever it is, it must be listed in `allowed_endpoints`, or `rex-relay`
refuses before opening a connection. Entries are compared in canonical form (scheme, lowercase
host, port), so `wss://rex-host.example.ts.net:443/` matches `https://rex-host.example.ts.net`.
URLs with credentials, queries or paths other than `/api/control` are rejected.

## Commands

Global options: `--endpoint URL`, `--config PATH`, `--json`.

`<session>` is a session id (`session:…`) or a label, resolved when the command runs. A label
matches a session's current label or, for a session rex-relay created, the label `new` gave it.
Claude Code renames the sessions it runs in, so the label you created a session with keeps
working after the rename. The session id is the stable handle: `new` prints it first, and it
works for any session. A label that matches more than one session is a usage error (exit 2) that
lists the matching ids; use one of them.

`<target>` is a session, optionally followed by a tab and a pane: `session`, `session:tab`, or
`session:tab:pane`. For example, `solar-creek:shell` or `solar-creek:1:2`. See
[Which pane](#which-pane).

| Command | What it does |
| --- | --- |
| `rex-relay list` | Lists sessions. `*` (or `"owned": true`) marks sessions rex-relay created, followed by their purpose. A renamed session shows `(created as LABEL)` after its current label. |
| `rex-relay tree [session]` | Shows each session's tabs and terminal panes: numbers, labels, ids, the program in each pane, and its OSC 7501 status. |
| `rex-relay status <target> [--block ID]` | Shows the Claude Code pane's status and message. |
| `rex-relay read <target> [--lines N] [--block ID]` | Prints the pane's last N lines (default 100; 0 for all). |
| `rex-relay new [label] [--cwd DIR] [--cmd claude] [--purpose TEXT]` | Creates a session with the initial label `label` (or one Rex picks), whose one pane runs `--cmd` (default `claude`, with arguments quoted as in a shell, though no shell runs it) in `--cwd`, and records it in the ownership registry. `--cwd` is an absolute path, `~`, or a path under `~/`; the Rex host expands `~` to its own home directory. `new` fails if the directory doesn't exist on the Rex host. To check, it opens a session labelled `rex-relay cwd check` and closes it again. |
| `rex-relay send <target> "msg"` | Pastes the message into the Claude Code pane, presses Enter once, then streams the pane's new output until Claude Code stops. Owned sessions only, unless `--allow-foreign`. |
| `rex-relay run <target> -- <command>` | Types a command line into a shell pane that is idle at its prompt, presses Enter, and streams the new output until the prompt returns. Owned sessions only, unless `--allow-foreign`. See [Running shell commands](#running-shell-commands). |
| `rex-relay wait <target>` | Streams the pane's new output until Claude Code stops working. Returns at once if it isn't working. |
| `rex-relay close <session>` | Closes a session rex-relay created, and removes it from the registry. There is no override: other sessions are never closed. |

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
was created with, and is left out when `new` got none. Changes take an exclusive lock on
`owned.json.lock` and replace the file atomically (write to a temporary file, fsync, rename); the
file is `0600`. A registry that can't
be parsed is an error, never treated as empty. `close` on a registered id that the server no
longer has removes it from the registry without sending anything.

`send`, `wait`, and `run` options:

- `--timeout DURATION`: how long to wait. Defaults to `30m`, or the config's `timeout`.
- `--quiet`: don't stream the pane's output or status changes. At the end, print only the last
  lines of the pane on stdout and the final status on stderr. Exit codes don't change.
- `--lines N`: with `--quiet`, how many lines to print at the end. Defaults to 20; 0 prints all.
- `--poll DURATION`: how often to read the pane while streaming. Defaults to `1s`.

`send` also takes `--answer`, `--allow-foreign`, `--max-bytes N`, and `--block ID`.

### Which pane

A target can name a tab and a pane after its session. Rex calls tabs windows and panes blocks.
`rex-relay tree` shows the names and numbers.

- A tab matches by label, window id, or number, counting from 1.
- A pane matches by label, block id (with or without `block:`), or number within its tab.
- In `session:name`, `name` is a tab if one matches. Otherwise it names panes by label anywhere
  in the session, so `solar-creek:shell` finds the panes labeled `shell`.
- The whole target is tried as a session id or label first, because both can contain colons.

If the target names exactly one pane, `rex-relay` uses it, whatever runs in it. Ownership is still
checked for its session. If the target names a whole session, a tab, or several panes,
`rex-relay` picks among them as follows.

A session can have several terminal panes. `rex-relay` uses the one whose program is Claude Code,
recognised by its OSC 7501 status records (`app`/`effective_app` = `claude`), its foreground
process (`claude`, or the npm package's `claude-code/cli.js`), or a terminal title containing
"Claude Code". If several qualify, the focused pane of the active window wins; otherwise
`rex-relay` lists the candidates and asks for `--block`. `--block` takes a block id, the id without
`block:`, or a unique prefix of it.

### How `send` delivers

Before typing, `send` checks that the pane is at Claude Code's input prompt. It reads the bottom
of the pane's screen and looks for the prompt: a `❯` line directly under a `────` rule, or `│ >` under `╭───╮` in
older Claude Code versions. If the screen shows a numbered menu instead, such as the
"Do you trust the files in this folder?" dialog or a permission request, `send` refuses. In a
menu, Enter picks the highlighted option. A screen that matches neither layout, or a pane that
hasn't enabled bracketed paste (mode 2004), is refused too. The refusal says which it found: a
dialog, Claude Code still starting, or a pane that doesn't run Claude Code.

Claude Code reports no status for its folder-trust dialog, so `status` and `wait` read the screen
when the pane is Idle. A menu there is a Dialog: `status` shows the folder it asks about, and
`wait` exits 10 and prints the dialog's text.

The message is written as one bracketed paste (`ESC[200~ … ESC[201~`), so newlines stay inside
the message, followed after a short pause by a single carriage return (Enter).

After Enter, `send` follows the pane's `program_status_changed` events and ends at the first stop
after the message: Done, Needs approval, Question, Needs input, Needs sign-in, or Error. The
status the pane had before the message doesn't count. Each report carries the time it was made,
so a new Done counts even when the pane already showed Done before the message. A pane whose
records disappear after Working counts as Done only once it has stayed that way for 5 seconds,
because Claude Code clears its record for a moment during some turns.

### Lost connections

Every command retries its first connection if it can't be made, such as during a network blip.
It notes each failed attempt on stderr and tries again after 1, 2, 4, and then every 8 seconds.
After `connect_timeout` (default 60 seconds) it gives up with exit code 16. A server that answers
but refuses the client, such as for an unsupported API revision, isn't retried.

`send` and `wait` re-read the pane's status every 3 seconds, whether or not events arrive. That
read catches a missed event. Its round trip is also the heartbeat. A connection that goes quiet
without closing, as when a network path dies with no FIN or RST, fails it within about 13
seconds: the 3-second poll plus `call_timeout`. `run` checks the shell every quarter second the
same way. `rex-relay` doesn't send WebSocket pings, because the status read already tests the
whole path to the server, not only its WebSocket layer.

If any call to the server fails or takes longer than `call_timeout` (default 10 seconds),
`rex-relay` drops the connection and opens a new one. Looking up the server's name counts as a call
too, so an unreachable DNS server can't stall a reconnect. It continues the same client on the
server (the welcome's reconnect token), joins the session again, re-reads the pane's status, and
keeps waiting. A Done reported while the connection was down still ends the wait. Failed attempts
repeat after 1, 2, 4, and then every 8 seconds.


`rex-relay` never types anything after a reconnect. The message went out once, before the wait
began.

If the connection doesn't come back within `connect_timeout` (default 60 seconds), or before
`--timeout` runs out, `rex-relay` exits 16. In that case Claude Code's state is unknown. Check it
with `rex-relay status` once the server is reachable.

### Running shell commands

`run` is for shell panes, not Claude Code. It picks the pane in the target that is idle at a
shell prompt, which means the shell (`zsh`, `bash`, `fish`, and so on) is the pane's
foreground program. It refuses with exit 3 if the target pane, or every pane in the target,
is busy or runs something else.

The words after `--` are joined with spaces and typed as one line, as `ssh` does. Quote
anything the shell should see quoted:

```sh
rex-relay run solar-creek:shell -- 'cargo test 2>&1 | tail -20'
```

The command line follows the same rules as a `send` message: the same size limit, no control
characters, and the same audit log entries (as `run`, with its hash, never its text). It also
can't contain a newline or a tab. A newline would run the part before it, and a tab would ask
the shell to complete a word.

`run` counts the command as finished when the shell is the foreground program again and the
screen has changed since the command was typed, on two checks in a row. It then exits 0, or 15
if `--timeout` runs out first. Rex doesn't report a command's exit status, so `run` can't
either. To check whether a command succeeded, make it print something, such as
`-- 'make; echo "exit $?"'`. A command line with several commands (`a; b`) could look finished
in the instant between them. Prefer one command, or `&&`.

`run` can't see text someone typed at the shell prompt without pressing Enter, and its command
would be added to that text. It also takes `--timeout`, `--quiet`, `--lines`, `--poll`,
`--allow-foreign`, `--max-bytes`, and `--block`, which work as they do for `send`.

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
`restored`. With `--quiet`, a `{"type":"tail","lines":[…]}` object comes just before the result
instead of `output` objects. `list`, `status`, `read`, `new`
and `close` print `session`, `status`, `text`, `created` and `closed` objects.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Done (or the command succeeded). `wait` also returns 0 for an idle pane. |
| 1 | Error: connection, protocol, session or pane not found, pane closed. |
| 2 | Usage error, including a label that matches more than one session. |
| 3 | Refused by a guard rail. Nothing was typed. |
| 10 | Needs approval: Claude Code is waiting for a permission decision, or shows a dialog such as folder trust. |
| 11 | Question: Claude Code asked a question. |
| 12 | Needs input: blocked, without saying on what. |
| 13 | Needs sign-in: waiting for a login or credential. |
| 14 | Error: the program reported an error. |
| 15 | Timeout: still working when `--timeout` ran out. |
| 16 | Lost connection: the first connection failed for `connect_timeout`, or the connection broke during `send`, `wait`, or `run` and didn't come back within `connect_timeout`. In the second case, Claude Code's state is unknown. |

## Guard rails

1. **Ownership by id.** `send` refuses any session whose id isn't in the ownership registry for
   the endpoint in use, unless `--allow-foreign`; the check happens before rex-relay attaches to the
   session. Labels prove nothing: Claude Code renames sessions, and anyone can give a session
   any label.
2. **Closing owned sessions only.** `close` closes a session only if its id is in the registry;
   there is no flag to override that. A refused `close` sends no close request. Foreign sessions
   can never be closed or killed through rex-relay.
3. **No auto-answers.** `rex-relay` never answers an approval or question by itself. `send` refuses
   while the pane is in Needs approval or Question unless `--answer` says the message is the
   answer. It always refuses while the pane is in Needs sign-in, so it never types into a
   credential prompt.
4. **Idle shells only.** `run` types only into a shell that is idle at its prompt, never into
   Claude Code or a running command, and refuses multi-line commands.
5. **Input prompt only.** `send` types only at Claude Code's input prompt, which it finds on the
   pane's screen. It refuses menus, such as the folder-trust dialog and permission requests, even
   with `--answer`, and it refuses screens it doesn't recognize.
6. **Text only, bounded.** There are no raw-key or kill commands, and `close` is limited as
   above. Messages may contain
   printable text, newline and tab; any other control character (ESC, CR, Ctrl-C, DEL, C1
   controls, NUL) is refused. Messages larger than `max_message_bytes` (default 32 KiB) are
   refused; `--max-bytes` can lower that limit for one call but not raise it.
7. **Audit log.** Every mutating call (`new`, `send`, `run`, `close`), refused ones included,
   appends a JSON line to `~/.local/state/rex-relay/audit.jsonl` (or `$XDG_STATE_HOME/rex-relay/`,
   or the config's `audit_log`): timestamp, command, canonical endpoint, session, block, the
   message's SHA-256 and length (never its text), and the result. A delivered `send` writes a `sent`
   entry, then one with the outcome (`done`, `needs_approval`, `timeout`, …). The file is created
   `0600` in a `0700` directory; if it can't be opened, nothing is sent. No credentials are logged.
8. **Endpoint allowlist.** Only endpoints in `allowed_endpoints` are contacted; there is no
   hardcoded endpoint.

These rails limit what `rex-relay` does. They don't limit other clients of the Rex server. Read
[SECURITY.md](SECURITY.md) before you expose a Rex server on a network.

## Releasing

Pushing a tag that matches `Cargo.toml`'s version, such as `v0.1.0`, runs
`.github/workflows/release.yml`. It builds the binaries, then publishes a GitHub release with one
archive per target (the binary, the agent skill, and the docs), a `.sha256` file for each, a
combined `SHA256SUMS`, and `install.sh`. The release fails if the tag and the version differ.

## Development

The minimum supported Rust is 1.89, set by `rust-version` in `Cargo.toml` and checked in CI.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`cargo test` runs unit tests and end-to-end tests of the `rex-relay` binary against a scripted mock
of the control endpoint (`tests/mock_server.rs`). Tests never contact a real server.

`vendor/rexrs` is a vendored copy of the [rexrs](https://github.com/a10y/rexrs) client with API
revision 8 patches; see [`vendor/rexrs/NOTICE`](vendor/rexrs/NOTICE).

## Live-test checklist

Run these by hand against a real Rex server you control, in a throwaway directory. Use a
dedicated config whose `allowed_endpoints` holds only that server.

- [ ] `rex-relay --endpoint https://rex-host.example.ts.net list` without that endpoint in the
      allowlist exits 3 and opens no connection.
- [ ] With it allowed, `rex-relay list` shows the sessions; an `https://` endpoint connects over
      TLS (Tailscale Serve).
- [ ] `SID=$(rex-relay --json new smoke --cwd /tmp/rex-relay-smoke --purpose "live smoke test" | jq -r .session_id)`
      creates `smoke`, Claude Code starts in the pane, `owned.json` gains the id with
      its purpose, and the audit log gains a `created` entry.
- [ ] `rex-relay list` marks the session `*` and shows the purpose.
- [ ] `rex-relay status "$SID"` finds the Claude Code pane and shows its status.
- [ ] `rex-relay send "$SID" "Reply with the word pong."` streams the reply and exits 0 (Done). The
      audit log gains `sent` and `done` entries with the hash, not the text.
- [ ] After Claude Code renames the session, `rex-relay list` still marks it `*`, and `send` by its
      new label and by its original label both work.
- [ ] A multi-line message arrives as one prompt (bracketed paste) and is submitted once.
- [ ] `rex-relay new` in a directory Claude Code hasn't trusted yet, then an immediate
      `rex-relay send`, exits 3 with "never picks menu options". The trust dialog stays open, and
      `rex-relay wait` exits 10 and prints its text.
- [ ] `rex-relay send "$SID" "Create hello.txt containing hi."` exits 10 at the permission prompt
      (with default permissions); a second plain `send` then exits 3; nothing was approved.
- [ ] Answer the prompt in Rex by hand, then `rex-relay wait "$SID"` exits 0 once done.
- [ ] A prompt that makes Claude Code ask a question exits 11.
- [ ] `--timeout 5s` on a long task exits 15, and `--json` ends with a `timeout` result.
- [ ] During a long `send`, drop the network between `rex-relay` and Rex for 20 seconds (such as
      `tailscale down` on the relay machine, then `tailscale up`). `rex-relay` reports `connection
      lost`, then `connection restored`, and still exits 0 when Claude Code finishes. Claude Code
      received the message once.
- [ ] With the network down for longer than `--timeout`, `send` exits 16.
- [ ] Create a session by hand in Rex and label it `fake`: `rex-relay send fake "hi"`
      exits 3 (not in the registry); with `--allow-foreign` it sends; `rex-relay close fake`
      exits 3 and the session stays open.
- [ ] A message with an escape character (`$'\e[A'`) or over 32 KiB exits 3.
- [ ] In a session with a shell pane and a Claude Code pane, `send` picks Claude Code;
      `--block` with the shell's id is refused (no bracketed paste).
- [ ] `rex-relay read "$SID" --lines 20` prints the pane's last 20 lines.
- [ ] `rex-relay close "$SID"` closes the session, removes it from `owned.json`, and logs `closed`.
      Close `fake` in Rex by hand.

## Agent skill

[`skills/rex-relay/SKILL.md`](skills/rex-relay/SKILL.md) teaches an agent to use `rex-relay`: which
command fits which job, what each exit code means and what to do next, the `DONE:` and
`BLOCKED:` reply convention, and which flags only the user may approve. Release archives
include it under `skills/rex-relay`. To install it for Claude Code, copy the directory into
`~/.claude/skills/`.

## Security

[SECURITY.md](SECURITY.md) describes the threat model and how to report a vulnerability.

## License

rex-relay is licensed under the [MIT License](LICENSE).

The vendored copy of rexrs in `vendor/rexrs` keeps its own license, MIT OR Apache-2.0, at your
option. See [`vendor/rexrs/NOTICE`](vendor/rexrs/NOTICE).
