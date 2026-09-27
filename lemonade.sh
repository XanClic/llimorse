#!/usr/bin/env bash
# lemonade — run lemon in a disposable container on a tmpfs worktree.
#
# Run from inside the checkout of the repo you're working in (any
# subdirectory works; the worktree is keyed on the top-level path).
# Design: lemonade.DESIGN.md.
#
#   lemonade.sh              # lemon --zesty in the container
#   lemonade.sh <extra args> # forwarded after --zesty
#   lemonade.sh --system <host file> # push a system prompt file (any
#                                     # host path) into the container; may be
#                                     # repeated — the files are concatenated
#                                     # in the order given —; with no
#                                     # --system, LEMON.md / AGENTS.md /
#                                     # CLAUDE.md in the working directory
#                                     # are pushed in automatically. Before
#                                     # any of them, a short built-in
#                                     # description of the container
#                                     # environment is always passed as a
#                                     # --system file of its own, noting the
#                                     # agent is free to do whatever it
#                                     # wants, including installing packages
#   lemonade.sh --subagent-system <host file> # like --system, but for
#                                     # lemon's subagent prompt: repeated
#                                     # files are concatenated in the order
#                                     # given; with no --subagent-system,
#                                     # LEMON.SUBAGENT.md in the working
#                                     # directory is pushed in
#                                     # automatically, and with no such
#                                     # file lemon's built-in subagent
#                                     # prompt is used
# If the host has a ~/.lemonade.toml, it is mounted into the container
# at /lemonade.toml and passed to lemon with --config, where it supplies
# base values for lemon's arguments (the command line, including
# everything lemonade appends, still wins). An explicit --config in the
# arguments wins over the file: it names a container path lemonade
# cannot resolve, and lemon refuses the flag twice
#   lemonade.sh --resume /sessions/<file> # continue a previous session
#   lemonade.sh --force-fresh # the worktree holds work this checkout
#                             # lacks, or a clean slate is wanted: wipe
#                             # the worktree (session logs and shared
#                             # directory kept) and start from a fresh
#                             # clone of the checkout
#   lemonade.sh --ignore-divergence # the worktree holds work this
#                             # checkout lacks: start on it as-is instead
#                             # of refusing. Nothing is wiped or
#                             # re-cloned; the work still dies at reboot
#                             # until fetched
#
# The container runs with --rm: it, its writable layer (runtime dnf
# installs) and the anonymous volume holding target/ are gone when lemon
# exits. The worktree, the session logs and the shared /share directory
# under ${TMPDIR:-/tmp}/lemon are gone at reboot. A SearXNG
# sidecar for web_search runs on a private per-session network and dies
# when the script exits.
#
# The worktree outlives the session (until reboot), so it and the checkout
# can move apart — and it is on tmpfs, so work in it that has not been
# fetched into the checkout dies at reboot. A session therefore refuses to
# start while the worktree holds commits the checkout lacks or uncommitted
# changes: fetch them (git fetch lemon-worktree), discard them
# (--force-fresh), or start on the worktree as-is with --ignore-divergence
# and fetch later. A worktree that is merely behind the checkout is
# re-cloned with a note — nothing in it is missing from the checkout, so
# the clone loses nothing. When lemon exits, the script reports any
# un-fetched work, so the fetch decision happens while it is still fresh.
#
# The container never writes the worktree: it is mounted as an overlay
# (podman's :O), so lemon works on a copy and the worktree's .git stays
# as the host left it — safe to use with any git command. When lemon
# exits, a wrapper in the container exports the session's git state (all
# branches and tags, HEAD, and uncommitted changes including untracked
# files; not files ignored by git) as a git bundle, and the script
# imports it into the worktree. The bundle, which is data, is the only
# thing that crosses from the container to the host. If the container is
# killed rather than lemon exiting, nothing is exported, and the
# session's work is lost.

set -euo pipefail

# Diagnostic prefixes: Note/Warning bold, Error bold red, so they stand out
# from regular output and from the raw podman/git output piped through below.
# NO_COLOR (https://no-color.org) disables the coloring.
if [ -n "${NO_COLOR:-}" ]; then
    note() { printf 'Note: %s\n' "$*"; }
    warn() { printf 'Warning: %s\n' "$*" >&2; }
    err()  { printf 'Error: %s\n' "$*" >&2; }
else
    note() { printf '\033[1mNote:\033[0m %s\n' "$*"; }
    warn() { printf '\033[1mWarning:\033[0m %s\n' "$*" >&2; }
    err()  { printf '\033[1;31mError:\033[0m %s\n' "$*" >&2; }
fi

IMAGE=lemon-dev

# Must run inside the work tree of the target repo; --show-toplevel fails
# clearly otherwise and gives the canonical path to key the worktree on.
TOPLEVEL=$(git rev-parse --show-toplevel)

# Per-checkout state on tmpfs (or $TMPDIR), under one root: trees/ holds
# the worktree, sessions/ holds lemon's session logs (the resume handle),
# share/ holds the directory mounted at /share for host<->container data
# exchange; all three named after the checkout's directory name plus a
# hash of its top-level path — the basename keeps the name human-readable,
# the hash keeps sibling checkouts with the same basename from colliding,
# and subdirectories of one checkout share one state dir. All die at
# reboot; a $TMPDIR on disk keeps all.
LEMON_ROOT="${TMPDIR:-/tmp}/lemon"
HASH=$(printf '%s' "$TOPLEVEL" | sha256sum | cut -c1-12)
KEY="$(basename "$TOPLEVEL")-$HASH"
WORKTREE_ROOT="$LEMON_ROOT/trees"
WORKTREE="$WORKTREE_ROOT/$KEY"
SESSIONS_ROOT="$LEMON_ROOT/sessions"
SESSIONS="$SESSIONS_ROOT/$KEY"
mkdir -p "$SESSIONS"
SHARE_ROOT="$LEMON_ROOT/share"
SHARE_DIR="$SHARE_ROOT/$KEY"
mkdir -p "$SHARE_DIR"
# Where the container leaves the session's git state for import_session;
# empty between sessions.
XFER="$LEMON_ROOT/xfer/$KEY"

