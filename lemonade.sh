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
#                                     # are pushed in automatically
#   lemonade.sh --resume /sessions/<file> # continue a previous session
#   lemonade.sh --force-fresh # the worktree holds work this checkout
#                             # lacks, or a clean slate is wanted: wipe
#                             # the worktree (session logs kept) and start
#                             # from a fresh clone of the checkout
#   lemonade.sh --ignore-divergence # the worktree holds work this
#                             # checkout lacks: start on it as-is instead
#                             # of refusing. Nothing is wiped or
#                             # re-cloned; the work still dies at reboot
#                             # until fetched
#
# The container runs with --rm: it, its writable layer (runtime dnf
# installs) and the anonymous volume holding target/ are gone when lemon
# exits. The worktree and the
# session logs under ${TMPDIR:-/tmp}/lemon are gone at reboot. A SearXNG
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
# both named after the checkout's directory name plus a hash of its
# top-level path — the basename keeps the name human-readable, the hash
# keeps sibling checkouts with the same basename from colliding, and
# subdirectories of one checkout share one state dir. Both die at reboot;
# a $TMPDIR on disk keeps both.
LEMON_ROOT="${TMPDIR:-/tmp}/lemon"
HASH=$(printf '%s' "$TOPLEVEL" | sha256sum | cut -c1-12)
KEY="$(basename "$TOPLEVEL")-$HASH"
WORKTREE_ROOT="$LEMON_ROOT/trees"
WORKTREE="$WORKTREE_ROOT/$KEY"
SESSIONS_ROOT="$LEMON_ROOT/sessions"
SESSIONS="$SESSIONS_ROOT/$KEY"
mkdir -p "$SESSIONS"

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
# sessions dir is what --resume reads back.
note "State for this checkout lives under $LEMON_ROOT (tmpfs by default; dies at reboot):"
printf '      Worktree:   %s\n' "$WORKTREE"
printf '      Sessions:   %s (lemon session logs; the --resume handle)\n' "$SESSIONS"

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
# --force-fresh: discard the worktree's state — the divergence gate below
# would refuse to start while it holds un-fetched work — or simply start
# from a clean slate. Only the worktree directory is wiped; the session
# logs under $SESSIONS are kept.
#
# --ignore-divergence: override the gate's refusal to start while the
# worktree holds work this checkout lacks. The worktree is started as-is:
# nothing is wiped or re-cloned (a re-clone would destroy the work), and
# the post-session reminder still reports it.
SYSFILES=()
SYSFILE_TEMP=""
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
        # The cleanup trap further down also removes this; the early trap
        # covers the window before it is installed (a pull or build failure).
        trap 'rm -f "$SYSFILE_TEMP"' EXIT
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
# The worktree has no remotes (the review gate), so this checkout cannot
# see commits lemon made there unless it fetches them: the fetch below is
# read-only and lands in lemon-worktree/* remote-tracking refs — nothing
# is merged. With no worktree yet (first run, or one just wiped) there is
# nothing to compare.
check_divergence() {
    WT_AT_RISK=0
    WT_STALE=0
    RISK_LINES=()
    STALE_REASONS=()
    [ -d "$WORKTREE/.git" ] || return 0

    git -C "$TOPLEVEL" fetch lemon-worktree --quiet

    local b wt_tip host_tip
    while IFS= read -r b; do
        [ -n "$b" ] || continue
        wt_tip=$(git -C "$WORKTREE" rev-parse --verify --quiet "refs/heads/$b") || continue
        if ! host_tip=$(git -C "$TOPLEVEL" rev-parse --verify --quiet "refs/heads/$b"); then
            WT_AT_RISK=1
            RISK_LINES+=("$b: exists only in the worktree (no such branch in this checkout)")
            RISK_LINES+=("        worktree @ $(git -C "$WORKTREE" rev-parse --short "refs/heads/$b")")
        elif ! git -C "$TOPLEVEL" merge-base --is-ancestor "lemon-worktree/$b" "refs/heads/$b"; then
            WT_AT_RISK=1
            RISK_LINES+=("$b: $(git -C "$TOPLEVEL" rev-list --count "refs/heads/$b..$wt_tip") commit(s) in the worktree that this checkout lacks")
            RISK_LINES+=("        worktree @ $(git -C "$WORKTREE" rev-parse --short "refs/heads/$b"), checkout @ $(git -C "$TOPLEVEL" rev-parse --short "refs/heads/$b")")
        elif [ "$(git -C "$TOPLEVEL" rev-parse "refs/heads/$b")" != "$(git -C "$TOPLEVEL" rev-parse "lemon-worktree/$b")" ]; then
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

# Worktree: cloned for the session, then the container owns it. Re-cloned
# when the divergence gate below decides the worktree is stale, or when
# --force-fresh is given.
#
# --force-fresh first: the flag is an explicit request to destroy whatever
# is in the worktree — half-dead or not — so it runs before the half-dead
# check below. Only the worktree directory goes; the session logs under
# $SESSIONS are kept, they are the --resume handles.
if [ -n "$FORCE_FRESH" ]; then
    note "--force-fresh: removing $WORKTREE and re-cloning from $TOPLEVEL."
    rm -rf "$WORKTREE"
fi

if [ -e "$WORKTREE" ] && [ ! -d "$WORKTREE/.git" ]; then
    # Half-dead worktree (interrupted clone or partial deletion): git
    # clone would refuse the non-empty dir on this and every later run.
    err "$WORKTREE exists but is not a git worktree; remove it and retry: rm -rf '$WORKTREE'"
    exit 1
fi

# The source checkout keeps a remote named lemon-worktree pointing at the
# worktree, so the divergence check below — and the user's own fetches —
# address the worktree by name, not by path: `git fetch lemon-worktree
# <branch>` (and `git push lemon-worktree <branch>` mid-session). Added
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
    # Lets you push a branch back into the checked-out worktree
    # mid-session: git push "$WORKTREE" <branch>
    # (refused while the worktree is dirty).
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
        ca-certificates \
        curl \
        findutils \
        gcc \
        gcc-c++ \
        git \
        glibc-devel \
        make \
        ninja-build \
        pkgconf-pkg-config \
        python3 \
        python3-pip \
        which \
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
}
trap cleanup EXIT

NET_ID=$(podman network create "$NET")
note "Created private network '$NET' (id ${NET_ID:0:12}): only lemon and the SearXNG sidecar share it; it is removed when this script exits"
SID=$(podman run --rm -d --name "$SEARXNG" --network "$NET" \
    -v "$SEARXNG_SETTINGS:/etc/searxng/settings.yml:ro,z" \
    "$SEARXNG_IMAGE")
note "SearXNG sidecar running — lemon's web_search backend (container '$SEARXNG', id ${SID:0:12}); it is killed when this script exits"

MOUNTS=(-v "$WORKTREE:/work:z"
        -v "$SESSIONS:/sessions:z"
        -v "$LEMON_BIN:/usr/local/bin/lemon:ro,z")
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

podman run --rm -it \
    --network "$NET" \
    "${MOUNTS[@]}" \
    "${TERMINAL_ENV[@]}" \
    -w /work \
    "$IMAGE" \
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

# Post-session reminder: un-fetched work in the worktree is on tmpfs and
# dies at reboot. The gate above refuses to start while it exists, but the
# moment to fetch is now, while the session is fresh in the user's head —
# not at the next hard stop. Same check as the gate; a reminder, not a
# stop, the session already ran. Its fetch also leaves the lemon-worktree/*
# refs ready, so `git merge lemon-worktree/<branch>` works immediately.
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
