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
                                       # (repeatable: files concatenated in order)
    ./lemonade.sh --resume /sessions/<file> # continue a previous session
    ./lemonade.sh --force-fresh       # wipe the worktree (session logs
                                       # kept) and re-clone the checkout
    ./lemonade.sh --ignore-divergence # start on a worktree that holds
                                       # un-fetched work, as-is

Steps:

1. Lemonade's own flags are intercepted from the arguments (see System
   prompt below), then the divergence gate runs against any existing
   worktree (see The divergence gate below): un-fetched work aborts the
   run (or starts as-is with --ignore-divergence), a worktree that is a
   strict subset of the checkout is re-cloned with a note.
2. Build the image (refresh) from the Containerfile embedded in the
   script itself, passed on stdin (`-f -`) with no context argument —
   the context is podman's internal temp dir holding that file only, so
   nothing from the target repo is ever referenced. The layer cache makes
   this a seconds-long no-op while the upstream `fedora:latest` tag hasn't
   moved; one full rebuild when it does.
   The base image is pulled first for freshness; offline, the build falls
   back to the cached copy and fails clearly if there is none.
3. If the session worktree still doesn't exist (first run,
   `--force-fresh`, or a re-clone by the divergence gate): `git clone
   <top-level> "${TMPDIR:-/tmp}/lemon/trees/<basename>-<hash>"`, then the
   full branch set is mirrored into the worktree as local branches (a
   plain clone leaves the rest as remote-tracking refs, which the
   divergence gate's per-branch comparison assumes away), then\
   `git remote remove origin` (the review gate),
   `git config receive.denyCurrentBranch updateInstead` (so the host can
   push into the checked-out branch between sessions; refused while dirty),
   and a copy of the author identity: `user.name`/`user.email` are
   resolved where the source checkout lives (local config, then host
   global/system) and pinned in the worktree's local config, because the
   container has no host-side gitconfig and would otherwise commit with
   no author. Resolved once at clone time; a long-lived worktree keeps
   the identity it was cloned with. Separately, every run ensures the
   source checkout has a remote `lemon-worktree` pointing at the worktree
   (added if missing), so the git commands below use the name, not the
   path.
4. Start the SearXNG sidecar for `web_search` on a private per-session
   network.
5. `podman run --rm -it` on that network with the worktree as an overlay
   at `/work` (see The container never writes the worktree below), the
   export handover directory at `/xfer`, the
   per-checkout session-log directory at `/sessions`, and the lemon
   binary bind-mounted — plus the built-in environment description at
   `/system-prompt-env.md`, always, and the chosen system prompt file at
   `/system-prompt.md` when one is chosen (see System prompt below) —
   running `lemon --zesty --llama-url
   http://<LLAMA_HOST>:8080 --searxng-url http://lemonade-searxng-<pid>:8080
   [--session-logs /sessions] "$@"`. The script appends `--session-logs
   /sessions` unless the caller already passes one, and prints the resume
   command for the newest log when lemon exits (see Session logs and
   resume below).

When lemon exits, the script imports the session's exported git state
into the worktree, then runs the divergence check once more and
reports any un-fetched work in the worktree, so the fetch-or-discard
decision happens while the session is fresh (see The divergence gate
below).

The container runs with `--rm`: when lemon exits, the container and its
writable layer are deleted. That is the whole session-end ritual.

## Image (Containerfile embedded in `lemonade.sh`)

`FROM fedora:latest` — Fedora over Arch because Fedora's base is stable
per release: with the base image pulled at session start (then built
with `--pull=never`, so an offline run can use the cached copy), a full rebuild
happens when a release ships, not on every session.

Installed via dnf (one package manager for the whole image): bash,
bind-utils, ca-certificates, curl, diffutils, file, findutils, gcc,
gcc-c++, git, glibc-devel, iproute, jq, lsof, make, netcat, ninja-build,
patch, pkgconf-pkg-config, procps-ng, python3, python3-pip, ripgrep,
rsync, socat, strace, tmux, tree, which, zip, rustup. Besides the build
toolchain, that is the toolbox an agent reaches for without asking:
text and data tools, network and process inspection, and tmux for
checking how a TUI renders.
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

Between sessions, the host can push into the worktree:

    git push lemon-worktree main

with `receive.denyCurrentBranch=updateInstead` set in the worktree, which
refreshes the checked-out branch — and is refused while the worktree is
dirty, which is the behavior you want. Not while a session runs: the
worktree is then the lower layer of the container's overlay, which must
not change underneath a mounted overlay, and the import at session end
would overwrite the push anyway.

### The container never writes the worktree

The worktree's `.git` is an ordinary repository on the host, meant to be
used with any git command, a shell prompt's git integration included. A
repository an agent has written is not safe for that: git executes
commands named in a repository's own config (`core.fsmonitor`, for
one) and runs its hooks, and a repository owned by the host user passes
git's ownership check. So the container never gets to write it.

