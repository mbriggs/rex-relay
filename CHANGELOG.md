# Changelog

This project follows [Semantic Versioning](https://semver.org/). Before 1.0, minor releases may
change the command-line interface.

## Unreleased

## [0.1.1] - 2026-11-05

### Added

- A Dialog status for menus Claude Code shows without reporting a status, such as the
  folder-trust dialog. `status` shows the folder it names, and `wait` exits 10 and prints the
  dialog's text instead of reporting Idle.
- `new --auto` starts Claude Code in auto permission mode, the same as
  `--cmd "claude --permission-mode auto"`. It can't be combined with `--cmd`.
- `status` and `tree` show Claude Code's permission mode, read from the footer under its
  prompt: `default`, `acceptEdits`, `plan`, `auto`, `dontAsk`, `bypassPermissions`, or
  `unknown` when the prompt isn't on screen. JSON output has it as `mode`.
- `new --wait-ready [--timeout D]` waits until Claude Code is at its input prompt, so a `send`
  right after it is accepted. A startup dialog exits 10 with its text; a timeout exits 15 with
  the pane's last lines (#14).

### Changed

- `status` and `tree` text output puts the permission mode in brackets after the status, such
  as `Working [auto] — Editing files`.

- `send`'s refusals at the screen check name what the pane shows: a dialog, Claude Code still
  starting, or a pane that doesn't run Claude Code. Only the last one suggests `--block`.

### Fixed

- The label given to `new` keeps resolving for `status`, `read`, `wait`, `send`, `run`, `tree`,
  and `close` after Claude Code renames the session. `list` shows the original label next to the
  current one. A label that matches more than one session now exits 2 (was 1) and lists the
  matching ids (#19).

- `new --cwd` expands a leading `~` to the Rex host's home directory, and fails if the
  directory doesn't exist there. Before, Rex started the pane in the home directory without an
  error. `--cwd` must be an absolute path, `~`, or a path under `~/` (#13).
- `status`, `tree`, `send` and `wait` no longer report Done while Claude Code's footer shows
  background shells still running. The pane is Background (`"shells": N` in JSON), and `send`
  and `wait` keep waiting until the shells finish (#34).

## [0.1.0] - 2026-11-04

The first release, under the MIT License.

### Changed

- The command is named `rex-relay`, matching the crate and its config, state, and log paths.
  During development it was called `rexctl`. Release archives, `install.sh`, and the agent skill
  (`skills/rex-relay`) use the new name, and `install.sh` reads `REX_RELAY_VERSION`,
  `REX_RELAY_INSTALL_DIR`, and `REX_RELAY_BASE_URL`. A `rexctl` binary installed earlier isn't
  removed or updated; delete it by hand.

### Added

- The `rex-relay` commands `list`, `tree`, `status`, `read`, `new`, `send`, `wait`, `run`, and
  `close`, which drive Claude Code and shell panes in Rex sessions over control API revision 8.
- Exit codes for each way Claude Code can stop: done, needs approval, question, needs input,
  needs sign-in, error, and timeout. Exit code 16 means the server couldn't be reached: the first
  connection failed for `connect_timeout` (default 60 seconds), or the connection broke during
  `send`, `wait`, or `run` and didn't come back within `connect_timeout`.
- Retries for every command's first connection, with the same backoff as reconnects. Each failed
  attempt is noted on stderr.
- Reconnects during `send` and `wait`. A call that fails or takes longer than `call_timeout`
  (default 10 seconds) starts a reconnect. `rex-relay` then joins the session again, re-reads the
  pane's status, and keeps waiting, and it never types the message again. The status is also
  re-read every 3 seconds, so a missed Done still ends the wait.
- `--json`, which prints one JSON object per line.
- `rex-relay run <target> -- <command>`, which types one command line into a shell pane idle at its
  prompt and waits for the prompt to return.
- An ownership registry that tracks the sessions `new` creates by session id. `send` targets
  only owned sessions unless `--allow-foreign`, and `close` closes only owned sessions.
- Guard rails. `send` types only plain text, only at Claude Code's input prompt, and never into
  a menu, a pending approval (without `--answer`), or a sign-in prompt. Messages have a size
  limit.
- An endpoint allowlist, and `https://`, `http://`, and `unix://` endpoints. `https://` uses
  rustls.
- An audit log of every `new`, `send`, and `close`, which records message hashes, never message
  text.
- An agent skill, `skills/rex-relay/SKILL.md`, shipped in release archives.
- A vendored copy of the rexrs client with API revision 8 patches. See `vendor/rexrs/NOTICE`.
