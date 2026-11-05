# Streaming Claude Code transcripts

Status: design, not built. Evidence gathered 2026-11-05 on Matt's Mac with Claude Code 2.1.318
and a Rex server on control API revision 8.

`rex-relay` today learns what Claude Code is doing from the pane: OSC 7501 status and the
screen. This document proposes reading Claude Code's own transcript, the JSONL file it writes
under `~/.claude/projects`, through a small `rex-relay` component on the Rex host. The transcript
gives an exact end-of-turn marker and the final assistant message as text, and it is
append-only, so a client that was disconnected can catch up from where it left off.

## Recommendation

- **Mapping.** Rex's `terminal.process` gives the pane's foreground pid. Claude Code writes
  `~/.claude/sessions/<pid>.json`, which names the Claude session id. The transcript is
  `~/.claude/projects/<slug>/<session id>.jsonl`.
- **Done.** A `system` record with subtype `turn_duration` ends every completed turn. The final
  message is the text of the last assistant message before it.
- **Needs approval or a question.** The transcript can't tell a pending approval from a running
  tool. The session file's `status: "waiting"` can, and OSC 7501 says which kind.
- **Transport.** A host daemon, `rex-relay host`, connects to the local Rex socket as an ordinary
  Rex client and answers `client.perform` requests from the remote `rex-relay`. Events ride the
  existing Rex connection. Nothing listens on a new port, and the tailnet ACLs don't change.

## 1. From a pane to its transcript

### What works

1. Call `com.superlogical.terminal.process` on the pane. Use `foreground.pid` when the
   foreground program is Claude Code (argv0 `claude`). For this session's pane it was 26944,
   reported while a Bash tool call was running, so Claude Code stays the foreground program
   while its tools run.
2. Read `~/.claude/sessions/<pid>.json`. For pid 26944:

   ```json
   {"pid": 26944, "sessionId": "72e5a7b7-af1a-4798-9404-33d860194aa0",
    "cwd": "/Users/mbriggs/src/rex-relay", "procStart": "Thu Nov  5 02:36:45 2026",
    "version": "2.1.318", "kind": "interactive", "status": "busy",
    "statusUpdatedAt": 1793883628461, "name": "rex-relay-1c",
    "messagingSocketPath": "/tmp/cc-socks/26944.sock", "…": "…"}
   ```