The worktree is mounted at `/work` as an overlay (podman's `:O`): lemon
sees the tree as the host left it and changes a copy, which podman
discards with the container. When lemon exits, a wrapper around it in
the container (`EXPORT_SESSION` in the script) writes the session's git
state to `/xfer`, a per-checkout directory under
`${TMPDIR:-/tmp}/lemon/xfer/`: a `git bundle` of all refs, and HEAD.
Uncommitted changes — untracked, non-ignored files included — go into
the bundle as a commit on `refs/session/uncommitted`. Files ignored by
git are not carried back.

The script then imports that into the worktree (`import_session`):
branches and tags mirror the bundle (branches the session deleted are
deleted), HEAD is set, the working tree is reset to HEAD and cleaned of
untracked files, and the uncommitted changes are laid on top as unstaged
changes and untracked files. Only the bundle, which git reads as data as
it would a fetch from any untrusted remote, and a validated HEAD line
cross from the container to the host.

If the container is killed rather than lemon exiting, nothing is
exported, and the session's work is lost. If the script dies between the
container's exit and the import, the export waits in `/xfer`, and the
next run imports it before anything else.

Iteration does not survive the session: once the worktree is gone
(reboot, or manual deletion), anything not fetched is gone, uncommitted
work included. That is the intended pressure to land work. The
worktree itself outlives the session — the next run reuses it — so the
checkout and the worktree can move apart between sessions; the
divergence gate below decides what that means.

## The divergence gate

The worktree outlives the session (until reboot or manual deletion) and
sits on tmpfs, so work in it that has not been fetched into the checkout
dies at reboot, and the two sides can move apart in either direction.
Every run therefore compares the checkout and the worktree — branch by
branch, plus the worktree's working tree and HEAD — and acts on the
result:

- **at risk** — a worktree branch holds commits the checkout lacks
  (including a branch the checkout no longer has), the worktree has
  uncommitted changes, or the worktree is on a detached HEAD (a commit
  on no branch). The run refuses to start, lists each offender with the
  worktree and checkout tips, and offers the exits: `git fetch
  lemon-worktree` and then merge, `lemonade.sh --force-fresh` to
  discard the work, or `lemonade.sh --ignore-divergence` to start on
  the worktree as-is. The last overrides only the refusal: nothing is
  wiped or re-cloned (a re-clone would destroy the at-risk work), the
  work is left exactly as found, and the post-session reminder still
  reports it — the flag defers the fetch-or-discard decision, it does
  not make it.
- **stale** — nothing at risk, but the worktree is missing state the
  checkout has: the checkout is ahead on a branch, has a branch the
  worktree lacks, or has switched branch. No decision to make — the run
  re-clones the worktree with a note and starts; every commit in it is
  already in the checkout, so the clone loses nothing. The re-clone is
  suppressed while at risk, since it would destroy that work.
- **equal** — the run starts as is.

`--force-fresh` wipes the worktree directory — the session logs under
the same key are kept — and re-clones from the checkout. A wipe rather
than a push, even though
`receive.denyCurrentBranch=updateInstead` would allow a force-push into
the checked-out branch: a wipe has no checked-out-branch or dirty-tree
edge cases, and its contract is simply "the worktree becomes exactly
the checkout".

The comparison fetches nothing: the checkout receives the worktree's
work only when the user fetches it. A worktree tip whose commit the
checkout does not have at all is work the checkout lacks by definition;
one the checkout does have is checked for ancestry there. The number of
commits at risk is counted in the worktree, which has the checkout's
commits from the clone. When lemon exits, the same check runs again as a
reminder rather than a stop, so the fetch-or-discard decision happens
while the session is fresh in the user's head.

## Ephemeral storage, by lifetime

Most disposable inside, least outside:

| what                | where                          | dies with        |
| ------------------- | ------------------------------ | ---------------- |
| `target/` artifacts | anonymous podman volume mounted at `/work/target` | `podman rm` (automatic via `--rm`) |
| SearXNG sidecar     | container + private network; settings in a `${TMPDIR:-/tmp}` temp file | script exit (EXIT trap) |
| `/work` changes     | the container's overlay upper layer | `podman rm` (after the export) |
| session export      | `${TMPDIR:-/tmp}/lemon/xfer/<basename>-<hash>`, mounted at `/xfer` (tmpfs) | the import |
| worktree (code)     | `${TMPDIR:-/tmp}/lemon/trees/<basename>-<hash>` (tmpfs) | reboot |
| session logs        | `${TMPDIR:-/tmp}/lemon/sessions/<basename>-<hash>`, mounted at `/sessions` (tmpfs) | reboot |
| your checkout       | host disk                      | never |

`target/` is more ephemeral than the code, on disk rather than RAM, and
inside the container it appears where cargo expects it: `podman run`
gets `-v /work/target`, an anonymous volume (in podman's volume storage
on disk) mounted over cargo's default target directory — but only when
the repo root has a `Cargo.toml`, since the redirect is cargo-specific.
Podman sets the mount up at container start, nested inside the `/work`
overlay, so the container needs no CAP_SYS_ADMIN (which mounting
from inside would, and which a coding agent should not get); `--rm`
deletes anonymous volumes along with the container. The `target/`
mount point is created in the overlay, not in the worktree on the
host. Worktrees from before the volume carry a
`target -> /target` symlink left by the old entrypoint; the script
removes it before mounting, since podman would resolve the mount
destination through it.

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
`rm -rf ${TMPDIR:-/tmp}/lemon/{trees,sessions}/<basename>-<hash>`
forgets a checkout's tree and its sessions together. Deliberate: the log is a record of an
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

The flag may be repeated: the files are concatenated on the host —
order preserved, one blank line between them, each file's trailing
newlines normalized to one — into a temp file that is mounted instead,
so lemon still only ever sees the single `/system-prompt.md`. The temp
file dies with the session (the EXIT trap removes it); a single
`--system` file is mounted directly, no temp file.

With no `--system` given, `LEMON.md` / `AGENTS.md` / `CLAUDE.md` in the
working directory — where lemonade was invoked, any subdirectory,
deliberately not the top-level — is detected and pushed in, in that
order: these are where per-project agent instructions already
conventionally live, so the common case needs no flag. An explicit
`--system` always wins, and a missing file is a clear error before
anything is pulled or built.

A first `--system` file is pushed in unconditionally, before whatever the
above chose (or alone, when nothing was chosen): a short description of
the environment the agent runs in, embedded in the script, written to a
temp file, and mounted read-only at `/system-prompt-env.md`. It tells
the agent that it is root in a disposable Fedora container, that `/work`
is a copy whose git state is carried back at session end (ignored files
are not), that a full toolchain is preinstalled, and that it is free to
do whatever it wants — including installing more packages with dnf or
pip, whose installs die with the container. It is passed first, so in
the concatenated prompt the environment precedes the user's file.

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
- The sidecar and network names carry the script's PID
  (`lemonade-searxng-<pid>`, `lemonade-<pid>`), so sessions in
  different checkouts never collide. The flip side: a run killed
  without its EXIT trap (SIGKILL) leaves its sidecar running, and no
  later run removes it; `podman rm -f lemonade-searxng-<pid>` does.
- Bind mounts carry the `:z` SELinux label: a no-op on hosts without
  SELinux, and what an enforcing host needs. The worktree's `:O` overlay
  mount takes no relabel option; whether it works on an enforcing host
  is untested.
- The image assumes the host lemon binary is glibc/x86_64, matching
  Fedora.
- The container has full network by default; the container boundary plus
  the git gate are the controls, not network isolation.
- A worktree directory that exists without a `.git` (interrupted clone,
  partial deletion) aborts the run with a message; `rm -rf` it — or run
  with `--force-fresh` — to proceed.
- One session per checkout: the worktree hash is keyed on the path, so
  two lemonade runs from the same checkout share one worktree and one
  export directory. Each starts from the worktree as it was, and each
  import replaces the worktree's state with its own session's: the
  session that ends last wins, and the other's work is lost (a session
  starting while another runs also clears the export directory under
  it). Running two concurrently is unsupported by design; there is
  deliberately no guard.
- `podman run` uses `-it`: a TTY is assumed, and for now that is a hard
  requirement, because lemon itself requires a TTY — headless or piped
  invocations cannot work. If lemon ever runs without a TTY, making `-t`
  conditional on `[ -t 0 ]` (three lines) is the next step; it changes
  nothing for interactive use.
