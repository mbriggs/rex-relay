# shellcheck shell=sh
# Helpers shared by the dev workflow scripts. Source this file; don't run it.

prog=${0##*/}

# die REASON: report an operational failure and exit 1.
die() {
	printf '%s: %s\n' "$prog" "$*" >&2
	exit 1
}

# usage REASON: report misuse and exit 2.
usage() {
	printf '%s: %s\n' "$prog" "$*" >&2
	exit 2
}

is_number() {
	case $1 in '' | *[!0-9]*) return 1 ;; esac
}

need_repo() {
	git rev-parse --git-dir >/dev/null 2>&1 || usage "not inside a git repository"
}

# The primary checkout is the first entry git lists, whichever worktree we run from.
primary_root() {
	git worktree list --porcelain | sed -n '1s/^worktree //p'
}

# issue_path N: where the worktree for issue N lives, next to the primary checkout.
issue_path() {
	root=$(primary_root)
	printf '%s/%s-issue-%s\n' "$(dirname "$root")" "$(basename "$root")" "$1"
}

current_branch() {
	git symbolic-ref --short -q HEAD || die "HEAD is detached; check out a branch first"
}

require_clean() {
	[ -z "$(git status --porcelain)" ] || die "working tree has uncommitted changes"
}

# issue_number BRANCH: print N for a branch named issue-<n>-<slug>, or fail.
issue_number() {
	case $1 in
	issue-*-*)
		n=${1#issue-}
		n=${n%%-*}
		is_number "$n" && printf '%s\n' "$n"
		;;
	*) return 1 ;;
	esac
}