# One-time move from the old layout (${TMPDIR:-/tmp}/lemon-worktrees):
# same filesystem, so it is a rename; an orphaned old-layout worktree
# would otherwise sit in tmpfs until reboot.
OLD_WORKTREE="${TMPDIR:-/tmp}/lemon-worktrees/$KEY"
if [ -d "$OLD_WORKTREE" ] && [ ! -e "$WORKTREE" ]; then
    mkdir -p "$WORKTREE_ROOT"
    mv "$OLD_WORKTREE" "$WORKTREE"
    note "Moved a worktree from the old layout: $OLD_WORKTREE -> $WORKTREE"
fi

# Where the pieces live: the worktree is what the container works in, the
# sessions dir is what --resume reads back, the share dir is where files
# are exchanged between host and container.
note "State for this checkout lives under $LEMON_ROOT (tmpfs by default; dies at reboot):"
printf '      Worktree:   %s\n' "$WORKTREE"
printf '      Sessions:   %s (lemon session logs; the --resume handle)\n' "$SESSIONS"
printf '      Share:      %s (mounted at /share; host<->container data exchange; dies with the rest at reboot)\n' "$SHARE_DIR"

# Argument interception: these flags are lemonade's, not lemon's, so they
# are consumed here and rewritten for the container; everything else is
# forwarded.
#
# --system: lemon's --system takes a path that must exist inside the
# container (unlike its other path flags it is not a container path), so
# lemonade.sh intercepts it and bind-mounts the host file — any path, in
# the repo or not — at /system-prompt.md, forwarding the container path.
# Repeated --system files are concatenated on the host (order preserved,
# one blank line between them) into a temp file that is mounted instead,
# so lemon still only ever sees the single /system-prompt.md.
# With no --system given, LEMON.md / AGENTS.md / CLAUDE.md in the working
# directory (where lemonade.sh was invoked, any subdirectory) is detected later
# and pushed in; an explicit --system always wins.
#
# --subagent-system: lemon's flag of the same name for the subagent prompt;
# intercepted and resolved exactly like --system (mount at
# /subagent-system-prompt.md). Like --system there is a working-directory
# fallback: with no --subagent-system, LEMON.SUBAGENT.md in the working
# directory (where lemonade.sh was invoked, any subdirectory) is pushed in
# if present; otherwise lemon's built-in subagent prompt is used. An
# explicit --subagent-system always wins.
#
# --force-fresh: discard the worktree's state — the divergence gate below
# would refuse to start while it holds un-fetched work — or simply start
# from a clean slate. Only the worktree directory is wiped; the session
# logs under $SESSIONS and the shared directory under $SHARE_DIR are kept.
#
# --ignore-divergence: override the gate's refusal to start while the
# worktree holds work this checkout lacks. The worktree is started as-is:
# nothing is wiped or re-cloned (a re-clone would destroy the work), and
# the post-session reminder still reports it.
SYSFILES=()
SYSFILE_TEMP=""
SUBAGENT_SYSFILES=()
SUBAGENT_SYSFILE_TEMP=""
FORCE_FRESH=""
IGNORE_DIVERGENCE=""
NEW_ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --system)
            [ $# -ge 2 ] || { err "--system needs a value"; exit 1; }
            SYSFILES+=("$2"); shift 2
            ;;
        --system=*)
            SYSFILES+=("${1#--system=}"); shift
            ;;
        --subagent-system)
            [ $# -ge 2 ] || { err "--subagent-system needs a value"; exit 1; }
            SUBAGENT_SYSFILES+=("$2"); shift 2
            ;;
        --subagent-system=*)
            SUBAGENT_SYSFILES+=("${1#--subagent-system=}"); shift
            ;;
        --force-fresh)
            FORCE_FRESH=1
            shift
            ;;
        --ignore-divergence)
            IGNORE_DIVERGENCE=1
            shift
            ;;
        *)
            NEW_ARGS+=("$1"); shift
            ;;
    esac
done
set -- "${NEW_ARGS[@]}"

# Host-built lemon binary the container runs. Stale until you rebuild on
# the host — by design. `|| true` keeps set -e from killing the script
# before the check below can print the helpful error.
LEMON_BIN="${LEMON_BIN:-$(command -v lemon || true)}"

# Where llama-server runs, as reachable from the container. The default
# is podman's name for the host; set LLAMA_HOST for another machine.
LLAMA_HOST="${LLAMA_HOST:-host.containers.internal}"

[ -x "$LEMON_BIN" ] || { err "lemon binary not found (set LEMON_BIN to point at it)"; exit 1; }

# Canonicalize to an absolute path: podman treats a -v source that does not
# begin with '.' or '/' as a named volume, so a bare relative LEMON_BIN
# (e.g. target/release/lemon) dies with a confusing volume-name error.
# realpath also resolves symlinks, which bind-mounts prefer. (Strict mode
# is fine: the -x check above guarantees the file exists.)
LEMON_BIN=$(realpath "$LEMON_BIN")

