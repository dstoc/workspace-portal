# Proposal: confine the daemon's host path resolution beneath the entry target

## Motivation

A workspace-portal entry maps a top-level name to one host directory. The
guarantee that makes this "controlled" exposure is that the **daemon never
reads or writes anything outside that target directory**. Everything the daemon
serves into the workspace — and therefore into a container that bind-mounts the
workspace — must come from beneath `entry.target`.

Today the daemon reconstructs host paths by lexical join and operates on them
with symlink-following libc calls, without re-checking that the result is still
beneath the entry root. A backing-store mutation between when a path component
was validated and when the daemon acts on it can make the daemon follow a
symlink out of the entry and serve a host path into the container. This proposal
closes that window.

## Threat model (and what is explicitly *not* the threat)

**Out of scope — container-side symlink resolution is already safe.** When the
daemon encounters a symlink it does not follow it; `readlink`
(`src/fs/callbacks.rs:1087`) returns the link text and the *consumer's* kernel
resolves it. An absolute symlink such as `escape -> /` therefore resolves
against the **consumer's** root. A container reading
`/work/ws/escape/etc/hostname` reads the *container's* `/etc/hostname`, never the
host's; `..`-climbing links are likewise bounded by the consumer's mount
namespace. This is correct behavior and nothing here changes it. (An earlier
framing of this proposal incorrectly claimed the daemon "follows" such links —
it does not.)

**In scope — the daemon being raced into reading outside the entry.** The daemon
runs on the host and resolves host paths itself for non-symlink operations. It
caches `inode → PortalPath` and, on a later request for that inode, rebuilds the
host path as `entry.target.join(relative)` (`src/fs/resolve.rs:27`) and acts on
it with libc calls that follow symlinks in every path component:

- `open` / `create` open the joined path with `OpenOptions::open`
  (`src/fs/callbacks.rs:140`, `:675`) — follows symlinks.
- `lookup` / `getattr` `lstat` it (`fs::symlink_metadata`,
  `src/fs/callbacks.rs:240`, `:299`); `lstat` does not follow the *final*
  component but **does follow every intermediate** one.
- `readdir` reads the joined dir (`fs::read_dir`, `:84`).
- `setattr` chmod/utimens, `mkdir`, `symlink`, `unlink`, `rmdir`, `link`,
  `rename`, `readlink` operate on the joined path or its parent.

In normal operation each intermediate component was validated as a real
directory during the per-component `lookup`, so the join is safe. The hole is
**TOCTOU**: an actor with write access to the entry's backing directory (for
example, another process writing into `/home/me/exposed`) can, after the daemon
has cached an inode for `/ws/sub/file`, replace `sub` with a symlink to `/etc`.
The next `open`/`stat`/`write` for that inode rebuilds `/home/me/exposed/sub/file`,
follows the swapped `sub` to `/etc/file`, and the daemon reads a **host** file
and serves its bytes into the container. The same applies to write-side ops
(creating or renaming through a swapped component lands outside the entry).

This is the limitation `docs/workspace-portal.md` records as "symlink
confinement is not yet hardened with `openat2`-style resolution" (§Known current
limits; §Near-term future work).

## Proposal

Make "every daemon host operation resolves strictly beneath `entry.target`" a
hard invariant enforced by the kernel at resolution time, rather than an
emergent property of per-component lookup that a race can break.

Hold the entry target as a directory file descriptor and resolve the
entry-relative path against it with `openat2(2)` using
`RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`. `RESOLVE_BENEATH` fails the
resolution (`EXDEV`) the instant any component — including a symlink swapped in
by a racing writer, or a `..` — would leave the base directory. A symlink that
stays *within* the entry still resolves, so legitimate in-entry links are
unaffected; only escapes are refused. Because the fd pins the entry root by
inode, swapping the root's *path* afterward cannot redirect resolution.

### Resolution helper

Add one entry point in `src/fs/resolve.rs` (or a new `src/fs/safe_open.rs`) that
every host-touching callback routes through:

```rust
/// Open `relative` beneath `entry_root`, refusing any component (symlink, `..`,
/// or magic link) that escapes `entry_root`. `flags`/`mode` are for the leaf.
fn open_beneath(entry_root: &Path, relative: &Path, flags: libc::c_int, mode: libc::mode_t)
    -> Result<OwnedFd>;
```

It opens `entry_root` once with `O_PATH | O_DIRECTORY | O_NOFOLLOW` (so a root
that has itself been swapped to a symlink fails closed), then issues a single
`openat2` for `relative`:

```rust
open_how {
    flags: flags as u64,
    mode:  mode as u64,
    resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
}
```

`openat2` is called via `libc::syscall(libc::SYS_openat2, ...)`, matching the
codebase's existing direct-libc usage for `utimensat`/`futimens`/`statvfs`
(`src/fs/callbacks.rs:481`, `:491`, `:1402`). No new dependency.

`RESOLVE_NO_SYMLINKS` (reject *all* symlinks, not just escaping ones) is a
stricter alternative — defensible because the daemon should never need to
traverse a symlink component (the consumer's kernel already resolved any
legitimate ones) — but it interacts awkwardly with `O_PATH | O_NOFOLLOW` stats
of a symlink *leaf*. `RESOLVE_BENEATH` is the recommended default; it satisfies
the requirement (nothing outside the target is reachable) without that edge.

### Routing the callbacks through it

