# Development workflow

Each issue gets its own git worktree next to the primary checkout, on a branch named
`issue-<n>-<slug>`. Four scripts in `scripts/` create the worktree, keep it current, open the
pull request, and clean up after the merge. They need only `git` and `gh`, and they work from any
worktree of the repository.

```
rex-relay/                  primary checkout, on main
rex-relay-issue-12/         worktree on issue-12-dev-workflow
rex-relay-issue-15/         worktree on issue-15-...
```

## Start an issue

```sh
scripts/worktree-new 12
cd ../rex-relay-issue-12
```

`worktree-new <n> [slug]` creates the worktree from `origin/main` and prints its path. Without a
slug it builds one from the issue title, so `gh` must be logged in. Pass a slug to skip that:
`scripts/worktree-new 12 dev-workflow`. If the worktree already exists, the script prints its
path and changes nothing. If a local `issue-12-*` branch exists, the script checks out that
branch instead of making a new one.

## Stay current

```sh
scripts/worktree-sync
```

On `main` this fast-forwards to `origin/main`. On any other branch it rebases onto `origin/main`.
The script refuses to run with uncommitted changes. If the rebase stops on a conflict, resolve it
and run `git rebase --continue`.

## Open the pull request

Run the same checks CI runs before you push:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Then:

```sh
scripts/pr-open
scripts/pr-open --title "Add dev workflow scripts"
```

`pr-open` pushes the branch with `--force-with-lease` and opens a PR against `main`. The title
defaults to the issue title. The body lists the commit subjects and ends with `Closes #<n>`. If a
PR for the branch is already open, the script pushes and prints that PR's URL, so run it again
after each round of review fixes.

It refuses to push when the tree is dirty, HEAD is detached, the branch name doesn't match
`issue-<n>-<slug>`, there are no commits ahead of `origin/main`, or the branch is behind
`origin/main`. For the last case, run `worktree-sync` first.

## Clean up after the merge

Run this from the primary checkout or another worktree, not from the one being removed:

```sh
scripts/worktree-done 12
scripts/worktree-done ../rex-relay-issue-12
```

`worktree-done <n|path>` removes the worktree, deletes its local branch, and prunes stale
worktree entries. It only proceeds once the branch has landed. A branch has landed when its tip
is in `origin/main`, or when a merged PR's head commit matches its tip. The second check covers
squash merges, whose commits never appear in `main`. To throw away work that never landed, add
`--discard`. That also removes a worktree with uncommitted changes.

After a squash merge, other branches that were based on the merged branch still carry its
original commits. Running `worktree-sync` on them may conflict on those commits. Drop them with
`git rebase --onto origin/main <merged-branch-tip>`.

## Errors

Every error prints `<script>: <reason>` to stderr. Misuse, such as a bad argument or running from
the wrong place, exits with 2. A failed git or `gh` step exits with 1.

Check the scripts with `shellcheck -x scripts/*`.
