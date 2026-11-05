---
name: rexctl
description: rexctl drives Claude Code panes and shell panes in Rex sessions. Use when a task hands work to a Claude Code session in Rex, checks on or waits for one, or runs a command in a Rex shell pane.
---

# rexctl

`rexctl` types into Rex terminal panes, with guard rails, and reports through its exit code how
the program stopped. Its endpoint and allowlist come from `~/.config/rex-relay/config.toml`.
`rexctl <command> --help` lists every flag.

## Targets

A target is `session`, `session:tab`, or `session:tab:pane`. Each part matches by label, id, or
the number `rexctl tree` shows, as in `solar-creek:shell` or `solar-creek:1:2`. Use session ids
in anything you run twice: Claude Code renames the sessions it runs in, so a label goes stale.

## Commands

| Command | Use it to |
| --- | --- |
| `rexctl tree [session]` | See sessions, tabs, panes, what runs in each, and each pane's status. |
| `rexctl list` | See sessions. `*` marks the ones you created, which `send` and `run` accept. |
| `rexctl status <target>` | Check whether Claude Code is working, done, or waiting on a person. |
| `rexctl read <target> --lines N` | Read the last lines of a pane. |
| `rexctl new [label] --cwd DIR --purpose TEXT` | Start a Claude Code session you own. Keep the session id it prints. |
| `rexctl send <target> "<task>"` | Hand Claude Code a task, then wait until it stops. |
| `rexctl wait <target>` | Keep waiting on a pane that is still working, without typing anything. |
| `rexctl run <target> -- '<command line>'` | Run one shell command in a shell pane idle at its prompt. |
| `rexctl close <session>` | Close a session you created, once its work is finished. |

For `send`, `wait`, and `run`, pass `--quiet --lines 40`. You get the pane's last 40 lines and
the final status instead of the whole stream. Add `--json` when you parse the output. The last
object is `{"type":"result",…,"exit_code":N}`.

## Handing Claude Code a task

End every task message with this line, so the reply says how the task ended:

```
End your reply with one final line starting DONE: (finished, one-line summary) or BLOCKED: (what you need).
```

When `send` exits 0, find that line near the end of the tail:

- `DONE:` means the task is finished. Report the summary.
- `BLOCKED:` means Claude Code needs something only the user can give. Relay it to the user.
- Neither means the reply ended some other way. Run `rexctl read <target> --lines 80` and judge.

A session in a directory Claude Code hasn't trusted yet opens with a trust dialog. A person has
to answer it in Rex. Until then, `send` refuses with exit 3.

## Exit codes

| Code | Meaning | Next |
| --- | --- | --- |
| 0 | Done. `run`: the shell prompt is back. | Check for `DONE:` or `BLOCKED:`. |
| 1 | Error, such as a target that matches nothing or more than one session. | Read stderr and fix the target. |
| 2 | Bad arguments. | Check `--help`. |
| 3 | A guard rail refused. Nothing was typed. | Read the reason and tell the user. |
| 10 | Needs approval: Claude Code wants permission for a tool. | Tell the user what it asks (`rexctl status` shows it). They answer in Rex. |
| 11 | Question: Claude Code asked something. | Same as 10. |
| 12 | Needs input, of an unknown kind. | Same as 10. |
| 13 | Needs sign-in. | Same as 10. |
| 14 | Claude Code reported an error. | `rexctl read` the pane and report it. |
| 15 | Timeout: still working. | `rexctl wait <target>` to keep waiting. |
| 16 | Lost connection: the Rex server is unreachable. | Once it's back, `rexctl status`, then `wait`. |

After exit 15, the message was typed once. Keep following the task with `wait`, because sending
it again starts it twice. Exit 16 can come before or after the message went out. Once the server
is back, `rexctl read` the pane: if the task is there, `wait` on it, and if not, send it.

## Safety rules

The user decides these. Each flag below overrides a guard rail, so pass it only when the
user has said so for this task:

- `--allow-foreign` reaches a session `rexctl` didn't create, which belongs to someone else.
  Pass it only for a session the user named.
- `--clear-input` deletes unsent text from Claude Code's input box. That text is usually a
  person's draft. Pass it only when the user said the text can go.
- `--answer` makes your message the reply to a pending approval or question. Pass it only to
  relay the user's own answer.

`run` types a command as the user Rex runs as. Run only commands the task needs, and ask before
anything destructive. Rex doesn't report a command's exit status, so append `; echo "exit $?"`
when success matters. `run` refuses (exit 3) while the shell is busy. Wait for it, or target
another pane.

A refusal (exit 3) is a guard rail working. Report it, and leave the message as it was rather
than rewording it to get past the rail.