3. Check `procStart` against the process's start time, read in UTC (`TZ=UTC ps -o lstart= -p
   <pid>`). It matched for every live session file on this Mac. A mismatch means the file is
   stale and the pid was reused, so treat the pane as unmapped.
4. Open `~/.claude/projects/<slug>/<sessionId>.jsonl`. The slug is the `cwd` with `/` replaced
   by `-` (`-Users-mbriggs-src-rex-relay`). The evidence covers only slashes, so when that path
   is missing, fall back to the glob `~/.claude/projects/*/<sessionId>.jsonl`. The session id is
   a UUID, so the glob finds at most one file.

### What each case does

Each case ran against a scratch Claude Code session in a scratch Rex session.

| Case | Pid | Claude session id | Transcript |
| --- | --- | --- | --- |
| Fresh start | New | New | Not created until the first prompt (or `/clear`) |
| `/clear` | Same | New. The session file is rewritten in place. | A new file |
| `claude --resume <id>` | New | Same as the resumed one | Appended to the same file (7,150 → 12,774 bytes) |
| Rex session renamed | Same | Same | Same. Claude Code's `name` field is its own and unrelated. |
| Process exits | Gone | Session file deleted | Kept |

Three rules follow from the table:

- **Re-read the session file while watching.** `/clear` changes the session id under the same
  pid, so a cached id goes stale. Emit a `session_changed` event when it does.
- **Map from pane to session, never back.** After `--resume`, two live pids named the same
  session id at once.
- **Wait for the file.** A fresh session has an id before it has a transcript.

### What doesn't work

- **Open files.** Claude Code doesn't hold its transcript open. `lsof -p 26944` shows no
  `.jsonl` file, only `/tmp/cc-socks/26944.sock`.
- **The command line.** Rex returns only argv0, and `claude --resume` without an id hides the
  session anyway.
- **The terminal title and the Rex label.** Neither is unique, and Claude Code renames the Rex
  session.
- **`messagingSocketPath`.** It looks like a peer-messaging socket (`peerFeatures` includes
  `notify_idle`), but it's undocumented. Don't build on it.

All of `~/.claude/sessions` and the transcript format are undocumented internals of Claude Code.
The host daemon should check `version` in the session file and fall back to OSC 7501 when it
meets a version it hasn't been tested with.

## 2. What the transcript says

Each line is one JSON record with a `type`, and some have a `subtype`. These are the records
that matter, counted in this session's transcript of 2,000-odd records:

| Record | Meaning |
| --- | --- |
| `user` with string content, or a `text` block, not `isMeta` | A prompt. This includes turns Claude Code starts on its own, such as `<task-notification>` when a background command ends. |
| `assistant` | One content block of a model message: `thinking`, `text`, or `tool_use`. A message with several blocks is split across records that share `message.id`. |
| `user` with `tool_result` blocks | A tool's result. `is_error: true` marks a failure or a refusal. |
| `system` / `turn_duration` | The turn ended. It carries `durationMs` and `messageCount`. |

### Turn finished

`system/turn_duration` marks the end of a turn. In this session, all 14 completed turns end with
exactly one. The only two prompts without one were the turn in progress and a turn interrupted
during generation, which left only an `api-request` record. A turn where the user declined an
approval also ends with `turn_duration`, after a `tool_result` with `is_error: true` ("The user
doesn't want to proceed with this tool use…").

An interrupted turn has no end marker. The next prompt record is the only sign that it ended.

### Final assistant message

Take the assistant records after the turn's prompt and before its `turn_duration`. Find the last
`message.id` that has `text` blocks, and join its text blocks. Its `stop_reason` is `end_turn`,
or `stop_sequence` in one turn here.

### Needs approval, or a question

While a permission prompt waited for an answer, the three sources said:

| Source | What it showed |
| --- | --- |
| OSC 7501 | `blocked`, kind `permission`, msg `approve Bash: touch /tmp/rr-map/approval-probe` |
| `~/.claude/sessions/<pid>.json` | `status: "waiting"` |
| Transcript | Last record: an assistant `tool_use` (Bash) with no result yet |

A tool that is still running looks the same in the transcript. So `waiting` in the session file
is the signal, and OSC 7501 supplies the kind and message. The session file's statuses seen here
were `busy`, `idle`, and `waiting`. A question asked with the AskUserQuestion tool wasn't tested.
The expectation is the same: an unanswered `tool_use` and `waiting`.

### Compared with OSC 7501

The transcript is the better trigger for Done:

- **It's on time.** In the first live `--clear-input` test, the transcript's `turn_duration`
  is 03:30:16.789Z. The OSC `done` record arrived at 03:30:18.566Z, 1.8 seconds later, and its
  status record was briefly cleared in between. That record carried a generated summary
  ("replied 'pong' as requested; no tools used"). In the next turn, whose record had no summary,
  OSC lagged by 14 ms.
- **It doesn't disappear.** OSC 7501 is a live status that can be cleared and replaced. A
  transcript record stays, so a client that missed it can still read it.
- **It carries the reply.** OSC carries only a short status message.

This corrects commit `1d86444`. That commit read the cleared status as Claude Code still working,
but the transcript shows the turn had already ended when `rex-relay` reported Done. The 5-second
idle grace it added costs up to 5 seconds at the end of a turn and is otherwise harmless. Once the
transcript drives Done, the grace only matters for the OSC fallback.

OSC 7501 stays the better source for the blocked kinds (`permission`, `question`, `auth`) and
works without the host daemon, so it remains the fallback.

## 3. Getting events to the client

The client runs on another machine and reaches the Rex host only through the Rex port on the
tailnet. The transcript lives on the Rex host, so something there has to read it.

### Option A: ride the Rex connection (recommended)

`rex-relay host` connects to the local Rex socket as a Rex client, labelled `rex-relay-host`. The
remote `rex-relay` finds it with `client.list` and calls `client.perform` on it. Rex delivers
each request to the daemon as a `client_perform_requested` event, and the daemon answers with
`client.reply`. Per the API docs:

- `timeout_ms` defaults to 10,000 and is capped at 30,000.
- `args` and the reply are JSON objects.
- The daemon doesn't have to declare its actions. "A client that listed none is asked anyway."
- Requests to one client wait in line and are answered one at a time, at most 16 waiting.
- Rex tells the daemon which client asked, "so a client can't claim to be another."

Clients on this server already declare actions for others to call: `rex -C <client> actions`
lists the Rex app's (`client.window.new` and others). Neither the daemon nor a
`client.perform` call between two `rex-relay` processes has been tried yet, so the first
implementation step is a prototype of that round trip.

Two actions are enough:

- `relay.locate {session_id, block_id}` returns the Claude session id and whether a transcript
  exists.
- `relay.events {session_id, block_id, cursor, max_bytes}` returns the events after `cursor`,
  and a new cursor.

A cursor is `<claude session id>:<byte offset>`. The transcript only grows, so an offset stays
valid, and after `/clear` the daemon reports the new session id. The daemon answers at once
instead of holding requests open, because one long poll would block every other caller in the
line. The client polls every second while it waits, as it already polls status every 3 seconds.
Replies are paged to stay under the connection's `max_message_bytes` (1 MiB here).

**Security.** Rex identifies every connection. `client.list` shows `principal.kind: "peer"` with
uid and pid for Unix-socket clients, and `kind: "tailscale"` with `login_name`, `node`, and
`tags` for clients on the tailnet. The remote `rex-relay` client connected to this server today
shows up as the latter.
The daemon looks up the caller's principal and serves only principals on an allowlist in its
config, such as a tag. Anyone who can reach the Rex port can already read every screen and type
into every shell, so the daemon doesn't widen *who* can see transcripts. It widens *what* they
can see: a transcript has the whole history, tool inputs, and tool output that never reached the
screen. So:

- The daemon serves only events from panes it maps itself. It never accepts a path from the
  caller.
- By default, events carry prompt and assistant text, tool names, and `is_error`, not tool
  inputs or outputs.
- The allowlist starts empty, so nothing is served until Matt adds a principal.

**What Matt changes.** Run `rex-relay host` on the Rex host as a launchd agent, as the user Rex
and Claude Code run as, and give it a config with the principal allowlist. No ACL or Tailscale
Serve change.

### Option B: a daemon on its own tailnet port

The daemon serves Server-Sent Events or a WebSocket on a port of its own.

- **For:** true push, and no Rex request limits or queue.
- **Against:** a second listener with its own authentication. Tailscale Serve can pass the
  caller's identity, but that's one more thing to get right. A wrong ACL exposes transcripts.
- **What Matt changes:** an ACL grant from the relay's tag to the new port, a Tailscale Serve
  entry, and the launchd agent.

### Option C: a webhook that wakes the client

The daemon, or a Claude Code `Stop` or `Notification` hook, posts to a URL on the client's
machine when a turn ends or needs input.

- **For:** instant wake-up with no polling.
- **Against:** the client needs an inbound port, which is an ACL change in the other direction.
  The client also needs retries and catch-up anyway, because it can't receive while down. A hook
  would live in Matt's Claude Code settings and run in every Claude Code process.
- **Best use:** a wake-up signal on top of A, carrying only a session id and a cursor, not the
  data channel.

## 4. Proposed commands

All of these use the host daemon when it answers and say so when it doesn't. Commands that can,
fall back to today's OSC behaviour.

- **`rex-relay stream <target> [--since CURSOR] [--follow]`** prints one JSON event per line
  from the pane's transcript: `turn_started`, `assistant_text`, `tool_use` (name only),
  `tool_result` (`is_error` only), `needs_input` (kind and message from OSC),
  `turn_finished` (duration and final text), and `session_changed`. Every event carries its
  cursor. Without `--since`, it starts at the end of the file. `--follow` keeps going across
  turns, reconnects, and `/clear`.
- **`send --reply`** prints only the final assistant message of the turn the message started,
  instead of the screen. `send` records the transcript cursor before it types. The first prompt
  after that cursor whose text hashes to the message's SHA-256 (already in the audit log) is its
  turn. Exit codes don't change.
- **`wait --since CURSOR`** catches up after a dropped connection. If a `turn_finished` after
  the cursor already exists, it exits at once with that turn's result. `send`'s `sent` event
  includes the cursor, so after exit 16 a caller runs `rex-relay wait <target> --since
  <cursor>` once the server is back, even hours later, and gets the real outcome.

## Open questions for Matt

- How the daemon runs: a launchd agent from the dotfiles, or something else.
- The first principal for the allowlist: a tag such as `tag:rex-relay`, or the Linux node's name.
- Whether tool inputs and outputs may ever leave the host, behind a flag, or never.
- How far back `stream --since` may read. The whole file, or only the current session id.

## How the evidence was gathered

Every check ran against this session (pid 26944), the live Rex server, and three scratch Rex
sessions running Claude Code in this repository, which were closed afterwards. The checks read
`~/.claude/sessions/*.json`, compared `procStart` with `TZ=UTC ps -o lstart=`, and counted record
types in transcripts by structure only, without printing their content. They also typed `/clear`
into a scratch session, started `claude --resume <id>` in another, renamed a Rex session with
`rex session rename`, and raised one real permission prompt (`touch` in `--permission-mode
default`), declined with Esc. The scratch sessions left two transcripts in
`~/.claude/projects/-Users-mbriggs-src-rex-relay/`: `264027bc-…` and the approval probe's.

## Prototype findings: `client.perform` round trip

Tried 2026-11-05 against the live Rex server on control API revision 8, with both processes on
the Rex host over the Unix socket. `rex-relay host --prototype` connects with the label
`rex-relay-host` and answers `relay.ping`. `rex-relay debug perform <client> <action> [args]`
finds the client in `client.list` by id or label and calls `client.perform`. Both commands are
hidden.

- **The round trip works.** The daemon declares no actions and is asked anyway, as the API docs
  say. `relay.ping` came back with its args and the caller's `client_id` and `principal`.
- **The principal arrives in the event.** `client_perform_requested` carries
  `requester.principal`, so the daemon doesn't need its own `client.list` call to check a
  caller. A local caller showed `kind: "peer"`, `transport: "unix"`, `uid`, `gid`, and `pid`.
  A caller on the tailnet wasn't tried. The unit tests decode a `tailscale` principal with tags.
- **The daemon receives requests without attaching.** A connection that attached to nothing
  still gets `client_perform_requested`.
- **Errors come back with their codes.** An action the daemon answers with `not_supported`
  reaches the caller as `not supported: <message>`. A client that isn't connected is caught
  before the call, by its absence from `client.list`.
- **A timeout leaves a late reply behind.** With the daemon stopped (`SIGSTOP`), a 2-second
  `timeout_ms` failed after 2 seconds with `timeout`. When the daemon resumed, it answered the
  stale request, and `client.reply` failed with `not_found`. The first prototype exited on that
  error. It now notes the error and keeps serving, and later pings succeed. A real daemon has to
  expect this whenever it is slower than the caller's timeout.
- **The caller's read timeout must cover `timeout_ms`.** The server answers `client.perform`
  only after the other client has, so the caller's socket read timeout is raised to the perform
  timeout plus `call_timeout` for that call. `debug perform` refuses a `--timeout` over 30s,
  the server's cap, and defaults to 10s.
