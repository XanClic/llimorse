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
#                                     # host path) into the container;
#                                     # with no --system, AGENTS.md /
#                                     # CLAUDE.md in the working directory
#                                     # is pushed in automatically
#   lemonade.sh --resume /sessions/<file> # continue a previous session
#
# The container runs with --rm: it and its writable layer (target/,
# runtime dnf installs) are gone when lemon exits. The worktree and the
# session logs under ${TMPDIR:-/tmp}/lemon are gone at reboot. A SearXNG
# sidecar for web_search runs on a private per-session network and dies
# when the script exits.

set -euo pipefail

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
fi

# Host-built lemon binary the container runs. Stale until you rebuild on
# the host — by design. `|| true` keeps set -e from killing the script
# before the check below can print the helpful error.
LEMON_BIN="${LEMON_BIN:-$(command -v lemon || true)}"

# Where llama-server runs, as reachable from the container. The default
# is podman's name for the host; set LLAMA_HOST for another machine.
LLAMA_HOST="${LLAMA_HOST:-host.containers.internal}"

[ -x "$LEMON_BIN" ] || { echo "lemon binary not found (set LEMON_BIN)" >&2; exit 1; }

# Canonicalize to an absolute path: podman treats a -v source that does not
# begin with '.' or '/' as a named volume, so a bare relative LEMON_BIN
# (e.g. target/release/lemon) dies with a confusing volume-name error.
# realpath also resolves symlinks, which bind-mounts prefer. (Strict mode
# is fine: the -x check above guarantees the file exists.)
LEMON_BIN=$(realpath "$LEMON_BIN")

# Base image: pull for freshness; if the registry is unreachable, fall
# back to the cached copy. (Tag must match the FROM below.)
if ! podman pull --quiet fedora:latest; then
    echo "Warning: could not pull fedora:latest (offline or registry unreachable);" >&2
    echo "         falling back to the locally cached base image." >&2
    podman image inspect fedora:latest >/dev/null 2>&1 || {
        echo "Error: no cached fedora:latest either — cannot build the image." >&2
        exit 1
    }
fi

# SearXNG sidecar image: same pull-and-fallback pattern as the base.
SEARXNG_IMAGE=searxng/searxng
if ! podman pull --quiet "$SEARXNG_IMAGE"; then
    echo "Warning: could not pull $SEARXNG_IMAGE (offline or registry unreachable);" >&2
    echo "         falling back to the locally cached image." >&2
    podman image inspect "$SEARXNG_IMAGE" >/dev/null 2>&1 || {
        echo "Error: no cached $SEARXNG_IMAGE either — cannot start the sidecar." >&2
        exit 1
    }
fi

# Image: refresh. Cached no-op unless the upstream base tag moved.
# The Containerfile is embedded below and passed on stdin (-f -); with no
# context argument, the context is podman's internal temp dir holding this
# file only — nothing from any repository is ever referenced.
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

# Build artifacts: when the repo root has a Cargo.toml, the entrypoint
# symlinks /work/target — cargo's default target directory — to the
# writable-layer /target, so builds land on the container's writable
# layer (disk, not the worktree's tmpfs) and die with the container.
# A symlink rather than a bind mount: mounting would need CAP_SYS_ADMIN,
# which a coding agent should not get. Deliberately no CARGO_TARGET_DIR:
# the target repo's own cargo config must not be overridden.
ENV PATH="/root/.cargo/bin:${PATH}"
ENV WORKTREE=/work

# "$@" is what lemonade.sh passes (lemon --zesty ...).
ENTRYPOINT ["/bin/sh", "-c", "if [ -d \"$WORKTREE\" ] && [ -f \"$WORKTREE/Cargo.toml\" ]; then mkdir -p /target && rm -rf \"$WORKTREE/target\" && ln -s /target \"$WORKTREE/target\"; fi; if [ $# -eq 0 ]; then exec bash; fi; exec \"$@\"", "--"]
LEMON_CONTAINERFILE

# Worktree: clone once per session, then the container owns it.
if [ -e "$WORKTREE" ] && [ ! -d "$WORKTREE/.git" ]; then
    # Half-dead worktree (interrupted clone or partial deletion): git
    # clone would refuse the non-empty dir on this and every later run.
    echo "Error: $WORKTREE exists but is not a git worktree." >&2
    echo "       remove it and retry: rm -rf '$WORKTREE'" >&2
    exit 1
fi
if [ ! -d "$WORKTREE/.git" ]; then
    mkdir -p "$WORKTREE_ROOT"
    git clone "$TOPLEVEL" "$WORKTREE"
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
        echo "Warning: no user.name/user.email in the git config of" >&2
        echo "         $TOPLEVEL; commits in the worktree will have no author." >&2
    fi
fi

