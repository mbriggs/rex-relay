---
name: rex-relay
description: rex-relay drives Claude Code panes and shell panes in Rex sessions. Use when a task hands work to a Claude Code session in Rex, checks on or waits for one, or runs a command in a Rex shell pane.
---

# rex-relay

`rex-relay` types into Rex terminal panes, with guard rails, and reports through its exit code how
the program stopped. Its endpoint and allowlist come from `~/.config/rex-relay/config.toml`.
`rex-relay <command> --help` lists every flag.

## Targets

A target is `session`, `session:tab`, or `session:tab:pane`. Each part matches by label, id, or
the number `rex-relay tree` shows, as in `solar-creek:shell` or `solar-creek:1:2`. Claude Code
renames the sessions it runs in, but a session you created with `new` still answers to its
original label. The session id is the stable handle, so use it in anything you run twice. A label
that matches two sessions exits 2 and lists their ids.

## Commands

| Command | Use it to |
| --- | --- |
| `rex-relay tree [session]` | See sessions, tabs, panes, what runs in each, and each pane's status. |
| `rex-relay list` | See sessions. `*` marks the ones you created, which `send` and `run` accept. |
| `rex-relay status <target>` | Check whether Claude Code is working, done, or waiting on a person. |
| `rex-relay read <target> --lines N` | Read the last lines of a pane. |
| `rex-relay new [label] --cwd DIR --purpose TEXT` | Start a Claude Code session you own. Keep the session id it prints. `DIR` is a path on the Rex host; quote `~/...` so the Rex host expands it, not your shell. |
| `rex-relay send <target> "<task>"` | Hand Claude Code a task, then wait until it stops. |
| `rex-relay wait <target>` | Keep waiting on a pane that is still working, without typing anything. |
| `rex-relay run <target> -- '<command line>'` | Run one shell command in a shell pane idle at its prompt. |
| `rex-relay close <session>` | Close a session you created, once its work is finished. |

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
- Neither means the reply ended some other way. Run `rex-relay read <target> --lines 80` and judge.

A session in a directory Claude Code hasn't trusted yet opens with a trust dialog. A person has
to answer it in Rex. Until then, `send` refuses with exit 3.

## Exit codes

| Code | Meaning | Next |
| --- | --- | --- |
| 0 | Done. `run`: the shell prompt is back. | Check for `DONE:` or `BLOCKED:`. |
| 1 | Error, such as a target that matches nothing. | Read stderr and fix the target. |
| 2 | Bad arguments, or a label that matches more than one session. | Check `--help`, or use one of the session ids stderr lists. |
| 3 | A guard rail refused. Nothing was typed. | Read the reason and tell the user. |
| 10 | Needs approval: Claude Code wants permission for a tool. | Tell the user what it asks (`rex-relay status` shows it). They answer in Rex. |
| 11 | Question: Claude Code asked something. | Same as 10. |
| 12 | Needs input, of an unknown kind. | Same as 10. |
| 13 | Needs sign-in. | Same as 10. |
| 14 | Claude Code reported an error. | `rex-relay read` the pane and report it. |
| 15 | Timeout: still working. | `rex-relay wait <target>` to keep waiting. |
| 16 | Lost connection: the Rex server stayed unreachable for about a minute. | Once it's back, `rex-relay status`, then `wait`. |

After exit 15, the message was typed once. Keep following the task with `wait`, because sending
it again starts it twice. Exit 16 can come before or after the message went out. Once the server
is back, `rex-relay read` the pane: if the task is there, `wait` on it, and if not, send it.

## Safety rules

The user decides these. Each flag below overrides a guard rail, so pass it only when the
user has said so for this task:

- `--allow-foreign` reaches a session `rex-relay` didn't create, which belongs to someone else.
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
