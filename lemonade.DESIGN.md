# lemonade: running lemon in a disposable container

## Goal

Run lemon (YOLO mode, `--zesty`) against a throwaway clone of the repo
you're working in, inside a rootless podman container, such that:

- the agent can do whatever it likes inside the container,
- nothing it does reaches your checkout except through an explicit
  `git fetch` by a human,
- nothing ephemeral lingers after the session.

## The session (`lemonade.sh`)

Run from inside the checkout of the repo you're working in (any
subdirectory; the worktree is keyed on the top-level path):

    ./lemonade.sh                 # lemon --zesty
    ./lemonade.sh <extra args>    # forwarded after --zesty
    ./lemonade.sh --system <host file> # push a system prompt in

Steps:

1. Build the image (refresh) from the Containerfile embedded in the
   script itself, passed on stdin (`-f -`) with no context argument —
   the context is podman's internal temp dir holding that file only, so
   nothing from the target repo is ever referenced. The layer cache makes
   this a seconds-long no-op while the upstream `fedora:latest` tag hasn't
   moved; one full rebuild when it does.
   The base image is pulled first for freshness; offline, the build falls
   back to the cached copy and fails clearly if there is none.
2. If the session worktree doesn't exist yet:
   `git clone <top-level> "${TMPDIR:-/tmp}/lemon/trees/<basename>-<hash>"`, then
   `git remote remove origin` (the review gate),
   `git config receive.denyCurrentBranch updateInstead` (so the host can
   push into the checked-out branch mid-session; refused while dirty),
   and a copy of the author identity: `user.name`/`user.email` are
   resolved where the source checkout lives (local config, then host
   global/system) and pinned in the worktree's local config, because the
   container has no host-side gitconfig and would otherwise commit with
   no author. Resolved once at clone time; a long-lived worktree keeps
   the identity it was cloned with. Separately, every run ensures the
   source checkout has a remote `lemon-worktree` pointing at the worktree
   (added if missing), so the git commands below use the name, not the
   path.
3. Start the SearXNG sidecar for `web_search` on a private per-session
   network.
4. `podman run --rm -it` on that network with the worktree, the
   per-checkout session-log directory at `/sessions`, and the lemon
   binary bind-mounted — plus the system prompt file at
   `/system-prompt.md` when one is chosen (see System prompt below) —
   running `lemon --zesty --llama-url
   http://<LLAMA_HOST>:8080 --searxng-url http://lemonade-searxng:8080
   [--session-logs /sessions] "$@"`. The script appends `--session-logs
   /sessions` unless the caller already passes one, and prints the resume
   command for the newest log when lemon exits (see Session logs and
   resume below).

The container runs with `--rm`: when lemon exits, the container and its
writable layer are deleted. That is the whole session-end ritual.

## Image (Containerfile embedded in `lemonade.sh`)

`FROM fedora:latest` — Fedora over Arch because Fedora's base is stable
per release: with `--pull=always` at session start, a full rebuild
happens when a release ships, not on every session.

Installed via dnf (one package manager for the whole image): bash,
ca-certificates, curl, findutils, gcc, gcc-c++, git, glibc-devel, make,
ninja-build, pkgconf-pkg-config, python3, python3-pip, which, rustup.
Rust comes from the Fedora `rustup` package; `rustup-init
--default-toolchain nightly` performs the toolchain install. A
`rust-toolchain.toml` in the checkout overrides the default inside the
tree.

The container runs as root (rootless podman maps it to the host user, so
files stay host-owned). `dnf` works at runtime for the long tail: the
agent can install whatever a task needs. Those installs live in the
writable layer and die with the container. Anything needed every session
gets promoted into the Containerfile.

## The git design

The worktree has **no remotes**. That is the review gate: in YOLO mode
the agent can commit, rebase, and push as much as it likes, but its only
exit path to your checkout is a human running

    git fetch lemon-worktree <branch>

from your checkout. The script keeps a remote named `lemon-worktree`
pointing at the worktree path in the source checkout (added if missing),
so the command is the same on every checkout. It lives only on the host
side — the worktree itself still has nowhere to push — so the gate is
unchanged.

Iteration while the session is alive is host-initiated:

    git push lemon-worktree main

with `receive.denyCurrentBranch=updateInstead` set in the worktree, which
refreshes the checked-out branch — and is refused while the worktree is
dirty, which is the behavior you want.

Iteration does not survive the session: once the worktree is gone
(reboot, or manual deletion), anything not fetched is gone, uncommitted
work included. That is the intended pressure to land work. The
worktree itself outlives the session — the next run reuses it — so if
the host checkout has switched branches in the meantime, lemonade warns
before starting; the worktree keeps its own branch.

## Ephemeral storage, by lifetime

Most disposable inside, least outside:

| what                | where                          | dies with        |
| ------------------- | ------------------------------ | ---------------- |
| `target/` artifacts | container writable layer, symlinked at `/work/target` | `podman rm` (automatic via `--rm`) |
| SearXNG sidecar     | container + private network; settings in a `${TMPDIR:-/tmp}` temp file | script exit (EXIT trap) |
| worktree (code)     | `${TMPDIR:-/tmp}/lemon/trees/<hash>` (tmpfs) | reboot |
| session logs        | `${TMPDIR:-/tmp}/lemon/sessions/<hash>`, mounted at `/sessions` (tmpfs) | reboot |
| your checkout       | host disk                      | never |