# The source checkout keeps a remote named lemon-worktree pointing at the
# worktree, so a session's commits are fetched by name, not by path:
# `git fetch lemon-worktree <branch>` (and `git push lemon-worktree
# <branch>` mid-session). Added once and re-checked on every run, so the
# check also recovers from a manually removed remote. It lives only in
# the source checkout — the worktree itself has no remotes, so the review
# gate is untouched.
if ! git -C "$TOPLEVEL" remote get-url lemon-worktree >/dev/null 2>&1; then
    git -C "$TOPLEVEL" remote add lemon-worktree "$WORKTREE"
fi

# The worktree outlives the session (until reboot or manual deletion), so
# the host checkout may have moved since it was cloned. "Start fresh"
# removes the whole state dir — tree and session logs — because a resumed
# conversation is about the tree it ran in.
WT_REF=$(git -C "$WORKTREE" rev-parse --abbrev-ref HEAD 2>/dev/null || echo none)
HOST_REF=$(git -C "$TOPLEVEL" rev-parse --abbrev-ref HEAD 2>/dev/null || echo none)
WT_HEAD=$(git -C "$WORKTREE" rev-parse --short HEAD 2>/dev/null || echo none)
HOST_HEAD=$(git -C "$TOPLEVEL" rev-parse --short HEAD 2>/dev/null || echo none)
if [ "$WT_REF" != "$HOST_REF" ]; then
    echo "Warning: worktree is on '$WT_REF' @ $WT_HEAD but this checkout is" >&2
    echo "         on '$HOST_REF' @ $HOST_HEAD. lemon will start from the" >&2
    echo "         worktree's branch; to start fresh: rm -rf '$LEMON_ROOT/$KEY'" >&2
elif [ "$WT_HEAD" != "$HOST_HEAD" ]; then
    echo "Note: worktree $WT_REF @ $WT_HEAD differs from host @ $HOST_HEAD (un-fetched work?)" >&2
fi

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

SEARXNG=lemonade-searxng
NET="lemonade-$$"
cleanup() {
    podman rm -f "$SEARXNG" >/dev/null 2>&1 || true
    podman network rm "$NET" >/dev/null 2>&1 || true
    rm -f "$SEARXNG_SETTINGS"
}
trap cleanup EXIT

podman network create "$NET"
podman rm -f "$SEARXNG" >/dev/null 2>&1 || true  # stale container from a crashed run
podman run --rm -d --name "$SEARXNG" --network "$NET" \
    -v "$SEARXNG_SETTINGS:/etc/searxng/settings.yml:ro,z" \
    "$SEARXNG_IMAGE"

# System prompt: lemon's --system takes a path that must exist inside the
# container (unlike its other path flags it is not a container path), so
# lemonade.sh intercepts it and bind-mounts the host file — any path, in
# the repo or not — at /system-prompt.md, forwarding the container path.
# With no --system given, AGENTS.md / CLAUDE.md in the working directory
# (where lemonade.sh was invoked, any subdirectory) is detected and
# pushed in; an explicit --system always wins.
SYSFILE=""
NEW_ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --system)
            [ $# -ge 2 ] || { echo "Error: --system needs a value" >&2; exit 1; }
            SYSFILE="$2"; shift 2
            NEW_ARGS+=(--system /system-prompt.md)
            ;;
        --system=*)
            SYSFILE="${1#--system=}"
            NEW_ARGS+=(--system /system-prompt.md)
            shift
            ;;
        *)
            NEW_ARGS+=("$1"); shift
            ;;
    esac
done
set -- "${NEW_ARGS[@]}"

MOUNTS=(-v "$WORKTREE:/work:z"
        -v "$SESSIONS:/sessions:z"
        -v "$LEMON_BIN:/usr/local/bin/lemon:ro,z")
if [ -n "$SYSFILE" ]; then
    [ -f "$SYSFILE" ] || { echo "Error: system prompt file not found: $SYSFILE" >&2; exit 1; }
else
    for cand in AGENTS.md CLAUDE.md; do
        if [ -f "$cand" ]; then
            SYSFILE="$cand"
            echo "Note: using $SYSFILE in the working directory as the system prompt"
            set -- "$@" --system /system-prompt.md
            break
        fi
    done
fi
if [ -n "$SYSFILE" ]; then
    # Canonicalize to absolute: podman treats a -v source without a
    # leading '/' as a named volume (same trap as LEMON_BIN above).
    SYSFILE=$(realpath "$SYSFILE")
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

podman run --rm -it \
    --network "$NET" \
    "${MOUNTS[@]}" \
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
    for f in $(ls -1t "$SESSIONS"); do
        [ -s "$SESSIONS/$f" ] && { newest="$f"; break; }
    done
    if [ -n "$newest" ]; then
        echo "To continue this conversation: lemonade.sh --resume /sessions/$newest"
    fi
fi

exit "$status"