# Environment description: an unconditional --system file that comes before
# the user's (resolved below), or stands alone when no user file is chosen.
# Lemon's --system is repeatable and concatenates the files in the order
# given, so this is the head and the user's instructions follow. It is
# embedded in this script, written to a temp file, and mounted at
# /system-prompt-env.md: what the agent runs in, and that it is free to do
# whatever it wants, including installing more packages.
ENV_PROMPT=$(mktemp "${TMPDIR:-/tmp}/lemonade-prompt.XXXXXX")
cat > "$ENV_PROMPT" <<'LEMONADE_PROMPT'
You are an agent running as root inside a disposable Podman container (Fedora),
with the working directory /work: the git repository of the project to work on.
The container itself — including anything you install into it — is deleted when
the session ends. /work is a copy; when the session ends, its git state is
carried back to the host for the user to fetch: all branches and tags, HEAD,
and uncommitted changes, including untracked files. Files ignored by git are
not carried back. /share is readable and writable from both the container
and the host; it is the channel for exchanging files with the user during
the session.

You are free to do whatever you want. A full development toolchain is
preinstalled (git, rustup with a nightly toolchain, cargo, make/gcc, python3
and pip, tmux, ripgrep, curl, and more); whenever you need anything else,
install it — `dnf install -y <package>`, `pip install <package>`, or however
else you like. Installs made at runtime live in the container's writable layer
and die with it, so install what you need, when you need it.
LEMONADE_PROMPT
# The cleanup trap further down also removes this; the early trap covers the
# window before it is installed (a gate refusal, pull or build failure).
trap 'rm -f "$ENV_PROMPT"' EXIT
# The environment description is unconditional and comes first: the user's
# --system file, resolved below, is appended after it.
set -- "$@" --system /system-prompt-env.md