`target/` is more ephemeral than the code, on disk rather than RAM, and
inside the container it appears where cargo expects it: the entrypoint
symlinks `/work/target` to the writable-layer `/target` — but only when
the repo root has a `Cargo.toml`, since the redirect is cargo-specific;
other repos get no symlink and no empty `target/` dir. It is a symlink,
not a mount: the writable layer isn't addressable from the host, and
mounting it inside the container would need CAP_SYS_ADMIN, which a
coding agent should not get. On the host the worktree's `target` is a
dangling symlink whose point only exists inside the container's mount
namespace; git ignores it when the checkout's `.gitignore` matches a
file (as cargo's own default, `/target`, does), though a `target/`-only
pattern would leave it visible as untracked.

The worktree hash is a sha256 prefix of the checkout's top-level path
(not the basename) so two checkouts with the same directory name don't
collide; runs from subdirectories of one checkout share its worktree.
`/tmp` is assumed to be tmpfs; `$TMPDIR` is honored if set (and may
point at a disk location, which is a conscious choice).

## Session logs and resume

Lemon records each session as a timestamped JSONL file
(`--session-logs <dir>`), flushed per message. lemonade points that at
`/sessions`, a bind mount of the per-checkout directory
`${TMPDIR:-/tmp}/lemon/sessions/<basename>-<hash>` — a sibling of the
worktree under the same key — so the log survives the container's `--rm`
and the session can be resumed after a crash or a clean exit:

    ./lemonade.sh --resume /sessions/<file>

When lemon exits, if the script itself chose the log location, it prints
the resume command for the newest non-empty log, so the argument never
has to be typed; a failed lemon still gets the hint, because a crash is
exactly when it is wanted. Resuming loads the history into a fresh log
file; the resumed file stays as a snapshot. A caller-supplied
`--session-logs` suppresses both the injection and the hint.

The logs share the worktree's lifetime: they die at reboot, and
`rm -rf ${TMPDIR:-/tmp}/lemon/<basename>-<hash>` forgets a checkout's
tree and its sessions together. Deliberate: the log is a record of an
iteration, and the iteration's code state is durable only once fetched;
a fresh tree makes the old conversation stale. A `$TMPDIR` on disk keeps
both, if a session is meant to outlive a boot.

## System prompt

Lemon's `--system` takes a path that must exist inside the container,
and the worktree is a clone — a host file reaches it only if it is in
the repo, and then only in the form it was cloned in. lemonade
intercepts the flag (both `--system <file>` and `--system=<file>`),
bind-mounts the host file — any path, in the repo or not — read-only at
`/system-prompt.md`, and forwards the container path instead. The
`realpath` canonicalization is the same trap as the lemon binary: a
relative `-v` source is a named volume to podman.

With no `--system` given, `AGENTS.md` / `CLAUDE.md` in the working
directory — where lemonade was invoked, any subdirectory, deliberately
not the top-level — is detected and pushed in, `AGENTS.md` first: these
are where per-project agent instructions already conventionally live, so
the common case needs no flag, and the vendor-neutral name wins when a
directory carries both. An explicit `--system` always wins, a
missing file is a clear error, and no candidate means no `--system` at
all — lemon's default stands.

## The lemon binary

The host build is bind-mounted read-only at `/usr/local/bin/lemon`
(`LEMON_BIN` overrides the default `command -v lemon`). The value is
canonicalized to an absolute path with `realpath` before it reaches
podman: a `-v` source that does not begin with `.` or `/` is treated by
podman as a named volume, so a bare relative path (e.g.
`target/release/lemon`) would otherwise die with a confusing
volume-name error. It is older than
the code the agent edits — by design: the bootstrap runs the last host
build, and the next `lemonade.sh` run picks up whatever you compiled on
the host in the meantime. No rebuild of anything is involved.

## Notes

- A host crash mid-session can leave the session network behind;
  `podman network prune` clears it.
- Bind mounts always carry the `:z` SELinux label: a no-op on hosts
  without SELinux, and what an enforcing host needs.
- The image assumes the host lemon binary is glibc/x86_64, matching
  Fedora.
- The container has full network by default; the container boundary plus
  the git gate are the controls, not network isolation.
- A worktree directory that exists without a `.git` (interrupted clone,
  partial deletion) aborts the run with a message; `rm -rf` it to
  proceed.
- One session per checkout: the worktree hash is keyed on the path, so
  two lemonade runs from the same checkout share one worktree, and two
  containers writing one `.git` is index-lock contention. Running two
  concurrently is unsupported by design; there is deliberately no guard.
- `podman run` uses `-it`: a TTY is assumed, and for now that is a hard
  requirement, because lemon itself requires a TTY — headless or piped
  invocations cannot work. If lemon ever runs without a TTY, making `-t`
  conditional on `[ -t 0 ]` (three lines) is the next step; it changes
  nothing for interactive use.