- **`open` / `create`** — `openat2` for the leaf, replacing `OpenOptions::open`.
- **`lookup`, `getattr`, `directory_attr`, `file_attr`, `readdir` child stats** —
  open the leaf `O_PATH | O_NOFOLLOW` beneath the root, then
  `fstatat(fd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW)`. This preserves today's
  "don't follow the final component" stat semantics while confining the
  intermediates that the TOCTOU abuses.
- **`readdir` body** — open the directory fd beneath the root, then
  `fdopendir`/iterate from that fd instead of `fs::read_dir` on a joined path.
- **`mkdir`, `symlink`, `unlink`, `rmdir`, `link`, `rename`, `readlink`,
  `setattr` (chmod/utimens)** — resolve the *parent* directory fd beneath the
  root, then use the `*at` form (`mkdirat`, `symlinkat`, `unlinkat`,
  `renameat`, `readlinkat`, `fchmodat`/`utimensat` with `AT_SYMLINK_NOFOLLOW`)
  on the final single component. The leaf is never followed.

`rename` and `link` already require both endpoints to resolve to the *same*
entry (cross-entry rename rejected at `src/fs/resolve.rs:86`, cross-entry link at
`src/fs/callbacks.rs:990`), so one base fd serves both sides.

Opening the root fd per operation is one extra syscall; caching a per-entry root
`O_PATH` fd in `FuseRuntime`, invalidated when an entry is removed or retargeted
(generation change, `src/state.rs:171`), is a straightforward later optimization
and is not required for correctness.

### Failure behavior

An escaping resolution returns `EXDEV` (or `ELOOP` for a swapped-in root); the
callback maps it to an errno for the caller and serves nothing. This is the
fail-closed default appropriate for a confinement boundary.

### Kernel requirement

`openat2` requires Linux 5.6 (2020); the daemon already targets Linux + `fuse3`.
If `openat2` returns `ENOSYS`, the daemon **fails closed** — it refuses to mount
and logs the kernel requirement — rather than silently reverting to the
unconfined join. A probe `openat2` on the workspace dir at mount time makes this
a single clear error instead of per-request failures.

## Non-goals

- Changing how the *consumer's* kernel resolves symlinks. That is already
  namespace-correct (see Threat model) and unchanged.
- Confining symlink *content* written via `symlink(2)` — bytes are still stored
  verbatim, consistent with `docs/proposals/symlink-creation.md`.
- Hiding or rewriting symlinks that point outside the entry. They remain visible
  via `readlink`; only the *daemon's own* resolution is confined.
- Subtree-level read-only rules inside an entry — separate (§Near-term future
  work).

## Verification

A path-based "swap then re-read" check is unreliable and must not be used: with
`TTL = 0` (`src/fs.rs:27`) the kernel re-issues `lookup` for each component, the
daemon reports the swapped component as a symlink, and the *consumer's* kernel
resolves it — so the daemon's vulnerable join never runs and the check passes
for the wrong reason. Two reliable checks instead:

1. **Unit test of the resolver (primary gate).** Construct, on disk, the
   already-swapped state — `root/sub` is a symlink pointing outside `root`, plus
   an in-entry `root/ok -> ./real` — and call the new `open_beneath(root, …)`
   helper directly. Assert `open_beneath(root, "sub/file", …)` returns `EXDEV`
   and `open_beneath(root, "ok", …)` succeeds. Deterministic, no FUSE, no
   kernel-cache timing; this is what the success criteria hang on.

2. **End-to-end held-fd test.** The daemon resolves by *cached inode* (no
   re-walk) for operations on an already-open handle, which is the reachable
   trigger. The probe must be `fstat` on a held *file* handle, not `readdir`:
   `getattr` (`src/fs/callbacks.rs:333`) ignores the file handle and re-derives
   `entry.target.join(relative)` from the cached inode, and with `TTL = 0` every
   `fstat` re-queries the daemon, so no kernel cache sits in front of it. (A
   `readdir` probe gives a false pass — the kernel issues a readahead `READDIR`
   at `opendir` time, *before* the swap, and serves the iterator from that
   pre-swap cache.) `fuse_e2e_backing_store_swap_stays_confined_to_entry` in
   `tests/fuse_e2e.rs`: open and hold a file handle to `docs/sub/probe` (so the
   daemon caches `/docs/sub/probe`), then on the backing store replace `sub`
   with a symlink to a directory *outside* the entry target that contains a
   `probe` of a distinct size, then `fstat` the held handle. On current code the
   daemon follows the swapped `sub` and returns the outside file's size; the
   test asserts the outside size is never served. Contents cannot leak through
   the held fd (it was opened pre-swap against the in-entry file), so this is a
   metadata-confinement check. This test is added now and is expected to **fail
   against current code** (demonstrating the gap is reachable) and pass once the
   resolver is confined.

3. **No regression.** Normal read/write/readdir/create/rename and legitimate
   in-entry symlink traversal are unchanged — the existing E2E suite passes,
   including `fuse_e2e_symlinks_cover_traversal_and_broken_targets`.

## Success criteria

- After a backing-store component is swapped to an escaping symlink, the daemon
  serves an errno, never a path outside the entry target, for read, stat, open,
  readdir, write, create, rename, and remove.
- In-entry symlinks and normal operations are unaffected.
- On a kernel without `openat2`, the daemon refuses to start rather than
  mounting unconfined.
- `docs/workspace-portal.md` no longer lists daemon-side path confinement as a
  known limit; the (already-correct) container-side symlink behavior is
  documented as intended.