# System prompt file(s): resolved here, before any pulling or building, so
# a mistyped --system fails at once. With no --system, LEMON.md / AGENTS.md
# / CLAUDE.md in the working directory is picked up. Several --system files
# are concatenated (order preserved, one blank line between them) into a
# temp file that is mounted; a single one is mounted as-is. Canonicalized
# to absolute: podman treats a -v source without a leading '/' as a named
# volume (same trap as LEMON_BIN above). Mounted at /system-prompt.md
# further down.
SYSFILE=""
if [ ${#SYSFILES[@]} -gt 0 ]; then
    for f in "${SYSFILES[@]}"; do
        [ -f "$f" ] || { err "System prompt file not found: $f"; exit 1; }
    done
    if [ ${#SYSFILES[@]} -eq 1 ]; then
        SYSFILE=$(realpath "${SYSFILES[0]}")
    else
        SYSFILE=$(mktemp "${TMPDIR:-/tmp}/lemon-system-prompt.XXXXXX")
        SYSFILE_TEMP="$SYSFILE"
        # The cleanup trap further down also removes these; the early trap
        # covers the window before it is installed (a pull or build failure).
        trap 'rm -f "$ENV_PROMPT" "$SYSFILE_TEMP"' EXIT
        first=1
        for f in "${SYSFILES[@]}"; do
            if [ "$first" -eq 1 ]; then first=0; else printf '\n\n' >> "$SYSFILE"; fi
            # $() strips trailing newlines and printf puts exactly one back,
            # so the blank-line join holds even for files without a final
            # newline.
            printf '%s\n' "$(cat -- "$f")" >> "$SYSFILE"
        done
    fi
    set -- "$@" --system /system-prompt.md
else
    for cand in LEMON.md AGENTS.md CLAUDE.md; do
        if [ -f "$cand" ]; then
            SYSFILE=$(realpath "$cand")
            note "Using $cand in the working directory as the system prompt"
            set -- "$@" --system /system-prompt.md
            break
        fi
    done
fi

# Subagent system prompt file(s): lemon's --subagent-system takes a path
# that must exist inside the container, just like --system, so it gets the
# same host-to-container resolution. Like --system there is a
# working-directory fallback: with no --subagent-system, LEMON.SUBAGENT.md
# in the working directory (where lemonade.sh was invoked, any
# subdirectory, deliberately not the top-level) is pushed in if present,
# so the common case needs no flag; otherwise lemon's built-in subagent
# prompt is used. An explicit --subagent-system always wins. Mounted at
# /subagent-system-prompt.md further down.
SUBAGENT_SYSFILE=""
if [ ${#SUBAGENT_SYSFILES[@]} -gt 0 ]; then
    for f in "${SUBAGENT_SYSFILES[@]}"; do
        [ -f "$f" ] || { err "Subagent system prompt file not found: $f"; exit 1; }
    done
    if [ ${#SUBAGENT_SYSFILES[@]} -eq 1 ]; then
        SUBAGENT_SYSFILE=$(realpath "${SUBAGENT_SYSFILES[0]}")
    else
        SUBAGENT_SYSFILE=$(mktemp "${TMPDIR:-/tmp}/lemon-subagent-system-prompt.XXXXXX")
        SUBAGENT_SYSFILE_TEMP="$SUBAGENT_SYSFILE"
        # The cleanup trap further down also removes these; the early trap
        # covers the window before it is installed (a pull or build failure).
        trap 'rm -f "$ENV_PROMPT" "$SYSFILE_TEMP" "$SUBAGENT_SYSFILE_TEMP"' EXIT
        first=1
        for f in "${SUBAGENT_SYSFILES[@]}"; do
            if [ "$first" -eq 1 ]; then first=0; else printf '\n\n' >> "$SUBAGENT_SYSFILE"; fi
            # $() strips trailing newlines and printf puts exactly one back,
            # so the blank-line join holds even for files without a final
            # newline.
            printf '%s\n' "$(cat -- "$f")" >> "$SUBAGENT_SYSFILE"
        done
    fi
    set -- "$@" --subagent-system /subagent-system-prompt.md
else
    if [ -f LEMON.SUBAGENT.md ]; then
        SUBAGENT_SYSFILE=$(realpath LEMON.SUBAGENT.md)
        note "Using LEMON.SUBAGENT.md in the working directory as the subagent system prompt"
        set -- "$@" --subagent-system /subagent-system-prompt.md
    fi
fi

# Config file: if the host has a ~/.lemonade.toml, mount it into the
# container at /lemonade.toml and hand it to lemon with --config, where
# it supplies base values for lemon's arguments (the command line —
# including everything lemonade appends — still wins; lemon merges the
# config under the flags). Lemon's --config, like --system, takes a path
# that must exist inside the container, so the host file is mounted
# rather than passed by its host path. An explicit --config in the
# arguments wins: it names a container path lemonade cannot resolve, and
# lemon refuses the flag twice.
CONFIG_FILE=""
if [ -n "${HOME:-}" ] && [ -f "$HOME/.lemonade.toml" ]; then
    case " $* " in
        *" --config "* | *" --config="*)
            note "Not using ~/.lemonade.toml: a --config was already given"
            ;;
        *)
            CONFIG_FILE=$(realpath "$HOME/.lemonade.toml")
            set -- "$@" --config /lemonade.toml
            note "Mounting the host's lemonade config $CONFIG_FILE into the container at /lemonade.toml (--config)"
            ;;
    esac
fi

# Divergence check: compare this checkout with the worktree and set the
# globals the divergence gate reads:
#   WT_AT_RISK=1   the worktree holds commits this checkout lacks (on any
#                  branch, including a branch this checkout no longer has),
#                  has uncommitted changes, or is on a detached HEAD — work
#                  that dies at reboot if it is never fetched
#   WT_STALE=1     nothing at risk, but the worktree is missing state this
#                  checkout has: a branch the checkout is ahead on, a branch
#                  only the checkout has, or a different checked-out branch
#   RISK_LINES, STALE_REASONS  the human-readable report lines
#
# Nothing is fetched: the checkout only receives the worktree's work when
# the user fetches it. A worktree tip whose commit the checkout does not
# have at all holds work the checkout lacks by definition; one it does
# have is checked for ancestry in the checkout. The worktree's .git is
# never written by the container (see the overlay mount below), so running
# git in it here is safe. With no worktree yet (first run, or one just
# wiped) there is nothing to compare.
check_divergence() {
    WT_AT_RISK=0
    WT_STALE=0
    RISK_LINES=()
    STALE_REASONS=()
    [ -d "$WORKTREE/.git" ] || return 0

    local b wt_tip host_tip n
    while IFS= read -r b; do
        [ -n "$b" ] || continue
        wt_tip=$(git -C "$WORKTREE" rev-parse --verify --quiet "refs/heads/$b") || continue
        if ! host_tip=$(git -C "$TOPLEVEL" rev-parse --verify --quiet "refs/heads/$b"); then
            WT_AT_RISK=1
            RISK_LINES+=("$b: exists only in the worktree (no such branch in this checkout)")
            RISK_LINES+=("        worktree @ $(git -C "$WORKTREE" rev-parse --short "$wt_tip")")
        elif ! git -C "$TOPLEVEL" cat-file -e "$wt_tip^{commit}" 2>/dev/null ||
             ! git -C "$TOPLEVEL" merge-base --is-ancestor "$wt_tip" "$host_tip"; then
            WT_AT_RISK=1
            # Counted in the worktree: it has the checkout's commits from
            # the clone, while the checkout may not have the worktree's.
            n=$(git -C "$WORKTREE" rev-list --count "$wt_tip" "^$host_tip" 2>/dev/null || echo "some")
            RISK_LINES+=("$b: $n commit(s) in the worktree that this checkout lacks")
            RISK_LINES+=("        worktree @ $(git -C "$WORKTREE" rev-parse --short "$wt_tip"), checkout @ $(git -C "$TOPLEVEL" rev-parse --short "$host_tip")")
        elif [ "$wt_tip" != "$host_tip" ]; then
            WT_STALE=1
            STALE_REASONS+=("this checkout is ahead of the worktree on $b")
        fi
    done < <(git -C "$WORKTREE" for-each-ref --format='%(refname:short)' refs/heads)

    local c
    while IFS= read -r c; do
        [ -n "$c" ] || continue
        if ! git -C "$WORKTREE" rev-parse --verify --quiet "refs/heads/$c" >/dev/null; then
            WT_STALE=1
            STALE_REASONS+=("branch $c exists only in this checkout")
        fi
    done < <(git -C "$TOPLEVEL" for-each-ref --format='%(refname:short)' refs/heads)

    local wt_ref host_ref
    wt_ref=$(git -C "$WORKTREE" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)
    host_ref=$(git -C "$TOPLEVEL" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)
    if [ "$wt_ref" = "HEAD" ]; then
        WT_AT_RISK=1
        RISK_LINES+=("the worktree is on a detached HEAD; a commit there may be on no branch at all")
    elif [ "$wt_ref" != "$host_ref" ]; then
        WT_STALE=1
        STALE_REASONS+=("worktree is on '$wt_ref', this checkout is on '$host_ref'")
    fi

    local dirty line
    dirty=$(git -C "$WORKTREE" status --porcelain 2>/dev/null || true)
    if [ -n "$dirty" ]; then
        WT_AT_RISK=1
        RISK_LINES+=("uncommitted changes in the worktree:")
        while IFS= read -r line; do
            RISK_LINES+=("        $line")
        done <<< "$dirty"
    fi
}

# Print a report-line array collected by check_divergence, indented to sit
# under the caller's "Error:" / "Note:" prefix ($1 = the base column).
print_risk_lines() {
    local l
    for l in "${RISK_LINES[@]}"; do printf '%*s%s\n' "$1" '' "$l"; done
}
print_stale_reasons() {
    local l
    for l in "${STALE_REASONS[@]}"; do printf '%*s%s\n' "$1" '' "$l"; done
}

# Session export: runs in the container as `bash -c "$EXPORT_SESSION"
# export-session <agent command...>`. It runs the agent, then writes the
# session's git state from /work (the overlay copy) to /xfer: a bundle of
# all refs, and HEAD (a symbolic ref, or a SHA when detached). Uncommitted
# changes — untracked, non-ignored files included — become a commit on
# refs/session/uncommitted, built with a throwaway index so the agent's
# index is not needed. The files are written under temporary names and
# renamed at the end, so a half-written export never looks complete.
# INT is caught (not ignored, which the agent would inherit), so a Ctrl-C
# that ends the agent does not also end the wrapper before the export.
EXPORT_SESSION=$(cat <<'EXPORT_SESSION'
trap : INT
"$@"
status=$?
cd /work || exit "$status"
git update-ref -d refs/session/uncommitted 2>/dev/null
if git rev-parse --verify --quiet HEAD >/dev/null; then
    idx=$(mktemp)
    if GIT_INDEX_FILE="$idx" git read-tree HEAD &&
       GIT_INDEX_FILE="$idx" git add -A &&
       tree=$(GIT_INDEX_FILE="$idx" git write-tree) &&
       [ "$tree" != "$(git rev-parse 'HEAD^{tree}')" ]; then
        wip=$(GIT_AUTHOR_NAME=session GIT_AUTHOR_EMAIL=session@localhost \
              GIT_COMMITTER_NAME=session GIT_COMMITTER_EMAIL=session@localhost \
              git commit-tree "$tree" -p HEAD -m 'Uncommitted changes at session end') &&
            git update-ref refs/session/uncommitted "$wip"
    fi
    rm -f "$idx"
fi
if { git symbolic-ref --quiet HEAD || git rev-parse --verify HEAD; } > /xfer/HEAD.tmp &&
   git bundle create --quiet /xfer/session.bundle.tmp --all &&
   mv /xfer/HEAD.tmp /xfer/HEAD &&
   mv /xfer/session.bundle.tmp /xfer/session.bundle; then
    :
else
    echo "Exporting the session's git state failed; its work will not reach the worktree." >&2
fi
exit "$status"
EXPORT_SESSION
)

# Session import: bring the state EXPORT_SESSION left in $XFER into the
# worktree, which then looks exactly as /work did when the agent exited:
# branches and tags mirror the bundle (--prune drops those the session
# deleted), HEAD is set, the working tree is reset to it and cleaned of
# untracked files (ignored ones stay), and uncommitted changes are laid
# on top as unstaged changes and untracked files. Only the bundle and the
# validated HEAD line are read; git treats the bundle as data, as it
# would a fetch from any untrusted remote. Idempotent: the export is
# removed only once everything succeeded, so an interrupted import is
# redone by the next run.
import_session() {
    local bundle="$XFER/session.bundle" head
    [ -e "$bundle" ] || return 0
    if [ ! -d "$WORKTREE/.git" ]; then
        warn "Discarding a session export in $XFER: the worktree it belongs to is gone."
        rm -rf "$XFER"
        return 0
    fi
    if [ -L "$bundle" ] || [ ! -f "$bundle" ] || [ -L "$XFER/HEAD" ] || [ ! -f "$XFER/HEAD" ]; then
        err "Malformed session export in $XFER; inspect it, or remove it to discard the session's work."
        exit 1
    fi
    head=$(head -c 256 "$XFER/HEAD" | head -n 1)
    if ! [[ $head =~ ^refs/heads/[A-Za-z0-9._/-]+$ || $head =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]]; then
        err "Malformed HEAD in the session export in $XFER; inspect it, or remove it to discard the session's work."
        exit 1
    fi

    git -C "$WORKTREE" fetch --quiet --prune --update-head-ok --no-write-fetch-head "$bundle" \
        '+refs/heads/*:refs/heads/*' '+refs/tags/*:refs/tags/*' '+refs/session/*:refs/session/*'
    if [[ $head == refs/* ]]; then
        git -C "$WORKTREE" symbolic-ref HEAD "$head"
    else
        git -C "$WORKTREE" update-ref --no-deref HEAD "$head"
    fi
    git -C "$WORKTREE" reset --quiet --hard
    git -C "$WORKTREE" clean --quiet -fd
    if git -C "$WORKTREE" rev-parse --verify --quiet refs/session/uncommitted >/dev/null; then
        git -C "$WORKTREE" read-tree -u --reset refs/session/uncommitted
        git -C "$WORKTREE" reset --quiet
        git -C "$WORKTREE" update-ref -d refs/session/uncommitted
    fi
    rm -f "$bundle" "$XFER/HEAD"
}

# Worktree: cloned for the session, then the container owns it. Re-cloned
# when the divergence gate below decides the worktree is stale, or when
# --force-fresh is given.
#
# --force-fresh first: the flag is an explicit request to destroy whatever
# is in the worktree — half-dead or not — so it runs before the half-dead
# check below. Only the worktree directory goes; the session logs under
# $SESSIONS (the --resume handles) and the shared directory under
# $SHARE_DIR are kept.
if [ -n "$FORCE_FRESH" ]; then
    note "--force-fresh: removing $WORKTREE and re-cloning from $TOPLEVEL."
    rm -rf "$WORKTREE" "$XFER"
fi

if [ -e "$WORKTREE" ] && [ ! -d "$WORKTREE/.git" ]; then
    # Half-dead worktree (interrupted clone or partial deletion): git
    # clone would refuse the non-empty dir on this and every later run.
    err "$WORKTREE exists but is not a git worktree; remove it and retry: rm -rf '$WORKTREE'"
    exit 1
fi

# An export still waiting in $XFER means the last run ended between the
# container's exit and the import (e.g. the script was killed): import it
# now, before the divergence gate looks at the worktree.
if [ -e "$XFER/session.bundle" ]; then
    note "Importing the git state of an earlier session that was never imported"
    import_session
fi

# The source checkout keeps a remote named lemon-worktree pointing at the
# worktree, so the user's fetches address the worktree by name, not by
# path: `git fetch lemon-worktree <branch>` (and `git push lemon-worktree
# <branch>` between sessions). Added
# once and re-checked on every run, so the check also recovers from a
# manually removed remote. It lives only in the source checkout — the
# worktree itself has no remotes, so the review gate is untouched.
if ! git -C "$TOPLEVEL" remote get-url lemon-worktree >/dev/null 2>&1; then
    git -C "$TOPLEVEL" remote add lemon-worktree "$WORKTREE"
    note "This checkout now has a 'lemon-worktree' remote pointing at the worktree; 'git fetch lemon-worktree' is how work comes back out of the container"
fi

# Divergence gate. The worktree outlives the session (until reboot or
# manual deletion), so it and this checkout can move apart — and the
# worktree is on tmpfs, so work in it that has not been fetched into this
# checkout dies at reboot.
#   at risk — the worktree holds commits this checkout lacks, has
#             uncommitted changes, or is on a detached HEAD. Refuse to
#             start: the user must fetch the work or explicitly discard
#             it with --force-fresh. A bare note is how work gets
#             forgotten, and forgetting is the failure mode.
#             --ignore-divergence overrides the refusal: start on the
#             worktree as-is. Nothing is wiped or re-cloned — the work
#             is left exactly as found — and the post-session reminder
#             still reports it.
#   stale   — the worktree is a strict subset of the checkout (the
#             checkout moved on, added branches, or switched branch).
#             Nothing at risk, no decision to make: re-clone with a note
#             and start; every commit in the worktree is already here.
#             Suppressed while at risk: re-cloning would destroy the
#             at-risk work.
#   equal   — start as is.
check_divergence
if [ "$WT_AT_RISK" = 1 ]; then
    if [ -z "$IGNORE_DIVERGENCE" ]; then
        err "The worktree has work that this checkout does not have."
        note "It lives on tmpfs and dies at reboot; the only durable copy of it is a fetch into this checkout:"
        print_risk_lines 7 >&2
        note "Fetch it:    git fetch lemon-worktree   # then merge lemon-worktree/<branch>"
        note "Or discard:  lemonade.sh --force-fresh"
        note "Or ignore:   lemonade.sh --ignore-divergence"
        exit 1
    fi
    warn "--ignore-divergence: starting anyway; the worktree holds work this checkout lacks (still on tmpfs — fetch it before reboot):"
    print_risk_lines 7 >&2
fi
if [ "$WT_STALE" = 1 ] && [ "$WT_AT_RISK" != 1 ]; then
    note "The worktree is behind this checkout; re-cloning it from $TOPLEVEL (every commit in it is already here, so this loses nothing):"
    print_stale_reasons 6
    rm -rf "$WORKTREE"
fi

if [ ! -d "$WORKTREE/.git" ]; then
    mkdir -p "$WORKTREE_ROOT"
    note "Cloning this checkout into the worktree at $WORKTREE (tmpfs; dies at reboot)"
    git clone "$TOPLEVEL" "$WORKTREE"
    # Mirror the checkout's full branch set into the worktree as local
    # branches — a plain clone leaves everything but the checked-out
    # branch as remote-tracking refs (and git refuses to fetch into the
    # checked-out branch). The divergence gate compares branch by branch
    # on the assumption that the worktree's local branches mirror the
    # checkout's; this keeps that true at clone time. The clone already
    # brought in every branch as an origin/* remote-tracking ref, so
    # this only adds local refs, no fetch.
    WT_HEAD_BRANCH=$(git -C "$WORKTREE" symbolic-ref --short HEAD 2>/dev/null || echo "")
    if [ -n "$WT_HEAD_BRANCH" ]; then
        while IFS= read -r ref; do
            b="${ref#refs/remotes/origin/}"
            [ "$b" = "HEAD" ] && continue
            [ "$b" = "$WT_HEAD_BRANCH" ] && continue
            git -C "$WORKTREE" branch "$b" "$ref"
        done < <(git -C "$WORKTREE" for-each-ref --format='%(refname)' 'refs/remotes/origin/')
    fi
    # Review gate: no origin, so nothing in the container can reach your
    # checkout except you fetching it.
    git -C "$WORKTREE" remote remove origin
    # Lets you push a branch into the checked-out worktree between
    # sessions: git push lemon-worktree <branch> (refused while the
    # worktree is dirty). Not during a session: the worktree is then the
    # lower layer of the container's overlay, which must not change
    # underneath it, and the import at session end overwrites it anyway.
    git -C "$WORKTREE" config receive.denyCurrentBranch updateInstead
    # Author identity: the container has no host-side ~/.gitconfig, so
    # resolve name and email where the source checkout lives (its local
    # config, then host global/system) and pin them in the worktree's
    # local config — otherwise commits lemon makes in the container have
    # no author. The worktree signs as the host would. If the host has
    # no identity at all, warn and leave the worktree without one (git
    # refuses commits until one is set).
    HOST_NAME=$(git -C "$TOPLEVEL" config user.name 2>/dev/null || true)
    HOST_EMAIL=$(git -C "$TOPLEVEL" config user.email 2>/dev/null || true)
    if [ -n "$HOST_NAME" ]; then
        git -C "$WORKTREE" config user.name "$HOST_NAME"
    fi
    if [ -n "$HOST_EMAIL" ]; then
        git -C "$WORKTREE" config user.email "$HOST_EMAIL"
    fi
    if [ -z "$HOST_NAME" ] && [ -z "$HOST_EMAIL" ]; then
        warn "No user.name/user.email in the git config of $TOPLEVEL; commits in the worktree will have no author."
    fi
fi

# Base image: pull for freshness; if the registry is unreachable, fall
# back to the cached copy. (Tag must match the FROM below.)
if ! podman pull --quiet fedora:latest; then
    warn "Could not pull fedora:latest (offline or registry unreachable); falling back to the locally cached base image."
    podman image inspect fedora:latest >/dev/null 2>&1 || {
        err "No cached fedora:latest either — cannot build the image."
        exit 1
    }
fi

# SearXNG sidecar image: same pull-and-fallback pattern as the base.
SEARXNG_IMAGE=searxng/searxng
if ! podman pull --quiet "$SEARXNG_IMAGE"; then
    warn "Could not pull $SEARXNG_IMAGE (offline or registry unreachable); falling back to the locally cached image."
    podman image inspect "$SEARXNG_IMAGE" >/dev/null 2>&1 || {
        err "No cached $SEARXNG_IMAGE either — cannot start the sidecar."
        exit 1
    }
fi

# Image: refresh. Cached no-op unless the upstream base tag moved.
# The Containerfile is embedded below and passed on stdin (-f -); with no
# context argument, the context is podman's internal temp dir holding this
# file only — nothing from any repository is ever referenced.
note "Building image '$IMAGE' from the embedded Containerfile (a cached no-op unless fedora:latest moved)"
podman build --pull=never -t "$IMAGE" -f - <<'LEMON_CONTAINERFILE'
# lemon-dev — development image for running lemon in a disposable container.
# Design and rationale: lemonade.DESIGN.md.
# Embedded in lemonade.sh; built at session start on stdin (-f -).
# Upstream tag unchanged -> cached no-op. Tag moved -> one full rebuild.

FROM fedora:latest

# Full dev toolchain, all from dnf so the image has exactly one package
# manager. rustup is packaged by Fedora; rustup-init below performs the
# actual toolchain installation. The agent can dnf-install anything else
# at runtime (long tail); such installs die with the container (--rm).
RUN dnf install -y \
        bash \
        bind-utils \
        ca-certificates \
        curl \
        diffutils \
        file \
        findutils \
        gcc \
        gcc-c++ \
        git \
        glibc-devel \
        iproute \
        jq \
        lsof \
        make \
        netcat \
        ninja-build \
        patch \
        pkgconf-pkg-config \
        procps-ng \
        python3 \
        python3-pip \
        ripgrep \
        rsync \
        socat \
        strace \
        tmux \
        tree \
        which \
        zip \
        rustup \
    && dnf clean all

# Nightly by default. A rust-toolchain.toml in the checkout takes
# precedence inside the tree (rustup resolves per directory).
RUN rustup-init -y --default-toolchain nightly

# Build artifacts: lemonade.sh mounts an anonymous volume at /work/target
# for Cargo repos (see MOUNTS there). Deliberately no CARGO_TARGET_DIR:
# the target repo's own cargo config must not be overridden.
ENV PATH="/root/.cargo/bin:${PATH}"

# "$@" is what lemonade.sh passes (lemon --zesty ...).
ENTRYPOINT ["/bin/sh", "-c", "if [ $# -eq 0 ]; then exec bash; fi; exec \"$@\"", "--"]
LEMON_CONTAINERFILE

# SearXNG sidecar (lemon's web_search tool): private per-session network,
# killed by the EXIT trap. It keeps the image's native port 8080 inside
# the network and is never published to the host, so it cannot collide
# with a host-side llama-server on 127.0.0.1:8080.
SEARXNG_SETTINGS=$(mktemp "${TMPDIR:-/tmp}/lemon-searxng-settings.XXXXXX")
cat > "$SEARXNG_SETTINGS" <<'LEMON_SEARXNG_SETTINGS'
use_default_settings: true

search:
  formats:
    - html
    - json

server:
  limiter: false
  secret_key: "local-agent-dev"
LEMON_SEARXNG_SETTINGS

# Both names are per-process, so lemonade sessions in different checkouts
# never touch each other's sidecar or network.
SEARXNG="lemonade-searxng-$$"
NET="lemonade-$$"
cleanup() {
    podman rm -f "$SEARXNG" >/dev/null 2>&1 || true
    podman network rm "$NET" >/dev/null 2>&1 || true
    rm -f "$SEARXNG_SETTINGS"
    rm -f "$SYSFILE_TEMP"
    rm -f "$SUBAGENT_SYSFILE_TEMP"
    rm -f "$ENV_PROMPT"
}
trap cleanup EXIT

NET_ID=$(podman network create "$NET")
note "Created private network '$NET' (id ${NET_ID:0:12}): only lemon and the SearXNG sidecar share it; it is removed when this script exits"
SID=$(podman run --rm -d --name "$SEARXNG" --network "$NET" \
    -v "$SEARXNG_SETTINGS:/etc/searxng/settings.yml:ro,z" \
    "$SEARXNG_IMAGE")
note "SearXNG sidecar running — lemon's web_search backend (container '$SEARXNG', id ${SID:0:12}); it is killed when this script exits"

# The worktree as an overlay (:O): lemon sees and changes a copy, whose
# changes podman discards with the container; what survives is what
# EXPORT_SESSION writes to /xfer. (:O takes no SELinux relabel option.)
rm -rf "$XFER"
mkdir -p "$XFER"
MOUNTS=(-v "$WORKTREE:/work:O"
        -v "$XFER:/xfer:z"
        -v "$SESSIONS:/sessions:z"
        -v "$SHARE_DIR:/share:z"
        -v "$LEMON_BIN:/usr/local/bin/lemon:ro,z"
        -v "$ENV_PROMPT:/system-prompt-env.md:ro,z")
# Host local time: the image ships /etc/localtime pointing at UTC, so
# without this the container reads GMT. We pass the host's zone as TZ
# and let the image resolve it against its own /usr/share/zoneinfo,
# which makes date, ls and git's timestamp offsets show the host's
# zone. The naive way is to mount the host's file, but its :z relabel
# is denied by SELinux (lsetxattr: operation not permitted), so we
# don't. readlink covers the common case (/etc/localtime is a symlink
# to the zone file); a copied file has no recoverable zone name, so we
# fall back to the caller's $TZ, if any.
TZ_ENV=()
if [ -L /etc/localtime ]; then
    zone=$(readlink -f /etc/localtime)
    case "$zone" in
        /usr/share/zoneinfo/*) TZ_ENV=(-e "TZ=${zone#/usr/share/zoneinfo/}") ;;
    esac
elif [ -n "${TZ:-}" ]; then
    TZ_ENV=(-e "TZ=$TZ")
fi
[ -n "${TZ_ENV[*]:-}" ] || note "Could not determine the host timezone; the container stays at UTC"
# Shared directory: mounted at /share, readable and writable from both
# the container and the host — the channel for exchanging files between
# them during the session. Under $LEMON_ROOT/share like the worktree and
# session logs, so it dies at reboot with them (a $TMPDIR on disk keeps
# it).
# Build artifacts: when the repo root has a Cargo.toml, an anonymous
# volume at /work/target — cargo's default target directory — so builds
# land on disk rather than the worktree's tmpfs, and die with the
# container (--rm removes anonymous volumes). Podman sets the mount up
# at container start, so the container needs no CAP_SYS_ADMIN. On the
# host, the worktree only gets an empty target/ mount point, which git
# ignores.
if [ -f "$WORKTREE/Cargo.toml" ]; then
    # Worktrees from before the volume carry a target -> /target symlink
    # made by the old entrypoint; podman would resolve the mount
    # destination through it.
    if [ -L "$WORKTREE/target" ]; then
        rm "$WORKTREE/target"
    fi
    MOUNTS+=(-v /work/target)
fi
if [ -n "$SYSFILE" ]; then
    MOUNTS+=(-v "$SYSFILE:/system-prompt.md:ro,z")
fi
if [ -n "$SUBAGENT_SYSFILE" ]; then
    MOUNTS+=(-v "$SUBAGENT_SYSFILE:/subagent-system-prompt.md:ro,z")
fi
if [ -n "$CONFIG_FILE" ]; then
    MOUNTS+=(-v "$CONFIG_FILE:/lemonade.toml:ro,z")
fi

# Session logs: lemon appends the current session to /sessions (mounted
# from $SESSIONS) so the log outlives the --rm container and can be
# resumed. Injected unless the caller already chose a location — clap
# rejects a flag passed twice.
hint_resume=0
case " $* " in
    *" --session-logs "* | *" --session-logs="*) ;;
    *) set -- "$@" --session-logs /sessions; hint_resume=1 ;;
esac

# Terminal env from the host: without it the container runs with TERM
# unset, which strips color from lemon's non-TUI output and removes the
# tmux signal term-ui reads to gate its OSC 99 passthrough. COLORTERM is
# passed through as in coolade.sh, defaulting to truecolor when the host
# does not export one, though term-ui does not read it yet.
TERMINAL_ENV=()
if [ -n "${TERM:-}" ]; then
    TERMINAL_ENV+=(-e "TERM=$TERM")
fi
if [ -n "${COLORTERM:-}" ]; then
    TERMINAL_ENV+=(-e "COLORTERM=$COLORTERM")
else
    TERMINAL_ENV+=(-e COLORTERM=truecolor)
fi

# Named after this process, like the network, so it can easily be found
podman run --rm -it --name "lemonade-$$" \
    --network "$NET" \
    "${MOUNTS[@]}" \
    "${TERMINAL_ENV[@]}" \
    "${TZ_ENV[@]}" \
    -w /work \
    "$IMAGE" \
    bash -c "$EXPORT_SESSION" export-session \
    lemon --zesty \
        --llama-url "http://$LLAMA_HOST:8080" \
        --searxng-url "http://$SEARXNG:8080" \
        "$@" || status=$?
status=${status:-0}

# Resume hint: point at the newest non-empty log — the one lemon just
# wrote (or the last real one, if this run died before logging a
# message). The path is the container's: the file must exist inside the
# next container as well. Printed even when lemon fails — a crash is
# exactly when the resume line is wanted.
if [ "$hint_resume" -eq 1 ]; then
    newest=""
    for f in "$SESSIONS"/*; do
        [ -s "$f" ] || continue
        if [ -z "$newest" ] || [ "$f" -nt "$newest" ]; then
            newest="$f"
        fi
    done
    if [ -n "$newest" ]; then
        note "To continue this conversation: lemonade.sh --resume /sessions/${newest##*/}"
    fi
fi

# Session import: the container is gone; carry its exported git state into
# the worktree. No export means the container ended without the wrapper
# getting to it (killed), or the export failed (the wrapper said why).
if [ -e "$XFER/session.bundle" ]; then
    import_session
else
    warn "The session exported no git state; whatever it did is lost."
fi

# Post-session reminder: un-fetched work in the worktree is on tmpfs and
# dies at reboot. The gate above refuses to start while it exists, but the
# moment to fetch is now, while the session is fresh in the user's head —
# not at the next hard stop. Same check as the gate; a reminder, not a
# stop, the session already ran.
check_divergence
if [ "$WT_AT_RISK" = 1 ]; then
    note "The worktree has work that this checkout does not have. It dies at reboot — fetch it now, or discard it deliberately:"
    print_risk_lines 6
    note "Fetch it:    git fetch lemon-worktree   # then merge lemon-worktree/<branch>"
    note "Or discard:  lemonade.sh --force-fresh"
elif [ "$WT_STALE" = 1 ]; then
    note "This checkout has moved past the worktree; the next run will re-clone it."
fi

exit "$status"
