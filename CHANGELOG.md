# Changelog

This project follows [Semantic Versioning](https://semver.org/). Before 1.0, minor releases may
change the command-line interface.

## [0.1.0] - Unreleased

The first release, under the MIT License.

### Added

- The `rexctl` commands `list`, `status`, `read`, `new`, `send`, `wait`, and `close`, which drive
  Claude Code panes in Rex sessions over control API revision 8.
- Exit codes for each way Claude Code can stop: done, needs approval, question, needs input,
  needs sign-in, error, and timeout. Exit code 16 means the server couldn't be reached: the first
  connection failed for `connect_timeout` (default 60 seconds), or the connection broke during
  `send` or `wait` and couldn't be restored before `--timeout`.
- Retries for every command's first connection, with the same backoff as reconnects. Each failed
  attempt is noted on stderr.
- Reconnects during `send` and `wait`. A call that fails or takes longer than `call_timeout`
  (default 10 seconds) starts a reconnect. `rexctl` then joins the session again, re-reads the
  pane's status, and keeps waiting, and it never types the message again. The status is also
  re-read every 3 seconds, so a missed Done still ends the wait.
- `--json`, which prints one JSON object per line.
- An ownership registry that tracks the sessions `new` creates by session id. `send` targets
  only owned sessions unless `--allow-foreign`, and `close` closes only owned sessions.
- Guard rails. `send` types only plain text, only at Claude Code's input prompt, and never into
  a menu, a pending approval (without `--answer`), or a sign-in prompt. Messages have a size
  limit.
- An endpoint allowlist, and `https://`, `http://`, and `unix://` endpoints. `https://` uses
  rustls.
- An audit log of every `new`, `send`, and `close`, which records message hashes, never message
  text.
- A vendored copy of the rexrs client with API revision 8 patches. See `vendor/rexrs/NOTICE`.
